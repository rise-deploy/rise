pub mod auth;
#[cfg(feature = "backend")]
pub mod bootstrap;
pub mod custom_domains;
pub mod deployment;
#[cfg(feature = "backend")]
pub mod ecr;
pub mod encryption;
pub mod env_vars;
pub mod environments;
pub mod error;
pub mod extensions;
pub mod frontend;
pub mod middleware;
pub mod oci;
pub mod platform;
pub mod project;
pub mod quickstart;
pub mod rate_limit;
pub mod registry;
#[cfg(feature = "backend")]
pub mod resources;
pub mod service_accounts;
pub mod settings;
pub mod ssrf;
pub mod state;
pub mod team;
pub mod workload_tokens;

use anyhow::Result;
use axum::{extract::Request, middleware as axum_middleware, response::Response, Router};
use state::{AppState, ControllerState};
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{classify::ServerErrorsFailureClass, trace::TraceLayer};
use tracing::{info, Span};

/// Build the standard trace layer used for request logging on both the main
/// server and the internal webhook listener.
macro_rules! trace_layer {
    () => {
        TraceLayer::new_for_http()
            .on_request(|request: &Request, _span: &Span| {
                let request_id = request
                    .extensions()
                    .get::<self::middleware::RequestId>()
                    .map(|rid| rid.0);
                let path = request.uri().path();
                tracing::info!(
                    method = %request.method(),
                    path = %path,
                    request_id = ?request_id,
                    "request started"
                );
            })
            .on_response(
                |response: &Response, latency: std::time::Duration, _span: &Span| {
                    let status = response.status();
                    let latency_ms = latency.as_millis();
                    let request_id = response
                        .headers()
                        .get("x-request-id")
                        .and_then(|h| h.to_str().ok());

                    if status.is_server_error() {
                        tracing::error!(
                            status = %status,
                            latency_ms = %latency_ms,
                            request_id = ?request_id,
                            "request completed with server error"
                        );
                    } else if status.is_client_error() {
                        tracing::warn!(
                            status = %status,
                            latency_ms = %latency_ms,
                            request_id = ?request_id,
                            "request completed with client error"
                        );
                    } else {
                        tracing::info!(
                            status = %status,
                            latency_ms = %latency_ms,
                            request_id = ?request_id,
                            "request completed successfully"
                        );
                    }
                },
            )
            .on_failure(
                |failure: ServerErrorsFailureClass,
                 latency: std::time::Duration,
                 _span: &Span| {
                    let latency_ms = latency.as_millis();
                    tracing::error!(
                        classification = ?failure,
                        latency_ms = %latency_ms,
                        "request failed unexpectedly"
                    );
                },
            )
    };
}

/// Run the HTTP server process with all enabled controllers
pub async fn run_server(settings: settings::Settings) -> Result<()> {
    let state = AppState::new(&settings).await?;

    // Construct ControllerState from AppState components for sharing with controllers
    let controller_state = ControllerState {
        db_pool: state.db_pool.clone(),
        encryption_provider: state.encryption_provider.clone(),
    };

    // Reconcile RiseProject CRDs as a background task — see
    // `backfill_rise_projects` for the full set of scenarios. The task is
    // non-blocking so the HTTP server starts immediately; projects are upserted
    // at a steady rate to avoid flooding the Kubernetes API and Metacontroller.
    #[cfg(feature = "backend")]
    if let Some(ref kube_client) = state.kube_client {
        use settings::DeploymentControllerSettings;
        let interval_ms = match &settings.deployment_controller {
            Some(DeploymentControllerSettings::Kubernetes {
                crd_upsert_interval_ms,
                ..
            }) => *crd_upsert_interval_ms,
            _ => 1000,
        };
        let kube_client = kube_client.clone();
        let db_pool = state.db_pool.clone();
        let controller_class = state.deployment_controller_class_name.clone();
        tokio::spawn(async move {
            if let Err(e) = deployment::crd::backfill_rise_projects(
                &kube_client,
                &db_pool,
                controller_class.as_deref(),
                std::time::Duration::from_millis(interval_ms),
            )
            .await
            {
                tracing::warn!("Failed to backfill RiseProject CRDs: {:?}", e);
            }
        });
    }

    // Spawn enabled controllers as background tasks
    let mut controller_handles = vec![];

    // Cooperative shutdown signal for all background controllers and extension
    // loops. Created in `AppState::new` (extensions start there) and shared here.
    let shutdown = state.shutdown.clone();

    // Cancel the shared token the moment a SIGINT/SIGTERM arrives, so the HTTP
    // drain and the controllers'/extensions' lease release proceed concurrently
    // — rather than the controllers holding their leases for the whole HTTP
    // drain and only stopping once `axum::serve` returns.
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            info!("Shutdown signal received; stopping background tasks");
            shutdown.cancel();
        });
    }

    // Start project controller (always enabled)
    info!("Starting project controller");
    let controller_state_clone = controller_state.clone();
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) =
            project::ProjectController::run(Arc::new(controller_state_clone), shutdown_clone).await
        {
            tracing::error!("Project controller error: {:#}", e);
        }
    });
    controller_handles.push(handle);

    // Start ECR controller if ECR registry is configured (requires aws feature)
    #[cfg(feature = "backend")]
    if let Some(settings::RegistrySettings::Ecr { .. }) = &settings.registry {
        info!("Starting ECR controller");
        let settings_clone = settings.clone();
        let controller_state_clone = controller_state.clone();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) =
                run_ecr_controller_loop(controller_state_clone, settings_clone, shutdown_clone)
                    .await
            {
                tracing::error!("ECR controller error: {:#}", e);
            }
        });
        controller_handles.push(handle);
    }

    // Start the resource GC worker. Always on under the backend feature: the
    // worker drains tombstoned rows produced by cascading deletes; without it,
    // tombstoned subtrees never collect. A small wrapper task awaits the
    // shutdown signal and releases the leader lease so a peer replica can
    // acquire immediately rather than waiting for the lease TTL to decay.
    #[cfg(feature = "backend")]
    {
        info!("Starting resource GC controller");
        let store = state.resource_store.clone();
        let pool = state.db_pool.clone();
        let gc_settings = settings.resource_gc.clone().unwrap_or_default();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) =
                resources::gc::ResourceGcController::run(pool, store, gc_settings, shutdown_clone)
                    .await
            {
                tracing::error!("Resource GC controller error: {:#}", e);
            }
        });
        controller_handles.push(handle);
    }

    // Start Entra active sync if configured
    if let Some(settings::ActiveSyncSource::Entra) = &settings.auth.active_sync_source {
        info!("Starting Entra ID active sync");
        let pool = state.db_pool.clone();
        let auth_settings = settings.auth.clone();
        let default_organization_uid = state.default_organization_uid;
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            auth::entra_sync::run_entra_sync_loop(
                pool,
                auth_settings,
                default_organization_uid,
                shutdown_clone,
            )
            .await;
        });
        controller_handles.push(handle);
    }

    // Public routes (no authentication)
    let public_routes = Router::new()
        .route("/health", axum::routing::get(health_check))
        .route("/version", axum::routing::get(version_info))
        .route(
            "/schema/rise-toml/v1",
            axum::routing::get(rise_toml_schema_v1_redirect),
        )
        // Platform capabilities are non-sensitive and must be reachable by
        // project service accounts (which have no project context here), so the
        // route is public.
        .merge(platform::routes::routes())
        .merge(auth::routes::public_routes())
        .merge(workload_tokens::routes::routes());

    // Auth-only routes (require authentication but NOT platform access)
    let auth_only_routes = Router::new()
        .route("/logs/capabilities", axum::routing::get(logs_capabilities))
        .merge(auth::routes::auth_only_routes())
        // Apply auth middleware only
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            auth::middleware::auth_middleware,
        ));

    // Platform routes (require authentication AND platform access)
    let platform_routes = Router::new()
        .merge(auth::routes::platform_routes())
        .merge(custom_domains::routes())
        .merge(project::routes::routes())
        .merge(team::routes::team_routes())
        .merge(registry::routes::routes())
        .merge(deployment::routes::deployment_routes())
        .merge(service_accounts::routes::routes())
        .merge(env_vars::routes::routes())
        .merge(environments::routes::routes())
        .merge(extensions::routes::routes())
        .merge(encryption::routes::routes())
        .merge(quickstart::routes::routes());

    #[cfg(feature = "backend")]
    let platform_routes = platform_routes.merge(resources::routes::routes());

    let platform_routes = platform_routes
        // Apply platform access middleware (runs second, after auth)
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            auth::middleware::platform_access_middleware,
        ))
        // Apply auth middleware (runs first)
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            auth::middleware::auth_middleware,
        ));

    // Nest all API routes under /api/v1
    let api_routes = public_routes.merge(auth_only_routes).merge(platform_routes);

    let app = Router::new()
        .nest("/api/v1", api_routes)
        // Well-known routes at root level (per OIDC spec)
        .merge(auth::routes::well_known_routes())
        // Root-level auth routes for custom domain support via Ingress routing
        .merge(auth::routes::rise_auth_routes())
        // OAuth/OIDC routes at root level (before frontend fallback)
        .merge(extensions::providers::oauth::routes::oauth_routes())
        .merge(frontend::routes::docs_routes())
        .merge(frontend::routes::frontend_routes())
        .with_state(state.clone())
        .layer(
            ServiceBuilder::new()
                // TraceLayer is added first so it wraps inner; request_id_middleware
                // is added last so it runs outermost (inserts RequestId before TraceLayer reads it)
                .layer(trace_layer!())
                .layer(axum_middleware::from_fn(
                    self::middleware::request_id_middleware,
                )),
        );

    let addr = format!("{}:{}", settings.server.host, settings.server.port);
    info!("HTTP server listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    // Spawn internal webhook listener for Metacontroller (separate port, IP-validated)
    #[cfg(feature = "backend")]
    let webhook_handle = if let Some(port) = state.metacontroller_webhook_port {
        let webhook_app = Router::new()
            .nest("/api/v1", deployment::routes::metacontroller_routes())
            .with_state(state.clone())
            .layer(trace_layer!())
            .layer(axum_middleware::from_fn(
                self::middleware::request_id_middleware,
            )); // request_id_middleware outermost → inserts ID before TraceLayer reads it

        let webhook_addr = format!("{}:{}", settings.server.host, port);
        info!("Metacontroller webhook listener on http://{}", webhook_addr);
        let webhook_listener = tokio::net::TcpListener::bind(&webhook_addr).await?;

        let webhook_shutdown = shutdown.clone();
        Some(tokio::spawn(async move {
            axum::serve(
                webhook_listener,
                webhook_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move { webhook_shutdown.cancelled().await })
            .await
        }))
    } else {
        None
    };

    // Graceful shutdown driven off the shared token (cancelled by the signal
    // watcher above), so the HTTP drain and lease release run concurrently.
    axum::serve(listener, app)
        .with_graceful_shutdown({
            let shutdown = shutdown.clone();
            async move { shutdown.cancelled().await }
        })
        .await?;

    #[cfg(feature = "backend")]
    if let Some(handle) = webhook_handle {
        let _ = handle.await;
    }

    info!("HTTP server shutdown complete");

    // Wait for all controller tasks AND the extension reconciliation tasks to
    // complete. Each observes the cancelled token, breaks its loop, and releases
    // its leader lease before its handle resolves — so a peer replica can take
    // over promptly instead of waiting out the lease TTL.
    controller_handles.extend(std::mem::take(
        &mut *state.extension_handles.lock().unwrap(),
    ));
    for handle in controller_handles {
        let _ = handle.await;
    }

    Ok(())
}

/// Run the ECR controller loop (for embedding in server process)
///
/// Manages ECR repository lifecycle:
/// - Creates repositories for new projects
/// - Cleans up repositories when projects are deleted
#[cfg(feature = "backend")]
async fn run_ecr_controller_loop(
    controller_state: ControllerState,
    settings: settings::Settings,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use crate::server::registry::models::EcrConfig;
    use crate::server::settings::RegistrySettings;

    // Extract ECR config from registry settings
    let ecr_config = match &settings.registry {
        Some(RegistrySettings::Ecr {
            region,
            account_id,
            repo_prefix,
            push_role_arn,
            auto_remove,
            access_key_id,
            secret_access_key,
        }) => EcrConfig {
            region: region.clone(),
            account_id: account_id.clone(),
            repo_prefix: repo_prefix.clone(),
            push_role_arn: push_role_arn.clone(),
            auto_remove: *auto_remove,
            access_key_id: access_key_id.clone(),
            secret_access_key: secret_access_key.clone(),
        },
        _ => {
            anyhow::bail!("ECR controller requires ECR registry configuration");
        }
    };
    let manager = Arc::new(ecr::EcrRepoManager::new(ecr_config).await?);

    info!("ECR controller started");
    ecr::EcrController::run(Arc::new(controller_state), manager, shutdown).await
}

async fn health_check() -> &'static str {
    "OK"
}

async fn version_info() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "repository": env!("CARGO_PKG_REPOSITORY"),
    }))
}

/// Static facts about the configured runtime log backend. Surfaced to the
/// frontend so the level filter dropdown and chart palette can be driven
/// dynamically instead of hardcoded to info/warn/error.
async fn logs_capabilities(
    axum::extract::State(state): axum::extract::State<state::AppState>,
) -> axum::Json<deployment::logs::LogsCapabilities> {
    let backend = state.runtime_log_backend.as_ref();
    axum::Json(deployment::logs::LogsCapabilities {
        backend: backend.backend_kind(),
        levels: backend.levels(),
        supports_volume: backend.supports_volume(),
        max_tail: backend.max_tail(),
    })
}

async fn rise_toml_schema_v1_redirect() -> impl axum::response::IntoResponse {
    axum::response::Redirect::permanent("/docs/schemas/rise-toml-v1.schema.json")
}

/// Wait for a shutdown signal (SIGTERM or SIGINT)
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received SIGINT (Ctrl+C), shutting down gracefully");
        },
        _ = terminate => {
            info!("Received SIGTERM, shutting down gracefully");
        },
    }
}
