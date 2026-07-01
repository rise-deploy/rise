pub mod auth;
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
pub mod project;
pub mod rate_limit;
pub mod registry;
pub mod settings;
pub mod ssrf;
pub mod state;
pub mod team;
pub mod workload_identity;

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

/// Flag the misconfig where a cluster-wide `external_pull_secret_name` can pull
/// any image on a shared external registry but `external_registry_hosts` is
/// empty — meaning the backend validates arriving images as third-party
/// (allowed unchecked) rather than as ours-in-spirit (project-scope enforced).
fn warn_if_external_registry_hosts_missing(settings: &settings::Settings) {
    let pull_secret_set = matches!(
        &settings.deployment_controller,
        Some(settings::DeploymentControllerSettings::Kubernetes {
            external_pull_secret_name: Some(_),
            ..
        })
    );
    if !pull_secret_set {
        return;
    }
    if settings.external_registry_hosts.is_empty() {
        tracing::warn!(
            "deployment_controller.external_pull_secret_name is set but \
             external_registry_hosts is empty — cross-project image \
             substitution on the shared external registry is unchecked."
        );
    }
}

/// Run the HTTP server process with all enabled controllers
pub async fn run_server(settings: settings::Settings) -> Result<()> {
    let state = AppState::new(&settings).await?;

    warn_if_external_registry_hosts_missing(&settings);

    // Construct ControllerState from AppState components for sharing with controllers
    let controller_state = ControllerState {
        db_pool: state.db_pool.clone(),
        encryption_provider: state.encryption_provider.clone(),
    };

    // Backfill missing RiseProject CRDs (upgrade migration + recovery)
    #[cfg(feature = "backend")]
    if let Some(ref kube_client) = state.kube_client {
        if let Err(e) = deployment::crd::backfill_rise_projects(kube_client, &state.db_pool).await {
            tracing::warn!("Failed to backfill RiseProject CRDs: {:?}", e);
        }
    }

    // Spawn enabled controllers as background tasks
    let mut controller_handles = vec![];

    // Start project controller (always enabled)
    info!("Starting project controller");
    let settings_clone = settings.clone();
    let controller_state_clone = controller_state.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = run_project_controller_loop(controller_state_clone, settings_clone).await {
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
        let handle = tokio::spawn(async move {
            if let Err(e) = run_ecr_controller_loop(controller_state_clone, settings_clone).await {
                tracing::error!("ECR controller error: {:#}", e);
            }
        });
        controller_handles.push(handle);
    }

    // Start Entra active sync if configured
    if let Some(settings::ActiveSyncSource::Entra) = &settings.auth.active_sync_source {
        info!("Starting Entra ID active sync");
        let pool = state.db_pool.clone();
        let auth_settings = settings.auth.clone();
        let handle = tokio::spawn(async move {
            auth::entra_sync::run_entra_sync_loop(pool, auth_settings).await;
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
        .merge(auth::routes::public_routes());

    // Auth-only routes (require authentication but NOT platform access)
    let auth_only_routes = Router::new()
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
        .merge(workload_identity::routes::routes())
        .merge(env_vars::routes::routes())
        .merge(environments::routes::routes())
        .merge(extensions::routes::routes())
        .merge(encryption::routes::routes())
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

        Some(tokio::spawn(async move {
            axum::serve(
                webhook_listener,
                webhook_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await
        }))
    } else {
        None
    };

    // Graceful shutdown support
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    #[cfg(feature = "backend")]
    if let Some(handle) = webhook_handle {
        let _ = handle.await;
    }

    info!("HTTP server shutdown complete");

    // Wait for all controller tasks to complete
    for handle in controller_handles {
        let _ = handle.await;
    }

    Ok(())
}

/// Run the project controller loop (for embedding in server process)
async fn run_project_controller_loop(
    controller_state: ControllerState,
    _settings: settings::Settings,
) -> Result<()> {
    let controller = Arc::new(project::ProjectController::new(Arc::new(controller_state)));
    controller.start();
    info!("Project controller started");

    // Wait for shutdown signal
    shutdown_signal().await;
    info!("Project controller shutdown complete");
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
            external_registry_hosts: settings.external_registry_hosts.clone(),
        },
        _ => {
            anyhow::bail!("ECR controller requires ECR registry configuration");
        }
    };
    let manager = Arc::new(ecr::EcrRepoManager::new(ecr_config).await?);

    let controller = Arc::new(ecr::EcrController::new(Arc::new(controller_state), manager));
    controller.start();
    info!("ECR controller started");

    // Wait for shutdown signal
    shutdown_signal().await;
    info!("ECR controller shutdown complete");
    Ok(())
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
