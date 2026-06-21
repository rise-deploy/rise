use config::{Config, ConfigError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Deserialize, Clone, JsonSchema)]
pub struct Settings {
    pub server: ServerSettings,
    pub auth: AuthSettings,
    #[serde(default)]
    pub database: DatabaseSettings,
    #[serde(default)]
    pub registry: Option<RegistrySettings>,
    #[serde(default)]
    pub deployment_controller: Option<DeploymentControllerSettings>,
    #[serde(default)]
    pub deployment_logs: DeploymentLogsSettings,
    #[serde(default)]
    pub encryption: Option<EncryptionSettings>,
    #[serde(default)]
    pub extensions: Option<ExtensionsSettings>,
    /// Background garbage-collection worker for the generic resource store.
    /// Drains tombstoned resource rows once their controller finalizers clear.
    /// Omitting the section keeps the worker running with the documented defaults;
    /// the worker is essential to draining cascading deletes and is therefore
    /// always on when the `backend` feature is enabled.
    #[serde(default)]
    pub resource_gc: Option<ResourceGcSettings>,
    /// Default Organization that owns existing typed data (users, teams,
    /// projects) until org-aware multi-tenancy lands. Bootstrap creates this
    /// Organization (under an advisory lock) before any backfill runs, so it
    /// is guaranteed to exist by the time controllers begin processing
    /// projects. Omitting the section accepts the documented defaults:
    /// name="default", display name="Default", no namespace prefix
    /// configured — the controller then synthesizes `org-{discriminator}-`
    /// as the per-Org namespace prefix. Set
    /// `default_organization.kubernetes_namespace_prefix` explicitly (e.g.
    /// `"rise-"`) to preserve historic naming on existing installs.
    #[serde(default)]
    pub default_organization: DefaultOrganizationSettings,
    /// Curated catalog of stateless container images surfaced as one-click
    /// "quickstart" deploys in the UI. The catalog is layered: `default.yaml`
    /// ships the built-in selection and any `quickstart.templates` list in a
    /// later layer (run-mode or `local.yaml`) *replaces* it entirely. Operators
    /// who want to add to the defaults must re-include them.
    #[serde(default)]
    pub quickstart: Option<QuickstartSettings>,
}

/// Catalog of quickstart templates exposed via `GET /api/v1/quickstart-templates`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct QuickstartSettings {
    #[serde(default)]
    pub templates: Vec<QuickstartTemplateConfig>,
}

/// A single quickstart catalog entry. Mirrors the API shape on the wire, with
/// one difference: `icon` accepts either a built-in icon name (resolved to
/// `/assets/quickstart/<name>.svg`) or an absolute URL/path (must start with
/// `http://`, `https://`, or `/`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct QuickstartTemplateConfig {
    /// Stable kebab-case identifier used in URLs and project records.
    pub id: String,
    /// Short, human-friendly name shown in cards and dialogs.
    pub display_name: String,
    /// One-line description for the card.
    pub tagline: String,
    /// Longer description shown in the deploy dialog.
    pub description: String,
    /// Built-in icon name (e.g. `welcome`, `whoami`, `httpbin`, `excalidraw`)
    /// resolved against `/assets/quickstart/<name>.svg`, OR an absolute URL or
    /// static path (`http://...`, `https://...`, `/some/path.svg`).
    pub icon: String,
    /// Fully-qualified container image. Pin a tag when one is published so the
    /// catalog is reproducible; floating tags (`:latest`) work but defeat the
    /// "Redeploy from template" drift detector.
    pub image: String,
    /// Port the container listens on.
    pub http_port: u16,
    /// Upstream link for users who want to learn more.
    pub learn_more_url: String,
    /// Free-form tags for filtering / categorisation.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional caveat surfaced in the deploy modal — used when the image
    /// violates a platform invariant (root user, privileged port) but is kept
    /// in the catalog for clusters that allow it.
    #[serde(default)]
    pub warning: Option<String>,
}

/// Configuration for the bootstrapped default Organization resource.
///
/// The `name` and `display_name` fields populate the Organization's metadata
/// and spec respectively. `kubernetes_namespace_prefix`, when set, is written
/// to the `kubernetes.rise.dev/namespace-prefix` annotation. Unrelated
/// annotations on an existing default Organization are preserved across
/// bootstrap runs.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DefaultOrganizationSettings {
    /// Resource name of the default Organization (DNS-label form). Default:
    /// `default`.
    #[serde(default = "default_default_organization_name")]
    pub name: String,
    /// Human-readable display name stored in `spec.displayName`. Default:
    /// `Default`.
    #[serde(default = "default_default_organization_display_name")]
    pub display_name: String,
    /// Extra annotations to apply to the Organization's metadata. The
    /// `kubernetes.rise.dev/namespace-prefix` annotation is managed
    /// separately by `kubernetes_namespace_prefix` and overrides any entry
    /// with the same key set here.
    #[serde(default)]
    pub annotations: std::collections::BTreeMap<String, String>,
    /// Value for the `kubernetes.rise.dev/namespace-prefix` annotation on the
    /// default Organization. When set, bootstrap stamps the annotation with
    /// this value; when `None`, the annotation is left unset and the
    /// Kubernetes controller falls back to `org-{discriminator}-` (see
    /// [`crate::server::bootstrap::DefaultOrganizationView::resolved_namespace_prefix`]).
    ///
    /// Upgrade note: installs that previously relied on the historic
    /// `rise-{project_name}` namespace naming must set this to `"rise-"`
    /// explicitly before deploying — otherwise the controller will switch
    /// to `org-{discriminator}-{project_name}` and orphan the legacy
    /// namespaces.
    #[serde(default)]
    pub kubernetes_namespace_prefix: Option<String>,
}

impl Default for DefaultOrganizationSettings {
    fn default() -> Self {
        Self {
            name: default_default_organization_name(),
            display_name: default_default_organization_display_name(),
            annotations: std::collections::BTreeMap::new(),
            kubernetes_namespace_prefix: None,
        }
    }
}

fn default_loki_timeout_secs() -> u64 {
    10
}

fn default_kubernetes_max_tail_lines() -> i64 {
    100_000
}

fn default_loki_project_label() -> String {
    "rise_project".to_string()
}

fn default_loki_deployment_id_label() -> String {
    "rise_deployment_id".to_string()
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct LokiLabels {
    /// Loki stream label that carries the Rise project name.
    #[serde(default = "default_loki_project_label")]
    pub project: String,
    /// Loki stream label that carries the Rise deployment id (e.g.
    /// "20241205-1234"). Together with `project`, this must uniquely
    /// identify a deployment's log stream.
    #[serde(default = "default_loki_deployment_id_label")]
    pub deployment_id: String,
}

impl Default for LokiLabels {
    fn default() -> Self {
        Self {
            project: default_loki_project_label(),
            deployment_id: default_loki_deployment_id_label(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KubernetesLogBackendSettings {
    /// Upper bound on the number of lines the backend will ever request from
    /// the kubelet in a single call. The frontend pages backward by widening
    /// `tail_lines`; once `tail_lines + skip_recent` reaches this ceiling,
    /// paging stops yielding new lines — the same outcome as when the
    /// kubelet's own ring buffer is exhausted. Default: 100000.
    #[serde(default = "default_kubernetes_max_tail_lines")]
    pub max_tail_lines: i64,
}

impl Default for KubernetesLogBackendSettings {
    fn default() -> Self {
        Self {
            max_tail_lines: default_kubernetes_max_tail_lines(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DeploymentLogsSettings {
    Kubernetes {
        #[serde(flatten)]
        config: KubernetesLogBackendSettings,
    },
    /// Stream logs directly from the Docker daemon (`docker logs`). Used with
    /// the Docker deployment controller. Requires no kube client — the bollard
    /// client is shared from the Docker deployment controller, so this variant
    /// carries no connection fields of its own.
    Docker {},
    Loki {
        url: String,
        #[serde(default)]
        tenant_id: Option<String>,
        #[serde(default)]
        bearer_token_env: Option<String>,
        #[serde(default = "default_loki_timeout_secs")]
        timeout_secs: u64,
        #[serde(default)]
        retention_hint: Option<String>,
        /// Override the Loki stream label names. Useful when pointing Rise
        /// at an operator-managed Loki/Alloy stack that labels logs
        /// differently than the bundled chart.
        #[serde(default)]
        labels: LokiLabels,
    },
}

impl Default for DeploymentLogsSettings {
    fn default() -> Self {
        Self::Kubernetes {
            config: KubernetesLogBackendSettings::default(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, JsonSchema)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub public_url: String,
    /// Development-only frontend proxy target (for Vite), e.g. "http://localhost:5173"
    /// When set, non-API frontend routes are proxied to this URL instead of serving embedded assets.
    /// An empty/whitespace value (e.g. an unset `${VAR:-}` env default) is treated as `None`
    /// so a production config can env-drive this without accidentally proxying to "".
    #[serde(default, deserialize_with = "deserialize_optional_nonempty_string")]
    pub frontend_dev_proxy_url: Option<String>,

    /// Whether to set Secure flag on cookies (true for HTTPS, false for HTTP development)
    // Accepts a native YAML boolean or a string ("true"/"false") via
    // deserialize_bool_flexible, so it can be driven by an env-interpolated
    // config value (e.g. cookie_secure: "${RISE_COOKIE_SECURE:-false}"), which
    // the loader resolves to a string before deserialization. Kept as `//` (not
    // `///`) so the generated JSON schema description is unchanged.
    #[serde(
        default = "default_cookie_secure",
        deserialize_with = "crate::server::ssrf::deserialize_bool_flexible"
    )]
    #[schemars(with = "bool")]
    pub cookie_secure: bool,

    /// Cookie domain for migration from deployments that previously used domain-scoped cookies.
    /// When configured, responses that set a new host-only `rise_jwt` cookie also include a
    /// `Max-Age=0` Set-Cookie with this Domain attribute to expire any stale domain-scoped
    /// cookies browsers may still be sending. Remove once the migration window has passed.
    #[serde(default)]
    pub cookie_domain: Option<String>,

    /// JWT signing secret for ingress authentication (base64-encoded, minimum 32 bytes)
    /// Generate with: openssl rand -base64 32
    /// Required for ingress authentication
    pub jwt_signing_secret: String,

    /// Optional RS256 private key in PEM format for JWT signing
    /// If not provided, a new key pair will be generated on startup (tokens will be invalidated on restart)
    /// To persist keys across restarts, generate with: openssl genrsa -out rs256.key 2048
    #[serde(default)]
    pub rs256_private_key_pem: Option<String>,

    /// Optional RS256 public key in PEM format for JWT verification
    /// If not provided, will be derived from rs256_private_key_pem or generated
    /// Generate from private key with: openssl rsa -in rs256.key -pubout -out rs256.pub
    #[serde(default)]
    pub rs256_public_key_pem: Option<String>,

    /// JWT claims to include from IdP token when issuing Rise JWTs
    /// Default: ["sub", "email", "name"]
    #[serde(default = "default_jwt_claims")]
    pub jwt_claims: Vec<String>,

    /// JWT token expiry duration in seconds
    /// Default: 86400 (24 hours)
    #[serde(default = "default_jwt_expiry_seconds")]
    pub jwt_expiry_seconds: u64,

    /// Directory containing static assets (Tera templates, SVGs, Vite build output).
    /// Defaults to the RISE_STATIC_DIR environment variable.
    #[serde(default = "default_static_dir")]
    pub static_dir: Option<String>,

    /// Directory containing built static documentation files (e.g., "/var/rise/docs")
    /// Defaults to the RISE_DOCS_DIR environment variable.
    #[serde(default = "default_docs_dir")]
    pub docs_dir: Option<String>,

    /// SSRF validation configuration.
    #[serde(default)]
    pub ssrf: super::ssrf::SsrfConfig,

    /// OAuth endpoint rate limiting configuration.
    #[serde(default)]
    pub oauth_rate_limit: OAuthRateLimitSettings,

    /// Maximum TTL in seconds for workload identity tokens issued via the token-exchange endpoint.
    /// Requests that specify a higher TTL are silently capped to this value.
    /// Default: 900 (15 minutes).
    #[serde(default = "default_workload_token_max_ttl_seconds")]
    pub workload_token_max_ttl_seconds: u64,

    /// Lifetime in seconds of Rise access tokens minted by the auth
    /// token-exchange endpoint (`POST /api/v1/auth/token`). Kept short because an
    /// exchanged token cannot be revoked mid-life. Default: 600 (10 minutes).
    #[serde(default = "default_auth_token_max_ttl_seconds")]
    pub auth_token_max_ttl_seconds: u64,
}

/// Rate limiting configuration for OAuth endpoints (authorize, callback, token).
///
/// Four independent limits are enforced:
/// - **Per-project**: keyed by project name — limits traffic to each project independently
///   so one busy project cannot starve others.
/// - **Per-IP**: keyed by client IP — limits traffic from a single source address.
/// - **Per-session**: keyed by a hash of the `rise_jwt` cookie — limits per-user traffic.
/// - **Global**: shared across all requests — caps total throughput.
///
/// Setting any `*_max` value to `0` effectively blocks all OAuth traffic for that tier
/// (every request will exceed the limit immediately).
#[derive(Debug, Deserialize, Clone, JsonSchema)]
pub struct OAuthRateLimitSettings {
    /// Maximum requests per project per window (default: 500)
    #[serde(default = "default_oauth_per_project_max")]
    pub per_project_max: u32,
    /// Window in seconds for per-project limit (default: 60)
    #[serde(default = "default_oauth_per_project_window_secs")]
    pub per_project_window_secs: u64,

    /// Maximum requests per client IP per window (default: 50)
    #[serde(default = "default_oauth_per_ip_max")]
    pub per_ip_max: u32,
    /// Window in seconds for per-IP limit (default: 60)
    #[serde(default = "default_oauth_per_ip_window_secs")]
    pub per_ip_window_secs: u64,

    /// Maximum requests per session (rise_jwt cookie) per window (default: 20)
    #[serde(default = "default_oauth_per_session_max")]
    pub per_session_max: u32,
    /// Window in seconds for per-session limit (default: 60)
    #[serde(default = "default_oauth_per_session_window_secs")]
    pub per_session_window_secs: u64,

    /// Maximum total OAuth requests per window across all clients (default: 2000)
    #[serde(default = "default_oauth_global_max")]
    pub global_max: u32,
    /// Window in seconds for the global limit (default: 60)
    #[serde(default = "default_oauth_global_window_secs")]
    pub global_window_secs: u64,
}

impl Default for OAuthRateLimitSettings {
    fn default() -> Self {
        Self {
            per_project_max: default_oauth_per_project_max(),
            per_project_window_secs: default_oauth_per_project_window_secs(),
            per_ip_max: default_oauth_per_ip_max(),
            per_ip_window_secs: default_oauth_per_ip_window_secs(),
            per_session_max: default_oauth_per_session_max(),
            per_session_window_secs: default_oauth_per_session_window_secs(),
            global_max: default_oauth_global_max(),
            global_window_secs: default_oauth_global_window_secs(),
        }
    }
}

fn default_cookie_secure() -> bool {
    true
}

/// Deserialize an `Option<String>` where a blank/whitespace-only value is `None`.
///
/// Lets a config env-drive an optional field via `${VAR:-}`: when the env var is
/// unset the loader yields the empty string `""`, which would otherwise become
/// `Some("")`. Trimming it to `None` keeps the "unset" semantics (e.g. so a
/// production config never proxies the frontend to "").
fn deserialize_optional_nonempty_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.filter(|s| !s.trim().is_empty()))
}

/// Accept a `u32` from either a JSON number or a numeric string, so a field can
/// be env-driven via `${VAR:-N}`. The settings loader interpolates `${VAR}` into
/// a JSON string, so `max_replicas: "${RISE_MAX_REPLICAS:-10}"` reaches serde as
/// the string `"10"`; serde won't coerce string→u32, so accept both forms.
fn deserialize_u32_flexible<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum U32OrString {
        Num(u32),
        Str(String),
    }

    match U32OrString::deserialize(deserializer)? {
        U32OrString::Num(n) => Ok(n),
        U32OrString::Str(s) => s
            .trim()
            .parse::<u32>()
            .map_err(|_| D::Error::custom(format!("invalid integer string {s:?}; expected a u32"))),
    }
}

/// Deserialize a list of strings, dropping blank/whitespace-only entries.
///
/// The settings loader interpolates `${VAR}` per string element, so an
/// env-driven single-element list like
/// `app_backend_host_aliases: ["${RISE_APP_BACKEND_HOST_ALIAS:-}"]` reaches
/// serde as `[""]` when the env var is unset (production) and `["rise.localhost"]`
/// when set (local). Filtering blanks turns the unset case into an empty list
/// while preserving real entries — sidestepping the inability to express an
/// *empty* YAML list via a scalar env var. Reused for any env-driven list whose
/// "off" state must be an empty list (host aliases, platform-access allowlists).
fn deserialize_blank_filtered_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(deserializer)?;
    Ok(raw.into_iter().filter(|s| !s.trim().is_empty()).collect())
}

fn default_static_dir() -> Option<String> {
    std::env::var("RISE_STATIC_DIR").ok()
}

fn default_docs_dir() -> Option<String> {
    std::env::var("RISE_DOCS_DIR").ok()
}

fn default_jwt_claims() -> Vec<String> {
    vec!["sub".to_string(), "email".to_string(), "name".to_string()]
}

fn default_jwt_expiry_seconds() -> u64 {
    86400 // 24 hours
}

fn default_workload_token_max_ttl_seconds() -> u64 {
    900 // 15 minutes
}

fn default_auth_token_max_ttl_seconds() -> u64 {
    600 // 10 minutes
}

fn default_allow_raw_external_tokens() -> bool {
    true
}

fn default_identity_token_ttl_seconds() -> u64 {
    3600 // 1 hour
}

fn default_oauth_per_project_max() -> u32 {
    500
}

fn default_oauth_per_project_window_secs() -> u64 {
    60
}

fn default_oauth_per_ip_max() -> u32 {
    50
}

fn default_oauth_per_ip_window_secs() -> u64 {
    60
}

fn default_oauth_per_session_max() -> u32 {
    20
}

fn default_oauth_per_session_window_secs() -> u64 {
    60
}

fn default_oauth_global_max() -> u32 {
    2000
}

fn default_oauth_global_window_secs() -> u64 {
    60
}

fn default_idp_group_sync_enabled() -> bool {
    true
}

fn default_active_sync_interval_secs() -> u64 {
    300 // 5 minutes
}

fn default_oauth_scopes() -> Vec<String> {
    // `offline_access` is intentionally omitted: the CLI does not use refresh
    // tokens, and the scope is rejected by some providers (e.g. Google). Add it
    // back via `auth.scopes` if your provider requires it for other reasons.
    vec![
        "openid".to_string(),
        "email".to_string(),
        "profile".to_string(),
    ]
}

/// Supported active sync sources for pulling users and groups
#[derive(Debug, Deserialize, Clone, JsonSchema, PartialEq)]
pub enum ActiveSyncSource {
    /// Microsoft Entra ID (Azure AD) - uses Microsoft Graph API to pull
    /// users and groups assigned to the configured app registration.
    Entra,
}

fn default_allow_team_creation() -> bool {
    true // Backward compatible - existing behavior
}

fn default_allow_list_all_teams() -> bool {
    false // Backward compatible - non-admins only see their own teams by default
}

#[derive(Debug, Deserialize, Clone, JsonSchema)]
pub struct AuthSettings {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// List of admin user emails for the default organization. Admins do NOT
    /// implicitly receive the Operator role — list the email in
    /// `operator_users` separately if needed.
    #[serde(default)]
    pub admin_users: Vec<String>,
    /// List of Operator user emails. Operators have full access to generic
    /// resource storage and built-in resource management. This is a separate
    /// role from `admin_users`. Matching is case-insensitive. Blank entries are
    /// filtered out so an unset `${VAR}` default collapses to an empty list.
    #[serde(default, deserialize_with = "deserialize_blank_filtered_list")]
    pub operator_users: Vec<String>,
    /// Trusted external controller identities. Each entry binds a stable
    /// controller ID to an OIDC issuer plus required claim constraints.
    /// Used by generic-resource controller endpoints.
    #[serde(default)]
    pub controllers: Vec<crate::server::auth::controller::ControllerIdentity>,
    /// Platform access control configuration
    #[serde(default)]
    pub platform_access: PlatformAccessConfig,
    /// Accept raw external OIDC tokens directly at request time (default: true).
    ///
    /// When `true`, a CI service account may present its external OIDC token
    /// directly to project-scoped endpoints, and Rise resolves the service
    /// account per request (the legacy two-phase path). When `false`, callers
    /// must first exchange their token at `POST /api/v1/auth/token` for a Rise
    /// access token.
    ///
    /// Leaving this `true` keeps the legacy attack surface and forfeits the
    /// security benefits of the exchange (no service-account DB lookups in the
    /// request hot path, and an authenticated `platform/capabilities`). While it
    /// is `true`, every raw-token request is counted per `(issuer, sub)` and
    /// surfaced as a `rise::deprecation` `tracing` event so operators can see
    /// which workload identities still need migrating. Defaults to `false`
    /// starting in 0.25.0; migrate CI to pre-exchange before upgrading.
    #[serde(default = "default_allow_raw_external_tokens")]
    pub allow_raw_external_tokens: bool,
    /// Allow regular users to create teams (default: true).
    /// When false, only admin users can create teams.
    #[serde(default = "default_allow_team_creation")]
    pub allow_team_creation: bool,
    /// Allow all users to list all teams (default: false).
    /// When false, non-admin users only see teams they are members of.
    #[serde(default = "default_allow_list_all_teams")]
    pub allow_list_all_teams: bool,
    /// Optional custom authorize endpoint URL
    /// If not set, will be discovered from issuer's .well-known/openid-configuration
    /// or default to {issuer}/authorize
    #[serde(default)]
    pub authorize_url: Option<String>,
    /// Optional custom token endpoint URL
    /// If not set, will be discovered from issuer's .well-known/openid-configuration
    /// or default to {issuer}/token
    #[serde(default)]
    pub token_url: Option<String>,
    /// Enable IdP group synchronization (default: true)
    /// When enabled, user team memberships are automatically synced from IdP groups claim on login
    #[serde(default = "default_idp_group_sync_enabled")]
    pub idp_group_sync_enabled: bool,
    /// Optional active sync source for pulling users and groups from an external IdP.
    /// When configured, Rise will periodically query the IdP for users and groups
    /// assigned to the app and sync them as Rise teams.
    #[serde(default)]
    pub active_sync_source: Option<ActiveSyncSource>,
    /// Interval in seconds for active sync polling (default: 300 = 5 minutes)
    #[serde(default = "default_active_sync_interval_secs")]
    pub active_sync_interval_secs: u64,
    /// OAuth2 scopes requested during login flows.
    /// Defaults to `["openid", "email", "profile"]`. `offline_access` is not
    /// requested by default (the CLI does not use refresh tokens, and the scope
    /// is rejected by some providers such as Google); add it here if needed.
    #[serde(default = "default_oauth_scopes")]
    pub scopes: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Clone, JsonSchema)]
pub struct DatabaseSettings {
    #[serde(default)]
    pub url: String,
}

fn default_repo_prefix() -> String {
    "rise/".to_string()
}

fn default_ingress_schema() -> String {
    "https".to_string()
}

fn default_controller_class_name() -> String {
    "default".to_string()
}

fn default_default_organization_name() -> String {
    "default".to_string()
}

fn default_default_organization_display_name() -> String {
    "Default".to_string()
}

fn default_node_selector() -> std::collections::HashMap<String, String> {
    let mut selector = std::collections::HashMap::new();
    selector.insert("kubernetes.io/arch".to_string(), "amd64".to_string());
    selector
}

/// Backend address for routing /.rise/* traffic to the Rise backend
#[derive(Debug, Clone)]
pub struct BackendAddress {
    pub host: String,
    pub port: u16,
}

impl BackendAddress {
    /// Parse backend address from a URL by extracting host and port
    /// Example: "http://172.17.0.1:3000" -> BackendAddress { host: "172.17.0.1", port: 3000 }
    pub fn from_url(url: &str) -> Result<Self, anyhow::Error> {
        let parsed = url::Url::parse(url)
            .map_err(|e| anyhow::anyhow!("Invalid URL for backend address: {}", e))?;

        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("URL missing host"))?
            .to_string();

        let port = parsed
            .port()
            .or_else(|| {
                // Default ports based on scheme
                match parsed.scheme() {
                    "http" => Some(80),
                    "https" => Some(443),
                    _ => None,
                }
            })
            .ok_or_else(|| anyhow::anyhow!("URL missing port and no default for scheme"))?;

        Ok(Self { host, port })
    }

    /// Check if the host is an IP address (vs a DNS name)
    pub fn is_ip_address(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }
}

/// TLS mode for custom domains
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CustomDomainTlsMode {
    /// All hosts (primary + custom domains) share the same TLS secret
    Shared,
    /// Each custom domain gets its own tls-{domain} secret (cert-manager integration)
    PerDomain,
}

fn default_metacontroller_webhook_port() -> u16 {
    3001
}

// ── Docker deployment controller defaults ──────────────────────────────

#[cfg(feature = "backend")]
fn default_traefik_entrypoint() -> String {
    "web".to_string()
}

#[cfg(feature = "backend")]
pub(crate) fn default_label_namespace() -> String {
    "rise.dev".to_string()
}

#[cfg(feature = "backend")]
fn default_container_prefix() -> String {
    "rise".to_string()
}

#[cfg(feature = "backend")]
fn default_docker_access_classes() -> std::collections::HashMap<String, Option<AccessClass>> {
    let mut classes = std::collections::HashMap::new();
    classes.insert(
        "public".to_string(),
        Some(AccessClass {
            display_name: "Public".to_string(),
            description: "Fully public — no authentication required.".to_string(),
            ingress_class: "traefik".to_string(),
            access_requirement: AccessRequirement::None,
            custom_annotations: std::collections::HashMap::new(),
        }),
    );
    classes
}

#[cfg(feature = "backend")]
fn default_reconcile_interval_secs() -> u64 {
    5
}

fn default_crd_upsert_interval_ms() -> u64 {
    1000
}

fn default_custom_domain_tls_mode() -> CustomDomainTlsMode {
    CustomDomainTlsMode::PerDomain
}

/// Access requirement level for project ingress
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum AccessRequirement {
    /// No authentication required - fully public access
    None,
    /// Must be authenticated, but no project membership required
    Authenticated,
    /// Must be authenticated AND have project membership (owner or team member)
    Member,
}

/// Access class configuration for ingress authentication
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct AccessClass {
    /// Display name for UI (e.g., "Public")
    pub display_name: String,

    /// Description for UI
    pub description: String,

    /// Ingress class to use
    pub ingress_class: String,

    /// Access requirement level
    pub access_requirement: AccessRequirement,

    /// Optional custom nginx annotations
    #[serde(default)]
    pub custom_annotations: std::collections::HashMap<String, String>,
}

/// Validate that a Docker deployment controller can actually enforce the
/// authentication required by its configured access classes.
///
/// The Docker backend wires ingress authentication via a Traefik forwardAuth
/// middleware whose address is derived from `auth_backend_url`. If that URL is
/// empty/blank, no middleware is stamped and any access class requiring
/// `Authenticated`/`Member` would be served PUBLICLY — failing open. To fail
/// closed instead, this rejects the configuration up-front, naming the
/// offending access class(es).
///
/// `null`-valued access classes (used to remove inherited entries) are ignored.
/// Returns the names of offending access classes (sorted) when the config is
/// invalid, or an empty `Vec` when it is acceptable.
pub fn docker_access_classes_missing_auth_backend_url(
    access_classes: &std::collections::HashMap<String, Option<AccessClass>>,
    auth_backend_url: &str,
) -> Vec<String> {
    if !auth_backend_url.trim().is_empty() {
        return Vec::new();
    }

    let mut offending: Vec<String> = access_classes
        .iter()
        .filter_map(|(name, ac)| {
            ac.as_ref().and_then(|ac| {
                (ac.access_requirement != AccessRequirement::None).then(|| name.clone())
            })
        })
        .collect();
    offending.sort();
    offending
}

/// Default resource values for new deployments when not specified by the user
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeploymentDefaults {
    /// Default number of replicas (default: 1)
    #[serde(default = "default_replicas")]
    pub replicas: u32,

    /// Default CPU allocation (default: "500m") — sets both K8s request and limit
    #[serde(default = "default_cpu")]
    pub cpu: String,

    /// Default memory allocation (default: "256Mi") — sets both K8s request and limit
    #[serde(default = "default_memory")]
    pub memory: String,
}

impl Default for DeploymentDefaults {
    fn default() -> Self {
        Self {
            replicas: default_replicas(),
            cpu: default_cpu(),
            memory: default_memory(),
        }
    }
}

/// Platform-level constraints for deployment resources.
/// Per-environment overrides (stored in the database) can narrow these ranges.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeploymentConstraints {
    /// Minimum replicas allowed (default: 1)
    #[serde(default = "default_min_replicas")]
    pub min_replicas: u32,

    /// Maximum replicas allowed (default: 1). Accepts a number or a numeric
    /// string so it can be env-driven (e.g. `"${RISE_MAX_REPLICAS:-10}"`).
    #[serde(
        default = "default_max_replicas",
        deserialize_with = "deserialize_u32_flexible"
    )]
    #[schemars(with = "u32")]
    pub max_replicas: u32,

    /// Minimum CPU allowed (default: "100m")
    #[serde(default = "default_min_cpu")]
    pub min_cpu: String,

    /// Maximum CPU allowed (default: "2")
    #[serde(default = "default_max_cpu")]
    pub max_cpu: String,

    /// Minimum memory allowed (default: "64Mi")
    #[serde(default = "default_min_memory")]
    pub min_memory: String,

    /// Maximum memory allowed (default: "2Gi")
    #[serde(default = "default_max_memory")]
    pub max_memory: String,
}

impl Default for DeploymentConstraints {
    fn default() -> Self {
        Self {
            min_replicas: default_min_replicas(),
            max_replicas: default_max_replicas(),
            min_cpu: default_min_cpu(),
            max_cpu: default_max_cpu(),
            min_memory: default_min_memory(),
            max_memory: default_max_memory(),
        }
    }
}

/// Health probe configuration
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HealthProbeConfig {
    /// Enable liveness probes (default: true)
    #[serde(default = "default_true")]
    pub liveness_enabled: bool,

    /// Enable readiness probes (default: true)
    #[serde(default = "default_true")]
    pub readiness_enabled: bool,

    /// Path for HTTP probes (default: "/")
    #[serde(default = "default_probe_path")]
    pub path: String,

    /// Initial delay in seconds (default: 10)
    #[serde(default = "default_initial_delay")]
    pub initial_delay_seconds: i32,

    /// Period in seconds (default: 10)
    #[serde(default = "default_period_seconds")]
    pub period_seconds: i32,

    /// Timeout in seconds (default: 5)
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: i32,

    /// Failure threshold (default: 3)
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: i32,
}

/// NetworkPolicy configuration for deployed apps
///
/// Uses Kubernetes NetworkPolicy types directly. Egress semantics:
/// - null: policyTypes is ["Ingress"] only, Kubernetes does not restrict egress
/// - Empty list: policyTypes includes "Egress" with no rules = deny all egress
/// - Non-empty list: explicit egress rules enforced
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct NetworkPolicyConfig {
    /// Ingress rules
    pub ingress: Vec<k8s_openapi::api::networking::v1::NetworkPolicyIngressRule>,
    /// Egress rules (null = unrestricted egress)
    pub egress: Option<Vec<k8s_openapi::api::networking::v1::NetworkPolicyEgressRule>>,
}

// Default functions for pod security settings
fn default_use_default_service_account_for_production() -> bool {
    true
}

fn default_pod_security_enabled() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_replicas() -> u32 {
    1
}

fn default_cpu() -> String {
    "500m".to_string()
}

fn default_memory() -> String {
    "256Mi".to_string()
}

fn default_min_replicas() -> u32 {
    1
}

fn default_max_replicas() -> u32 {
    1
}

fn default_min_cpu() -> String {
    "100m".to_string()
}

fn default_max_cpu() -> String {
    "2".to_string()
}

fn default_min_memory() -> String {
    "64Mi".to_string()
}

fn default_max_memory() -> String {
    "2Gi".to_string()
}

fn default_probe_path() -> String {
    "/".to_string()
}

fn default_initial_delay() -> i32 {
    10
}

fn default_period_seconds() -> i32 {
    10
}

fn default_timeout_seconds() -> i32 {
    5
}

fn default_failure_threshold() -> i32 {
    3
}

/// Deployment controller configuration
// Config enums are read once at startup, never stored in hot collections, so the
// size disparity between the (large) Kubernetes variant and the (small) Docker
// variant doesn't matter — boxing every Kubernetes field would only obscure the
// settings schema.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DeploymentControllerSettings {
    /// Kubernetes deployment controller
    #[cfg(feature = "backend")]
    Kubernetes {
        /// Optional kubeconfig path (defaults to in-cluster or ~/.kube/config)
        #[serde(default)]
        kubeconfig: Option<String>,

        /// Ingress URL template for production (default) deployment group
        /// Supports both subdomain and sub-path routing:
        ///   Subdomain: "{project_name}.apps.rise.dev"
        ///   Sub-path: "rise.dev/{project_name}"
        /// Must contain {project_name} placeholder
        production_ingress_url_template: String,

        /// Ingress URL template for staging (non-default) deployment groups
        /// Supports both subdomain and sub-path routing:
        ///   Subdomain: "{project_name}-{deployment_group}.preview.rise.dev"
        ///   Sub-path: "rise.dev/{project_name}/{deployment_group}"
        /// Must contain both {project_name} and {deployment_group} placeholders
        /// If not set, falls back to inserting "-{deployment_group}" before first dot
        #[serde(default)]
        staging_ingress_url_template: Option<String>,

        /// Ingress URL template for named environments (e.g., staging, dev)
        /// Only used for non-production environments whose primary deployment group matches
        /// the deployment's group. Production environments use `production_ingress_url_template`.
        /// Supports both subdomain and sub-path routing:
        ///   Subdomain: "{environment}--{project_name}.apps.rise.dev"
        ///   Sub-path: "rise.dev/{project_name}/{environment}"
        /// Must contain both {project_name} and {environment} placeholders
        /// If not set, environment-specific URLs are not generated.
        #[serde(default)]
        environment_ingress_url_template: Option<String>,

        /// Optional port number to append to all generated ingress URLs
        /// Used for development environments with port-forwarding (e.g., kubectl port-forward)
        /// Example: 8080 → "https://myapp.apps.rise.local:8080"
        /// If not set, URLs use standard ports (80 for HTTP, 443 for HTTPS)
        #[serde(default)]
        ingress_port: Option<u16>,

        /// URL scheme for generated ingress URLs
        /// Used to specify whether URLs should use "http" or "https"
        /// Example: "http" → "http://myapp.apps.rise.local"
        /// Defaults to "https"
        #[serde(default = "default_ingress_schema")]
        ingress_schema: String,

        /// Backend URL for Nginx auth subrequests (internal cluster URL)
        /// Example: "http://rise-backend.default.svc.cluster.local:3000"
        /// This is the URL Nginx will use internally within the cluster to validate authentication.
        /// For Minikube development, use the Docker bridge IP to reach host (e.g., "http://host.minikube.internal:3000").
        auth_backend_url: String,

        /// Public backend URL for browser redirects during authentication
        /// Example: "https://rise.dev"
        /// This must be the public URL where the backend is accessible via Ingress.
        /// The domain should share a parent with app domains for cookie sharing (see struct docs).
        auth_signin_url: String,

        /// Stable identifier for this deployment controller, written to the
        /// default Organization's `spec.deploymentControllerClass` at startup.
        /// The Kubernetes controller only reconciles projects whose
        /// Organization's `spec.deploymentControllerClass` matches this value.
        /// Stamped as a value of the `rise.dev/controller-class` label on
        /// each `RiseProject` CR, so it must be a valid Kubernetes label
        /// value (alphanumeric / `-` / `_` / `.`, no `/`). Defaults to
        /// `default`.
        #[serde(default = "default_controller_class_name")]
        controller_class_name: String,

        /// Labels to apply to all managed namespaces
        /// Example: {"environment": "production", "team": "platform"}
        #[serde(default)]
        namespace_labels: std::collections::HashMap<String, String>,

        /// Annotations to apply to all managed namespaces
        /// Example: {"company.com/team": "platform", "cost-center": "engineering"}
        #[serde(default)]
        namespace_annotations: std::collections::HashMap<String, String>,

        /// Ingress annotations to apply to all deployed application ingresses
        /// These apply to both primary ingresses and custom domain ingresses
        /// For annotations specific to custom domains only, use custom_domain_ingress_annotations
        /// Example: {"nginx.ingress.kubernetes.io/proxy-body-size": "10m"}
        #[serde(default)]
        ingress_annotations: std::collections::HashMap<String, String>,

        /// TLS secret name for primary ingress certificates
        /// If set, enables TLS on primary ingresses with this secret
        /// For custom domain TLS, see custom_domain_tls_mode and custom_domain_ingress_annotations
        /// Example: "rise-apps-tls" (secret must exist in each namespace)
        #[serde(default)]
        ingress_tls_secret_name: Option<String>,

        /// TLS mode for custom domains
        /// - "shared": All custom domains share ingress_tls_secret_name (requires it to be set)
        /// - "per-domain": Each custom domain gets its own tls-{domain} secret
        ///   (works with cert-manager when custom_domain_ingress_annotations are configured)
        ///
        /// Defaults to "per-domain"
        #[serde(default = "default_custom_domain_tls_mode")]
        custom_domain_tls_mode: CustomDomainTlsMode,

        /// Annotations to apply ONLY to custom domain ingresses (not primary ingresses)
        /// Use this for cert-manager integration or other custom domain-specific configuration
        /// Example: {"cert-manager.io/cluster-issuer": "letsencrypt-prod"}
        #[serde(default)]
        custom_domain_ingress_annotations: std::collections::HashMap<String, String>,

        /// Node selector for pod placement (controls which nodes pods can run on)
        /// Default: {"kubernetes.io/arch": "amd64"}
        /// Example: {"kubernetes.io/arch": "amd64", "node-type": "compute"}
        #[serde(default = "default_node_selector")]
        node_selector: std::collections::HashMap<String, String>,

        /// Optional name of an existing imagePullSecret to use for deployments
        ///
        /// If not specified:
        ///   - With a registry provider (e.g., ECR): The controller creates and manages the secret
        ///   - Without a registry provider: No image pull secret is used
        ///
        /// If specified:
        ///   - The named secret must exist in each project namespace
        ///   - The controller will NOT create or manage the secret
        ///   - Useful for static registries where credentials are managed externally
        ///
        /// Example: "my-registry-secret"
        #[serde(default)]
        image_pull_secret_name: Option<String>,

        /// Access classes defining ingress authentication levels
        /// Key: access class identifier (e.g., "public", "private")
        /// Value: access class configuration (display info, ingress settings)
        /// Use `null` in YAML to remove an inherited access class from parent configs
        access_classes: std::collections::HashMap<String, Option<AccessClass>>,

        /// Host aliases to inject into pod specs (hostname -> IP address)
        /// Maps hostnames to IP addresses, injected as Kubernetes hostAliases.
        /// Useful for local development where pods need to resolve custom hostnames.
        /// Example: {"rise.local": "192.168.49.1"}
        #[serde(default)]
        host_aliases: std::collections::HashMap<String, String>,

        /// Extra projected service account tokens to mount into every deployed app pod.
        /// Key becomes the in-pod filename under /var/run/secrets/rise/tokens/, value is the audience.
        /// Example: {"vault": "https://vault.example.com"}
        #[serde(default)]
        extra_service_token_audiences: std::collections::HashMap<String, String>,

        /// When true, deployments in the production environment use the namespace's
        /// `default` ServiceAccount instead of creating `env-{name}`.
        /// Useful for backwards compatibility when existing IAM bindings (e.g., IRSA)
        /// are configured on the `default` SA.
        /// Defaults to true.
        #[serde(default = "default_use_default_service_account_for_production")]
        use_default_service_account_for_production: bool,

        /// NetworkPolicy configuration for deployed apps
        network_policy: NetworkPolicyConfig,

        /// Pod security settings (enabled by default)
        /// Set to false to disable security context enforcement
        #[serde(default = "default_pod_security_enabled")]
        pod_security_enabled: bool,

        /// Default resource values for new deployments when not specified by the user
        #[serde(default)]
        deployment_defaults: DeploymentDefaults,

        /// Platform-level constraints for deployment resources
        #[serde(default)]
        deployment_constraints: DeploymentConstraints,

        /// Health probe configuration
        /// If not set, uses defaults (HTTP probes on app port at "/" path)
        #[serde(default)]
        health_probes: Option<HealthProbeConfig>,

        /// Lifetime in seconds of workload identity tokens auto-minted by the controller
        /// and mounted into deployment pods. The controller re-mints tokens when they
        /// are older than half this value. Default: 3600 (1 hour).
        #[serde(default = "default_identity_token_ttl_seconds")]
        identity_token_ttl_seconds: u64,

        /// Port for the internal metacontroller webhook listener.
        /// Webhook endpoints are served on this separate port instead of the main HTTP port.
        /// Defaults to 3001.
        #[serde(default = "default_metacontroller_webhook_port")]
        metacontroller_webhook_port: u16,

        /// Kubernetes namespace where metacontroller pods run.
        /// When set, Rise validates that webhook requests originate from a live
        /// metacontroller pod IP. Absent in development (IP validation disabled).
        #[serde(default)]
        metacontroller_pod_namespace: String,

        /// Label selector used to find metacontroller pods.
        /// Defaults to "app.kubernetes.io/name=metacontroller-operator".
        #[serde(default)]
        metacontroller_pod_label_selector: Option<String>,

        /// Interval in milliseconds between consecutive RiseProject CRD upserts
        /// during the startup backfill. The backfill runs as a background task,
        /// touching every active project's CRD at this rate so that version-label
        /// changes (introduced by an upgrade) are propagated to Metacontroller
        /// gradually rather than all at once. Defaults to 1000 (1 per second).
        #[serde(default = "default_crd_upsert_interval_ms")]
        crd_upsert_interval_ms: u64,
    },

    /// Docker deployment controller.
    ///
    /// Deploys app containers to a single Docker host and lets Traefik's Docker
    /// provider route to them via container labels Rise stamps. No Kubernetes,
    /// no Metacontroller — reconciliation runs in-process (see
    /// `controller::docker::DockerReconciler`).
    #[cfg(feature = "backend")]
    Docker {
        /// Docker daemon connection. `None` uses bollard's local defaults
        /// (unix socket / npipe, or `DOCKER_HOST`). May be a unix socket path
        /// (`unix:///var/run/docker.sock`) or a TCP URL (`tcp://host:2375`).
        #[serde(default)]
        docker_host: Option<String>,

        /// Ingress URL template for the production (default) deployment group.
        /// Same semantics as the Kubernetes variant. Must contain
        /// `{project_name}`. Drives both the computed app URLs and the Traefik
        /// `Host(...)` router rules.
        production_ingress_url_template: String,

        /// Ingress URL template for staging (non-default) deployment groups.
        /// Must contain both `{project_name}` and `{deployment_group}`.
        #[serde(default)]
        staging_ingress_url_template: Option<String>,

        /// Ingress URL template for named environments.
        /// Must contain both `{project_name}` and `{environment}`.
        #[serde(default)]
        environment_ingress_url_template: Option<String>,

        /// Access classes defining ingress authentication levels, keyed by
        /// identifier (e.g. "public", "private"). Mirrors the Kubernetes variant
        /// so the typed project API can validate a project's access class. For
        /// access classes whose `access_requirement` is `Authenticated`/`Member`,
        /// the Docker controller stamps Traefik forwardAuth middleware labels
        /// (requires `auth_backend_url` to be set). A permissive "public" class
        /// is provided by default; operators may override.
        /// Use `null` in YAML to remove an inherited access class.
        #[serde(default = "default_docker_access_classes")]
        access_classes: std::collections::HashMap<String, Option<AccessClass>>,

        /// Internal URL Traefik uses to reach the Rise backend for the
        /// forwardAuth subrequest (e.g. `http://rise:3000` on the compose
        /// network). Used to build the `forwardauth.address` middleware label
        /// pointing at `/api/v1/auth/ingress`. Required when any configured
        /// access class has a non-`None` access requirement
        /// (`Authenticated`/`Member`); the backend refuses to start otherwise,
        /// to avoid serving those projects publicly with no auth enforced.
        #[serde(default)]
        auth_backend_url: String,

        /// Browser-facing base URL for the login redirect (e.g. the public URL
        /// `http://localhost:3000`). Traefik forwardAuth has no nginx-style
        /// `auth-signin`, so the `ingress_auth` handler 302-redirects
        /// unauthenticated browsers to `{auth_signin_url}/api/v1/auth/signin`.
        /// If empty, falls back to the server `public_url`.
        #[serde(default)]
        auth_signin_url: String,

        /// Optional port appended to all generated ingress URLs (e.g. `8080`).
        #[serde(default)]
        ingress_port: Option<u16>,

        /// URL scheme for generated ingress URLs (`http` or `https`).
        /// Defaults to `https`.
        #[serde(default = "default_ingress_schema")]
        ingress_schema: String,

        /// Docker network shared with Traefik. App containers are attached to
        /// this network so Traefik can reach them, and it is emitted as the
        /// `traefik.docker.network` label.
        traefik_network: String,

        /// Traefik entrypoint name routers bind to (e.g. `web`, `websecure`).
        /// Defaults to `web`.
        #[serde(default = "default_traefik_entrypoint")]
        traefik_entrypoint: String,

        /// Optional Traefik certresolver name. When set, routers get
        /// `tls=true` + `tls.certresolver=<name>` labels for automatic TLS.
        #[serde(default)]
        traefik_certresolver: Option<String>,

        /// **LOCAL-DEV ONLY.** Hostname(s) to alias to the Rise backend's IP on
        /// the shared `traefik_network`, injected as `extra_hosts` on every
        /// managed app container the controller creates.
        ///
        /// Apps that validate the `rise_jwt` cookie or perform OIDC discovery
        /// against the public issuer host (e.g. `rise.localhost`) must be able
        /// to reach the Rise backend at that host. In a local stack the public
        /// host resolves to the container's own loopback, not the backend, so
        /// the controller stamps `HostConfig.extra_hosts` mapping each alias to
        /// the backend's reachable IP (resolved at reconcile time from the
        /// `auth_backend_url` host via Docker DNS).
        ///
        /// **Leave empty in production.** Production relies on public DNS +
        /// Traefik for the issuer host (with correct TLS termination); injecting
        /// an `extra_hosts` override there would wrongly bypass Traefik and break
        /// TLS. Empty/absent (the default) → no injection.
        ///
        /// Mirrors the Kubernetes `host_aliases` mechanism (see
        /// `resource_builder.rs`, which injects `rise.local → host IP` into pods).
        ///
        /// Blank entries are filtered out at load time, so an env-driven
        /// single-element list (`["${RISE_APP_BACKEND_HOST_ALIAS:-}"]`) collapses
        /// to an empty list — and no injection — when the env var is unset
        /// (production).
        #[serde(default, deserialize_with = "deserialize_blank_filtered_list")]
        app_backend_host_aliases: Vec<String>,

        /// **LOCAL-DEV ONLY.** Explicit IP/value to alias the
        /// `app_backend_host_aliases` hosts to (injected as `extra_hosts` on
        /// managed app containers), used VERBATIM instead of resolving the
        /// `auth_backend_url` host via DNS.
        ///
        /// Set to Docker's special `host-gateway` so apps reach a host-run
        /// backend (`mise br docker`) on both Docker Desktop and Linux — Docker
        /// replaces `host-gateway` with the host gateway per container at create
        /// time. Empty/absent (the default) → fall back to DNS resolution of the
        /// `auth_backend_url` host. **Leave unset in production** (the
        /// containerized backend is resolved by Docker DNS).
        #[serde(default, deserialize_with = "deserialize_optional_nonempty_string")]
        app_backend_ip: Option<String>,

        /// Label namespace prefix for Rise bookkeeping labels
        /// (e.g. `rise.dev/managed-by`). Defaults to `rise.dev`.
        #[serde(default = "default_label_namespace")]
        label_namespace: String,

        /// Prefix for generated container names (`<prefix>_<project>_...`).
        /// Defaults to `rise`.
        #[serde(default = "default_container_prefix")]
        container_prefix: String,

        /// Stable identifier for this deployment controller. The reconciler only
        /// reconciles projects matching this class. Defaults to `default`.
        #[serde(default = "default_controller_class_name")]
        controller_class_name: String,

        /// Interval in seconds between reconcile ticks. Defaults to 5.
        #[serde(default = "default_reconcile_interval_secs")]
        reconcile_interval_secs: u64,

        /// Default resource values for new deployments when not specified.
        #[serde(default)]
        deployment_defaults: DeploymentDefaults,

        /// Platform-level constraints for deployment resources.
        #[serde(default)]
        deployment_constraints: DeploymentConstraints,

        /// Health probe configuration. The reconciler probes routable
        /// containers over the shared network using these settings.
        #[serde(default)]
        health_probes: Option<HealthProbeConfig>,

        /// Lifetime in seconds of workload identity tokens minted for
        /// deployments. Default: 3600 (1 hour).
        #[serde(default = "default_identity_token_ttl_seconds")]
        identity_token_ttl_seconds: u64,

        /// **Dev-only.** Publish each app container's HTTP port to a random
        /// `127.0.0.1` host port so a host-run backend (`mise br docker` /
        /// Docker Desktop, where container bridge IPs aren't routable from the
        /// host) can health-probe the app directly. Leave off in production —
        /// the containerized backend reaches app containers over the
        /// `rise_default` network.
        // Accepts a native YAML boolean or a string ("true"/"false") via
        // deserialize_bool_flexible, so it can be driven by an env-interpolated
        // config value (e.g. publish_app_ports:
        // "${RISE_PUBLISH_APP_PORTS:-false}"), which the loader resolves to a
        // string before deserialization. Kept as `//` (not `///`) so the
        // generated JSON schema description is unchanged.
        #[serde(
            default,
            deserialize_with = "crate::server::ssrf::deserialize_bool_flexible"
        )]
        #[schemars(with = "bool")]
        publish_app_ports: bool,

        /// Base URL of Traefik's API (e.g. `http://rise-traefik:8080` in-network
        /// or `http://localhost:8090` for a host-run dev backend). Read to learn
        /// per-server health status (the top-level `serverStatus` map) from
        /// Traefik so the old deployment is retired only once the new servers are
        /// actually in Traefik's rotation. Optional basic-auth may be embedded in
        /// the URL (userinfo). **Required for projects that set a `health_check`**:
        /// Traefik's `serverStatus` is the authoritative readiness signal (no
        /// fallback), so without a reachable API a health-checked deployment never
        /// becomes Healthy. May be left empty only when no project uses health
        /// checks (ready-when-running gates on run-state alone).
        #[serde(default, deserialize_with = "deserialize_optional_nonempty_string")]
        traefik_api_url: Option<String>,
    },
}

/// Registry provider configuration
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RegistrySettings {
    Ecr {
        #[allow(dead_code)]
        region: String,
        account_id: String,
        /// Literal prefix for ECR repository names (e.g., "rise/" → repos named "rise/{project}")
        #[serde(default = "default_repo_prefix")]
        #[allow(dead_code)]
        repo_prefix: String,
        /// IAM role ARN for push operations (assumed to generate scoped credentials)
        #[allow(dead_code)]
        push_role_arn: String,
        /// Whether to automatically delete ECR repos when projects are deleted
        #[serde(default)]
        #[allow(dead_code)]
        auto_remove: bool,
        #[serde(default)]
        #[allow(dead_code)]
        access_key_id: Option<String>,
        #[serde(default)]
        #[allow(dead_code)]
        secret_access_key: Option<String>,
    },
    #[serde(rename = "oci-client-auth")]
    OciClientAuth {
        registry_url: String,
        #[serde(default)]
        namespace: String,
        /// Optional client-facing registry URL for CLI push operations
        /// If not specified, defaults to registry_url
        #[serde(default)]
        client_registry_url: Option<String>,
    },
    /// GitLab container registry — mints scoped JWTs per deployment
    #[serde(rename = "gitlab")]
    GitLab {
        /// GitLab instance URL (e.g., "https://gitlab.com")
        gitlab_url: String,
        /// Registry URL (e.g., "registry.gitlab.com")
        registry_url: String,
        /// Full image path prefix within the registry
        /// (e.g., "my-org/my-project" or "my-org/my-project/rise-apps")
        namespace: String,
        /// GitLab username for JWT auth endpoint
        username: String,
        /// Personal Access Token or Deploy Token
        token: String,
        /// When true, the Kubernetes controller creates and manages image pull secrets
        /// in each project namespace. Set to false if the cluster has its own pull mechanism.
        #[serde(default)]
        mint_pull_secrets: bool,
        /// Optional client-facing registry URL override
        #[serde(default)]
        client_registry_url: Option<String>,
    },
    /// JFrog Artifactory Docker registry — mints scoped access tokens per operation
    #[serde(rename = "jfrog")]
    Jfrog {
        /// Docker registry host:port (e.g., "localhost:3082")
        registry_host: String,
        /// Optional client-facing registry host override for CLI push
        #[serde(default)]
        client_registry_url: Option<String>,
        /// Docker repository key in JFrog (e.g., "rise-docker-local")
        docker_repo_key: String,
        /// Token provider configuration
        token_provider: JfrogTokenProviderSettings,
        /// Artifact permission letters for push tokens (default: "r,w")
        #[serde(default = "default_jfrog_push_permissions")]
        push_permissions: String,
        /// Artifact permission letters for pull tokens (default: "r")
        #[serde(default = "default_jfrog_pull_permissions")]
        pull_permissions: String,
        /// TTL in seconds for push tokens (default: 600)
        #[serde(default = "default_jfrog_push_token_ttl")]
        push_token_ttl: u64,
        /// TTL in seconds for pull tokens used by K8s image pull secrets (default: 86400)
        /// For Vault mode, the Vault role's max_ttl must be >= this value.
        #[serde(default = "default_jfrog_pull_token_ttl")]
        pull_token_ttl: u64,
        /// Whether K8s controller creates image pull secrets (default: true)
        #[serde(default = "default_true")]
        mint_pull_secrets: bool,
    },
}

/// Token provider configuration for JFrog registry
#[derive(Debug, Deserialize, Clone, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum JfrogTokenProviderSettings {
    /// HashiCorp Vault with vault-plugin-secrets-artifactory
    Vault {
        /// Vault server address (fallback: VAULT_ADDR env var)
        #[serde(default)]
        vault_addr: Option<String>,
        /// Vault token (fallback: VAULT_TOKEN env var)
        #[serde(default)]
        vault_token: Option<String>,
        /// Path to a file containing the Vault token (fallback: VAULT_TOKEN_FILE env var).
        /// Re-read on each request to support token rotation.
        #[serde(default)]
        vault_token_file: Option<String>,
        /// Vault secrets engine mount path (default: "artifactory")
        #[serde(default = "default_vault_mount_path")]
        vault_mount_path: String,
        /// Vault role name
        vault_role: String,
        /// When true (default), Rise sends per-operation scopes to Vault, overriding
        /// the role's default scope. With the Rise fork of vault-plugin-secrets-artifactory,
        /// configure admin `allow_scope_override="opt-in"` and role
        /// `allow_scope_override=true` with narrow `allowed_scopes`.
        /// When false, Rise omits the scope parameter and the role's configured scope is used.
        #[serde(default = "default_true")]
        scope_override: bool,
    },
    /// JFrog's own access token API with an admin token
    Direct {
        /// JFrog platform URL (e.g., "http://rise-jfrog:8082")
        jfrog_url: String,
        /// Admin access token for minting scoped tokens
        admin_token: String,
    },
}

fn default_jfrog_push_permissions() -> String {
    "r,w".to_string()
}

fn default_jfrog_pull_permissions() -> String {
    "r".to_string()
}

fn default_jfrog_push_token_ttl() -> u64 {
    600
}

fn default_jfrog_pull_token_ttl() -> u64 {
    86400 // 24 hours
}

fn default_vault_mount_path() -> String {
    "artifactory".to_string()
}

/// Encryption provider configuration
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum EncryptionSettings {
    /// Local AES-256-GCM encryption using a symmetric key
    #[serde(rename = "aes-gcm-256")]
    Local {
        /// Base64-encoded 32-byte encryption key
        /// Generate with: openssl rand -base64 32
        key: String,
    },
    /// AWS KMS encryption
    #[serde(rename = "aws-kms")]
    AwsKms {
        #[allow(dead_code)]
        region: String,
        /// KMS key ID or ARN
        key_id: String,
        /// Optional static credentials (development only)
        #[serde(default)]
        #[allow(dead_code)]
        access_key_id: Option<String>,
        /// Optional static credentials (development only)
        #[serde(default)]
        #[allow(dead_code)]
        secret_access_key: Option<String>,
    },
}

impl Settings {
    pub fn json_schema_value() -> serde_json::Value {
        let schema = schemars::schema_for!(Settings);
        schema.to_value()
    }

    fn substitute_env_vars_in_string_with(
        s: &str,
        env_lookup: &impl Fn(&str) -> Option<String>,
    ) -> String {
        let re = regex::Regex::new(r"\$\{([^}:]+)(?::-([^}]*))?\}").unwrap();

        re.replace_all(s, |caps: &regex::Captures| {
            let var_name = &caps[1];
            let default_value = caps.get(2).map(|m| m.as_str());

            env_lookup(var_name).unwrap_or_else(|| default_value.unwrap_or("").to_string())
        })
        .to_string()
    }

    fn config_value_to_json_with(
        value: &config::Value,
        env_lookup: &impl Fn(&str) -> Option<String>,
    ) -> serde_json::Value {
        use config::ValueKind;

        match &value.kind {
            ValueKind::Nil => serde_json::Value::Null,
            ValueKind::Boolean(b) => serde_json::Value::Bool(*b),
            ValueKind::I64(i) => serde_json::Value::Number((*i).into()),
            ValueKind::I128(i) => serde_json::Value::Number((*i as i64).into()),
            ValueKind::U64(u) => serde_json::Value::Number((*u).into()),
            ValueKind::U128(u) => serde_json::Value::Number((*u as u64).into()),
            ValueKind::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            ValueKind::String(s) => {
                // Perform environment variable substitution
                serde_json::Value::String(Self::substitute_env_vars_in_string_with(s, env_lookup))
            }
            ValueKind::Table(table) => {
                let mut map = serde_json::Map::new();
                for (k, v) in table.iter() {
                    map.insert(k.clone(), Self::config_value_to_json_with(v, env_lookup));
                }
                serde_json::Value::Object(map)
            }
            ValueKind::Array(arr) => {
                let vec: Vec<serde_json::Value> = arr
                    .iter()
                    .map(|value| Self::config_value_to_json_with(value, env_lookup))
                    .collect();
                serde_json::Value::Array(vec)
            }
        }
    }

    /// Try to add a `.yaml` config file.
    /// Returns Ok(true) if a file was loaded, Ok(false) if no file found (when not required)
    fn try_add_config_file(
        builder: &mut config::ConfigBuilder<config::builder::DefaultState>,
        config_dir: &str,
        name: &str,
        required: bool,
    ) -> Result<bool, ConfigError> {
        let path = format!("{}/{}.yaml", config_dir, name);
        if std::path::Path::new(&path).exists() {
            tracing::info!("Loading config file: {}", path);
            // Pass the explicit path and format so the loader can't pick up a
            // sibling file with a different extension.
            *builder = builder
                .clone()
                .add_source(config::File::with_name(&path).format(config::FileFormat::Yaml));
            return Ok(true);
        }

        if required {
            Err(ConfigError::Message(format!(
                "Required config file not found: {}",
                path
            )))
        } else {
            tracing::debug!("Optional config file not found: {}", path);
            Ok(false)
        }
    }

    fn validate_extra_service_token_audiences(
        audiences: &std::collections::HashMap<String, String>,
    ) -> Result<(), ConfigError> {
        let token_name_re = regex::Regex::new(r"^[A-Za-z0-9._-]+$").map_err(|e| {
            ConfigError::Message(format!("Failed to compile token name regex: {}", e))
        })?;

        for (name, audience) in audiences {
            if name.is_empty() {
                return Err(ConfigError::Message(
                    "extra_service_token_audiences contains an empty token name".to_string(),
                ));
            }

            if name == "." || name == ".." || !token_name_re.is_match(name) {
                return Err(ConfigError::Message(format!(
                    "extra_service_token_audiences token name '{}' is invalid; use only letters, numbers, '.', '_' or '-'",
                    name
                )));
            }

            if audience.trim().is_empty() {
                return Err(ConfigError::Message(format!(
                    "extra_service_token_audiences token '{}' has an empty audience",
                    name
                )));
            }
        }

        Ok(())
    }

    pub fn new() -> Result<Self, ConfigError> {
        let run_mode = env::var("RISE_CONFIG_RUN_MODE").unwrap_or_else(|_| "development".into());
        let config_dir = env::var("RISE_CONFIG_DIR").unwrap_or_else(|_| "config".into());

        Self::new_with_env(&config_dir, &run_mode, &|name| env::var(name).ok())
    }

    fn new_with_env(
        config_dir: &str,
        run_mode: &str,
        env_lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let mut builder = Config::builder();

        // Load `.yaml` config files in order. Later files override earlier ones.

        // 1. Load shipped defaults (optional). Carries the built-in quickstart
        //    catalog and other defaults that operators can override. Loaded
        //    first so a run-mode or local override replaces these values.
        Self::try_add_config_file(&mut builder, config_dir, "default", false)?;

        // 2. Load environment-specific config (required)
        Self::try_add_config_file(&mut builder, config_dir, run_mode, true)?;

        // 3. Load local config (optional, not checked into git)
        Self::try_add_config_file(&mut builder, config_dir, "local", false)?;

        // Build config and substitute environment variables
        let config = builder.build()?;

        // Get the root value and convert to JSON with env var substitution
        let root_value = config
            .cache
            .into_table()
            .map_err(|e| ConfigError::Message(format!("Failed to get config table: {}", e)))?;

        // Convert config values to serde_json::Value (with env var substitution in strings)
        let mut json_map = serde_json::Map::new();
        for (k, v) in root_value.iter() {
            json_map.insert(k.clone(), Self::config_value_to_json_with(v, env_lookup));
        }
        let json_value = serde_json::Value::Object(json_map);

        // Deserialize from JSON value and collect unused fields
        let mut unused_fields = Vec::new();
        let mut settings: Settings = serde_ignored::deserialize(json_value, |path| {
            unused_fields.push(path.to_string());
        })
        .map_err(|e| ConfigError::Message(format!("Failed to deserialize settings: {}", e)))?;

        // Warn about unused fields
        for field in &unused_fields {
            tracing::warn!("Unknown configuration field in backend config: {}", field);
        }

        // Fall back to DATABASE_URL environment variable if database.url is not set in config
        if settings.database.url.is_empty() {
            if let Some(database_url) = env_lookup("DATABASE_URL") {
                if !database_url.is_empty() {
                    settings.database.url = database_url;
                }
            }
        }

        // Validate that database URL is set
        if settings.database.url.is_empty() {
            return Err(ConfigError::Message(
                "Database URL not configured. Set DATABASE_URL environment variable or [database] url in config".to_string()
            ));
        }

        // Validate that JWT signing secret is set and valid
        if settings.server.jwt_signing_secret.is_empty() {
            return Err(ConfigError::Message(
                "JWT signing secret not configured. Set RISE_SERVER__JWT_SIGNING_SECRET environment variable or [server] jwt_signing_secret in config. Generate with: openssl rand -base64 32".to_string()
            ));
        }

        // Validate deployment controller settings if configured
        if let Some(DeploymentControllerSettings::Kubernetes {
            ref production_ingress_url_template,
            ref staging_ingress_url_template,
            ref environment_ingress_url_template,
            ref access_classes,
            ref extra_service_token_audiences,
            ref controller_class_name,
            ..
        }) = settings.deployment_controller
        {
            Self::validate_format_string(
                production_ingress_url_template,
                "production_ingress_url_template",
                "{project_name}",
            )?;

            Self::validate_label_value(
                controller_class_name,
                "deployment_controller.controller_class_name",
            )?;

            if let Some(ref staging_template) = staging_ingress_url_template {
                Self::validate_format_string(
                    staging_template,
                    "staging_ingress_url_template",
                    "{project_name}",
                )?;
                Self::validate_format_string(
                    staging_template,
                    "staging_ingress_url_template",
                    "{deployment_group}",
                )?;
            }

            if let Some(ref environment_template) = environment_ingress_url_template {
                Self::validate_format_string(
                    environment_template,
                    "environment_ingress_url_template",
                    "{project_name}",
                )?;
                Self::validate_format_string(
                    environment_template,
                    "environment_ingress_url_template",
                    "{environment}",
                )?;
            }

            Self::validate_extra_service_token_audiences(extra_service_token_audiences)?;

            // Filter out null access classes (used to remove inherited entries)
            // and validate the remaining ones
            let active_classes: Vec<_> = access_classes
                .iter()
                .filter_map(|(id, class)| class.as_ref().map(|c| (id, c)))
                .collect();

            if active_classes.is_empty() {
                return Err(ConfigError::Message(
                    "Kubernetes deployment_controller requires at least one access class to be configured. \
                     Add access_classes to your configuration file.".to_string()
                ));
            }

            for (id, class) in active_classes {
                if class.display_name.is_empty() {
                    return Err(ConfigError::Message(format!(
                        "Access class '{}' has empty display_name",
                        id
                    )));
                }
                if class.description.is_empty() {
                    return Err(ConfigError::Message(format!(
                        "Access class '{}' has empty description",
                        id
                    )));
                }
                if class.ingress_class.is_empty() {
                    return Err(ConfigError::Message(format!(
                        "Access class '{}' has empty ingress_class",
                        id
                    )));
                }
            }
        }

        // Validate the Docker deployment controller variant with the same
        // contract as the Kubernetes variant: the ingress templates must carry
        // their placeholders (otherwise every project collides on one Traefik
        // Host(...) rule) and the controller class name must be a valid label.
        if let Some(DeploymentControllerSettings::Docker {
            ref production_ingress_url_template,
            ref staging_ingress_url_template,
            ref environment_ingress_url_template,
            ref controller_class_name,
            ..
        }) = settings.deployment_controller
        {
            Self::validate_format_string(
                production_ingress_url_template,
                "production_ingress_url_template",
                "{project_name}",
            )?;

            Self::validate_label_value(
                controller_class_name,
                "deployment_controller.controller_class_name",
            )?;

            if let Some(ref staging_template) = staging_ingress_url_template {
                Self::validate_format_string(
                    staging_template,
                    "staging_ingress_url_template",
                    "{project_name}",
                )?;
                Self::validate_format_string(
                    staging_template,
                    "staging_ingress_url_template",
                    "{deployment_group}",
                )?;
            }

            if let Some(ref environment_template) = environment_ingress_url_template {
                Self::validate_format_string(
                    environment_template,
                    "environment_ingress_url_template",
                    "{project_name}",
                )?;
                Self::validate_format_string(
                    environment_template,
                    "environment_ingress_url_template",
                    "{environment}",
                )?;
            }
        }

        if let Some(ref resource_gc) = settings.resource_gc {
            resource_gc.validate()?;
        }

        if let Some(ref quickstart) = settings.quickstart {
            quickstart.validate()?;
        }

        Ok(settings)
    }

    /// Validate that a format string contains the required placeholder
    fn validate_format_string(
        format_str: &str,
        field_name: &str,
        required_placeholder: &str,
    ) -> Result<(), ConfigError> {
        if !format_str.contains(required_placeholder) {
            return Err(ConfigError::Message(format!(
                "Kubernetes configuration error: '{}' must contain '{}' placeholder. Got: '{}'",
                field_name, required_placeholder, format_str
            )));
        }
        Ok(())
    }

    /// Validate that a string is a valid Kubernetes label value.
    ///
    /// Kubernetes label values must match the regex
    /// `^[A-Za-z0-9]([A-Za-z0-9_.-]{0,61}[A-Za-z0-9])?$`, i.e. they must be
    /// 1-63 characters long, start and end with an alphanumeric character, and
    /// only contain alphanumerics, '-', '_', and '.'. Notably, '/' is not
    /// permitted — stamping a value with '/' onto a label causes the API
    /// server to reject the object with `FieldValueInvalid`.
    fn validate_label_value(value: &str, field_name: &str) -> Result<(), ConfigError> {
        let label_value_re = regex::Regex::new(r"^[A-Za-z0-9]([A-Za-z0-9_.-]{0,61}[A-Za-z0-9])?$")
            .map_err(|e| {
                ConfigError::Message(format!("Failed to compile label value regex: {}", e))
            })?;

        if value.is_empty() {
            return Err(ConfigError::Message(format!(
                "'{}' must be a valid Kubernetes label value (length 1-63); got empty string. \
                 Note: '/' is not permitted in label values.",
                field_name
            )));
        }

        if value.len() > 63 {
            return Err(ConfigError::Message(format!(
                "'{}' must be a valid Kubernetes label value (length 1-63); got '{}' ({} chars). \
                 Note: '/' is not permitted in label values.",
                field_name,
                value,
                value.len()
            )));
        }

        if !label_value_re.is_match(value) {
            return Err(ConfigError::Message(format!(
                "'{}' must be a valid Kubernetes label value matching \
                 '^[A-Za-z0-9]([A-Za-z0-9_.-]{{0,61}}[A-Za-z0-9])?$'; got '{}'. \
                 Note: '/' is not permitted in label values.",
                field_name, value
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_u32_flexible_accepts_number_and_numeric_string() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(deserialize_with = "deserialize_u32_flexible")]
            n: u32,
        }
        // Plain number (e.g. config/default.yaml).
        let from_num: Holder = serde_yaml::from_str("n: 7").unwrap();
        assert_eq!(from_num.n, 7);
        // Numeric string (e.g. an interpolated `${RISE_MAX_REPLICAS:-10}`).
        let from_str: Holder = serde_yaml::from_str("n: \"10\"").unwrap();
        assert_eq!(from_str.n, 10);
        // A non-numeric string is rejected.
        assert!(serde_yaml::from_str::<Holder>("n: \"abc\"").is_err());
    }

    fn ok_template(id: &str) -> QuickstartTemplateConfig {
        QuickstartTemplateConfig {
            id: id.to_string(),
            display_name: "Disp".to_string(),
            tagline: "Tag".to_string(),
            description: "Desc".to_string(),
            icon: "welcome".to_string(),
            image: "x/y:1".to_string(),
            http_port: 8080,
            learn_more_url: "https://example.com".to_string(),
            tags: vec![],
            warning: None,
        }
    }

    #[test]
    fn quickstart_validate_accepts_valid_catalog() {
        let qs = QuickstartSettings {
            templates: vec![ok_template("welcome"), ok_template("whoami")],
        };
        qs.validate().expect("valid catalog should pass");
    }

    #[test]
    fn quickstart_validate_rejects_non_kebab_id() {
        let mut t = ok_template("Welcome");
        t.id = "Welcome".to_string();
        let qs = QuickstartSettings { templates: vec![t] };
        assert!(qs.validate().is_err());
    }

    #[test]
    fn quickstart_validate_rejects_duplicate_id() {
        let qs = QuickstartSettings {
            templates: vec![ok_template("welcome"), ok_template("welcome")],
        };
        assert!(qs.validate().is_err());
    }

    #[test]
    fn quickstart_validate_rejects_empty_required_field() {
        let mut t = ok_template("welcome");
        t.image = "  ".to_string();
        let qs = QuickstartSettings { templates: vec![t] };
        assert!(qs.validate().is_err());
    }

    #[test]
    fn quickstart_validate_rejects_zero_port() {
        let mut t = ok_template("welcome");
        t.http_port = 0;
        let qs = QuickstartSettings { templates: vec![t] };
        assert!(qs.validate().is_err());
    }

    #[test]
    fn quickstart_validate_rejects_unparseable_learn_more_url() {
        let mut t = ok_template("welcome");
        t.learn_more_url = "not a url".to_string();
        let qs = QuickstartSettings { templates: vec![t] };
        assert!(qs.validate().is_err());
    }

    #[test]
    fn quickstart_validate_rejects_non_http_learn_more_url() {
        let mut t = ok_template("welcome");
        t.learn_more_url = "javascript:alert(1)".to_string();
        let qs = QuickstartSettings { templates: vec![t] };
        assert!(qs.validate().is_err());
    }

    #[test]
    fn quickstart_validate_accepts_http_and_https_learn_more_urls() {
        for url in ["http://example.com", "https://example.com/path"] {
            let mut t = ok_template("welcome");
            t.learn_more_url = url.to_string();
            let qs = QuickstartSettings { templates: vec![t] };
            qs.validate()
                .unwrap_or_else(|e| panic!("{url} should pass: {e:?}"));
        }
    }

    #[test]
    fn test_substitute_env_vars_in_string_basic() {
        let result = Settings::substitute_env_vars_in_string_with("${TEST_VAR}", &|name| {
            (name == "TEST_VAR").then(|| "test_value".to_string())
        });
        assert_eq!(result, "test_value");
    }

    #[test]
    fn test_substitute_env_vars_in_string_with_default() {
        let result =
            Settings::substitute_env_vars_in_string_with("${MISSING_VAR:-default_value}", &|_| {
                None
            });
        assert_eq!(result, "default_value");
    }

    #[test]
    fn test_substitute_env_vars_in_string_override_default() {
        let result = Settings::substitute_env_vars_in_string_with(
            "${OVERRIDE_VAR:-default_value}",
            &|name| (name == "OVERRIDE_VAR").then(|| "actual_value".to_string()),
        );
        assert_eq!(result, "actual_value");
    }

    #[test]
    fn test_substitute_env_vars_in_string_multiple() {
        let result =
            Settings::substitute_env_vars_in_string_with(
                "${VAR1} and ${VAR2}",
                &|name| match name {
                    "VAR1" => Some("value1".to_string()),
                    "VAR2" => Some("value2".to_string()),
                    _ => None,
                },
            );
        assert_eq!(result, "value1 and value2");
    }

    #[test]
    fn test_substitute_env_vars_in_string_no_substitution() {
        let result = Settings::substitute_env_vars_in_string_with("plain_value", &|_| None);
        assert_eq!(result, "plain_value");
    }

    #[test]
    fn test_unused_fields_warning() {
        // This test verifies that unused fields are detected during deserialization
        // We can't easily test the warning output itself, but we can verify the config
        // still loads successfully even with unknown fields

        use std::fs;
        use tempfile::TempDir;

        // Create a temporary directory with a development.yaml config
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("development.yaml");

        fs::write(
            &config_path,
            r#"
server:
  host: "0.0.0.0"
  port: 3000
  public_url: "http://localhost:3000"
  jwt_signing_secret: "test-secret-key-for-testing-123456"
  unknown_field: "should trigger warning"

database:
  url: "postgres://test@localhost/test"

auth:
  issuer: "http://localhost:5556"
  client_id: "test"
  client_secret: "test"

deployment_controller:
  type: "kubernetes"
  ingress_class: "nginx"
  production_ingress_url_template: "{project_name}.test.local"
  auth_backend_url: "http://localhost:3000"
  auth_signin_url: "http://localhost:3000"
  metacontroller_pod_namespace: "metacontroller"
  network_policy:
    ingress:
      - from:
          - podSelector:
              matchLabels: {}
    egress: null
  access_classes:
    public:
      display_name: "Public"
      description: "Test public access"
      ingress_class: "nginx"
      access_requirement: None

unknown_top_level: "also unknown"
"#,
        )
        .unwrap();

        // This should load successfully despite unknown fields
        // (The warnings would appear in logs)
        let result =
            Settings::new_with_env(temp_dir.path().to_str().unwrap(), "development", &|_| None);

        // Config should load successfully (warnings are logged, not errors)
        assert!(
            result.is_ok(),
            "Config should load despite unknown fields: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_validate_extra_service_token_audiences_accepts_empty_map() {
        let audiences = std::collections::HashMap::new();
        assert!(Settings::validate_extra_service_token_audiences(&audiences).is_ok());
    }

    #[test]
    fn test_validate_extra_service_token_audiences_rejects_invalid_name() {
        let mut audiences = std::collections::HashMap::new();
        audiences.insert(
            "vault/token".to_string(),
            "https://vault.example.com".to_string(),
        );

        let error = Settings::validate_extra_service_token_audiences(&audiences)
            .expect_err("invalid token name should fail validation");

        assert!(error
            .to_string()
            .contains("extra_service_token_audiences token name 'vault/token' is invalid"));
    }

    #[test]
    fn controller_class_name_default_is_valid() {
        // The literal default value used when no override is configured must pass
        // the label-value validation. Keep this in sync with
        // `default_controller_class_name`.
        let default_value = default_controller_class_name();
        Settings::validate_label_value(
            &default_value,
            "deployment_controller.controller_class_name",
        )
        .expect("default controller_class_name must be a valid Kubernetes label value");
    }

    #[test]
    fn controller_class_name_with_slash_rejects() {
        let error = Settings::validate_label_value(
            "my/class",
            "deployment_controller.controller_class_name",
        )
        .expect_err("controller_class_name with '/' must be rejected");

        let message = error.to_string();
        assert!(
            message.contains("deployment_controller.controller_class_name"),
            "error message should name the field, got: {}",
            message
        );
        assert!(
            message.contains("my/class"),
            "error message should include the invalid value, got: {}",
            message
        );
        assert!(
            message.contains("'/'"),
            "error message should mention that '/' is not permitted, got: {}",
            message
        );
    }

    #[test]
    fn controller_class_name_empty_rejects() {
        let error =
            Settings::validate_label_value("", "deployment_controller.controller_class_name")
                .expect_err("empty controller_class_name must be rejected");

        let message = error.to_string();
        assert!(
            message.contains("deployment_controller.controller_class_name"),
            "error message should name the field, got: {}",
            message
        );
        assert!(
            message.contains("empty") || message.contains("1-63"),
            "error message should mention length/empty, got: {}",
            message
        );
    }

    #[test]
    fn test_settings_load_with_extra_service_token_audiences() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("development.yaml");

        fs::write(
            &config_path,
            r#"
server:
  host: "0.0.0.0"
  port: 3000
  public_url: "http://localhost:3000"
  jwt_signing_secret: "test-secret-key-for-testing-123456"

database:
  url: "postgres://test@localhost/test"

auth:
  issuer: "http://localhost:5556"
  client_id: "test"
  client_secret: "test"

deployment_controller:
  type: "kubernetes"
  production_ingress_url_template: "{project_name}.test.local"
  auth_backend_url: "http://localhost:3000"
  auth_signin_url: "http://localhost:3000"
  metacontroller_pod_namespace: "metacontroller"
  extra_service_token_audiences:
    vault: "https://vault.example.com"
  network_policy:
    ingress:
      - from:
          - podSelector:
              matchLabels: {}
    egress: null
  access_classes:
    public:
      display_name: "Public"
      description: "Test public access"
      ingress_class: "nginx"
      access_requirement: None
"#,
        )
        .unwrap();

        let result =
            Settings::new_with_env(temp_dir.path().to_str().unwrap(), "development", &|_| None);

        let settings = result.expect("config with extra service token audiences should load");
        let Some(DeploymentControllerSettings::Kubernetes {
            extra_service_token_audiences,
            ..
        }) = settings.deployment_controller
        else {
            panic!("expected kubernetes deployment_controller");
        };

        assert_eq!(
            extra_service_token_audiences.get("vault"),
            Some(&"https://vault.example.com".to_string())
        );
    }

    #[test]
    fn test_database_url_config_takes_precedence_over_env() {
        // Verify that database.url in config is not overridden by DATABASE_URL env var
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("development.yaml");

        fs::write(
            &config_path,
            r#"
server:
  host: "0.0.0.0"
  port: 3000
  public_url: "http://localhost:3000"
  jwt_signing_secret: "test-secret-key-for-testing-123456"

database:
  url: "postgres://config-user@config-host/config-db"

auth:
  issuer: "http://localhost:5556"
  client_id: "test"
  client_secret: "test"
"#,
        )
        .unwrap();

        let env_lookup = |name: &str| {
            (name == "DATABASE_URL").then(|| "postgres://env-user@env-host/env-db".to_string())
        };

        let settings = Settings::new_with_env(
            temp_dir.path().to_str().unwrap(),
            "development",
            &env_lookup,
        )
        .expect("config should load");

        // The explicit config value must win over DATABASE_URL
        assert_eq!(
            settings.database.url,
            "postgres://config-user@config-host/config-db"
        );
    }

    #[test]
    fn test_database_url_env_used_as_fallback_when_config_unset() {
        // Verify that DATABASE_URL env var is used when database.url is not set in config
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("development.yaml");

        fs::write(
            &config_path,
            r#"
server:
  host: "0.0.0.0"
  port: 3000
  public_url: "http://localhost:3000"
  jwt_signing_secret: "test-secret-key-for-testing-123456"

auth:
  issuer: "http://localhost:5556"
  client_id: "test"
  client_secret: "test"
"#,
        )
        .unwrap();

        let env_lookup = |name: &str| {
            (name == "DATABASE_URL").then(|| "postgres://env-user@env-host/env-db".to_string())
        };

        let settings = Settings::new_with_env(
            temp_dir.path().to_str().unwrap(),
            "development",
            &env_lookup,
        )
        .expect("config should load using DATABASE_URL fallback");

        assert_eq!(settings.database.url, "postgres://env-user@env-host/env-db");
    }

    #[test]
    fn test_run_mode_config_is_required_even_if_local_exists() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let local_path = temp_dir.path().join("local.yaml");

        fs::write(
            &local_path,
            r#"
server:
  host: "0.0.0.0"
  port: 3000
  public_url: "http://localhost:3000"
  jwt_signing_secret: "test-secret-key-for-testing-123456"

database:
  url: "postgres://test@localhost/test"

auth:
  issuer: "http://localhost:5556"
  client_id: "test"
  client_secret: "test"
"#,
        )
        .unwrap();

        let result =
            Settings::new_with_env(temp_dir.path().to_str().unwrap(), "production", &|_| None);

        assert!(
            result.is_err(),
            "Config should fail without required run_mode file"
        );
    }

    #[test]
    fn test_resource_gc_settings_defaults() {
        let s = ResourceGcSettings::default();
        assert_eq!(s.interval_secs, 10);
        assert_eq!(s.batch_size, 50);
        assert_eq!(s.max_batches_per_tick, 4);
        assert_eq!(s.stuck_threshold_secs, 3600);
    }

    #[test]
    fn test_resource_gc_settings_partial_yaml_fills_defaults() {
        // Only override two fields; the rest must fall back to defaults.
        let s: ResourceGcSettings =
            serde_yaml::from_str("interval_secs: 5\nbatch_size: 100\n").unwrap();
        assert_eq!(s.interval_secs, 5);
        assert_eq!(s.batch_size, 100);
        assert_eq!(s.max_batches_per_tick, 4);
        assert_eq!(s.stuck_threshold_secs, 3600);
    }

    #[test]
    fn test_resource_gc_settings_rejects_invalid_values() {
        use std::fs;
        use tempfile::TempDir;

        let cases: &[(&str, &str)] = &[
            ("interval_secs", "interval_secs: 0"),
            ("batch_size", "batch_size: 0"),
            ("batch_size_negative", "batch_size: -5"),
            ("max_batches_per_tick", "max_batches_per_tick: 0"),
        ];

        for (case, override_yaml) in cases {
            let temp_dir = TempDir::new().unwrap();
            let config_path = temp_dir.path().join("development.yaml");
            let yaml = format!(
                r#"
server:
  host: "0.0.0.0"
  port: 3000
  public_url: "http://localhost:3000"
  jwt_signing_secret: "test-secret-key-for-testing-123456"

database:
  url: "postgres://test@localhost/test"

auth:
  issuer: "http://localhost:5556"
  client_id: "test"
  client_secret: "test"

resource_gc:
  {}
"#,
                override_yaml
            );
            fs::write(&config_path, &yaml).unwrap();

            let result =
                Settings::new_with_env(temp_dir.path().to_str().unwrap(), "development", &|_| None);
            assert!(
                result.is_err(),
                "resource_gc case '{}' should fail validation",
                case
            );
        }
    }

    #[test]
    fn test_resource_gc_section_is_optional() {
        // Loading a config that omits `resource_gc` must succeed and leave
        // the field as `None`; the runtime then falls back to defaults.
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("development.yaml");

        fs::write(
            &config_path,
            r#"
server:
  host: "0.0.0.0"
  port: 3000
  public_url: "http://localhost:3000"
  jwt_signing_secret: "test-secret-key-for-testing-123456"

database:
  url: "postgres://test@localhost/test"

auth:
  issuer: "http://localhost:5556"
  client_id: "test"
  client_secret: "test"
"#,
        )
        .unwrap();

        let settings =
            Settings::new_with_env(temp_dir.path().to_str().unwrap(), "development", &|_| None)
                .expect("config should load without resource_gc section");
        assert!(settings.resource_gc.is_none());
    }

    fn access_class(req: AccessRequirement) -> AccessClass {
        AccessClass {
            display_name: "Disp".to_string(),
            description: "Desc".to_string(),
            ingress_class: "traefik".to_string(),
            access_requirement: req,
            custom_annotations: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn docker_controller_with_member_class_and_empty_auth_backend_url_is_rejected() {
        let mut classes = std::collections::HashMap::new();
        classes.insert(
            "public".to_string(),
            Some(access_class(AccessRequirement::None)),
        );
        classes.insert(
            "private".to_string(),
            Some(access_class(AccessRequirement::Member)),
        );

        let offending = docker_access_classes_missing_auth_backend_url(&classes, "");
        assert_eq!(offending, vec!["private".to_string()]);

        // Whitespace-only is treated as blank.
        let offending_ws = docker_access_classes_missing_auth_backend_url(&classes, "   ");
        assert_eq!(offending_ws, vec!["private".to_string()]);
    }

    #[test]
    fn docker_controller_with_authenticated_class_and_empty_auth_backend_url_is_rejected() {
        let mut classes = std::collections::HashMap::new();
        classes.insert(
            "authed".to_string(),
            Some(access_class(AccessRequirement::Authenticated)),
        );

        let offending = docker_access_classes_missing_auth_backend_url(&classes, "");
        assert_eq!(offending, vec!["authed".to_string()]);
    }

    #[test]
    fn docker_controller_with_only_public_class_is_accepted() {
        let mut classes = std::collections::HashMap::new();
        classes.insert(
            "public".to_string(),
            Some(access_class(AccessRequirement::None)),
        );

        assert!(docker_access_classes_missing_auth_backend_url(&classes, "").is_empty());
    }

    #[test]
    fn docker_controller_with_member_class_and_auth_backend_url_is_accepted() {
        let mut classes = std::collections::HashMap::new();
        classes.insert(
            "public".to_string(),
            Some(access_class(AccessRequirement::None)),
        );
        classes.insert(
            "private".to_string(),
            Some(access_class(AccessRequirement::Member)),
        );

        // Mirrors config/docker.yaml (private + auth URL set).
        assert!(
            docker_access_classes_missing_auth_backend_url(&classes, "http://rise:3000").is_empty()
        );
    }

    /// Stage the real shipped `config/docker.yaml` plus a minimal `default.yaml`
    /// in a temp dir and load it through the settings overlay chain with
    /// `run_mode=docker`, returning the parsed `Settings`. `env` supplies the
    /// env vars the config interpolates (`${VAR}` / `${VAR:-default}`).
    fn load_shipped_docker_config(
        env: &std::collections::HashMap<&'static str, &'static str>,
    ) -> Result<Settings, ConfigError> {
        use std::fs;
        use tempfile::TempDir;

        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let docker_yaml = fs::read_to_string(format!("{manifest_dir}/config/docker.yaml"))
            .expect("config/docker.yaml must exist");

        let temp_dir = TempDir::new().unwrap();
        // Minimal default.yaml so the overlay's first (optional) layer exists;
        // the docker.yaml layer carries everything the assertions check.
        fs::write(temp_dir.path().join("default.yaml"), "{}\n").unwrap();
        fs::write(temp_dir.path().join("docker.yaml"), docker_yaml).unwrap();

        let owned: std::collections::HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let env_lookup = move |name: &str| owned.get(name).cloned();

        Settings::new_with_env(temp_dir.path().to_str().unwrap(), "docker", &env_lookup)
    }

    #[test]
    fn shipped_docker_config_loads_with_local_defaults() {
        // LOCAL: only DATABASE_URL set — everything else falls back to the
        // local-friendly defaults baked into config/docker.yaml.
        let mut env = std::collections::HashMap::new();
        env.insert("DATABASE_URL", "postgres://u@rise-postgres/rise");
        // The local overlay sets this so app containers get rise.localhost ->
        // backend via extra_hosts.
        env.insert("RISE_APP_BACKEND_HOST_ALIAS", "rise.localhost");
        // `mise br docker` sets this so the alias is stamped verbatim as
        // rise.localhost:host-gateway (host-run backend, portable across
        // Docker Desktop and Linux) instead of being DNS-resolved.
        env.insert("RISE_APP_BACKEND_IP", "host-gateway");
        // The local overlay opens the SSRF guard to reach the internal
        // http://rise-dex:5556 issuer. Set as string "true" (env-interpolated) to
        // exercise the string->bool flexible deserialization.
        env.insert("RISE_SSRF_ALLOW_PRIVATE", "true");
        env.insert("RISE_SSRF_ALLOW_HTTP", "true");
        let settings = load_shipped_docker_config(&env)
            .expect("shipped docker.yaml must load with only DATABASE_URL set");

        assert_eq!(settings.server.public_url, "http://rise.localhost:3000");
        assert!(!settings.server.cookie_secure, "local: cookie_secure false");
        // LOCAL: SSRF guard opened via the overlay's string "true" env values.
        assert!(
            settings.server.ssrf.allow_private_networks,
            "local: ssrf.allow_private_networks must be true via overlay env"
        );
        assert!(
            settings.server.ssrf.allow_http,
            "local: ssrf.allow_http must be true via overlay env"
        );
        // trusted_hosts allowlists the internal services regardless of mode.
        assert!(
            settings
                .server
                .ssrf
                .trusted_hosts
                .iter()
                .any(|h| h == "rise-dex"),
            "ssrf.trusted_hosts must include rise-dex"
        );

        let Some(DeploymentControllerSettings::Docker {
            ingress_schema,
            traefik_certresolver,
            traefik_entrypoint,
            production_ingress_url_template,
            ingress_port,
            app_backend_host_aliases,
            app_backend_ip,
            deployment_constraints,
            traefik_api_url,
            ..
        }) = &settings.deployment_controller
        else {
            panic!("expected docker deployment_controller");
        };
        // traefik_api_url: env-driven `${RISE_TRAEFIK_API_URL:-http://rise-traefik:8080}`
        // defaults to the in-network standalone Traefik API (the serverStatus
        // rolling gate works out of the box). Host-run dev overrides it to the
        // published dashboard port.
        assert_eq!(
            traefik_api_url.as_deref(),
            Some("http://rise-traefik:8080"),
            "local: traefik_api_url must default to the in-network Traefik API"
        );
        // Replicas: env-driven max (RISE_MAX_REPLICAS) defaults to 10 so the
        // Docker backend allows horizontal scale-out out of the box (the
        // platform default of 1 would reject replicas>1 at the API).
        assert_eq!(
            deployment_constraints.max_replicas, 10,
            "docker default max_replicas must be 10 (env-driven, unset)"
        );
        // LOCAL: alias populated so the controller injects rise.localhost ->
        // backend into app containers.
        assert_eq!(
            app_backend_host_aliases,
            &vec!["rise.localhost".to_string()],
            "local: app_backend_host_aliases must contain the issuer host"
        );
        // LOCAL: explicit host-gateway override (used verbatim, skips DNS).
        assert_eq!(
            app_backend_ip.as_deref(),
            Some("host-gateway"),
            "local: app_backend_ip must be the host-gateway override"
        );
        assert_eq!(ingress_schema, "http");
        assert_eq!(traefik_entrypoint, "web");
        // ingress_port is omitted from docker.yaml -> defaults to None.
        assert_eq!(*ingress_port, None);
        // Empty `${RISE_CERTRESOLVER:-}` -> after normalization this means no TLS.
        // The raw parsed value is Some("") here; normalize_certresolver collapses
        // it to None (asserted via the labels module test). Assert it's blank.
        assert!(
            traefik_certresolver
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty(),
            "local certresolver must be blank, got {traefik_certresolver:?}"
        );
        assert_eq!(
            crate::server::deployment::controller::docker::labels::normalize_certresolver(
                traefik_certresolver.clone()
            ),
            None
        );
        // Rise's own `{project_name}` placeholder is preserved literally.
        assert_eq!(
            production_ingress_url_template,
            "{project_name}.rise.localhost"
        );
    }

    #[test]
    fn shipped_docker_config_loads_with_prod_env() {
        // PROD: env vars flip the same file to https / le / the real domain.
        let mut env = std::collections::HashMap::new();
        env.insert("DATABASE_URL", "postgres://u@rise-postgres/rise");
        env.insert("PUBLIC_URL", "https://rise.example.com");
        env.insert("RISE_INGRESS_DOMAIN", "example.com");
        env.insert("RISE_INGRESS_SCHEME", "https");
        env.insert("RISE_CERTRESOLVER", "le");
        env.insert("RISE_TRAEFIK_ENTRYPOINT", "websecure");
        env.insert("RISE_COOKIE_SECURE", "true");
        let settings = load_shipped_docker_config(&env)
            .expect("shipped docker.yaml must load with prod env set");

        assert_eq!(settings.server.public_url, "https://rise.example.com");
        assert!(settings.server.cookie_secure, "prod: cookie_secure true");
        // PROD: SSRF guard is fail-closed by default (the local overlay sets
        // RISE_SSRF_ALLOW_PRIVATE / RISE_SSRF_ALLOW_HTTP=true, neither set here).
        assert!(
            !settings.server.ssrf.allow_private_networks,
            "prod: ssrf.allow_private_networks must default closed"
        );
        assert!(
            !settings.server.ssrf.allow_http,
            "prod: ssrf.allow_http must default closed"
        );

        let Some(DeploymentControllerSettings::Docker {
            ingress_schema,
            traefik_certresolver,
            traefik_entrypoint,
            production_ingress_url_template,
            app_backend_host_aliases,
            app_backend_ip,
            traefik_api_url,
            ..
        }) = &settings.deployment_controller
        else {
            panic!("expected docker deployment_controller");
        };
        // PROD: traefik_api_url unset -> defaults to the in-network standalone
        // Traefik API (rise-traefik:8080), reached over rise_default. The
        // standalone Traefik enables its API internally (--api.insecure=true)
        // without publishing :8080, so the serverStatus gate works with no extra
        // env on the backend.
        assert_eq!(
            traefik_api_url.as_deref(),
            Some("http://rise-traefik:8080"),
            "prod: traefik_api_url must default to the in-network Traefik API"
        );
        // PROD: RISE_APP_BACKEND_HOST_ALIAS unset -> the single blank entry is
        // filtered out -> empty list -> NO extra_hosts injection (public DNS +
        // Traefik handle the issuer host with correct TLS).
        assert!(
            app_backend_host_aliases.is_empty(),
            "prod: app_backend_host_aliases must be empty, got {app_backend_host_aliases:?}"
        );
        // PROD: RISE_APP_BACKEND_IP unset -> blank `${...:-}` -> None, so the
        // controller falls back to DNS resolution (containerized backend), and
        // never injects a host-gateway override.
        assert!(
            app_backend_ip.is_none(),
            "prod: app_backend_ip must be None, got {app_backend_ip:?}"
        );
        assert_eq!(ingress_schema, "https");
        assert_eq!(traefik_entrypoint, "websecure");
        assert_eq!(
            crate::server::deployment::controller::docker::labels::normalize_certresolver(
                traefik_certresolver.clone()
            ),
            Some("le".to_string())
        );
        // Env-interpolated host, with `{project_name}` still literal.
        assert_eq!(
            production_ingress_url_template,
            "{project_name}.example.com"
        );
    }

    #[test]
    fn docker_e2e_local_overlay_overrides_only_identity_ttl() {
        // The e2e mounts config/docker-e2e.local.yaml as /etc/rise/local.yaml so
        // the backend loads it as the optional `local` layer over docker.yaml.
        // Assert the real override file deep-merges: it lowers
        // identity_token_ttl_seconds while leaving sibling deployment_controller
        // fields (and the docker variant tag) intact. Guards against a
        // config-merge regression that would wipe the rest of the section.
        use std::fs;
        use tempfile::TempDir;

        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let docker_yaml = fs::read_to_string(format!("{manifest_dir}/config/docker.yaml")).unwrap();
        let local_yaml =
            fs::read_to_string(format!("{manifest_dir}/config/docker-e2e.local.yaml")).unwrap();

        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("default.yaml"), "{}\n").unwrap();
        fs::write(temp_dir.path().join("docker.yaml"), docker_yaml).unwrap();
        fs::write(temp_dir.path().join("local.yaml"), local_yaml).unwrap();

        let env: std::collections::HashMap<String, String> =
            [("DATABASE_URL", "postgres://u@rise-postgres/rise")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let settings =
            Settings::new_with_env(temp_dir.path().to_str().unwrap(), "docker", &|name| {
                env.get(name).cloned()
            })
            .expect("docker.yaml + e2e local overlay must load");

        let Some(DeploymentControllerSettings::Docker {
            identity_token_ttl_seconds,
            production_ingress_url_template,
            ..
        }) = &settings.deployment_controller
        else {
            panic!("expected docker deployment_controller after local overlay merge");
        };
        // Overridden by the local overlay.
        assert_eq!(*identity_token_ttl_seconds, 60);
        // Sibling key preserved from docker.yaml (deep merge, not replace).
        assert_eq!(
            production_ingress_url_template,
            "{project_name}.rise.localhost"
        );
    }

    #[test]
    fn docker_controller_ignores_null_access_classes() {
        // A `null` entry (used to remove an inherited class) must not trip the
        // check even when auth_backend_url is empty.
        let mut classes = std::collections::HashMap::new();
        classes.insert(
            "public".to_string(),
            Some(access_class(AccessRequirement::None)),
        );
        classes.insert("private".to_string(), None);

        assert!(docker_access_classes_missing_auth_backend_url(&classes, "").is_empty());
    }
}

/// Extensions configuration
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ExtensionsSettings {
    #[serde(default)]
    pub providers: Vec<ExtensionProviderConfig>,
}

/// Snowflake authentication configuration
#[cfg(feature = "backend")]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "auth_type", rename_all = "snake_case")]
pub enum SnowflakeAuth {
    Password {
        password: String,
    },
    PrivateKey {
        #[serde(flatten)]
        key_source: PrivateKeySource,
        #[serde(default)]
        private_key_password: Option<String>,
    },
}

/// Private key source (path or inline PEM)
#[cfg(feature = "backend")]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PrivateKeySource {
    Path { private_key_path: String },
    Inline { private_key: String },
}

/// Extension provider configuration
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ExtensionProviderConfig {
    #[cfg(feature = "backend")]
    AwsRdsProvisioner {
        region: String,
        instance_size: String,
        disk_size: i32, // in GiB
        /// Template for RDS instance identifiers
        /// Available placeholders: {prefix}, {project_name}, {extension_name}
        /// Default: "{prefix}-{project_name}-{extension_name}"
        #[serde(default = "default_instance_id_template")]
        instance_id_template: String,
        /// Prefix for RDS instance identifiers
        /// Must match the IAM policy prefix configured in your Terraform infrastructure
        /// Default: "rise"
        #[serde(default = "default_instance_id_prefix")]
        instance_id_prefix: String,
        /// Default engine version to use if not specified in project extension spec
        /// Use AWS CLI to find versions: aws rds describe-db-engine-versions --engine postgres --query "DBEngineVersions[*].EngineVersion"
        #[serde(default = "default_engine_version")]
        default_engine_version: String,
        /// VPC security group IDs for the RDS instance
        #[serde(default)]
        vpc_security_group_ids: Option<Vec<String>>,
        /// DB subnet group name for VPC placement
        #[serde(default)]
        db_subnet_group_name: Option<String>,
        /// Backup retention period in days (1-35, default: 7)
        #[serde(default = "default_backup_retention_days")]
        backup_retention_days: i32,
        /// Preferred backup window in UTC (e.g., "03:00-04:00")
        #[serde(default)]
        backup_window: Option<String>,
        /// Preferred maintenance window (e.g., "sun:04:00-sun:05:00")
        #[serde(default)]
        maintenance_window: Option<String>,
        #[serde(default)]
        access_key_id: Option<String>,
        #[serde(default)]
        secret_access_key: Option<String>,
    },

    #[cfg(feature = "backend")]
    #[serde(rename = "aws-s3-bucket")]
    AwsS3BucketProvisioner {
        region: String,
        /// Prefix for S3 bucket and IAM user names. Must match the Terraform IAM policy prefix.
        /// Default: "rise"
        #[serde(default = "default_s3_bucket_prefix")]
        bucket_prefix: String,
        /// Template for bucket names.
        /// Placeholders: {prefix}, {project_name}, {extension_name}
        /// Default: "{prefix}-{project_name}-{extension_name}"
        #[serde(default = "default_s3_bucket_name_template")]
        bucket_name_template: String,
        /// ARN of the IAM permissions boundary policy (output `s3_user_permissions_boundary_arn`
        /// from the `rise-aws` Terraform module). All IAM users created by this extension will
        /// have this boundary attached, preventing privilege escalation.
        user_permissions_boundary_arn: String,
        #[serde(default)]
        access_key_id: Option<String>,
        #[serde(default)]
        secret_access_key: Option<String>,
    },

    #[cfg(feature = "backend")]
    #[serde(rename = "snowflake-oauth-provisioner")]
    SnowflakeOAuthProvisioner {
        /// Snowflake account identifier (e.g., "myorg.us-east-1")
        account: String,
        /// Snowflake user with CREATE INTEGRATION privilege
        user: String,
        /// Snowflake role to use (must have CREATE INTEGRATION ON ACCOUNT privilege)
        /// Typically ACCOUNTADMIN or a custom role with appropriate grants
        #[serde(default)]
        role: Option<String>,
        /// Snowflake warehouse to use for queries
        /// Required for executing SQL statements
        #[serde(default)]
        warehouse: Option<String>,
        /// Authentication configuration (password, private key, or JWT)
        #[serde(flatten)]
        auth: SnowflakeAuth,
        /// Prefix for SECURITY INTEGRATION names (default: "rise")
        #[serde(default = "default_integration_name_prefix")]
        integration_name_prefix: String,
        /// Default blocked roles for OAuth (default: ["ACCOUNTADMIN", "ORGADMIN", "SECURITYADMIN"])
        #[serde(default = "default_blocked_roles")]
        default_blocked_roles: Vec<String>,
        /// Default OAuth scopes (default: ["refresh_token"])
        #[serde(default = "default_scopes")]
        default_scopes: Vec<String>,
        /// Refresh token validity in seconds (default: 7776000 = 90 days)
        #[serde(default = "default_refresh_token_validity_seconds")]
        refresh_token_validity_seconds: i64,
        /// How often (in seconds) to re-verify that the SECURITY INTEGRATION still
        /// exists in Snowflake once the extension is `Available`. The provisioner
        /// runs an inner 5s tick for transitional states, but `Available`
        /// extensions are only re-verified after this interval elapses. Default:
        /// 3600 (1 hour). The metadata query used for the check (`SHOW
        /// INTEGRATIONS`) does not activate the configured warehouse, so a
        /// healthy steady state lets Snowflake auto-suspend the warehouse.
        #[serde(default = "default_snowflake_verify_interval_seconds")]
        verify_interval_seconds: u64,
    },
}

#[allow(dead_code)]
fn default_instance_id_template() -> String {
    "{prefix}-{project_name}-{extension_name}".to_string()
}

#[allow(dead_code)]
fn default_instance_id_prefix() -> String {
    "rise".to_string()
}

#[allow(dead_code)]
fn default_engine_version() -> String {
    "18.2".to_string()
}

#[allow(dead_code)]
fn default_backup_retention_days() -> i32 {
    7 // 7 days of backup retention (reasonable default for production)
}

#[allow(dead_code)]
fn default_s3_bucket_prefix() -> String {
    "rise".to_string()
}

#[allow(dead_code)]
fn default_s3_bucket_name_template() -> String {
    "{prefix}-{project_name}-{extension_name}".to_string()
}

#[allow(dead_code)]
fn default_integration_name_prefix() -> String {
    "rise".to_string()
}

#[allow(dead_code)]
fn default_blocked_roles() -> Vec<String> {
    vec![
        "ACCOUNTADMIN".to_string(),
        "ORGADMIN".to_string(),
        "SECURITYADMIN".to_string(),
    ]
}

#[allow(dead_code)]
fn default_scopes() -> Vec<String> {
    vec!["refresh_token".to_string()]
}

#[allow(dead_code)]
fn default_refresh_token_validity_seconds() -> i64 {
    7776000 // 90 days
}

#[allow(dead_code)]
fn default_snowflake_verify_interval_seconds() -> u64 {
    3600 // 1 hour
}

/// Platform access control configuration
#[derive(Debug, Deserialize, Clone, JsonSchema)]
pub struct PlatformAccessConfig {
    /// Policy: "allow_all" (default) or "restrictive"
    #[serde(default = "default_platform_access_policy")]
    pub policy: PlatformAccessPolicy,

    /// User emails explicitly granted platform access
    #[serde(default, deserialize_with = "deserialize_blank_filtered_list")]
    pub allowed_user_emails: Vec<String>,

    /// IdP groups whose members get platform access
    #[serde(default, deserialize_with = "deserialize_blank_filtered_list")]
    pub allowed_idp_groups: Vec<String>,
}

/// Platform access policy enum
#[derive(Debug, Deserialize, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlatformAccessPolicy {
    AllowAll,    // Default: all authenticated users can use platform
    Restrictive, // Only allowlist matches can use platform
}

impl Default for PlatformAccessConfig {
    fn default() -> Self {
        Self {
            policy: PlatformAccessPolicy::AllowAll,
            allowed_user_emails: vec![],
            allowed_idp_groups: vec![],
        }
    }
}

fn default_platform_access_policy() -> PlatformAccessPolicy {
    PlatformAccessPolicy::AllowAll
}

impl QuickstartSettings {
    /// Reject malformed catalog entries at load time so a typo in a config
    /// file fails the process instead of degrading the deploy modal.
    fn validate(&self) -> Result<(), ConfigError> {
        let id_re = regex::Regex::new(r"^[a-z0-9]+(-[a-z0-9]+)*$").map_err(|e| {
            ConfigError::Message(format!("Failed to compile quickstart id regex: {}", e))
        })?;

        let mut seen_ids = std::collections::HashSet::new();
        for tpl in &self.templates {
            if !id_re.is_match(&tpl.id) {
                return Err(ConfigError::Message(format!(
                    "quickstart template id '{}' must be lowercase kebab-case (letters, digits, hyphens)",
                    tpl.id
                )));
            }
            if !seen_ids.insert(&tpl.id) {
                return Err(ConfigError::Message(format!(
                    "quickstart template id '{}' is defined more than once",
                    tpl.id
                )));
            }
            for (field, value) in [
                ("display_name", &tpl.display_name),
                ("tagline", &tpl.tagline),
                ("description", &tpl.description),
                ("icon", &tpl.icon),
                ("image", &tpl.image),
                ("learn_more_url", &tpl.learn_more_url),
            ] {
                if value.trim().is_empty() {
                    return Err(ConfigError::Message(format!(
                        "quickstart template '{}' has empty {}",
                        tpl.id, field
                    )));
                }
            }
            if tpl.http_port == 0 {
                return Err(ConfigError::Message(format!(
                    "quickstart template '{}' has invalid http_port 0",
                    tpl.id
                )));
            }
            match url::Url::parse(tpl.learn_more_url.trim()) {
                Ok(u) if matches!(u.scheme(), "http" | "https") => {}
                Ok(u) => {
                    return Err(ConfigError::Message(format!(
                        "quickstart template '{}' learn_more_url has unsupported scheme '{}'; only http and https are allowed",
                        tpl.id, u.scheme()
                    )));
                }
                Err(e) => {
                    return Err(ConfigError::Message(format!(
                        "quickstart template '{}' learn_more_url '{}' is not a valid URL: {}",
                        tpl.id, tpl.learn_more_url, e
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Background garbage-collection worker for the generic resource store.
///
/// Polls `list_pending_collection` and drives `try_collect` per row. Each tick
/// processes at most `batch_size × max_batches_per_tick` rows, capping the
/// effective throughput at `batch_size × max_batches_per_tick / interval_secs`
/// rows per second. Rows whose `deletion_timestamp` is older than
/// `stuck_threshold_secs` are flagged with a `warn!` log per sweep.
#[derive(Debug, Deserialize, Clone, JsonSchema)]
pub struct ResourceGcSettings {
    /// Sweep cadence in seconds. Must be >= 1 (`tokio::time::interval` panics
    /// on zero). Default: 10.
    #[serde(default = "default_resource_gc_interval_secs")]
    #[schemars(range(min = 1))]
    pub interval_secs: u64,
    /// Rows per `list_pending_collection` call. Must be >= 1. Default: 50.
    #[serde(default = "default_resource_gc_batch_size")]
    #[schemars(range(min = 1))]
    pub batch_size: i64,
    /// Maximum number of consecutive full batches processed in a single tick.
    /// Lets a deep backlog drain faster without monopolizing the worker. Must
    /// be >= 1. Default: 4.
    #[serde(default = "default_resource_gc_max_batches_per_tick")]
    #[schemars(range(min = 1))]
    pub max_batches_per_tick: u32,
    /// Seconds after which a tombstoned row is logged as `stuck` on each
    /// sweep. Logs-only; does not change collection behavior. Default: 3600.
    #[serde(default = "default_resource_gc_stuck_threshold_secs")]
    pub stuck_threshold_secs: u64,
}

impl ResourceGcSettings {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.interval_secs == 0 {
            return Err(ConfigError::Message(
                "resource_gc.interval_secs must be >= 1".to_string(),
            ));
        }
        if self.batch_size < 1 {
            return Err(ConfigError::Message(format!(
                "resource_gc.batch_size must be >= 1, got {}",
                self.batch_size
            )));
        }
        if self.max_batches_per_tick == 0 {
            return Err(ConfigError::Message(
                "resource_gc.max_batches_per_tick must be >= 1".to_string(),
            ));
        }
        Ok(())
    }
}

impl Default for ResourceGcSettings {
    fn default() -> Self {
        Self {
            interval_secs: default_resource_gc_interval_secs(),
            batch_size: default_resource_gc_batch_size(),
            max_batches_per_tick: default_resource_gc_max_batches_per_tick(),
            stuck_threshold_secs: default_resource_gc_stuck_threshold_secs(),
        }
    }
}

fn default_resource_gc_interval_secs() -> u64 {
    10
}

fn default_resource_gc_batch_size() -> i64 {
    50
}

fn default_resource_gc_max_batches_per_tick() -> u32 {
    4
}

fn default_resource_gc_stuck_threshold_secs() -> u64 {
    3600
}
