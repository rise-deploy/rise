use crate::server::auth::{
    cookie_helpers::CookieSettings,
    jwt::JwtValidator,
    oauth::OAuthClient,
    token_storage::{DbTokenStore, TokenStore},
};
use crate::server::encryption::EncryptionProvider;
use crate::server::registry::{
    models::OciClientAuthConfig, providers::OciClientAuthProvider, RegistryProvider,
};
use rise_backend_auth::RiseTokenSigner;

use crate::db::models::User;
use crate::server::auth::controller::ControllerIdentity;
#[cfg(feature = "backend")]
use crate::server::registry::{
    models::{EcrConfig, GitLabRegistryConfig, JfrogConfig, JfrogTokenProvider},
    providers::{EcrProvider, GitLabRegistryProvider, JfrogProvider},
};
use crate::server::settings::{
    AuthSettings, EncryptionSettings, RegistrySettings, ServerSettings, Settings,
};
use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "backend")]
use crate::server::deployment::controller::DeploymentBackend;

/// Minimal state for controllers - database access and encryption
#[derive(Clone)]
pub struct ControllerState {
    pub db_pool: PgPool,
    #[allow(dead_code)]
    pub encryption_provider: Option<Arc<dyn EncryptionProvider>>,
    /// How much deployment history to keep. Carried here rather than read from
    /// a global because the retention pass runs in a controller, and a
    /// controller's inputs should be visible in the state it was handed.
    pub deployment_retention: crate::server::settings::DeploymentRetentionSettings,
}

/// Full state for HTTP server
#[derive(Clone)]
pub struct AppState {
    pub db_pool: PgPool,
    /// Cooperative shutdown signal for background controllers and extension
    /// reconciliation loops. Cancelled by `run_server` once a SIGINT/SIGTERM is
    /// received, so each leader-elected loop exits and releases its lease.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// JoinHandles of the extension reconciliation tasks (started here), so
    /// `run_server` can await their graceful lease release on shutdown alongside
    /// the core controllers. `Arc<Mutex<…>>` because `AppState` is `Clone`.
    pub extension_handles: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    pub jwt_validator: Arc<JwtValidator>,
    pub jwt_signer: Arc<RiseTokenSigner>,
    pub oauth_client: Arc<OAuthClient>,
    pub registry_provider: Arc<dyn RegistryProvider>,
    pub oci_client: Arc<crate::server::oci::OciClient>,
    pub admin_users: Arc<Vec<String>>,
    /// IdP groups whose members are admins (case-insensitive name match).
    pub admin_idp_groups: Arc<Vec<String>>,
    /// Operator role allowlist (case-insensitive email match).
    pub operator_users: Arc<Vec<String>>,
    /// IdP groups whose members hold the Operator role.
    pub operator_idp_groups: Arc<Vec<String>>,
    /// Controller identities keyed by `id`. Consumed by future generic
    /// resource endpoints; unused in PR3.
    #[allow(dead_code)]
    pub controllers: Arc<HashMap<String, ControllerIdentity>>,
    /// Controller identities indexed by issuer URL for middleware lookup.
    pub controllers_by_issuer: Arc<HashMap<String, Vec<ControllerIdentity>>>,
    /// Generic-resource store used by the `/api/v1/resources` HTTP API and
    /// (later) by internal controllers wanting to reconcile against Rise state
    /// without a network round-trip.
    #[cfg(feature = "backend")]
    pub resource_store: Arc<dyn rise_resource_api::ResourceStore>,
    /// The authorization choke point for the generic resource API (ADR-0001 §4,
    /// §5). It holds the same Postgres store as `resource_store`, kept concrete
    /// so an authorization-changing write can rebuild it over its own
    /// `SERIALIZABLE` transaction.
    #[cfg(feature = "backend")]
    pub resource_authorizer: crate::server::authz::ResourceAuthorizer,
    /// Resource UID of the default Organization. Populated by the bootstrap
    /// pass at startup; typed APIs use this to stamp newly created
    /// users/teams/projects with the configured default Organization.
    #[cfg(feature = "backend")]
    pub default_organization_uid: uuid::Uuid,
    /// `controller_class_name` for the configured Kubernetes deployment
    /// controller. The webhook only reconciles projects whose Organization's
    /// `spec.deploymentControllerClass` matches this value. `None` when no
    /// Kubernetes deployment controller is configured.
    #[cfg(feature = "backend")]
    pub deployment_controller_class_name: Option<String>,
    /// Per-Organization reads (controller class, namespace prefix) for the
    /// deployment backends, cached behind a short TTL.
    #[cfg(feature = "backend")]
    pub org_view: Arc<dyn rise_backend_core::OrganizationView>,
    pub auth_settings: Arc<AuthSettings>,
    pub server_settings: Arc<ServerSettings>,
    pub token_store: Arc<dyn TokenStore>,
    pub cookie_settings: CookieSettings,
    pub public_url: String,
    pub encryption_provider: Option<Arc<dyn EncryptionProvider>>,
    pub deployment_backend: Arc<dyn crate::server::deployment::controller::DeploymentBackend>,
    /// The deployment persistence boundary (`DeploymentStore` trait, implemented
    /// by `PgDeploymentStore`). The Metacontroller webhook and the
    /// identity-refresh/CRD-backfill controllers read and mutate deployment
    /// state through this trait rather than reaching into `crate::db` directly,
    /// so they can move into a backend crate later.
    #[cfg(feature = "backend")]
    pub deployment_store: Arc<dyn rise_backend_core::DeploymentStore>,
    #[cfg(feature = "backend")]
    pub runtime_log_backend: Arc<dyn crate::server::deployment::logs::RuntimeLogBackend>,
    pub extension_registry: Arc<crate::server::extensions::registry::ExtensionRegistry>,
    /// Rate limiter for encrypt endpoint (key: "encrypt:{user_id}", value: request count)
    pub encrypt_rate_limiter: Arc<moka::future::Cache<String, u32>>,
    /// Rate limiter for OAuth endpoints (token, authorize, callback)
    pub oauth_rate_limiter: Arc<crate::server::rate_limit::OAuthRateLimiter>,
    pub access_classes:
        Arc<std::collections::HashMap<String, crate::server::settings::AccessClass>>,
    /// Project-name labels reserved for control-plane hosts. Normalized to
    /// lowercase at startup because DNS hostnames are case-insensitive.
    pub reserved_project_names: Arc<std::collections::HashSet<String>>,
    /// Browser-facing base URL used to build the login redirect when the
    /// `ingress_auth` handler runs in Traefik mode (`signin_redirect=1`).
    /// Sourced from the Docker controller's `auth_signin_url` (falling back to
    /// `public_url`); for other backends it is just `public_url`.
    pub signin_base_url: String,
    /// Production ingress URL template (for custom domain validation)
    pub production_ingress_url_template: Option<String>,
    /// Staging ingress URL template (for custom domain validation)
    pub staging_ingress_url_template: Option<String>,
    /// Environment ingress URL template (for custom domain validation)
    pub environment_ingress_url_template: Option<String>,
    /// Ingress URL scheme (e.g., "https" or "http")
    pub ingress_schema: String,
    /// Optional ingress port (for development environments)
    pub ingress_port: Option<u16>,
    /// Native architecture accepted by the configured deployment runtime.
    ///
    /// Kubernetes derives this from its architecture node selector. Docker
    /// derives it from the connected daemon (which may be remote), rather than
    /// from the machine running the Rise process.
    #[cfg(feature = "backend")]
    pub runtime_arch: Option<String>,
    /// Kubernetes client for direct API calls (pod health checks, log streaming)
    #[cfg(feature = "backend")]
    pub kube_client: Option<kube::Client>,
    /// Port for the internal metacontroller webhook listener
    #[cfg(feature = "backend")]
    pub metacontroller_webhook_port: Option<u16>,
    /// Everything the Metacontroller webhook handlers need, or `None` when the
    /// Kubernetes controller is not configured.
    #[cfg(feature = "backend")]
    pub webhook_ctx: Option<Arc<crate::server::deployment::webhook::WebhookContext>>,
    /// Default resource values for new deployments
    #[cfg(feature = "backend")]
    pub deployment_defaults: Option<crate::server::settings::DeploymentDefaults>,
    /// Platform-level constraints for deployment resources
    #[cfg(feature = "backend")]
    pub deployment_constraints: Option<crate::server::settings::DeploymentConstraints>,
    /// Lifetime in seconds of controller-minted workload identity tokens.
    /// Refresh threshold is derived as TTL / 2.
    #[cfg(feature = "backend")]
    pub identity_token_ttl_seconds: u64,
    /// Catalog of quickstart templates (resolved from config at startup).
    /// Empty when no operator-configured catalog is present.
    pub quickstart_templates: Arc<Vec<crate::server::quickstart::models::QuickstartTemplate>>,
}

/// Initialize encryption provider from settings
async fn init_encryption_provider(
    encryption_settings: Option<&EncryptionSettings>,
) -> Result<Option<Arc<dyn EncryptionProvider>>> {
    if let Some(encryption_config) = encryption_settings {
        match encryption_config {
            EncryptionSettings::Local { key } => {
                use crate::server::encryption::providers::local::LocalEncryptionProvider;
                let provider = LocalEncryptionProvider::new(key)
                    .context("Failed to initialize local encryption provider")?;

                // Test encryption/decryption at startup
                tracing::info!("Testing local encryption provider...");
                test_encryption_provider(&provider).await?;
                tracing::info!("✓ Local AES-256-GCM encryption provider initialized and validated");

                Ok(Some(Arc::new(provider)))
            }
            #[cfg(feature = "backend")]
            EncryptionSettings::AwsKms {
                region,
                key_id,
                access_key_id,
                secret_access_key,
            } => {
                use crate::server::encryption::providers::aws_kms::AwsKmsEncryptionProvider;
                let provider = AwsKmsEncryptionProvider::new(
                    region,
                    key_id.clone(),
                    access_key_id.clone(),
                    secret_access_key.clone(),
                )
                .await
                .context("Failed to initialize AWS KMS encryption provider")?;

                // Test encryption/decryption at startup
                tracing::info!("Testing AWS KMS encryption provider with key {}...", key_id);
                test_encryption_provider(&provider).await.with_context(|| {
                    format!(
                        "KMS provider initialized but encryption test failed. \
                         Please verify: 1) Key ARN/ID '{}' is valid, \
                         2) AWS credentials are available, \
                         3) IAM permissions include kms:Encrypt and kms:Decrypt, \
                         4) Key is enabled and not pending deletion",
                        key_id
                    )
                })?;
                tracing::info!("✓ AWS KMS encryption provider initialized and validated");

                Ok(Some(Arc::new(provider)))
            }
            #[cfg(not(feature = "backend"))]
            EncryptionSettings::AwsKms { key_id, .. } => {
                anyhow::bail!(
                    "AWS KMS encryption is configured (key: {}) but the 'aws' feature is not enabled. \
                     Please rebuild with --features aws or use a pre-built binary with AWS support.",
                    key_id
                )
            }
        }
    } else {
        tracing::info!("No encryption provider configured - secret environment variables will not be available");
        Ok(None)
    }
}

/// Test an encryption provider with a sample encrypt/decrypt round-trip
async fn test_encryption_provider(provider: &dyn EncryptionProvider) -> Result<()> {
    const TEST_PLAINTEXT: &str = "rise-encryption-test-12345";

    let ciphertext = provider
        .encrypt(TEST_PLAINTEXT)
        .await
        .context("Encryption test failed")?;

    let decrypted = provider
        .decrypt(&ciphertext)
        .await
        .context("Decryption test failed")?;

    if decrypted != TEST_PLAINTEXT {
        anyhow::bail!("Encryption round-trip test failed: decrypted value does not match original");
    }

    Ok(())
}

/// Initialize Kubernetes deployment backend from settings
///
/// Creates a slim `KubernetesBackend` that wraps `ResourceBuilder` and a kube client.
/// Provides URL computation, log streaming, and environment cleanup.
/// All reconciliation is handled by the Metacontroller sync webhook.
#[cfg(feature = "backend")]
async fn init_kubernetes_backend(
    resource_builder: Arc<crate::server::deployment::resource_builder::ResourceBuilder>,
    kube_client: kube::Client,
    store: Arc<dyn rise_backend_core::DeploymentStore>,
    controller_class: Option<String>,
) -> Result<Arc<dyn DeploymentBackend>> {
    use crate::server::deployment::controller::KubernetesBackend;

    let backend = KubernetesBackend::new(kube_client, resource_builder, store, controller_class);

    // Test Kubernetes API connection
    backend.test_connection().await?;
    tracing::info!("Kubernetes deployment backend initialized and connection tested");

    Ok(Arc::new(backend) as Arc<dyn DeploymentBackend>)
}

/// Extract the bare host from an `auth_backend_url` like `http://rise:3000`
/// (→ `rise`). Strips an optional `scheme://`, then a trailing `:port` and any
/// path. Returns `None` for an empty/host-less value. Kept dependency-free (no
/// `url` crate, which is `cli`-feature-gated) since the value is a simple
/// internal service URL.
#[cfg(feature = "backend")]
fn backend_host_from_url(auth_backend_url: &str) -> Option<String> {
    let trimmed = auth_backend_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Drop scheme.
    let after_scheme = match trimmed.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => trimmed,
    };
    // Authority ends at the first '/'.
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    // Strip userinfo if present.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    // Strip the port. Naive on bracketed IPv6 literals, which we don't expect
    // for an internal service URL.
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// LOCAL-DEV helper: resolve the Rise backend's IP on the shared Docker network
/// by extracting the host from `auth_backend_url` (e.g. `http://rise:3000` →
/// `rise`) and looking it up via the system resolver from inside this container.
///
/// Inside the rise-backend container, Docker DNS resolves the backend's service
/// name (`rise`) to its address on `traefik_network` — exactly the IP app
/// containers must alias the public issuer host to. Returns `None` on any parse
/// or resolution failure so the caller can skip injection gracefully.
#[cfg(feature = "backend")]
async fn resolve_backend_ip(auth_backend_url: &str) -> Option<String> {
    let host = backend_host_from_url(auth_backend_url)?;

    // Resolve via the system resolver. Append a dummy port so `lookup_host`
    // accepts the input; we only care about the IP. Prefer the first IPv4
    // address (Docker's default bridge networks are IPv4).
    let lookup = tokio::net::lookup_host((host.as_str(), 0)).await;
    match lookup {
        Ok(addrs) => {
            let mut first: Option<String> = None;
            for addr in addrs {
                let ip = addr.ip();
                if ip.is_ipv4() {
                    return Some(ip.to_string());
                }
                first.get_or_insert_with(|| ip.to_string());
            }
            first
        }
        Err(e) => {
            tracing::debug!(host = %host, "DNS lookup for backend IP failed: {:?}", e);
            None
        }
    }
}

/// Initialize the Docker deployment backend.
///
/// Builds a `ResourceBuilder` from the Docker variant's URL templates (the
/// Kubernetes-only fields are left empty/None), connects bollard, constructs the
/// `DockerBackend`, tests connectivity, and spawns the in-process
/// `DockerReconciler`. Returns the backend plus the bollard client so the log
/// backend can reuse it.
/// Initialize the ECS deployment backend and spawn its reconcile loop.
///
/// Mirrors [`init_docker_backend`]: build the backend-agnostic URL resolver,
/// connect the runtime client, verify reachability so a misconfiguration is a
/// startup error rather than a silently idle loop, then spawn the leader-elected
/// reconciler. Returns the backend plus the reconciler's join handle so
/// `run_server` can await a graceful lease release on shutdown.
#[cfg(feature = "backend")]
#[allow(clippy::too_many_arguments)]
async fn init_ecs_backend(
    settings: &crate::server::settings::DeploymentControllerSettings,
    // AWS account of the configured ECR registry, when the install uses one.
    // Checked against the ECS credentials' own account at startup.
    ecr_account_id: Option<&str>,
    registry_provider: Arc<dyn RegistryProvider>,
    encryption_provider: Option<Arc<dyn EncryptionProvider>>,
    resource_store: Arc<dyn rise_resource_api::ResourceStore>,
    db_pool: PgPool,
    store: Arc<dyn rise_backend_core::DeploymentStore>,
    public_url: &str,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(
    Arc<dyn DeploymentBackend>,
    tokio::task::JoinHandle<()>,
    crate::server::deployment::logs::EcsCloudWatchContext,
)> {
    use crate::server::settings::DeploymentControllerSettings;
    use rise_backend_core::DeploymentUrlBuilder;
    use rise_backend_ecs::reconciler::{EcsReconciler, ReconcilerConfig};
    use rise_backend_ecs::{client as ecs_client, EcsBackend};

    let DeploymentControllerSettings::Ecs {
        region,
        endpoint_url,
        access_key_id,
        secret_access_key,
        cluster,
        subnets,
        security_groups,
        assign_public_ip,
        execution_role_arn,
        task_role_arn,
        repository_credentials_secret_arn,
        log_group,
        resource_prefix,
        ssm_parameter_prefix,
        ssm_kms_key_id,
        cpu_architecture,
        production_ingress_url_template,
        staging_ingress_url_template,
        environment_ingress_url_template,
        ingress_port,
        ingress_schema,
        access_classes,
        auth_backend_url,
        traefik_entrypoint,
        traefik_certresolver,
        traefik_api_url,
        label_namespace,
        controller_class_name,
        reconcile_interval_secs,
        health_probes,
        ..
    } = settings
    else {
        anyhow::bail!("init_ecs_backend called with a non-ECS deployment controller");
    };

    let url_builder = Arc::new(DeploymentUrlBuilder {
        production_ingress_url_template: production_ingress_url_template.clone(),
        staging_ingress_url_template: staging_ingress_url_template.clone(),
        environment_ingress_url_template: environment_ingress_url_template.clone(),
        ingress_port: *ingress_port,
        ingress_schema: ingress_schema.clone(),
        registry_provider: registry_provider.clone(),
    });

    let aws_config = ecs_client::load(&ecs_client::AwsConfig {
        region,
        endpoint_url: endpoint_url.as_deref(),
        access_key_id: access_key_id.as_deref(),
        secret_access_key: secret_access_key.as_deref(),
    })
    .await;
    let ecs = ecs_client::ecs(&aws_config);
    let ssm = ecs_client::ssm(&aws_config);

    let backend = EcsBackend::new(
        ecs.clone(),
        cluster.clone(),
        url_builder.clone(),
        store.clone(),
    );
    backend.test_connection().await?;
    if let Some(account_id) = ecr_account_id {
        rise_backend_ecs::verify_ecr_same_account(&ecs_client::sts(&aws_config), account_id)
            .await?;
    }
    tracing::info!(cluster = %cluster, region = %region, "ECS deployment backend initialized");

    let health_path = health_probes
        .as_ref()
        .map(|h| h.path.clone())
        .unwrap_or_else(|| "/".to_string());

    let access_requirements: HashMap<String, crate::server::settings::AccessRequirement> =
        access_classes
            .iter()
            .filter_map(|(name, ac)| {
                ac.as_ref()
                    .map(|ac| (name.clone(), ac.access_requirement.clone()))
            })
            .collect();

    // Fail CLOSED, exactly as the Docker backend does: without an
    // `auth_backend_url` there is no forwardAuth middleware to stamp, so a
    // project whose access class requires authentication would be served
    // publicly. Refuse to start rather than serve it.
    let offending = crate::server::settings::access_classes_missing_auth_backend_url(
        access_classes,
        auth_backend_url,
    );
    if !offending.is_empty() {
        anyhow::bail!(
            "ECS deployment backend: access class(es) [{}] require authentication \
             (Authenticated/Member) but `deployment_controller.auth_backend_url` is empty. \
             Traefik forwardAuth cannot be enforced, so those projects would be served \
             publicly. Set `deployment_controller.auth_backend_url` to a URL reachable \
             from inside the cluster (a Cloud Map name or internal load balancer — not \
             the public URL), or change the access requirement to None.",
            offending.join(", ")
        );
    }

    if traefik_api_url.is_none() {
        tracing::warn!(
            "ECS deployment backend: `deployment_controller.traefik_api_url` is not set. \
             Traefik's serverStatus is the authoritative readiness signal with no fallback, \
             so any project that declares a `health_check` will never become Healthy."
        );
    }

    let reconciler = EcsReconciler::new(
        ecs,
        ssm,
        store.clone(),
        db_pool,
        url_builder,
        encryption_provider,
        resource_store,
        ReconcilerConfig {
            cluster: cluster.clone(),
            region: region.clone(),
            subnets: subnets.clone(),
            security_groups: security_groups.clone(),
            assign_public_ip: *assign_public_ip,
            execution_role_arn: execution_role_arn.clone(),
            task_role_arn: task_role_arn.clone(),
            repository_credentials_secret_arn: repository_credentials_secret_arn.clone(),
            log_group: log_group.clone(),
            resource_prefix: resource_prefix.clone(),
            ssm_parameter_prefix: ssm_parameter_prefix.clone(),
            ssm_kms_key_id: ssm_kms_key_id.clone(),
            cpu_architecture: cpu_architecture.clone(),
            controller_class: controller_class_name.clone(),
            label_namespace: label_namespace.clone(),
            reconcile_interval_secs: *reconcile_interval_secs,
            health_path,
            public_url: public_url.to_string(),
            auth_backend_url: auth_backend_url.clone(),
            access_classes: access_requirements,
            traefik_entrypoint: traefik_entrypoint.clone(),
            traefik_certresolver: rise_backend_traefik::normalize_certresolver(
                traefik_certresolver.clone(),
            ),
            traefik_api_url: traefik_api_url.clone(),
        },
    );
    let reconciler_handle = reconciler.spawn(shutdown);

    Ok((
        Arc::new(backend) as Arc<dyn DeploymentBackend>,
        reconciler_handle,
        crate::server::deployment::logs::EcsCloudWatchContext {
            sdk_config: aws_config,
            region: region.clone(),
            log_group: log_group.clone(),
            resource_prefix: resource_prefix.clone(),
        },
    ))
}

#[cfg(feature = "backend")]
#[allow(clippy::too_many_arguments)]
async fn init_docker_backend(
    settings: &crate::server::settings::DeploymentControllerSettings,
    registry_provider: Arc<dyn RegistryProvider>,
    encryption_provider: Option<Arc<dyn EncryptionProvider>>,
    resource_store: Arc<dyn rise_resource_api::ResourceApi>,
    jwt_signer: Arc<RiseTokenSigner>,
    db_pool: PgPool,
    store: Arc<dyn rise_backend_core::DeploymentStore>,
    public_url: &str,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(
    Arc<dyn DeploymentBackend>,
    bollard::Docker,
    tokio::task::JoinHandle<()>,
)> {
    use crate::server::deployment::controller::docker::reconciler::{
        DockerReconciler, ReconcilerConfig,
    };
    use crate::server::deployment::controller::{docker::client, DockerBackend};
    use crate::server::settings::DeploymentControllerSettings;
    use rise_backend_core::DeploymentUrlBuilder;

    let DeploymentControllerSettings::Docker {
        docker_host,
        production_ingress_url_template,
        staging_ingress_url_template,
        environment_ingress_url_template,
        ingress_port,
        ingress_schema,
        traefik_network,
        traefik_entrypoint,
        traefik_certresolver,
        label_namespace,
        container_prefix,
        controller_class_name,
        reconcile_interval_secs,
        health_probes,
        access_classes,
        auth_backend_url,
        app_backend_host_aliases,
        app_backend_ip: app_backend_ip_override,
        publish_app_ports,
        traefik_api_url,
        identity_token_ttl_seconds,
        ..
    } = settings
    else {
        return Err(anyhow::anyhow!(
            "init_docker_backend called with a non-Docker controller variant"
        ));
    };

    // URL/image resolver for the Docker runtime — the backend-agnostic subset
    // of deployment-spec computation the Docker controller needs. The K8s-only
    // resource-spec fields don't apply here.
    let url_builder = Arc::new(DeploymentUrlBuilder {
        production_ingress_url_template: production_ingress_url_template.clone(),
        staging_ingress_url_template: staging_ingress_url_template.clone(),
        environment_ingress_url_template: environment_ingress_url_template.clone(),
        ingress_port: *ingress_port,
        ingress_schema: ingress_schema.clone(),
        registry_provider: registry_provider.clone(),
    });

    // Connect bollard.
    let docker = client::connect(docker_host.as_deref())?;

    let backend = DockerBackend::new(docker.clone(), url_builder.clone(), store.clone());
    backend.test_connection().await?;
    tracing::info!("Docker deployment backend initialized and connection tested");

    // Spawn the in-process reconciler.
    let health_path = health_probes
        .as_ref()
        .map(|hp| hp.path.clone())
        .unwrap_or_else(|| "/".to_string());
    // The Docker backend keeps secret env vars as plain KEY=VALUE entries on the
    // container (visible via `docker inspect`) — an accepted, documented caveat.
    // Surface it once at startup so operators are aware.
    tracing::warn!(
        "Docker deployment backend: secret environment variables are stored as plain \
         container env and are visible via `docker inspect`. This is an accepted caveat \
         of the Docker runtime; use the Kubernetes backend for secret isolation."
    );

    // Flatten the configured access classes to name → access requirement for
    // the container builder, which only needs the requirement to decide whether
    // to stamp Traefik forwardAuth middleware. `None`-valued entries (used to
    // delete inherited classes) are skipped.
    let access_requirements: HashMap<String, crate::server::settings::AccessRequirement> =
        access_classes
            .iter()
            .filter_map(|(name, ac)| {
                ac.as_ref()
                    .map(|ac| (name.clone(), ac.access_requirement.clone()))
            })
            .collect();

    // Fail CLOSED: refuse to start if any non-`None` access class is configured
    // but forwardAuth cannot be wired because the internal backend URL is
    // missing. Otherwise such projects would be served publicly with no auth.
    let offending = crate::server::settings::access_classes_missing_auth_backend_url(
        access_classes,
        auth_backend_url,
    );
    if !offending.is_empty() {
        anyhow::bail!(
            "Docker deployment backend: access class(es) [{}] require authentication \
             (Authenticated/Member) but `deployment_controller.auth_backend_url` is empty. \
             Traefik forwardAuth cannot be enforced, so those projects would be served \
             publicly. Set `deployment_controller.auth_backend_url` (e.g. http://rise:3000) \
             to enable ingress authentication, or change the access requirement to None.",
            offending.join(", ")
        );
    }

    // LOCAL-DEV ONLY: resolve the backend's IP on the shared Traefik network so
    // the reconciler can stamp `extra_hosts` (alias → backend IP) on managed app
    // containers. We resolve the host from `auth_backend_url` (e.g. `rise` in
    // `http://rise:3000`) via DNS from inside this container — that yields the
    // backend's address on `traefik_network`. Empty alias list (the default /
    // production) → skip resolution entirely so no override is injected.
    //
    // Resolution failure is non-fatal: we log and leave the IP unset, which
    // disables injection (apps fall back to whatever the alias host resolves to)
    // rather than crashing the backend at startup.
    let app_backend_host_aliases = app_backend_host_aliases.clone();
    let app_backend_ip = if app_backend_host_aliases.is_empty() {
        None
    } else if let Some(ip) = app_backend_ip_override
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        // Explicit override (e.g. `host-gateway` for `mise br docker`): used
        // verbatim, skipping DNS resolution. The override can name a value that
        // is not DNS-resolvable from the host (e.g. `host-gateway`,
        // `host.docker.internal`), which Docker resolves per container.
        tracing::info!(
            backend_ip = %ip,
            aliases = ?app_backend_host_aliases,
            "Using explicit app_backend_ip override for app container host aliases (local dev)"
        );
        Some(ip.to_string())
    } else {
        match resolve_backend_ip(auth_backend_url).await {
            Some(ip) => {
                tracing::info!(
                    backend_ip = %ip,
                    aliases = ?app_backend_host_aliases,
                    "Resolved Rise backend IP for app container host aliases (local dev)"
                );
                Some(ip)
            }
            None => {
                tracing::warn!(
                    auth_backend_url = %auth_backend_url,
                    aliases = ?app_backend_host_aliases,
                    "Could not resolve Rise backend IP for app container host aliases; \
                     skipping extra_hosts injection (apps may fail to reach the issuer in local dev)"
                );
                None
            }
        }
    };

    let reconciler = DockerReconciler::new(
        docker.clone(),
        store,
        db_pool,
        url_builder,
        registry_provider,
        encryption_provider,
        resource_store,
        jwt_signer,
        *identity_token_ttl_seconds,
        ReconcilerConfig {
            controller_class: controller_class_name.clone(),
            label_namespace: label_namespace.clone(),
            container_prefix: container_prefix.clone(),
            traefik_network: traefik_network.clone(),
            traefik_entrypoint: traefik_entrypoint.clone(),
            traefik_certresolver: rise_backend_traefik::normalize_certresolver(
                traefik_certresolver.clone(),
            ),
            reconcile_interval_secs: *reconcile_interval_secs,
            health_path,
            public_url: public_url.to_string(),
            auth_backend_url: auth_backend_url.clone(),
            access_classes: access_requirements,
            app_backend_host_aliases,
            app_backend_ip,
            publish_app_ports: *publish_app_ports,
            traefik_api_url: traefik_api_url.clone(),
        },
    );
    // The reconcile loop runs under a leader election (mirrors the resource GC
    // worker): only the elected leader performs the destructive per-tick Docker
    // reconcile. In the single-instance standalone default this replica is
    // always the leader, so behavior is unchanged. The returned handle lets the
    // caller await graceful lease release on shutdown.
    let reconciler_handle = reconciler.spawn(shutdown);
    tracing::info!("Docker reconcile loop spawned (leader-gated)");

    Ok((
        Arc::new(backend) as Arc<dyn DeploymentBackend>,
        docker,
        reconciler_handle,
    ))
}

impl AppState {
    /// Check if a user is an admin.
    ///
    /// Granted by the `auth.admin_users` email allowlist or by membership in
    /// one of the `auth.admin_idp_groups` IdP groups.
    pub async fn is_admin(&self, user: &User) -> bool {
        self.has_role(&self.admin_users, &self.admin_idp_groups, user)
            .await
    }

    /// Check if a user has the Operator role.
    ///
    /// Operators are a separate role from admins: admins do NOT implicitly
    /// receive it. Granted by the `auth.operator_users` email allowlist or by
    /// membership in one of the `auth.operator_idp_groups` IdP groups.
    pub async fn is_operator(&self, user: &User) -> bool {
        self.has_role(&self.operator_users, &self.operator_idp_groups, user)
            .await
    }

    async fn has_role(
        &self,
        allowed_emails: &[String],
        allowed_groups: &[String],
        user: &User,
    ) -> bool {
        crate::server::auth::roles::has_role(&self.db_pool, allowed_emails, allowed_groups, user)
            .await
    }

    /// Run database migrations
    async fn run_migrations(pool: &PgPool) -> Result<()> {
        tracing::info!("Running database migrations...");
        sqlx::migrate!("./migrations")
            .run(pool)
            .await
            .context("Failed to run migrations")?;
        tracing::info!("Migrations completed successfully");
        Ok(())
    }

    /// Initialize full state for HTTP server
    pub async fn new(settings: &Settings) -> Result<Self> {
        tracing::info!("Initializing AppState for HTTP server");

        // Connect to PostgreSQL with server-optimized pool size
        let db_pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(&settings.database.url)
            .await
            .context("Failed to connect to PostgreSQL")?;

        tracing::info!("Successfully connected to PostgreSQL");

        // Run root migrations
        Self::run_migrations(&db_pool).await?;

        // Run resource-store migrations immediately after root migrations
        rise_resource_store_postgres::run_migrations(&db_pool)
            .await
            .context("Failed to run resource store migrations")?;

        // Run runtime-sync migrations (leader leases, schedules) in their own schema
        rise_runtime_sync::run_migrations(&db_pool)
            .await
            .context("Failed to run runtime-sync migrations")?;

        // Initialize the generic-resource store now that its schema exists. The
        // store is cheap to construct (it caches compiled JSON schemas lazily),
        // so we instantiate it once and clone the Arc into every handler.
        #[cfg(feature = "backend")]
        let pg_resource_store = Arc::new(rise_resource_store_postgres::PgResourceStore::new(
            db_pool.clone(),
        ));
        #[cfg(feature = "backend")]
        let resource_store: Arc<dyn rise_resource_api::ResourceStore> = pg_resource_store.clone();

        // Run default-Organization bootstrap. Must complete before
        // controllers begin processing typed projects, so we await it before
        // the rest of AppState comes up.
        #[cfg(feature = "backend")]
        let bootstrap_outcome =
            crate::server::bootstrap::run(&db_pool, resource_store.as_ref(), settings)
                .await
                .context("Default-Organization bootstrap failed")?;
        #[cfg(feature = "backend")]
        let default_organization_uid = bootstrap_outcome.default_organization_uid;

        // Initialize JWT validator (JWKS is fetched on-demand)
        let jwt_validator = Arc::new(JwtValidator::new(settings.server.ssrf.clone()));

        // Initialize JWT signer for ingress authentication (required)
        let jwt_signer = Arc::new(
            RiseTokenSigner::new(
                &settings.server.jwt_signing_secret,
                settings.server.public_url.clone(),
                settings.server.jwt_expiry_seconds,
                settings.server.jwt_claims.clone(),
                settings.server.rs256_private_key_pem.as_deref(),
                settings.server.rs256_public_key_pem.as_deref(),
            )
            .context("Failed to initialize JWT signer for ingress authentication")?,
        );
        tracing::info!(
            "Initialized JWT signer for ingress authentication (expiry: {}s)",
            settings.server.jwt_expiry_seconds
        );

        // Initialize OAuth2 client
        let oauth_client = Arc::new(
            OAuthClient::new(
                settings.auth.issuer.clone(),
                settings.auth.client_id.clone(),
                settings.auth.client_secret.clone(),
                settings.auth.authorize_url.clone(),
                settings.auth.token_url.clone(),
                settings.auth.scopes.clone(),
            )
            .await?,
        );

        // Initialize registry provider (required for server operation)
        let registry_provider: Arc<dyn RegistryProvider> = match &settings.registry {
            Some(registry_config) => match registry_config {
                #[cfg(feature = "backend")]
                RegistrySettings::Ecr {
                    region,
                    account_id,
                    repo_prefix,
                    push_role_arn,
                    auto_remove,
                    access_key_id,
                    secret_access_key,
                } => {
                    let ecr_config = EcrConfig {
                        region: region.clone(),
                        account_id: account_id.clone(),
                        repo_prefix: repo_prefix.clone(),
                        push_role_arn: push_role_arn.clone(),
                        auto_remove: *auto_remove,
                        access_key_id: access_key_id.clone(),
                        secret_access_key: secret_access_key.clone(),
                    };
                    let provider = EcrProvider::new(ecr_config)
                        .await
                        .context("Failed to initialize ECR registry provider")?;
                    tracing::info!("Initialized ECR registry provider");
                    Arc::new(provider)
                }
                #[cfg(not(feature = "backend"))]
                RegistrySettings::Ecr { account_id, .. } => {
                    anyhow::bail!(
                        "AWS ECR registry is configured (account: {}) but the 'aws' feature is not enabled. \
                         Please rebuild with --features aws or use a pre-built binary with AWS support.",
                        account_id
                    )
                }
                RegistrySettings::OciClientAuth {
                    registry_url,
                    namespace,
                    client_registry_url,
                    username,
                    password,
                } => {
                    let oci_config = OciClientAuthConfig {
                        registry_url: registry_url.clone(),
                        namespace: namespace.clone(),
                        client_registry_url: client_registry_url.clone(),
                        username: username.clone(),
                        password: password.clone(),
                    };
                    let provider = OciClientAuthProvider::new(oci_config)
                        .context("Failed to initialize OCI client-auth registry provider")?;
                    tracing::info!(
                        "Initialized OCI client-auth registry provider at {}",
                        registry_url
                    );
                    Arc::new(provider)
                }
                #[cfg(feature = "backend")]
                RegistrySettings::GitLab {
                    gitlab_url,
                    registry_url,
                    namespace,
                    username,
                    token,
                    mint_pull_secrets,
                    client_registry_url,
                } => {
                    let gitlab_config = GitLabRegistryConfig {
                        gitlab_url: gitlab_url.clone(),
                        registry_url: registry_url.clone(),
                        namespace: namespace.clone(),
                        username: username.clone(),
                        token: token.clone(),
                        mint_pull_secrets: *mint_pull_secrets,
                        client_registry_url: client_registry_url.clone(),
                    };
                    let provider = GitLabRegistryProvider::new(gitlab_config)
                        .context("Failed to initialize GitLab registry provider")?;
                    tracing::info!("Initialized GitLab registry provider at {}", registry_url);
                    Arc::new(provider)
                }
                #[cfg(not(feature = "backend"))]
                RegistrySettings::GitLab { registry_url, .. } => {
                    anyhow::bail!(
                        "GitLab registry is configured ({}) but the 'backend' feature is not enabled.",
                        registry_url
                    )
                }
                #[cfg(feature = "backend")]
                RegistrySettings::Jfrog {
                    registry_host,
                    client_registry_url,
                    docker_repo_key,
                    token_provider,
                    push_permissions,
                    pull_permissions,
                    push_token_ttl,
                    pull_token_ttl,
                    mint_pull_secrets,
                } => {
                    use crate::server::settings::JfrogTokenProviderSettings;

                    let resolved_token_provider = match token_provider {
                        JfrogTokenProviderSettings::Vault {
                            vault_addr,
                            vault_token,
                            vault_token_file,
                            vault_mount_path,
                            vault_role,
                            scope_override,
                        } => {
                            let addr = vault_addr
                                .clone()
                                .or_else(|| std::env::var("VAULT_ADDR").ok())
                                .context("Vault address not configured. Set vault_addr in config or VAULT_ADDR env var")?;

                            // Token resolution priority: config value > token_file config > VAULT_TOKEN_FILE env > VAULT_TOKEN env
                            let (token, token_file) = if let Some(t) = vault_token {
                                (t.clone(), None)
                            } else {
                                let file_path = vault_token_file
                                    .clone()
                                    .or_else(|| std::env::var("VAULT_TOKEN_FILE").ok());

                                if let Some(ref path) = file_path {
                                    // Read initial token from file for validation
                                    let initial_token = std::fs::read_to_string(path)
                                        .with_context(|| {
                                            format!(
                                                "Failed to read Vault token from file: {}",
                                                path
                                            )
                                        })?
                                        .trim()
                                        .to_string();
                                    if initial_token.is_empty() {
                                        anyhow::bail!("Vault token file '{}' is empty", path);
                                    }
                                    (initial_token, Some(path.clone()))
                                } else {
                                    let token = std::env::var("VAULT_TOKEN")
                                        .context("Vault token not configured. Set vault_token, vault_token_file in config, or VAULT_TOKEN/VAULT_TOKEN_FILE env var")?;
                                    (token, None)
                                }
                            };

                            JfrogTokenProvider::Vault {
                                vault_addr: addr,
                                vault_token: token,
                                vault_token_file: token_file,
                                vault_mount_path: vault_mount_path.clone(),
                                role: vault_role.clone(),
                                scope_override: *scope_override,
                            }
                        }
                        JfrogTokenProviderSettings::Direct {
                            jfrog_url,
                            admin_token,
                        } => {
                            if admin_token.is_empty() {
                                anyhow::bail!(
                                    "JFrog Direct token provider admin_token is empty. \
                                     Set admin_token in config or ensure the referenced \
                                     environment variable is set."
                                );
                            }
                            JfrogTokenProvider::Direct {
                                jfrog_url: jfrog_url.clone(),
                                admin_token: admin_token.clone(),
                            }
                        }
                    };

                    let resolved_client_host = client_registry_url
                        .clone()
                        .unwrap_or_else(|| registry_host.clone());

                    let jfrog_config = JfrogConfig {
                        token_provider: resolved_token_provider,
                        registry_host: registry_host.clone(),
                        client_registry_host: resolved_client_host,
                        docker_repo_key: docker_repo_key.clone(),
                        push_permissions: push_permissions.clone(),
                        pull_permissions: pull_permissions.clone(),
                        push_token_ttl: *push_token_ttl,
                        pull_token_ttl: *pull_token_ttl,
                        mint_pull_secrets: *mint_pull_secrets,
                    };

                    let provider = JfrogProvider::new(jfrog_config)
                        .context("Failed to initialize JFrog registry provider")?;
                    tracing::info!(
                        "Initialized JFrog registry provider at {}/{}",
                        registry_host,
                        docker_repo_key
                    );
                    Arc::new(provider)
                }
                #[cfg(not(feature = "backend"))]
                RegistrySettings::Jfrog { registry_host, .. } => {
                    anyhow::bail!(
                        "JFrog registry is configured ({}) but the 'backend' feature is not enabled.",
                        registry_host
                    )
                }
            },
            None => {
                anyhow::bail!(
                    "Registry provider is required for server operation. \
                     Please configure a registry in settings (ECR, OCI client-auth, or GitLab)"
                )
            }
        };

        // Initialize OCI client for direct registry interaction
        let oci_client = Arc::new(
            crate::server::oci::OciClient::new().context("Failed to initialize OCI client")?,
        );
        tracing::info!("Initialized OCI client for registry digest resolution");

        // Store admin users list
        let admin_users = Arc::new(settings.auth.admin_users.clone());
        if !admin_users.is_empty() {
            tracing::info!("Configured {} admin user(s)", admin_users.len());
        }
        let admin_idp_groups = Arc::new(settings.auth.admin_idp_groups.clone());
        if !admin_idp_groups.is_empty() {
            tracing::info!("Configured {} admin IdP group(s)", admin_idp_groups.len());
        }

        // Store operator users list (separate role from admin)
        let operator_users = Arc::new(settings.auth.operator_users.clone());
        if !operator_users.is_empty() {
            tracing::info!("Configured {} operator user(s)", operator_users.len());
        }
        let operator_idp_groups = Arc::new(settings.auth.operator_idp_groups.clone());
        if !operator_idp_groups.is_empty() {
            tracing::info!(
                "Configured {} operator IdP group(s)",
                operator_idp_groups.len()
            );
        }

        // The generic resource API's choke point. It carries the same operator
        // selectors: operator standing is one subject in the authorization model
        // (ADR-0001 §1), not a check in front of the API.
        #[cfg(feature = "backend")]
        let resource_authorizer = crate::server::authz::ResourceAuthorizer::new(
            pg_resource_store.clone(),
            db_pool.clone(),
            crate::server::authz::OperatorSelectors {
                users: operator_users.clone(),
                idp_groups: operator_idp_groups.clone(),
            },
        );

        // Validate and index configured controller identities
        let controller_indexes =
            crate::server::auth::controller::build_controller_indexes(&settings.auth.controllers)
                .context("Failed to load controller identities from auth.controllers")?;
        let controllers = Arc::new(controller_indexes.by_id);
        let controllers_by_issuer = Arc::new(controller_indexes.by_issuer);
        if !controllers.is_empty() {
            let ids: Vec<&str> = controllers.keys().map(String::as_str).collect();
            tracing::info!(
                "Configured {} controller identity(ies): {:?}",
                controllers.len(),
                ids
            );
        }

        // Store auth settings for issuer comparison
        let auth_settings = Arc::new(settings.auth.clone());

        // Store server settings for frontend config injection
        let server_settings = Arc::new(settings.server.clone());

        // Initialize token store for OAuth2 PKCE flow (DB-backed, safe in multi-replica deployments)
        let token_store: Arc<dyn TokenStore> = Arc::new(DbTokenStore::new(db_pool.clone()));
        tracing::info!("Initialized DB-backed token store for OAuth2 state");

        // Initialize cookie settings for session management
        let cookie_settings = CookieSettings {
            secure: settings.server.cookie_secure,
            cookie_domain: settings.server.cookie_domain.clone(),
        };
        if let Some(ref domain) = cookie_settings.cookie_domain {
            tracing::info!(
                "Configured session cookies: secure={}, cookie_domain={} (will expire old domain-scoped cookies)",
                cookie_settings.secure, domain
            );
        } else {
            tracing::info!(
                "Configured session cookies: secure={} (no Domain attribute — cookies scoped to current host)",
                cookie_settings.secure
            );
        }

        let public_url = settings.server.public_url.clone();
        let reserved_project_names = Arc::new(
            settings
                .reserved_project_names
                .iter()
                .map(|name| name.trim().to_ascii_lowercase())
                .collect(),
        );
        tracing::info!("Public URL: {}", public_url);
        if let Some(ref docs_dir) = settings.server.docs_dir {
            tracing::info!("Documentation directory: {}", docs_dir);
        } else {
            tracing::warn!("No docs_dir configured — documentation endpoints will return 404");
        }
        if let Some(frontend_proxy_url) = &settings.server.frontend_dev_proxy_url {
            tracing::info!(
                "Frontend dev proxy enabled: backend will proxy UI requests to {}",
                frontend_proxy_url
            );
        }

        // Initialize encryption provider
        let encryption_provider = init_encryption_provider(settings.encryption.as_ref()).await?;

        // Initialize deployment backend
        #[cfg(not(feature = "backend"))]
        compile_error!(
            "At least one deployment backend must be enabled. Please build with --features backend"
        );

        // Initialize ResourceBuilder and kube_client for Metacontroller webhook and deployment backend
        #[cfg(feature = "backend")]
        let (
            resource_builder,
            webhook_kube_client,
            ip_validator,
            webhook_port,
            deployment_defaults_opt,
            deployment_constraints_opt,
            identity_token_ttl_seconds,
            deployment_controller_class_name,
        ) = {
            use crate::server::deployment::resource_builder::ResourceBuilder;
            use crate::server::settings::DeploymentControllerSettings;

            if let Some(DeploymentControllerSettings::Kubernetes {
                kubeconfig,
                production_ingress_url_template,
                staging_ingress_url_template,
                environment_ingress_url_template,
                ingress_port,
                ingress_schema,
                auth_backend_url,
                auth_signin_url,
                namespace_labels,
                namespace_annotations,
                ingress_annotations,
                ingress_tls_secret_name,
                custom_domain_tls_mode,
                custom_domain_ingress_annotations,
                node_selector,
                image_pull_secret_name,
                access_classes,
                host_aliases,
                extra_service_token_audiences,
                use_default_service_account_for_production,
                network_policy,
                pod_security_enabled,
                deployment_defaults,
                deployment_constraints: _deployment_constraints,
                health_probes,
                metacontroller_webhook_port,
                metacontroller_pod_namespace,
                metacontroller_pod_label_selector,
                controller_class_name,
                identity_token_ttl_seconds,
                ..
            }) = &settings.deployment_controller
            {
                // Install default CryptoProvider for rustls (required for kube-rs HTTPS connections)
                rustls::crypto::ring::default_provider()
                    .install_default()
                    .ok();

                // Create kube client for webhook and deployment backend
                let kube_config = if let Some(path) = kubeconfig {
                    let kubeconfig_data = kube::config::Kubeconfig::read_from(path)?;
                    kube::Config::from_custom_kubeconfig(
                        kubeconfig_data,
                        &kube::config::KubeConfigOptions::default(),
                    )
                    .await?
                } else {
                    kube::Config::infer().await?
                };
                let kube_client = kube::Client::try_from(kube_config)?;

                let parsed_backend_address =
                    crate::server::settings::BackendAddress::from_url(auth_backend_url)?;

                let filtered_access_classes: std::collections::HashMap<_, _> = access_classes
                    .iter()
                    .filter_map(|(k, v)| v.as_ref().map(|ac| (k.clone(), ac.clone())))
                    .collect();

                // Namespace prefix is resolved per-request by the
                // webhook against the project's Organization, so
                // `ResourceBuilder` no longer carries a baked format
                // string.

                let rb = ResourceBuilder {
                    url_builder: rise_backend_core::DeploymentUrlBuilder {
                        production_ingress_url_template: production_ingress_url_template.clone(),
                        staging_ingress_url_template: staging_ingress_url_template.clone(),
                        environment_ingress_url_template: environment_ingress_url_template.clone(),
                        ingress_port: *ingress_port,
                        ingress_schema: ingress_schema.clone(),
                        registry_provider: registry_provider.clone(),
                    },
                    auth_backend_url: auth_backend_url.clone(),
                    auth_signin_url: auth_signin_url.clone(),
                    backend_address: Some(parsed_backend_address),
                    namespace_labels: namespace_labels.clone(),
                    namespace_annotations: namespace_annotations.clone(),
                    ingress_annotations: ingress_annotations.clone(),
                    ingress_tls_secret_name: ingress_tls_secret_name.clone(),
                    custom_domain_tls_mode: custom_domain_tls_mode.clone(),
                    custom_domain_ingress_annotations: custom_domain_ingress_annotations.clone(),
                    node_selector: node_selector.clone(),
                    image_pull_secret_name: image_pull_secret_name.clone(),
                    access_classes: filtered_access_classes,
                    host_aliases: host_aliases.clone(),
                    extra_service_token_audiences: extra_service_token_audiences.clone(),
                    use_default_service_account_for_production:
                        *use_default_service_account_for_production,
                    network_policy: network_policy.clone(),
                    pod_security_enabled: *pod_security_enabled,
                    health_probes: health_probes.clone(),
                };

                let run_mode =
                    std::env::var("RISE_CONFIG_RUN_MODE").unwrap_or_else(|_| "development".into());
                let validator = if metacontroller_pod_namespace.trim().is_empty() {
                    if run_mode != "development" {
                        anyhow::bail!(
                            "Refusing to start: kubernetes.metacontroller_pod_namespace is not \
                             set. Set it to the namespace where metacontroller pods run \
                             (e.g. the release namespace)."
                        );
                    }
                    tracing::warn!(
                        "Metacontroller webhook running without source IP validation — \
                         acceptable for development only"
                    );
                    None
                } else {
                    Some(Arc::new(
                        crate::server::deployment::ip_validator::MetacontrollerIpValidator::new(
                            kube_client.clone(),
                            metacontroller_pod_namespace.clone(),
                            metacontroller_pod_label_selector.clone(),
                        ),
                    ))
                };

                tracing::info!("Initialized ResourceBuilder for Metacontroller webhook");
                (
                    Some(Arc::new(rb)),
                    Some(kube_client),
                    validator,
                    Some(*metacontroller_webhook_port),
                    Some(deployment_defaults.clone()),
                    Some(_deployment_constraints.clone()),
                    *identity_token_ttl_seconds,
                    Some(controller_class_name.clone()),
                )
            } else if let Some(DeploymentControllerSettings::Docker {
                deployment_defaults,
                deployment_constraints,
                identity_token_ttl_seconds,
                controller_class_name,
                ..
            }) = &settings.deployment_controller
            {
                // Docker mode: no K8s ResourceBuilder / kube client / webhook,
                // but surface the deployment defaults/constraints/class so
                // request validation and identity-token minting behave the same.
                (
                    None,
                    None,
                    None,
                    None,
                    Some(deployment_defaults.clone()),
                    Some(deployment_constraints.clone()),
                    *identity_token_ttl_seconds,
                    Some(controller_class_name.clone()),
                )
            } else if let Some(DeploymentControllerSettings::Ecs {
                deployment_defaults,
                deployment_constraints,
                identity_token_ttl_seconds,
                controller_class_name,
                ..
            }) = &settings.deployment_controller
            {
                // ECS mode: no K8s ResourceBuilder / kube client / webhook, but
                // surface the deployment defaults/constraints/class so request
                // validation behaves identically across backends. Missing this
                // would silently reject every `replicas > 1` request (the
                // platform default max is 1) and leave the controller class
                // unset.
                (
                    None,
                    None,
                    None,
                    None,
                    Some(deployment_defaults.clone()),
                    Some(deployment_constraints.clone()),
                    *identity_token_ttl_seconds,
                    Some(controller_class_name.clone()),
                )
            } else {
                (None, None, None, None, None, None, 3600, None)
            }
        };

        // Cooperative shutdown signal shared by extension reconciliation loops
        // and the background controllers (see `run_server`). Cancelling it stops
        // every leader-elected loop and releases its lease. Created here (before
        // the deployment backend init) so the Docker reconciler's leader
        // election can be wired to it.
        let shutdown = tokio_util::sync::CancellationToken::new();

        // The single `DeploymentStore` implementation, shared by the deployment
        // backend, the Metacontroller webhook, and the identity-refresh/CRD
        // controllers. Constructed once here and threaded everywhere those need
        // deployment persistence.
        #[cfg(feature = "backend")]
        let deployment_store: Arc<dyn rise_backend_core::DeploymentStore> = Arc::new(
            crate::db::deployment_store::PgDeploymentStore::new(db_pool.clone()),
        );

        // Initialize the deployment backend by matching on the configured
        // controller variant. Kubernetes uses the slim Metacontroller-backed
        // backend; Docker connects bollard, builds its own ResourceBuilder, and
        // spawns the in-process reconcile loop (under a leader election).
        #[cfg(feature = "backend")]
        let (deployment_backend, docker_client, ecs_cloudwatch, reconciler_handle) = {
            use crate::server::settings::DeploymentControllerSettings;
            match &settings.deployment_controller {
                Some(DeploymentControllerSettings::Kubernetes { .. }) => {
                    let rb = resource_builder.clone().ok_or_else(|| {
                        anyhow::anyhow!("Deployment controller not configured. Please add deployment_controller configuration with type: kubernetes")
                    })?;
                    let kc = webhook_kube_client
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("Kubernetes client not initialized"))?;
                    (
                        init_kubernetes_backend(
                            rb,
                            kc,
                            deployment_store.clone(),
                            deployment_controller_class_name.clone(),
                        )
                        .await?,
                        None,
                        None,
                        None,
                    )
                }
                Some(docker_settings @ DeploymentControllerSettings::Docker { .. }) => {
                    let (backend, docker, reconciler_handle) = init_docker_backend(
                        docker_settings,
                        registry_provider.clone(),
                        encryption_provider.clone(),
                        resource_store.clone(),
                        jwt_signer.clone(),
                        db_pool.clone(),
                        deployment_store.clone(),
                        &public_url,
                        shutdown.clone(),
                    )
                    .await?;
                    (backend, Some(docker), None, Some(reconciler_handle))
                }
                Some(ecs_settings @ DeploymentControllerSettings::Ecs { .. }) => {
                    let ecr_account_id = match &settings.registry {
                        Some(crate::server::settings::RegistrySettings::Ecr {
                            account_id, ..
                        }) => Some(account_id.as_str()),
                        _ => None,
                    };
                    let (backend, reconciler_handle, cloudwatch) = init_ecs_backend(
                        ecs_settings,
                        ecr_account_id,
                        registry_provider.clone(),
                        encryption_provider.clone(),
                        resource_store.clone(),
                        db_pool.clone(),
                        deployment_store.clone(),
                        &public_url,
                        shutdown.clone(),
                    )
                    .await?;
                    (backend, None, Some(cloudwatch), Some(reconciler_handle))
                }
                None => {
                    return Err(anyhow::anyhow!(
                        "Deployment controller not configured. Please add a deployment_controller \
                         configuration block (type: kubernetes, docker or ecs)."
                    ));
                }
            }
        };

        // Surface the Docker controller's configured `label_namespace` so the
        // Docker log backend resolves containers by the same namespaced labels
        // the reconciler stamps (rather than a hardcoded literal).
        #[cfg(feature = "backend")]
        let docker_label_namespace = match &settings.deployment_controller {
            Some(crate::server::settings::DeploymentControllerSettings::Docker {
                label_namespace,
                ..
            })
            | Some(crate::server::settings::DeploymentControllerSettings::Ecs {
                label_namespace,
                ..
            }) => Some(label_namespace.clone()),
            _ => None,
        };
        #[cfg(feature = "backend")]
        let runtime_log_backend = crate::server::deployment::logs::init_runtime_log_backend(
            &settings.deployment_logs,
            webhook_kube_client.clone(),
            docker_client.clone(),
            docker_label_namespace.as_deref(),
            ecs_cloudwatch,
        )
        .await?;

        // Resolve the architecture from the deployment runtime itself. The
        // Docker daemon may be remote, so the backend process's compile-time or
        // host architecture is not authoritative.
        #[cfg(feature = "backend")]
        let runtime_arch = if let Some(arch) = deployment_backend.capabilities().runtime_arch {
            // Backends that pin an architecture by configuration report it
            // directly; the rest are detected below.
            Some(arch)
        } else if let Some(docker) = docker_client.as_ref() {
            let info = docker
                .info()
                .await
                .context("Failed to query Docker daemon architecture")?;
            let raw_arch = info
                .architecture
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Docker daemon did not report its architecture"))?;
            let arch = crate::server::platform::models::normalize_runtime_arch(raw_arch)
                .ok_or_else(|| anyhow::anyhow!("Docker daemon reported an empty architecture"))?;
            tracing::info!(runtime_arch = %arch, "Detected Docker daemon architecture");
            Some(arch)
        } else if let Some(crate::server::settings::DeploymentControllerSettings::Ecs {
            cpu_architecture,
            ..
        }) = &settings.deployment_controller
        {
            // Fargate's architecture is chosen by configuration, not detected:
            // the task definition declares it. Surfacing it here is what gives
            // the CLI its `--platform` hint, so an ARM64 cluster doesn't receive
            // silently-unrunnable amd64 images — which is exactly why an
            // architecture that will not normalise must stop startup rather
            // than quietly withdraw the hint. Settings canonicalise the value at
            // load, so reaching the error means the two have drifted.
            Some(
                crate::server::platform::models::normalize_runtime_arch(cpu_architecture)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "ECS deployment backend: could not normalize \
                             `deployment_controller.cpu_architecture` ({cpu_architecture:?}) to an \
                             OCI platform name, so the CLI would build images with no platform \
                             hint and Fargate could refuse to run them"
                        )
                    })?,
            )
        } else {
            None
        };

        // Initialize extension registry
        #[allow(unused_mut)]
        let mut extension_registry = crate::server::extensions::registry::ExtensionRegistry::new();

        // Collect the extension tasks' handles so `run_server` can await their
        // graceful lease release on shutdown. A plain Vec during startup (no lock
        // is held across `start()`); wrapped in Arc<Mutex<…>> in the returned state.
        // The Docker reconciler's leader-elected loop (if any) is tracked here too
        // so its lease is released gracefully on shutdown.
        let mut extension_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        #[cfg(feature = "backend")]
        if let Some(handle) = reconciler_handle {
            extension_handles.push(handle);
        }

        // Register extensions from configuration
        if let Some(ref extensions_config) = settings.extensions {
            #[allow(clippy::never_loop)]
            for provider_config in &extensions_config.providers {
                match provider_config {
                    #[cfg(feature = "backend")]
                    crate::server::settings::ExtensionProviderConfig::AwsRdsProvisioner {
                        region,
                        instance_size,
                        disk_size,
                        instance_id_template,
                        instance_id_prefix,
                        default_engine_version,
                        vpc_security_group_ids,
                        db_subnet_group_name,
                        backup_retention_days,
                        backup_window,
                        maintenance_window,
                        access_key_id,
                        secret_access_key,
                    } => {
                        tracing::info!("Initializing AWS RDS extension provider");

                        // Create AWS config
                        let mut aws_config_builder =
                            aws_config::defaults(aws_config::BehaviorVersion::latest())
                                .region(aws_config::Region::new(region.clone()));

                        // Use explicit credentials if provided
                        if let (Some(key_id), Some(secret_key)) = (access_key_id, secret_access_key)
                        {
                            aws_config_builder = aws_config_builder.credentials_provider(
                                aws_sdk_sts::config::Credentials::new(
                                    key_id,
                                    secret_key,
                                    None,
                                    None,
                                    "static-credentials",
                                ),
                            );
                        }

                        let aws_config = aws_config_builder.load().await;
                        let rds_client = aws_sdk_rds::Client::new(&aws_config);

                        // Get encryption provider (required for RDS)
                        let encryption_provider = encryption_provider.clone().ok_or_else(|| {
                            anyhow::anyhow!("Encryption provider required for AWS RDS extension")
                        })?;

                        // Create and register the extension
                        let aws_rds_provisioner =
                            crate::server::extensions::providers::aws_rds::AwsRdsProvisioner::new(
                                crate::server::extensions::providers::aws_rds::AwsRdsProvisionerConfig {
                                    rds_client,
                                    db_pool: db_pool.clone(),
                                    encryption_provider,
                                    region: region.clone(),
                                    instance_size: instance_size.clone(),
                                    disk_size: *disk_size,
                                    instance_id_template: instance_id_template.clone(),
                                    instance_id_prefix: instance_id_prefix.clone(),
                                    default_engine_version: default_engine_version.clone(),
                                    vpc_security_group_ids: vpc_security_group_ids.clone(),
                                    db_subnet_group_name: db_subnet_group_name.clone(),
                                    backup_retention_days: *backup_retention_days,
                                    backup_window: backup_window.clone(),
                                    maintenance_window: maintenance_window.clone(),
                                }
                            )
                            .await?;

                        let aws_rds_arc: Arc<dyn crate::server::extensions::Extension> =
                            Arc::new(aws_rds_provisioner);
                        extension_registry.register_type(aws_rds_arc.clone());

                        // Start the extension's reconciliation loop
                        extension_handles.push(aws_rds_arc.start(shutdown.clone()));

                        tracing::info!("AWS RDS extension provider initialized and started");
                    }
                    #[cfg(feature = "backend")]
                    crate::server::settings::ExtensionProviderConfig::AwsS3BucketProvisioner {
                        region,
                        bucket_prefix,
                        bucket_name_template,
                        user_permissions_boundary_arn,
                        access_key_id,
                        secret_access_key,
                    } => {
                        tracing::info!("Initializing AWS S3 bucket extension provider");

                        let mut aws_config_builder =
                            aws_config::defaults(aws_config::BehaviorVersion::latest())
                                .region(aws_config::Region::new(region.clone()));

                        if let (Some(key_id), Some(secret_key)) = (access_key_id, secret_access_key)
                        {
                            aws_config_builder = aws_config_builder.credentials_provider(
                                aws_sdk_sts::config::Credentials::new(
                                    key_id,
                                    secret_key,
                                    None,
                                    None,
                                    "static-credentials",
                                ),
                            );
                        }

                        let aws_config = aws_config_builder.load().await;
                        let s3_client = aws_sdk_s3::Client::new(&aws_config);
                        let iam_client = aws_sdk_iam::Client::new(&aws_config);

                        let encryption_provider = encryption_provider.clone().ok_or_else(|| {
                            anyhow::anyhow!("Encryption provider required for AWS S3 extension")
                        })?;

                        let aws_s3_provisioner =
                            crate::server::extensions::providers::aws_s3::AwsS3Provisioner::new(
                                crate::server::extensions::providers::aws_s3::AwsS3ProvisionerConfig {
                                    s3_client,
                                    iam_client,
                                    db_pool: db_pool.clone(),
                                    encryption_provider,
                                    region: region.clone(),
                                    bucket_prefix: bucket_prefix.clone(),
                                    bucket_name_template: bucket_name_template.clone(),
                                    user_permissions_boundary_arn: user_permissions_boundary_arn
                                        .clone(),
                                },
                            );

                        let aws_s3_arc: Arc<dyn crate::server::extensions::Extension> =
                            Arc::new(aws_s3_provisioner);
                        extension_registry.register_type(aws_s3_arc.clone());
                        extension_handles.push(aws_s3_arc.start(shutdown.clone()));

                        tracing::info!("AWS S3 bucket extension provider initialized and started");
                    }

                    // When no extension provider features are enabled, this ensures the match is exhaustive
                    #[allow(unreachable_patterns)]
                    _ => {
                        // This pattern is only reachable when no extension features are enabled
                        // In that case, we skip unknown provider types silently
                    }
                }
            }
        }

        // Register OAuth provider (always enabled)
        tracing::info!("Initializing OAuth extension provider");
        let encryption_provider_for_oauth = encryption_provider
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Encryption provider required for OAuth extension"))?;

        let oauth_provider = crate::server::extensions::providers::oauth::OAuthProvider::new(
            crate::server::extensions::providers::oauth::OAuthProviderConfig {
                db_pool: db_pool.clone(),
                encryption_provider: encryption_provider_for_oauth,
                http_client: reqwest::Client::new(),
                api_domain: public_url.clone(),
            },
        );

        let oauth_provider_arc: Arc<dyn crate::server::extensions::Extension> =
            Arc::new(oauth_provider);
        extension_registry.register_type(oauth_provider_arc.clone());
        extension_handles.push(oauth_provider_arc.start(shutdown.clone()));
        tracing::info!("OAuth extension provider initialized and started");

        // Register Snowflake OAuth provisioner (if configured)
        #[cfg(feature = "backend")]
        if let Some(ref extensions_config) = settings.extensions {
            for provider_config in &extensions_config.providers {
                #[allow(irrefutable_let_patterns)]
                if let crate::server::settings::ExtensionProviderConfig::SnowflakeOAuthProvisioner {
                    account,
                    user,
                    role,
                    warehouse,
                    auth,
                    integration_name_prefix,
                    default_blocked_roles,
                    default_scopes,
                    refresh_token_validity_seconds,
                    verify_interval_seconds,
                } = provider_config
                {
                    tracing::info!("Initializing Snowflake OAuth provisioner");

                    let snowflake_oauth_provisioner =
                        crate::server::extensions::providers::snowflake_oauth::SnowflakeOAuthProvisioner::new(
                            crate::server::extensions::providers::snowflake_oauth::SnowflakeOAuthProvisionerConfig {
                                db_pool: db_pool.clone(),
                                encryption_provider: encryption_provider.clone()
                                    .ok_or_else(|| anyhow::anyhow!("Encryption provider required for Snowflake OAuth provisioner"))?,
                                http_client: reqwest::Client::new(),
                                api_domain: public_url.clone(),
                                oauth_provider: Some(oauth_provider_arc.clone()),
                                account: account.clone(),
                                user: user.clone(),
                                role: role.clone(),
                                warehouse: warehouse.clone(),
                                auth: auth.clone(),
                                integration_name_prefix: integration_name_prefix.clone(),
                                default_blocked_roles: default_blocked_roles.clone(),
                                default_scopes: default_scopes.clone(),
                                refresh_token_validity_seconds: *refresh_token_validity_seconds,
                                verify_interval_seconds: *verify_interval_seconds,
                            },
                        );

                    // Validate credentials during startup - fail fast if invalid
                    snowflake_oauth_provisioner
                        .validate_credentials()
                        .await
                        .context("Failed to validate Snowflake credentials during startup")?;

                    let snowflake_oauth_arc: Arc<dyn crate::server::extensions::Extension> =
                        Arc::new(snowflake_oauth_provisioner);
                    extension_registry.register_type(snowflake_oauth_arc.clone());
                    extension_handles.push(snowflake_oauth_arc.start(shutdown.clone()));
                    tracing::info!("Snowflake OAuth provisioner initialized and started");
                }
            }
        }

        let extension_registry = Arc::new(extension_registry);

        // Initialize encrypt endpoint rate limiter (100 requests/hour per user)
        let encrypt_rate_limiter = Arc::new(
            moka::future::Cache::builder()
                .time_to_live(Duration::from_secs(3600)) // 1 hour
                .max_capacity(10_000) // Prevent memory exhaustion
                .build(),
        );
        tracing::info!("Initialized encrypt endpoint rate limiter (100 req/hour per user)");

        #[cfg(feature = "backend")]
        let org_view: Arc<dyn rise_backend_core::OrganizationView> =
            Arc::new(crate::server::organizations::CachedOrgView::new(
                pg_resource_store.clone(),
                deployment_store.clone(),
            ));

        // The Metacontroller webhook's own dependency bundle. `None` unless the
        // Kubernetes controller is configured, which is also what gates the
        // webhook listener in `server::run`.
        #[cfg(feature = "backend")]
        let webhook_ctx = match (resource_builder.as_ref(), webhook_kube_client.as_ref()) {
            (Some(rb), Some(kc)) => Some(Arc::new(
                crate::server::deployment::webhook::WebhookContext {
                    kube_client: kc.clone(),
                    resource_builder: rb.clone(),
                    ip_validator: ip_validator.clone(),
                    deployment_store: deployment_store.clone(),
                    org_view: org_view.clone(),
                    jwt_signer: jwt_signer.clone(),
                    encryption_provider: encryption_provider.clone(),
                    controller_class_name: deployment_controller_class_name.clone(),
                    identity_token_ttl_seconds,
                },
            )),
            _ => None,
        };

        // Initialize OAuth endpoint rate limiter
        let rl = &settings.server.oauth_rate_limit;
        let oauth_rate_limiter = Arc::new(crate::server::rate_limit::OAuthRateLimiter::new(rl));
        tracing::info!(
            per_project = %format!("{} req/{}s", rl.per_project_max, rl.per_project_window_secs),
            per_ip = %format!("{} req/{}s", rl.per_ip_max, rl.per_ip_window_secs),
            per_session = %format!("{} req/{}s", rl.per_session_max, rl.per_session_window_secs),
            global = %format!("{} req/{}s", rl.global_max, rl.global_window_secs),
            "Initialized OAuth endpoint rate limiter",
        );

        // Extract access_classes from deployment controller settings
        // Filter out null values (used to remove inherited access classes)
        let (
            access_classes,
            production_ingress_url_template,
            staging_ingress_url_template,
            environment_ingress_url_template,
            ingress_schema,
            ingress_port,
            signin_base_url,
        ) = if let Some(crate::server::settings::DeploymentControllerSettings::Kubernetes {
            access_classes,
            production_ingress_url_template,
            staging_ingress_url_template,
            environment_ingress_url_template,
            ingress_schema,
            ingress_port,
            ..
        }) = &settings.deployment_controller
        {
            let filtered: std::collections::HashMap<_, _> = access_classes
                .iter()
                .filter_map(|(k, v)| v.as_ref().map(|ac| (k.clone(), ac.clone())))
                .collect();
            (
                Arc::new(filtered),
                Some(production_ingress_url_template.clone()),
                staging_ingress_url_template.clone(),
                environment_ingress_url_template.clone(),
                ingress_schema.clone(),
                *ingress_port,
                // K8s uses nginx auth-signin (built in ResourceBuilder), so the
                // handler's Traefik signin-redirect mode is never engaged; default
                // to public_url for completeness.
                public_url.clone(),
            )
        } else if let Some(crate::server::settings::DeploymentControllerSettings::Docker {
            access_classes,
            production_ingress_url_template,
            staging_ingress_url_template,
            environment_ingress_url_template,
            ingress_schema,
            ingress_port,
            auth_signin_url,
            ..
        }) = &settings.deployment_controller
        {
            let filtered: std::collections::HashMap<_, _> = access_classes
                .iter()
                .filter_map(|(k, v)| v.as_ref().map(|ac| (k.clone(), ac.clone())))
                .collect();
            // Browser-facing signin base URL for the Traefik forwardAuth redirect.
            // Falls back to public_url when auth_signin_url is empty.
            let signin_base = if auth_signin_url.trim().is_empty() {
                public_url.clone()
            } else {
                auth_signin_url.clone()
            };
            (
                Arc::new(filtered),
                Some(production_ingress_url_template.clone()),
                staging_ingress_url_template.clone(),
                environment_ingress_url_template.clone(),
                ingress_schema.clone(),
                *ingress_port,
                signin_base,
            )
        } else if let Some(crate::server::settings::DeploymentControllerSettings::Ecs {
            access_classes,
            production_ingress_url_template,
            staging_ingress_url_template,
            environment_ingress_url_template,
            ingress_schema,
            ingress_port,
            auth_signin_url,
            ..
        }) = &settings.deployment_controller
        {
            // Same shape as the Docker arm: ECS is also Traefik-fronted, so the
            // handler's signin-redirect mode is engaged and needs a browser-facing
            // base URL. Omitting this arm would leave `access_classes` EMPTY, and
            // every project's access class would then fail API validation.
            let filtered: std::collections::HashMap<_, _> = access_classes
                .iter()
                .filter_map(|(k, v)| v.as_ref().map(|ac| (k.clone(), ac.clone())))
                .collect();
            let signin_base = if auth_signin_url.trim().is_empty() {
                public_url.clone()
            } else {
                auth_signin_url.clone()
            };
            (
                Arc::new(filtered),
                Some(production_ingress_url_template.clone()),
                staging_ingress_url_template.clone(),
                environment_ingress_url_template.clone(),
                ingress_schema.clone(),
                *ingress_port,
                signin_base,
            )
        } else {
            (
                Arc::new(std::collections::HashMap::new()),
                None,
                None,
                None,
                "https".to_string(),
                None,
                public_url.clone(),
            )
        };

        Ok(Self {
            db_pool,
            shutdown,
            extension_handles: Arc::new(std::sync::Mutex::new(extension_handles)),
            jwt_validator,
            jwt_signer,
            oauth_client,
            registry_provider,
            oci_client,
            admin_users,
            admin_idp_groups,
            operator_users,
            operator_idp_groups,
            controllers,
            controllers_by_issuer,
            #[cfg(feature = "backend")]
            resource_store,
            #[cfg(feature = "backend")]
            resource_authorizer,
            #[cfg(feature = "backend")]
            default_organization_uid,
            #[cfg(feature = "backend")]
            deployment_controller_class_name,
            #[cfg(feature = "backend")]
            org_view,
            auth_settings,
            server_settings,
            token_store,
            cookie_settings,
            public_url,
            encryption_provider,
            deployment_backend,
            #[cfg(feature = "backend")]
            deployment_store,
            #[cfg(feature = "backend")]
            runtime_log_backend,
            extension_registry,
            encrypt_rate_limiter,
            oauth_rate_limiter,
            access_classes,
            reserved_project_names,
            signin_base_url,
            production_ingress_url_template,
            staging_ingress_url_template,
            environment_ingress_url_template,
            ingress_schema,
            ingress_port,
            #[cfg(feature = "backend")]
            runtime_arch,
            #[cfg(feature = "backend")]
            kube_client: webhook_kube_client,
            #[cfg(feature = "backend")]
            metacontroller_webhook_port: webhook_port,
            #[cfg(feature = "backend")]
            webhook_ctx,
            #[cfg(feature = "backend")]
            deployment_defaults: deployment_defaults_opt,
            #[cfg(feature = "backend")]
            deployment_constraints: deployment_constraints_opt,
            #[cfg(feature = "backend")]
            identity_token_ttl_seconds,
            quickstart_templates: Arc::new(crate::server::quickstart::registry::from_settings(
                settings.quickstart.as_ref(),
            )),
        })
    }
}

#[cfg(all(test, feature = "backend"))]
mod backend_host_tests {
    use super::backend_host_from_url;

    #[test]
    fn parses_scheme_host_port() {
        assert_eq!(
            backend_host_from_url("http://rise:3000").as_deref(),
            Some("rise")
        );
    }

    #[test]
    fn parses_https_and_path() {
        assert_eq!(
            backend_host_from_url("https://rise.example.com:8443/api").as_deref(),
            Some("rise.example.com")
        );
    }

    #[test]
    fn parses_bare_host_port() {
        assert_eq!(backend_host_from_url("rise:3000").as_deref(), Some("rise"));
        assert_eq!(backend_host_from_url("rise").as_deref(), Some("rise"));
    }

    #[test]
    fn strips_userinfo() {
        assert_eq!(
            backend_host_from_url("http://user:pass@rise:3000").as_deref(),
            Some("rise")
        );
    }

    #[test]
    fn empty_or_hostless_is_none() {
        assert_eq!(backend_host_from_url(""), None);
        assert_eq!(backend_host_from_url("   "), None);
        assert_eq!(backend_host_from_url("http://"), None);
    }
}
