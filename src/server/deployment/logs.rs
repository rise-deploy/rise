use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::db::models::{Deployment, DeploymentStatus, Project};
use crate::server::deployment::resource_builder::ResourceBuilder;
use crate::server::settings::{DeploymentLogsSettings, KubernetesLogBackendSettings, LokiLabels};

mod cloudwatch;
use cloudwatch::CloudWatchLogBackend;

/// Loki 3.x's documented `detected_level` value set. Passed through verbatim
/// to clients; the frontend renders each via its own palette entry.
pub const LOKI_LEVELS: &[&str] = &[
    "unknown", "trace", "debug", "info", "warn", "error", "critical", "fatal",
];

/// The three levels the Kubernetes backend's internal regex classifier can
/// emit. Each line lands in exactly one of these.
pub const KUBERNETES_LEVELS: &[&str] = &["info", "warn", "error"];

/// Server-side cap on `?tail=` passed to Loki's `query_range`. Advertised via
/// `LogsCapabilities::max_tail` so the frontend can mirror the limit.
pub const LOKI_MAX_TAIL: i64 = 5000;

/// AWS context owned by the ECS deployment controller and shared with the
/// CloudWatch runtime-log reader. The writer and reader therefore use the same
/// credential chain, region, endpoint, log group and resource prefix.
#[derive(Clone)]
pub struct EcsCloudWatchContext {
    pub sdk_config: aws_config::SdkConfig,
    pub region: String,
    pub log_group: Option<String>,
    pub resource_prefix: String,
}

#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub follow: bool,
    pub tail_lines: Option<i64>,
    pub timestamps: bool,
    pub since_seconds: Option<i64>,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    /// Levels the caller wants to see. Empty means "all" (no filter).
    /// Loki passes these straight into a `| detected_level=~"a|b|..."` clause;
    /// the K8s backend filters post-classification.
    pub levels: Vec<String>,
    /// Optional case-insensitive substring users can type into the runtime
    /// logs search box. Empty/whitespace means "no filter".
    pub search: Option<String>,
    /// Skip this many of the most-recent qualifying lines before returning.
    /// Used by the Kubernetes backend to paginate older lines without an
    /// end-time filter — the frontend passes the number of lines it has
    /// already displayed. The Loki backend ignores this and uses its
    /// timestamp-windowed pagination instead.
    pub skip_recent: Option<i64>,
    /// Per-Organization namespace prefix resolved by the caller (see
    /// `resolve_project_namespace_prefix`). Used by the Kubernetes backend
    /// to compute the Pod namespace; the Loki backend ignores this and
    /// scopes its query by stream labels instead.
    pub namespace_prefix: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LogVolumeQuery {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub step_seconds: i64,
    /// Levels the caller wants counted. Empty means "all" (no filter).
    pub levels: Vec<String>,
    pub search: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStatusReason {
    NoLogsFound,
    RetentionExpiredPossible,
    HistoricalBackendNotConfigured,
    #[allow(dead_code)]
    BackendUnavailable,
    DeploymentNotReady,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogStatus {
    pub reason: LogStatusReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogVolumeResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<LogStatus>,
    pub start_time: String,
    pub end_time: String,
    pub step_seconds: i64,
    pub buckets: Vec<LogVolumeBucket>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogVolumeBucket {
    pub timestamp: String,
    pub total: u64,
    /// Sparse per-level counts. Keys are level strings emitted by the
    /// backend; zero counts are omitted. The sum across entries equals
    /// `total`.
    pub by_level: HashMap<String, u64>,
}

#[derive(Debug, Clone)]
pub enum LogEvent {
    /// A single log line plus the backend's classification of its level.
    ///
    /// `text` is the raw log line content (with an optional RFC3339 prefix
    /// when `timestamps=true`). `level` is the level string the configured
    /// backend emits — either Loki's `detected_level` (one of `LOKI_LEVELS`,
    /// defaulting to `"unknown"`) or the K8s regex classifier's output (one
    /// of `KUBERNETES_LEVELS`).
    Line {
        text: String,
        level: String,
    },
    Status(LogStatus),
    /// Sent once the initial backlog phase of a streaming request has been
    /// fully emitted, before the live-tail loop begins. `count` is the number
    /// of backlog lines yielded; the frontend uses it to decide whether older
    /// lines may still exist in the selected window.
    BacklogLoaded {
        count: usize,
    },
}

pub type LogEventStream = futures::stream::BoxStream<'static, Result<LogEvent>>;

/// Server-scoped capabilities of the configured log backend. Surfaced to the
/// frontend (and any other client) via `GET /api/v1/logs/capabilities` so the
/// filter UI and chart can be driven dynamically rather than hardcoded to
/// info/warn/error.
#[derive(Debug, Clone, Serialize)]
pub struct LogsCapabilities {
    pub backend: &'static str,
    pub levels: &'static [&'static str],
    pub supports_volume: bool,
    /// Server-side cap on `?tail=` for this backend, if any. Lets the frontend
    /// surface the ceiling instead of silently truncating the requested count.
    /// `None` means "no explicit advertised cap" (Kubernetes derives its cap
    /// from per-backend config rather than a global constant).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tail: Option<i64>,
}

#[async_trait]
pub trait RuntimeLogBackend: Send + Sync {
    /// Identifier for the backend variant (`"loki"` or `"kubernetes"`). Used
    /// only in the capabilities response — handlers branch on the trait, not
    /// this string.
    fn backend_kind(&self) -> &'static str;

    /// Full list of level strings the backend can emit. Drives both the
    /// `level` filter dropdown options and the chart's color palette on the
    /// frontend.
    fn levels(&self) -> &'static [&'static str];

    /// Whether the backend can return per-level volume buckets. The
    /// Kubernetes backend has no historical store, so this is `false` and
    /// the chart panel is hidden.
    fn supports_volume(&self) -> bool;

    /// Optional advertised cap on `tail` (lines) accepted by this backend.
    /// Returned via `/api/v1/logs/capabilities` so the frontend can constrain
    /// its UI rather than letting the server silently clamp the request.
    /// Default `None` = no advertised cap.
    fn max_tail(&self) -> Option<i64> {
        None
    }

    async fn stream_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogQuery,
    ) -> Result<LogEventStream>;

    async fn query_volume(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogVolumeQuery,
    ) -> Result<LogVolumeResponse>;
}

pub async fn init_runtime_log_backend(
    settings: &DeploymentLogsSettings,
    kube_client: Option<kube::Client>,
    docker_client: Option<bollard::Docker>,
    docker_label_namespace: Option<&str>,
    ecs_cloudwatch: Option<EcsCloudWatchContext>,
) -> Result<Arc<dyn RuntimeLogBackend>> {
    match settings {
        DeploymentLogsSettings::Kubernetes { config } => {
            let kube_client = kube_client.context("Kubernetes log backend requires kube client")?;
            Ok(Arc::new(KubernetesLogBackend {
                kube_client,
                config: config.clone(),
            }))
        }
        DeploymentLogsSettings::None { .. } => Ok(Arc::new(NoneLogBackend)),
        DeploymentLogsSettings::Docker { .. } => {
            let docker =
                docker_client.context("Docker log backend requires a connected Docker client")?;
            // The Docker log backend resolves containers by the same
            // bookkeeping labels the reconciler stamps, so it must use the
            // Docker controller's configured `label_namespace` rather than a
            // hardcoded literal. Falls back to the default when the Docker
            // controller isn't the active deployment backend.
            let label_namespace = docker_label_namespace
                .map(str::to_string)
                .unwrap_or_else(crate::server::settings::default_label_namespace);
            Ok(Arc::new(DockerLogBackend {
                docker,
                label_namespace,
            }))
        }
        DeploymentLogsSettings::Cloudwatch { retention_hint } => {
            let context = ecs_cloudwatch
                .context("CloudWatch log backend requires the ECS deployment controller")?;
            let log_group = context
                .log_group
                .context("CloudWatch log backend requires deployment_controller.log_group")?;
            Ok(Arc::new(
                CloudWatchLogBackend::new(
                    context.sdk_config,
                    context.region,
                    log_group,
                    context.resource_prefix,
                    retention_hint.clone(),
                )
                .await?,
            ))
        }
        DeploymentLogsSettings::Loki {
            url,
            tenant_id,
            bearer_token_env,
            timeout_secs,
            retention_hint,
            labels,
        } => {
            validate_loki_label_name("project", &labels.project)?;
            validate_loki_label_name("deployment_id", &labels.deployment_id)?;
            if let Some(tenant) = tenant_id.as_deref() {
                validate_header_value("tenant_id", tenant)?;
            }
            let bearer_token = bearer_token_env
                .as_ref()
                .map(|name| {
                    std::env::var(name)
                        .with_context(|| format!("Loki bearer_token_env '{}' is not set", name))
                })
                .transpose()?;
            if let Some(token) = bearer_token.as_deref() {
                validate_header_value("bearer_token", token)?;
            }
            Ok(Arc::new(LokiLogBackend::new(
                url.clone(),
                tenant_id.clone(),
                bearer_token,
                *timeout_secs,
                retention_hint.clone(),
                labels.clone(),
            )?))
        }
    }
}

struct KubernetesLogBackend {
    kube_client: kube::Client,
    config: KubernetesLogBackendSettings,
}

pub(crate) fn is_followable_status(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Deploying
            | DeploymentStatus::Healthy
            | DeploymentStatus::Unhealthy
            | DeploymentStatus::Cancelling
            | DeploymentStatus::Terminating
    )
}

#[async_trait]
impl RuntimeLogBackend for KubernetesLogBackend {
    fn backend_kind(&self) -> &'static str {
        "kubernetes"
    }

    fn levels(&self) -> &'static [&'static str] {
        KUBERNETES_LEVELS
    }

    fn supports_volume(&self) -> bool {
        false
    }

    async fn stream_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogQuery,
    ) -> Result<LogEventStream> {
        use k8s_openapi::api::core::v1::Pod;
        use kube::api::{Api, ListParams, LogParams};

        if matches!(
            deployment.status,
            DeploymentStatus::Pending
                | DeploymentStatus::Building
                | DeploymentStatus::Pushing
                | DeploymentStatus::Pushed
        ) {
            return Ok(status_stream(LogStatus {
                reason: LogStatusReason::DeploymentNotReady,
                message: Some(
                    "Deployment is not ready yet - no runtime logs are available.".into(),
                ),
                retention_hint: None,
            }));
        }

        let namespace_prefix = query
            .namespace_prefix
            .as_deref()
            .context("Kubernetes log backend requires the project's namespace_prefix")?;
        let namespace = ResourceBuilder::namespace_name(project, namespace_prefix);
        let pod_api: Api<Pod> = Api::namespaced(self.kube_client.clone(), &namespace);
        let pods = pod_api
            .list(&ListParams::default().labels(&format!(
                "rise.dev/deployment-id={}",
                deployment.deployment_id
            )))
            .await?;

        let Some(pod) = pods.items.first() else {
            return Ok(status_stream(LogStatus {
                reason: LogStatusReason::HistoricalBackendNotConfigured,
                message: Some(
                    "No active deployment pod was found and historical logs are not configured."
                        .into(),
                ),
                retention_hint: None,
            }));
        };

        let pod_name = pod
            .metadata
            .name
            .as_ref()
            .context("Pod name not found")?
            .clone();

        // `skip_recent` lets the frontend page backward without an end-time
        // filter: the kubelet returns lines in chronological order, so by
        // bumping `tail_lines` we widen the window backward and then drop
        // the trailing N qualifying lines (which the frontend already has).
        let skip_recent = query.skip_recent.unwrap_or(0).max(0);
        // Cap the requested tail at the configured ceiling. Once the frontend
        // hits this cap, paging stops yielding new lines — the same outcome
        // as when the kubelet's own ring buffer is exhausted.
        let max_tail = self.config.max_tail_lines.max(1);
        let effective_tail = query
            .tail_lines
            .map(|t| t.saturating_add(skip_recent).clamp(1, max_tail));

        let mut log_params = LogParams {
            follow: query.follow && is_followable_status(&deployment.status),
            timestamps: query.timestamps,
            ..Default::default()
        };
        if let Some(tail) = effective_tail {
            log_params.tail_lines = Some(tail);
        }
        if let Some(since) = query.since_seconds {
            log_params.since_seconds = Some(since);
        } else if let Some(start_time) = query.start_time {
            // The Kubernetes pods/log API only supports a since-anchored window.
            // `query.end_time` is unsupported here and is silently ignored — the
            // Loki backend is the supported path for explicit end-bounded ranges
            // (documented on the `end` query param in handlers.rs).
            let delta = (Utc::now() - start_time).num_seconds();
            if delta > 0 {
                log_params.since_seconds = Some(delta);
            } else {
                // Future-dated start_time: any kubelet response would predate
                // the requested window. Return an explicit empty result rather
                // than silently falling back to "all lines since container
                // start" (which would dump the entire log buffer).
                return Ok(status_stream(LogStatus {
                    reason: LogStatusReason::NoLogsFound,
                    message: Some(
                        "Requested start time is in the future; no log lines fall in the range."
                            .into(),
                    ),
                    retention_hint: None,
                }));
            }
        }

        let log_stream = pod_api.log_stream(&pod_name, &log_params).await?;
        let levels = query.levels.clone();
        let search = query.search.clone();
        let stream = async_stream::stream! {
            use futures::AsyncBufReadExt;
            use std::collections::VecDeque;
            let skip = skip_recent as usize;
            // Buffer the trailing `skip` qualifying lines so we can drop them
            // once the stream ends. While the buffer is full, evict the oldest
            // and yield it — that's a line the frontend doesn't already have.
            let mut trailing: VecDeque<(String, &'static str)> = VecDeque::with_capacity(skip.saturating_add(1));
            let mut lines = futures::io::BufReader::new(log_stream).lines();
            while let Some(line) = lines.next().await {
                let line = match line {
                    Ok(line) => line,
                    Err(e) => {
                        yield Err(anyhow::anyhow!("Log stream error: {}", e));
                        break;
                    }
                };
                if line.is_empty() { continue; }
                let level = classify_k8s_line(&line);
                if !levels.is_empty() && !levels.iter().any(|l| l == level) { continue; }
                if !line_matches_search(&line, search.as_deref()) { continue; }
                if skip == 0 {
                    yield Ok(LogEvent::Line { text: line, level: level.to_string() });
                } else {
                    trailing.push_back((line, level));
                    if trailing.len() > skip {
                        if let Some((out, level)) = trailing.pop_front() {
                            yield Ok(LogEvent::Line { text: out, level: level.to_string() });
                        }
                    }
                }
            }
            // Anything left in the buffer is in the trailing `skip` window and
            // intentionally dropped — those are the lines the frontend already has.
        };

        Ok(stream.boxed())
    }

    async fn query_volume(
        &self,
        _deployment: &Deployment,
        _project: &Project,
        query: LogVolumeQuery,
    ) -> Result<LogVolumeResponse> {
        Ok(LogVolumeResponse {
            status: Some(LogStatus {
                reason: LogStatusReason::HistoricalBackendNotConfigured,
                message: Some(
                    "Historical log volume isn't supported by the configured log backend.".into(),
                ),
                retention_hint: None,
            }),
            start_time: query.start_time.to_rfc3339(),
            end_time: query.end_time.to_rfc3339(),
            step_seconds: query.step_seconds,
            buckets: vec![],
        })
    }
}

/// A runtime log backend that serves no logs, only a clear reason.
///
/// Selected by `deployment_logs: { type: none }`. The explicit
/// `historical_backend_not_configured` status lets the logs UI render an empty
/// state for installs that intentionally disable runtime-log access.
struct NoneLogBackend;

#[async_trait]
impl RuntimeLogBackend for NoneLogBackend {
    fn backend_kind(&self) -> &'static str {
        "none"
    }

    fn levels(&self) -> &'static [&'static str] {
        &[]
    }

    fn supports_volume(&self) -> bool {
        false
    }

    async fn stream_logs(
        &self,
        _deployment: &Deployment,
        _project: &Project,
        _query: LogQuery,
    ) -> Result<LogEventStream> {
        Ok(status_stream(LogStatus {
            reason: LogStatusReason::HistoricalBackendNotConfigured,
            message: Some(
                "No runtime log backend is configured for this deployment backend. \
                 Configure `deployment_logs` (for example `type: loki`) to read \
                 application logs through Rise."
                    .into(),
            ),
            retention_hint: None,
        }))
    }

    async fn query_volume(
        &self,
        _deployment: &Deployment,
        _project: &Project,
        query: LogVolumeQuery,
    ) -> Result<LogVolumeResponse> {
        Ok(LogVolumeResponse {
            status: Some(LogStatus {
                reason: LogStatusReason::HistoricalBackendNotConfigured,
                message: Some("No runtime log backend is configured.".into()),
                retention_hint: None,
            }),
            start_time: query.start_time.to_rfc3339(),
            end_time: query.end_time.to_rfc3339(),
            step_seconds: query.step_seconds,
            buckets: vec![],
        })
    }
}

pub(super) fn status_stream(status: LogStatus) -> LogEventStream {
    futures::stream::once(async move { Ok(LogEvent::Status(status)) }).boxed()
}

/// Runtime log backend that streams directly from the Docker daemon.
///
/// Resolves the deployment's container(s) by the namespaced `deployment-id`
/// label Rise stamps (using the Docker controller's configured
/// `label_namespace`), then proxies `docker logs` into the shared
/// [`LogEventStream`]. Mirrors the Kubernetes backend's regex level
/// classification and lack of historical volume support.
struct DockerLogBackend {
    docker: bollard::Docker,
    /// Label namespace the Docker controller stamps containers with (e.g.
    /// `rise.dev`). Used to build the `<ns>/deployment-id` filter key.
    label_namespace: String,
}

impl DockerLogBackend {
    /// Find the most relevant container id for a deployment by its
    /// `<label_namespace>/deployment-id` label, scoped to the owning project.
    /// Prefers a running container.
    ///
    /// `deployment_id` is a `YYYYMMDD-HHMMSS` timestamp that is unique only
    /// *per project* (DB constraint `UNIQUE (deployment_id, project_id)`), so
    /// filtering on it alone could resolve to another project's container —
    /// a tenant-isolation breach. We therefore also scope by the `project`
    /// label (matching `project.name`, exactly as the reconciler stamps it)
    /// plus `managed-by=rise` for defense-in-depth, mirroring
    /// `list_actual_containers`.
    async fn resolve_container_id(
        &self,
        deployment: &Deployment,
        project: &Project,
    ) -> Result<Option<String>> {
        use crate::server::deployment::controller::docker::labels::{
            self, SUFFIX_DEPLOYMENT_ID, SUFFIX_MANAGED_BY, SUFFIX_PROJECT,
        };
        use bollard::container::ListContainersOptions;
        use std::collections::HashMap as StdHashMap;

        let ns = &self.label_namespace;
        let mut filters: StdHashMap<String, Vec<String>> = StdHashMap::new();
        filters.insert(
            "label".to_string(),
            vec![
                format!("{}={}", labels::ns_key(ns, SUFFIX_MANAGED_BY), "rise"),
                format!("{}={}", labels::ns_key(ns, SUFFIX_PROJECT), project.name),
                format!(
                    "{}={}",
                    labels::ns_key(ns, SUFFIX_DEPLOYMENT_ID),
                    deployment.deployment_id
                ),
            ],
        );
        let summaries = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            }))
            .await?;

        // Prefer a running container; fall back to any.
        let chosen = summaries
            .iter()
            .find(|c| c.state.as_deref() == Some("running"))
            .or_else(|| summaries.first())
            .and_then(|c| c.id.clone());
        Ok(chosen)
    }
}

#[async_trait]
impl RuntimeLogBackend for DockerLogBackend {
    fn backend_kind(&self) -> &'static str {
        "docker"
    }

    fn levels(&self) -> &'static [&'static str] {
        KUBERNETES_LEVELS
    }

    fn supports_volume(&self) -> bool {
        false
    }

    async fn stream_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogQuery,
    ) -> Result<LogEventStream> {
        use bollard::container::LogsOptions;

        if matches!(
            deployment.status,
            DeploymentStatus::Pending
                | DeploymentStatus::Building
                | DeploymentStatus::Pushing
                | DeploymentStatus::Pushed
        ) {
            return Ok(status_stream(LogStatus {
                reason: LogStatusReason::DeploymentNotReady,
                message: Some(
                    "Deployment is not ready yet - no runtime logs are available.".into(),
                ),
                retention_hint: None,
            }));
        }

        let Some(container_id) = self.resolve_container_id(deployment, project).await? else {
            return Ok(status_stream(LogStatus {
                reason: LogStatusReason::HistoricalBackendNotConfigured,
                message: Some(
                    "No active deployment container was found and historical logs are not \
                     configured."
                        .into(),
                ),
                retention_hint: None,
            }));
        };

        let tail = match query.tail_lines {
            Some(t) => t.max(1).to_string(),
            None => "all".to_string(),
        };
        // `since_seconds` is a *relative* "N seconds ago" value (matching the
        // K8s/Loki backends), but bollard's `LogsOptions.since` is an *absolute*
        // UNIX epoch timestamp. Convert relative → absolute. `start_time` is
        // already an absolute instant, so it passes through as-is.
        let since = query
            .since_seconds
            .map(|s| Utc::now().timestamp() - s)
            .or_else(|| query.start_time.map(|t| t.timestamp()).filter(|t| *t > 0))
            .unwrap_or(0);

        let options = LogsOptions::<String> {
            follow: query.follow && is_followable_status(&deployment.status),
            stdout: true,
            stderr: true,
            since,
            timestamps: query.timestamps,
            tail,
            ..Default::default()
        };

        let log_stream = self.docker.logs(&container_id, Some(options));
        let levels = query.levels.clone();
        let search = query.search.clone();
        let stream = async_stream::stream! {
            futures::pin_mut!(log_stream);
            while let Some(item) = log_stream.next().await {
                let output = match item {
                    Ok(output) => output,
                    Err(e) => {
                        yield Err(anyhow::anyhow!("Docker log stream error: {}", e));
                        break;
                    }
                };
                let raw = output.to_string();
                for line in raw.split('\n') {
                    let line = line.trim_end_matches('\r');
                    if line.is_empty() {
                        continue;
                    }
                    let level = classify_k8s_line(line);
                    if !levels.is_empty() && !levels.iter().any(|l| l == level) {
                        continue;
                    }
                    if !line_matches_search(line, search.as_deref()) {
                        continue;
                    }
                    yield Ok(LogEvent::Line {
                        text: line.to_string(),
                        level: level.to_string(),
                    });
                }
            }
        };

        Ok(stream.boxed())
    }

    async fn query_volume(
        &self,
        _deployment: &Deployment,
        _project: &Project,
        query: LogVolumeQuery,
    ) -> Result<LogVolumeResponse> {
        Ok(LogVolumeResponse {
            status: Some(LogStatus {
                reason: LogStatusReason::HistoricalBackendNotConfigured,
                message: Some(
                    "Historical log volume isn't supported by the configured log backend.".into(),
                ),
                retention_hint: None,
            }),
            start_time: query.start_time.to_rfc3339(),
            end_time: query.end_time.to_rfc3339(),
            step_seconds: query.step_seconds,
            buckets: vec![],
        })
    }
}

struct LokiLogBackend {
    query_url: String,
    tail_url: String,
    tenant_id: Option<String>,
    bearer_token: Option<String>,
    retention_hint: Option<String>,
    labels: LokiLabels,
    http_client: reqwest::Client,
}

impl LokiLogBackend {
    fn new(
        url: String,
        tenant_id: Option<String>,
        bearer_token: Option<String>,
        timeout_secs: u64,
        retention_hint: Option<String>,
        labels: LokiLabels,
    ) -> Result<Self> {
        let base_url = url.trim_end_matches('/').to_string();
        let query_url = format!("{}/loki/api/v1/query_range", base_url);
        let tail_url = format!("{}/loki/api/v1/tail", base_url);
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .context("Failed to build Loki HTTP client")?;

        Ok(Self {
            query_url,
            tail_url,
            tenant_id,
            bearer_token,
            retention_hint,
            labels,
            http_client,
        })
    }

    fn base_selector(&self, deployment: &Deployment, project: &Project) -> String {
        // {project, deployment_id} is enough to uniquely scope to a single
        // deployment's log stream — deployment_id is generated to be unique
        // within a project, and Rise enforces project-level authz upstream.
        format!(
            "{{{}=\"{}\",{}=\"{}\"}}",
            self.labels.project,
            escape_logql_label_value(&project.name),
            self.labels.deployment_id,
            escape_logql_label_value(&deployment.deployment_id),
        )
    }

    fn selector(
        &self,
        deployment: &Deployment,
        project: &Project,
        levels: &[String],
        search: Option<&str>,
    ) -> String {
        let base = self.base_selector(deployment, project);
        let with_level = append_detected_level_filter(&base, levels);
        append_search_filter(&with_level, search)
    }

    fn effective_start_time(&self, deployment: &Deployment, query: &LogQuery) -> DateTime<Utc> {
        if let Some(start_time) = query.start_time {
            start_time
        } else if let Some(since_seconds) = query.since_seconds {
            Utc::now() - Duration::seconds(since_seconds)
        } else {
            deployment.created_at
        }
    }

    fn effective_end_time(&self, query: &LogQuery) -> DateTime<Utc> {
        query.end_time.unwrap_or_else(Utc::now)
    }

    async fn query_range(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: &LogQuery,
    ) -> Result<Vec<LogLine>> {
        let selector = self.selector(deployment, project, &query.levels, query.search.as_deref());
        let end = self.effective_end_time(query);
        let start = self.effective_start_time(deployment, query);
        let tail = if query.follow {
            query.tail_lines.unwrap_or(1)
        } else {
            query.tail_lines.unwrap_or(1000)
        };
        let tail = tail.clamp(1, LOKI_MAX_TAIL);

        let url = format!(
            "{}?query={}&direction=BACKWARD&start={}&end={}&limit={}",
            self.query_url,
            urlencoding::encode(&selector),
            to_loki_nanos(start),
            to_loki_nanos(end),
            tail.max(1)
        );
        let mut request = self.http_client.get(url);
        request = self.apply_auth(request);

        let response = request.send().await.context("Failed to query Loki")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_else(|_| "".into());
            anyhow::bail!(
                "Loki query failed ({}): {}",
                status,
                truncate_for_error(body)
            );
        }

        let body: LokiQueryResponse = response.json().await.context("Invalid Loki response")?;
        let mut lines = body
            .data
            .result
            .into_iter()
            .flat_map(|stream| {
                let level = stream.stream.get("detected_level").cloned();
                stream.values.into_iter().map(move |v| (v, level.clone()))
            })
            .filter_map(|(value, level)| LogLine::from_loki_value(value, level.as_deref()))
            .collect::<Vec<_>>();
        lines.sort_by_key(|line| line.timestamp_nanos);
        Ok(lines)
    }

    fn apply_auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = if let Some(token) = &self.bearer_token {
            request.bearer_auth(token)
        } else {
            request
        };
        if let Some(tenant_id) = &self.tenant_id {
            request.header("X-Scope-OrgID", tenant_id)
        } else {
            request
        }
    }

    fn empty_status(&self, deployment: &Deployment) -> LogStatus {
        let retention_expired_possible = self
            .retention_hint
            .as_deref()
            .and_then(parse_duration_hint)
            .is_some_and(|retention| Utc::now() - deployment.created_at > retention);

        LogStatus {
            reason: if retention_expired_possible {
                LogStatusReason::RetentionExpiredPossible
            } else {
                LogStatusReason::NoLogsFound
            },
            message: None,
            retention_hint: self.retention_hint.clone(),
        }
    }

    async fn tail_stream(
        &self,
        deployment: Deployment,
        project: Project,
        query: LogQuery,
    ) -> Result<LogEventStream> {
        // Skip the historical query entirely when the caller only wants a
        // negligible backlog. Otherwise query_range would scan the full
        // [start, end] window just to drop all but one row at the limit.
        let initial = if matches!(query.tail_lines, Some(t) if t <= 1) {
            Vec::new()
        } else {
            self.query_range(&deployment, &project, &query).await?
        };
        let selector = self.selector(
            &deployment,
            &project,
            &query.levels,
            query.search.as_deref(),
        );
        let url = websocket_url(&self.tail_url, &selector);
        let tenant_id = self.tenant_id.clone();
        let bearer_token = self.bearer_token.clone();

        let stream = async_stream::try_stream! {
            let backlog_count = initial.len();
            for line in initial {
                let level = line.classified_level();
                yield LogEvent::Line { text: line.render(query.timestamps), level };
            }
            yield LogEvent::BacklogLoaded { count: backlog_count };

            let mut request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url.as_str())
                .context("Failed to build Loki tail websocket request")?;
            if let Some(token) = bearer_token {
                request.headers_mut().insert(
                    "Authorization",
                    tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {}", token))
                        .context("Invalid Loki bearer token header")?,
                );
            }
            if let Some(tenant) = tenant_id {
                request.headers_mut().insert(
                    "X-Scope-OrgID",
                    tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&tenant)
                        .context("Invalid Loki tenant header")?,
                );
            }

            let (ws_stream, _) = tokio_tungstenite::connect_async(request)
                .await
                .context("Failed to connect to Loki tail endpoint")?;
            let (mut write, mut read) = ws_stream.split();

            use futures::SinkExt;
            let idle = std::time::Duration::from_secs(30);
            let mut consecutive_timeouts: u32 = 0;
            loop {
                match tokio::time::timeout(idle, read.next()).await {
                    Ok(Some(message)) => {
                        consecutive_timeouts = 0;
                        let message = message.context("Loki tail websocket error")?;
                        if !message.is_text() {
                            continue;
                        }
                        let payload = message.into_text().context("Invalid Loki tail frame")?;
                        let response: LokiTailResponse = serde_json::from_str(&payload)
                            .context("Invalid Loki tail payload")?;
                        for stream in response.streams {
                            let stream_level = stream.stream.get("detected_level").cloned();
                            for value in stream.values {
                                if let Some(line) = LogLine::from_loki_value(value, stream_level.as_deref()) {
                                    let level = line.classified_level();
                                    yield LogEvent::Line {
                                        text: line.render(query.timestamps),
                                        level,
                                    };
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        consecutive_timeouts += 1;
                        if consecutive_timeouts >= 2 {
                            Err(anyhow::anyhow!(
                                "Loki tail websocket idle for {}s with no pong",
                                idle.as_secs() * 2
                            ))?;
                        }
                        write
                            .send(tokio_tungstenite::tungstenite::Message::Ping(Vec::new().into()))
                            .await
                            .context("Failed to send Loki tail websocket ping")?;
                    }
                }
            }
            // Symmetric shutdown: tell Loki we're done before dropping `write`
            // so the server-side handler can release its end of the connection
            // cleanly. Ignore the result — we're already exiting and Loki may
            // have closed first.
            let _ = write
                .send(tokio_tungstenite::tungstenite::Message::Close(None))
                .await;
        };

        Ok(stream.boxed())
    }

    /// Run a single `sum by (detected_level) (count_over_time(...))` query and
    /// return one (level label, points) entry per series Loki returns. The
    /// level label is the raw `detected_level` value (e.g. `"info"`, `"err"`,
    /// `"unknown"`); callers map it to the chart's Info/Warn/Error buckets.
    async fn query_counts_by_level(
        &self,
        query: String,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        step_seconds: i64,
    ) -> Result<Vec<(String, BTreeMap<i64, u64>)>> {
        let url = format!(
            "{}?query={}&start={}&end={}&step={}&direction=FORWARD",
            self.query_url,
            urlencoding::encode(&query),
            to_loki_nanos(start_time),
            to_loki_nanos(end_time),
            step_seconds.max(1)
        );
        let mut request = self.http_client.get(url);
        request = self.apply_auth(request);

        let response = request
            .send()
            .await
            .context("Failed to query Loki counts")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_else(|_| "".into());
            anyhow::bail!(
                "Loki counts query failed ({}): {}",
                status,
                truncate_for_error(body)
            );
        }

        let body: LokiMetricQueryResponse = response
            .json()
            .await
            .context("Invalid Loki counts response")?;

        let mut out = Vec::with_capacity(body.data.result.len());
        for series in body.data.result {
            let label = series
                .metric
                .get("detected_level")
                .cloned()
                .unwrap_or_default();
            let mut points: BTreeMap<i64, u64> = BTreeMap::new();
            for point in series.points() {
                points
                    .entry(point.timestamp_nanos)
                    .and_modify(|count| *count += point.count)
                    .or_insert(point.count);
            }
            out.push((label, points));
        }
        Ok(out)
    }

    fn build_count_buckets(
        series_by_level: &[(String, BTreeMap<i64, u64>)],
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        step_seconds: i64,
    ) -> Vec<LogVolumeBucket> {
        // Emit a contiguous row of buckets at right-edged timestamps spanning
        // (start_time, end_time] at step_seconds intervals — i.e. the first
        // bucket is at start_time + step and the last is at end_time. This
        // mirrors Loki's `count_over_time(...[Xs])` semantics, which at step
        // `t` returns the count for the preceding window `(t - X, t]`. The
        // frontend interprets each bucket the same way (see
        // frontend/src/features/log-volume-chart.tsx).
        //
        // For each bucket at right-edge `T` we sum counts whose timestamp falls
        // in `(T - step, T]`. Loki returns timestamps already aligned to the
        // step boundary so the exact-match lookup is the common case; we also
        // accept any data point within the bucket window as a defensive
        // fallback against slight drift.
        let step = Duration::seconds(step_seconds.max(1));
        let step_nanos = step.num_nanoseconds().unwrap_or(i64::MAX);

        let mut buckets = Vec::new();
        let mut current = start_time + step;
        while current <= end_time {
            let bucket_end = current.timestamp_nanos_opt().unwrap_or_default();
            let bucket_start_exclusive = bucket_end.saturating_sub(step_nanos);

            let mut total: u64 = 0;
            let mut by_level: HashMap<String, u64> = HashMap::new();
            for (level_label, points) in series_by_level {
                // Common case: Loki point lands exactly on the right edge.
                let exact = points.get(&bucket_end).copied().unwrap_or(0);
                // Fallback: any point within (bucket_start, bucket_end]. The
                // BTreeMap range is half-open at the low end so use +1 to make
                // the lower bound exclusive, then include `bucket_end`.
                let drift: u64 = points
                    .range(bucket_start_exclusive.saturating_add(1)..bucket_end)
                    .map(|(_, v)| *v)
                    .sum();
                let value = exact.saturating_add(drift);
                if value == 0 {
                    continue;
                }
                total = total.saturating_add(value);
                // Coerce empty/whitespace labels to "unknown" so the map's
                // keys are always non-empty. Loki returns the label verbatim
                // otherwise, including its own `"unknown"` for entries it
                // couldn't classify.
                let key = match level_label.trim() {
                    "" => "unknown".to_string(),
                    trimmed => trimmed.to_string(),
                };
                let slot = by_level.entry(key).or_insert(0);
                *slot = slot.saturating_add(value);
            }

            buckets.push(LogVolumeBucket {
                timestamp: current.to_rfc3339(),
                total,
                by_level,
            });
            current += step;
        }
        buckets
    }
}

#[async_trait]
impl RuntimeLogBackend for LokiLogBackend {
    fn backend_kind(&self) -> &'static str {
        "loki"
    }

    fn levels(&self) -> &'static [&'static str] {
        LOKI_LEVELS
    }

    fn supports_volume(&self) -> bool {
        true
    }

    fn max_tail(&self) -> Option<i64> {
        Some(LOKI_MAX_TAIL)
    }

    async fn stream_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogQuery,
    ) -> Result<LogEventStream> {
        if query.follow && is_followable_status(&deployment.status) {
            return self
                .tail_stream(deployment.clone(), project.clone(), query)
                .await;
        }

        let mut historical_query = query.clone();
        historical_query.follow = false;
        let lines = self
            .query_range(deployment, project, &historical_query)
            .await?;
        if lines.is_empty() {
            return Ok(status_stream(self.empty_status(deployment)));
        }

        let timestamps = query.timestamps;
        Ok(futures::stream::iter(lines.into_iter().map(move |line| {
            let level = line.classified_level();
            Ok(LogEvent::Line {
                text: line.render(timestamps),
                level,
            })
        }))
        .boxed())
    }

    async fn query_volume(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogVolumeQuery,
    ) -> Result<LogVolumeResponse> {
        if matches!(
            deployment.status,
            DeploymentStatus::Pending
                | DeploymentStatus::Building
                | DeploymentStatus::Pushing
                | DeploymentStatus::Pushed
        ) {
            return Ok(LogVolumeResponse {
                status: Some(LogStatus {
                    reason: LogStatusReason::DeploymentNotReady,
                    message: Some(
                        "Deployment is not ready yet - no runtime logs are available.".into(),
                    ),
                    retention_hint: None,
                }),
                start_time: query.start_time.to_rfc3339(),
                end_time: query.end_time.to_rfc3339(),
                step_seconds: query.step_seconds,
                buckets: vec![],
            });
        }

        // Align the bucket grid to the step (60s → :00, 300s → :00 :05 :10 …)
        // so the chart's X-axis labels are sensible and so the per-bucket
        // window query the frontend sends back matches Loki's count_over_time
        // window byte-for-byte. Round start down and end up to the grid.
        let step_seconds = query.step_seconds.max(1);
        let step_nanos = (step_seconds as i128) * 1_000_000_000;
        let aligned_nanos = |ts: DateTime<Utc>, round_up: bool| -> DateTime<Utc> {
            let ns = ts.timestamp_nanos_opt().unwrap_or_default() as i128;
            let mut q = ns / step_nanos;
            let r = ns % step_nanos;
            if round_up && r != 0 {
                q += 1;
            }
            DateTime::<Utc>::from_timestamp_nanos((q * step_nanos) as i64)
        };
        let aligned_start = aligned_nanos(query.start_time, false);
        let aligned_end = aligned_nanos(query.end_time, true);

        let base = self.base_selector(deployment, project);
        let range = format!("[{step_seconds}s]");
        // The chart reflects the same filters as the log list: same search
        // clause, same `detected_level` filter. When the caller passes
        // levels, the same alternation is used in both paths so the views
        // stay aligned by construction.
        let search = query.search.as_deref();
        let with_search = append_search_filter(&base, search);
        let selector = append_detected_level_filter(&with_search, &query.levels);
        // `sum by (detected_level)` collapses each detected_level value into
        // a single series. Per-bucket totals are the sum of all series; the
        // raw label is preserved in `by_level` so the frontend can render
        // each Loki-emitted value as its own segment.
        let count_query = format!("sum by (detected_level) (count_over_time(({selector}){range}))");

        let series = self
            .query_counts_by_level(count_query, aligned_start, aligned_end, step_seconds)
            .await?;

        let is_empty = series.iter().all(|(_, points)| points.is_empty());
        let buckets = Self::build_count_buckets(&series, aligned_start, aligned_end, step_seconds);

        Ok(LogVolumeResponse {
            status: if is_empty {
                Some(self.empty_status(deployment))
            } else {
                None
            },
            start_time: aligned_start.to_rfc3339(),
            end_time: aligned_end.to_rfc3339(),
            step_seconds,
            buckets,
        })
    }
}

#[derive(Debug)]
struct LogLine {
    timestamp_nanos: i128,
    line: String,
    /// `detected_level` extracted from the Loki entry's structured metadata
    /// (Loki 3.x query_range / tail responses emit a third array element
    /// alongside `[ts, line]` carrying structured metadata key/value pairs).
    /// `None` means the entry didn't include a `detected_level`; the line is
    /// then classified by regex (matching the K8s backend's behavior).
    detected_level: Option<String>,
}

impl LogLine {
    fn from_loki_value(value: LokiValue, stream_level: Option<&str>) -> Option<Self> {
        let LokiValue {
            timestamp,
            line,
            structured_metadata,
        } = value;
        let timestamp_nanos = timestamp.parse().ok()?;
        // Prefer per-entry structured metadata (Loki 3.x optional 3-tuple
        // shape), then fall back to the stream-level label (the form Loki
        // actually emits today). Either way, an empty string means "not
        // classified".
        let detected_level = structured_metadata
            .as_ref()
            .and_then(|m| m.get("detected_level"))
            .cloned()
            .or_else(|| stream_level.map(str::to_string))
            .filter(|s| !s.trim().is_empty());
        // Loki stores log entries with the trailing newline that the container
        // wrote to its log file. The Kubernetes backend doesn't see those
        // newlines because `AsyncBufRead::lines()` strips them — match that
        // behavior so renderers/SSE consumers don't end up with blank lines
        // between entries.
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        Some(Self {
            timestamp_nanos,
            line,
            detected_level,
        })
    }

    fn classified_level(&self) -> String {
        // Two cases:
        //   * Loki returned a concrete `detected_level` (`info`, `warn`,
        //     `unknown`, …) — pass through verbatim. This includes Loki's
        //     own `"unknown"`: when Loki actively says "I can't classify
        //     this", we trust it rather than guessing.
        //   * Loki returned no label at all (or empty/whitespace). Happens
        //     for in-flight entries on the WS tail because Loki's
        //     classifier runs during chunk processing, not against the
        //     ingester's in-memory write buffer. Fall back to the same
        //     regex the Kubernetes backend uses so the live tail is
        //     never un-classified for the user. Returns one of
        //     `KUBERNETES_LEVELS` — a subset of Loki's vocabulary, safe
        //     to render with the same palette.
        match self.detected_level.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => raw.to_string(),
            _ => classify_k8s_line(&self.line).to_string(),
        }
    }

    fn render(self, timestamps: bool) -> String {
        if !timestamps {
            return self.line;
        }
        let ts = DateTime::<Utc>::from_timestamp_nanos(self.timestamp_nanos as i64);
        format!("{} {}", ts.to_rfc3339(), self.line)
    }
}

/// A single entry from a Loki `query_range` or `tail` response.
///
/// Loki encodes entries as positional JSON arrays:
///   * `[timestamp_ns_string, line]` — historical 2-tuple form
///   * `[timestamp_ns_string, line, { "detected_level": "info", ... }]` —
///     Loki 3.x form when structured metadata (including the auto-detected
///     `detected_level`) is attached to the entry.
///
/// We accept either shape so older Loki deployments keep working.
#[derive(Debug)]
struct LokiValue {
    timestamp: String,
    line: String,
    structured_metadata: Option<std::collections::HashMap<String, String>>,
}

impl<'de> Deserialize<'de> for LokiValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};
        use std::fmt;

        struct LokiValueVisitor;
        impl<'de> Visitor<'de> for LokiValueVisitor {
            type Value = LokiValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a Loki log entry [timestamp, line] or [timestamp, line, metadata]")
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<LokiValue, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let timestamp: String = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let line: String = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let structured_metadata: Option<std::collections::HashMap<String, String>> =
                    seq.next_element()?;
                // Drain any trailing elements (forward compatibility).
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(LokiValue {
                    timestamp,
                    line,
                    structured_metadata,
                })
            }
        }

        deserializer.deserialize_seq(LokiValueVisitor)
    }
}

#[derive(Deserialize)]
struct LokiQueryResponse {
    data: LokiQueryData,
}

#[derive(Deserialize)]
struct LokiQueryData {
    result: Vec<LokiStream>,
}

#[derive(Deserialize)]
struct LokiTailResponse {
    streams: Vec<LokiStream>,
}

#[derive(Deserialize)]
struct LokiStream {
    /// Loki returns the stream's labels here. `detected_level` is the most
    /// important one for us — Loki 3.x attaches it as a stream-level label
    /// (not per-entry structured metadata), so reading it requires plucking
    /// from this map. Empty when the response has no labels.
    #[serde(default)]
    stream: std::collections::HashMap<String, String>,
    values: Vec<LokiValue>,
}

#[derive(Deserialize)]
struct LokiMetricQueryResponse {
    data: LokiMetricQueryData,
}

#[derive(Deserialize)]
struct LokiMetricQueryData {
    result: Vec<LokiMetricSeries>,
}

#[derive(Deserialize)]
struct LokiMetricSeries {
    #[serde(default)]
    metric: std::collections::HashMap<String, String>,
    #[serde(default)]
    values: Vec<MetricSample>,
    #[serde(default)]
    value: Option<MetricSample>,
}

#[derive(Clone)]
struct MetricSample(f64, String);

impl<'de> Deserialize<'de> for MetricSample {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};
        use std::fmt;

        struct CountField(String);
        impl<'de> Deserialize<'de> for CountField {
            fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct CountVisitor;
                impl<'de> Visitor<'de> for CountVisitor {
                    type Value = CountField;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("a string or numeric Loki sample count")
                    }
                    fn visit_str<E: de::Error>(
                        self,
                        v: &str,
                    ) -> std::result::Result<CountField, E> {
                        Ok(CountField(v.to_string()))
                    }
                    fn visit_string<E: de::Error>(
                        self,
                        v: String,
                    ) -> std::result::Result<CountField, E> {
                        Ok(CountField(v))
                    }
                    fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<CountField, E> {
                        Ok(CountField(v.to_string()))
                    }
                    fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<CountField, E> {
                        Ok(CountField(v.to_string()))
                    }
                    fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<CountField, E> {
                        Ok(CountField(v.to_string()))
                    }
                }
                deserializer.deserialize_any(CountVisitor)
            }
        }

        struct SampleVisitor;
        impl<'de> Visitor<'de> for SampleVisitor {
            type Value = MetricSample;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a Loki metric sample [timestamp, count]")
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<MetricSample, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let ts: f64 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let count: CountField = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                Ok(MetricSample(ts, count.0))
            }
        }

        deserializer.deserialize_seq(SampleVisitor)
    }
}

#[derive(Debug)]
struct CountPoint {
    timestamp_nanos: i64,
    count: u64,
}

impl LokiMetricSeries {
    fn points(&self) -> Vec<CountPoint> {
        let samples = if self.values.is_empty() {
            self.value.iter().cloned().collect::<Vec<_>>()
        } else {
            self.values.clone()
        };

        samples
            .into_iter()
            .filter_map(|MetricSample(ts_seconds, count_str)| {
                if !ts_seconds.is_finite() {
                    return None;
                }
                // Loki returns metric timestamps as JSON numbers (seconds). For step
                // sizes >= 1s — the only ones the chart uses — they are whole
                // seconds. Multiplying as f64 loses precision above ~2^53 ns
                // (early 2255), so round-then-multiply as i64 to land exactly on
                // bucket boundaries.
                let timestamp_nanos = (ts_seconds.round() as i64)
                    .checked_mul(1_000_000_000)
                    .unwrap_or(i64::MAX);
                let parsed = count_str.parse::<f64>().ok()?;
                if !parsed.is_finite() {
                    return None;
                }
                let count = parsed.round().max(0.0) as u64;
                Some(CountPoint {
                    timestamp_nanos,
                    count,
                })
            })
            .collect()
    }
}

fn escape_logql_label_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Cap an upstream response body before including it in an error message. Loki
/// can return very large HTML/JSON error pages and we don't want those flooding
/// the structured log line that surfaces the failure.
fn truncate_for_error(s: String) -> String {
    const MAX: usize = 1024;
    if s.len() <= MAX {
        return s;
    }
    // Truncate at a UTF-8 boundary at or before MAX so the resulting String is
    // still valid. `floor_char_boundary` is unstable, so do it by hand.
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut t = s;
    t.truncate(end);
    t.push_str("... (truncated)");
    t
}

/// Reject Loki/Prometheus label names that wouldn't be valid identifiers.
/// Prevents an operator-supplied override from producing malformed LogQL.
/// Validate a value that will be emitted as a (case-insensitive) HTTP header.
/// `reqwest::header::HeaderValue::from_str` rejects control bytes and non-ASCII
/// characters; doing the same check at backend init means an invalid Loki
/// `tenant_id` or bearer token surfaces as a startup error instead of a per-
/// request WS connect failure on every log stream.
fn validate_header_value(role: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("Loki '{}' header value must not be empty", role);
    }
    if !value.bytes().all(|b| b == b'\t' || (32..=126).contains(&b)) {
        anyhow::bail!(
            "Loki '{}' header value contains control or non-ASCII characters",
            role
        );
    }
    Ok(())
}

fn validate_loki_label_name(role: &str, name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {
            chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        }
        _ => false,
    };
    if !valid {
        anyhow::bail!(
            "Loki '{}' label name '{}' is invalid; must match [a-zA-Z_][a-zA-Z0-9_]*",
            role,
            name
        );
    }
    Ok(())
}

// LogQL regex patterns the Kubernetes backend uses for per-line
// classification. Loki classifies via its built-in `detected_level` metadata
// (passed through verbatim), so these are now K8s-only.
const LEVEL_REGEX_ERROR: &str = r"(?i)\b(error|err|fatal|panic|exception|failed)\b";
const LEVEL_REGEX_WARN: &str = r"(?i)\b(warn|warning)\b";

/// Classify a raw log line into one of the three `KUBERNETES_LEVELS`. The
/// Kubernetes backend has no upstream classifier (kubelet returns raw bytes),
/// so each line is scanned for error/warn keywords with an info catch-all.
pub(super) fn classify_k8s_line(line: &str) -> &'static str {
    use std::sync::OnceLock;
    static ERROR_RE: OnceLock<regex::Regex> = OnceLock::new();
    static WARN_RE: OnceLock<regex::Regex> = OnceLock::new();
    let err = ERROR_RE.get_or_init(|| regex::Regex::new(LEVEL_REGEX_ERROR).unwrap());
    let warn = WARN_RE.get_or_init(|| regex::Regex::new(LEVEL_REGEX_WARN).unwrap());
    if err.is_match(line) {
        "error"
    } else if warn.is_match(line) {
        "warn"
    } else {
        "info"
    }
}

/// Append a `| detected_level=~"a|b|..."` clause to a Loki selector when the
/// caller has provided one or more levels to filter on. Empty levels means
/// "no filter" — the original selector is returned unchanged. Each level is
/// regex-escaped so values are matched literally.
fn append_detected_level_filter(selector: &str, levels: &[String]) -> String {
    let cleaned: Vec<String> = levels
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(regex::escape)
        .collect();
    if cleaned.is_empty() {
        return selector.to_string();
    }
    let pattern = cleaned.join("|");
    format!("{selector} | detected_level=~\"{pattern}\"")
}

fn normalize_search(value: Option<&str>) -> Option<String> {
    value
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn append_search_filter(selector: &str, search: Option<&str>) -> String {
    let Some(text) = normalize_search(search) else {
        return selector.to_string();
    };
    // Loki's `|~` accepts a regex. We escape user input so it behaves as a
    // literal case-insensitive substring match. LogQL backtick-quoted literals
    // have no escape for the backtick character itself, so strip any from the
    // search before regex-escaping.
    let sanitized = text.replace('`', "");
    if sanitized.is_empty() {
        return selector.to_string();
    }
    let escaped = regex::escape(&sanitized);
    format!("{selector} |~ `(?i){escaped}`")
}

pub(crate) fn line_matches_search(line: &str, search: Option<&str>) -> bool {
    let Some(text) = normalize_search(search) else {
        return true;
    };
    let lower_line = line.to_lowercase();
    let lower_text = text.to_lowercase();
    lower_line.contains(&lower_text)
}

fn to_loki_nanos(ts: DateTime<Utc>) -> String {
    ts.timestamp_nanos_opt().unwrap_or_default().to_string()
}

fn websocket_url(http_url: &str, selector: &str) -> String {
    let mut url = http_url.to_string();
    if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{}", rest);
    } else if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{}", rest);
    }
    format!("{}?query={}", url, urlencoding::encode(selector))
}

/// Parse a short retention hint like `"7d"` or `"2w"`. Supported units:
/// `s` (seconds), `m` (minutes), `h` (hours), `d` (days), `w` (weeks; 7 days).
pub(super) fn parse_duration_hint(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if trimmed.len() < 2 {
        return None;
    }
    let (number, unit) = trimmed.split_at(trimmed.len() - 1);
    let number: i64 = number.parse().ok()?;
    match unit {
        "s" => Some(Duration::seconds(number)),
        "m" => Some(Duration::minutes(number)),
        "h" => Some(Duration::hours(number)),
        "d" => Some(Duration::days(number)),
        "w" => Some(Duration::weeks(number)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_loki_value_uses_stream_label_when_metadata_absent() {
        // Loki 3.x attaches `detected_level` as a stream-level label, not as
        // per-entry structured metadata. The 2-tuple `[ts, line]` is the
        // common shape; we have to read the stream label to surface a level.
        let value = LokiValue {
            timestamp: "1000000000".to_string(),
            line: "anything".to_string(),
            structured_metadata: None,
        };
        let parsed = LogLine::from_loki_value(value, Some("warn")).expect("valid loki value");
        assert_eq!(parsed.classified_level(), "warn");

        // Whitespace/empty stream labels are treated as "no classification"
        // — same as a missing label. The regex fallback runs against the
        // line content; here `"anything"` has no warn/error keyword so it
        // falls into the catch-all info bucket.
        let value = LokiValue {
            timestamp: "1000000000".to_string(),
            line: "anything".to_string(),
            structured_metadata: None,
        };
        let parsed = LogLine::from_loki_value(value, Some("  ")).expect("valid loki value");
        assert_eq!(parsed.classified_level(), "info");

        // Per-entry structured metadata wins over the stream-level label.
        let mut md = std::collections::HashMap::new();
        md.insert("detected_level".to_string(), "error".to_string());
        let value = LokiValue {
            timestamp: "1000000000".to_string(),
            line: "anything".to_string(),
            structured_metadata: Some(md),
        };
        let parsed = LogLine::from_loki_value(value, Some("warn")).expect("valid loki value");
        assert_eq!(parsed.classified_level(), "error");
    }

    #[test]
    fn from_loki_value_strips_trailing_newlines() {
        // Loki preserves the trailing newline the container wrote; strip it
        // so renderers/SSE consumers match the Kubernetes backend (whose
        // BufReader::lines() already strips line endings) and don't emit a
        // blank line between entries.
        let cases = [
            ("hello\n", "hello"),
            ("hello\r\n", "hello"),
            ("hello", "hello"),
        ];
        for (input, expected) in cases {
            let value = LokiValue {
                timestamp: "1000000000".to_string(),
                line: input.to_string(),
                structured_metadata: None,
            };
            let parsed = LogLine::from_loki_value(value, None).expect("valid loki value");
            assert_eq!(parsed.line, expected, "input {input:?}");
        }
    }

    #[test]
    fn build_count_buckets_is_right_edged_and_closed_at_end() {
        // Right-edged: each bucket at timestamp T represents (T - step, T], so
        // for [0s, 300s] at step=60s we expect 5 buckets at 60, 120, 180, 240,
        // 300 — NOT a bucket at 0 (that would cover (-60, 0], i.e. data from
        // before the user's window) and the last bucket must include the
        // aligned_end timestamp itself.
        let start = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let end = DateTime::<Utc>::from_timestamp(300, 0).unwrap();
        let buckets = LokiLogBackend::build_count_buckets(&[], start, end, 60);
        assert_eq!(buckets.len(), 5);
        // First bucket is at start + step, last bucket is at end.
        assert_eq!(
            buckets.first().map(|b| b.timestamp.as_str()),
            Some("1970-01-01T00:01:00+00:00")
        );
        assert_eq!(
            buckets.last().map(|b| b.timestamp.as_str()),
            Some("1970-01-01T00:05:00+00:00")
        );
    }

    #[test]
    fn build_count_buckets_places_point_at_aligned_end_into_last_bucket() {
        // A single Loki point at the aligned_end timestamp must land in the
        // LAST emitted bucket — confirming the (T - step, T] right-edged
        // window. Before the alignment fix this point landed in the
        // (non-existent) bucket past the end and was dropped.
        let step = 60;
        let start = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let end = DateTime::<Utc>::from_timestamp(300, 0).unwrap();
        let mut info = BTreeMap::new();
        info.insert(300i64 * 1_000_000_000, 42u64);
        let series = vec![("info".to_string(), info)];
        let buckets = LokiLogBackend::build_count_buckets(&series, start, end, step);
        assert_eq!(buckets.len(), 5);
        // Last bucket carries the value; earlier buckets are empty.
        assert_eq!(buckets.last().unwrap().total, 42);
        assert_eq!(
            buckets.last().unwrap().by_level.get("info").copied(),
            Some(42)
        );
        for b in &buckets[..buckets.len() - 1] {
            assert_eq!(b.total, 0);
            assert!(b.by_level.is_empty());
        }
    }

    #[test]
    fn build_count_buckets_passes_detected_level_through_verbatim() {
        // sum by (detected_level) returns one series per level value. The
        // bucket's `by_level` map preserves the raw label, and empty labels
        // coerce to "unknown" so the map's keys are always non-empty.
        let step = 60;
        let start = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let end = DateTime::<Utc>::from_timestamp(60, 0).unwrap();
        let bucket_ns = 60i64 * 1_000_000_000;
        let series = vec![
            (
                "info".to_string(),
                [(bucket_ns, 5u64)].into_iter().collect(),
            ),
            (
                "warn".to_string(),
                [(bucket_ns, 2u64)].into_iter().collect(),
            ),
            (
                "error".to_string(),
                [(bucket_ns, 1u64)].into_iter().collect(),
            ),
            (
                "unknown".to_string(),
                [(bucket_ns, 9u64)].into_iter().collect(),
            ),
            ("".to_string(), [(bucket_ns, 4u64)].into_iter().collect()),
        ];
        let buckets = LokiLogBackend::build_count_buckets(&series, start, end, step);
        assert_eq!(buckets.len(), 1);
        let b = &buckets[0];
        assert_eq!(b.by_level.get("info").copied(), Some(5));
        assert_eq!(b.by_level.get("warn").copied(), Some(2));
        assert_eq!(b.by_level.get("error").copied(), Some(1));
        // Both the explicit "unknown" series and the empty-label series
        // accumulate into the "unknown" key.
        assert_eq!(b.by_level.get("unknown").copied(), Some(9 + 4));
        // total = sum of all series counts.
        assert_eq!(b.total, 5 + 2 + 1 + 9 + 4);
    }

    #[test]
    fn build_count_buckets_drift_falls_into_correct_right_edged_bucket() {
        // A point that doesn't land on the boundary should be assigned to the
        // (T - step, T] window. A point at T - step (the bucket's lower edge)
        // belongs to the PREVIOUS bucket, not this one.
        let step = 60;
        let start = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let end = DateTime::<Utc>::from_timestamp(120, 0).unwrap();
        // 30s offset (mid-first-bucket) and 60s exact (right edge of first
        // bucket).
        let series = vec![(
            "info".to_string(),
            [
                (30i64 * 1_000_000_000, 7u64),
                (60i64 * 1_000_000_000, 11u64),
            ]
            .into_iter()
            .collect::<BTreeMap<i64, u64>>(),
        )];
        let buckets = LokiLogBackend::build_count_buckets(&series, start, end, step);
        assert_eq!(buckets.len(), 2);
        // First bucket (timestamp=60) covers (0, 60] → 7 + 11 = 18.
        assert_eq!(buckets[0].total, 18);
        // Second bucket (timestamp=120) covers (60, 120] → nothing.
        assert_eq!(buckets[1].total, 0);
    }

    #[test]
    fn escape_logql_label_value_escapes_backslash_and_quote() {
        // Plain name passes through unchanged.
        assert_eq!(escape_logql_label_value("my-project"), "my-project");
        // Backslash becomes \\
        assert_eq!(
            escape_logql_label_value(r"with\backslash"),
            r"with\\backslash"
        );
        // Double quote becomes \"
        assert_eq!(escape_logql_label_value(r#"with"quote"#), r#"with\"quote"#);
        // Both at once — order doesn't double-escape the inserted backslashes.
        // Input: with\and"both → expected: with\\and\"both
        assert_eq!(
            escape_logql_label_value(r#"with\and"both"#),
            r#"with\\and\"both"#
        );
    }

    #[test]
    fn append_detected_level_filter_handles_levels_and_regex_escape() {
        // Empty levels → no clause appended.
        let out = append_detected_level_filter("{job=\"x\"}", &[]);
        assert_eq!(out, "{job=\"x\"}");
        // Whitespace-only entries are dropped.
        let out =
            append_detected_level_filter("{job=\"x\"}", &["info".to_string(), "  ".to_string()]);
        assert_eq!(out, "{job=\"x\"} | detected_level=~\"info\"");
        // Multiple levels are alternated, regex-special chars escaped.
        let out =
            append_detected_level_filter("{job=\"x\"}", &["error".to_string(), "warn".to_string()]);
        assert_eq!(out, "{job=\"x\"} | detected_level=~\"error|warn\"");
    }

    #[test]
    fn truncate_for_error_keeps_short_strings_intact() {
        let short = "boom".to_string();
        assert_eq!(truncate_for_error(short.clone()), short);
    }

    #[test]
    fn truncate_for_error_truncates_long_strings_at_utf8_boundary() {
        let long: String = "a".repeat(2048);
        let out = truncate_for_error(long);
        assert!(out.ends_with("... (truncated)"));
        // Total length: 1024 + len("... (truncated)").
        assert_eq!(out.len(), 1024 + "... (truncated)".len());
    }

    #[test]
    fn append_search_filter_strips_backticks() {
        // Backticks would close the LogQL raw-string literal mid-filter and
        // produce a syntactically invalid query — must be stripped, not
        // regex-escaped.
        let out = append_search_filter("{job=\"x\"}", Some("a`b"));
        assert!(!out.contains('`') || out.matches('`').count() == 2);
        assert!(out.contains("ab"));
    }

    #[test]
    fn append_search_filter_with_only_backticks_is_noop() {
        // After stripping, an all-backtick search yields the original selector
        // rather than `|~ \`(?i)\``.
        let out = append_search_filter("{job=\"x\"}", Some("```"));
        assert_eq!(out, "{job=\"x\"}");
    }

    /// Mirror of the K8s backend's tail-offset slice math: given a buffer of
    /// chronologically-ordered lines from the kubelet (oldest first, most
    /// recent last), the backend yields all but the trailing `skip_recent`
    /// entries. This is the logic that lets the frontend page backward
    /// without an end-time filter.
    fn slice_for_skip_recent(lines: Vec<String>, skip_recent: usize) -> Vec<String> {
        use std::collections::VecDeque;
        let mut out = Vec::new();
        let mut trailing: VecDeque<String> = VecDeque::with_capacity(skip_recent.saturating_add(1));
        for line in lines {
            if skip_recent == 0 {
                out.push(line);
            } else {
                trailing.push_back(line);
                if trailing.len() > skip_recent {
                    if let Some(o) = trailing.pop_front() {
                        out.push(o);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn skip_recent_drops_the_trailing_window() {
        // Kubelet returns chronological [1..=10]. With skip_recent=4, we want
        // [1..=6] yielded (the older 6); the most recent 4 are what the
        // frontend already has.
        let lines: Vec<String> = (1..=10).map(|n| format!("line-{n}")).collect();
        let out = slice_for_skip_recent(lines, 4);
        assert_eq!(
            out,
            vec!["line-1", "line-2", "line-3", "line-4", "line-5", "line-6"]
        );
    }

    #[test]
    fn skip_recent_zero_is_passthrough() {
        let lines: Vec<String> = (1..=3).map(|n| format!("line-{n}")).collect();
        let out = slice_for_skip_recent(lines.clone(), 0);
        assert_eq!(out, lines);
    }

    #[test]
    fn skip_recent_larger_than_input_yields_nothing() {
        // Kubelet rotated; we've already shown more than it has left.
        let lines: Vec<String> = (1..=3).map(|n| format!("line-{n}")).collect();
        let out = slice_for_skip_recent(lines, 10);
        assert!(out.is_empty());
    }

    #[test]
    fn loki_metric_response_parses_numeric_timestamps_and_string_counts() {
        let payload = r#"{
          "status":"success",
          "data":{
            "resultType":"matrix",
            "result":[{"metric":{},"values":[[1700000000,"5"],[1700000060,"3"]]}]
          }
        }"#;
        let parsed: LokiMetricQueryResponse =
            serde_json::from_str(payload).expect("response should parse");
        let series = &parsed.data.result[0];
        let sample = &series.values[0];
        assert_eq!(sample.0, 1700000000.0);
        let points = series.points();
        assert_eq!(points[0].count, 5);
        assert_eq!(points[1].count, 3);
    }

    #[test]
    fn volume_pipeline_aligns_detected_level_series_to_right_edged_buckets() {
        // End-to-end-ish: take a realistic `sum by (detected_level)` response,
        // parse it the way `query_counts_by_level` does, and feed the result
        // into `build_count_buckets`. The point at timestamp=aligned_end must
        // land in the LAST bucket and the per-level counts must round-trip
        // verbatim into `by_level` (no remapping).
        let payload = r#"{
          "status":"success",
          "data":{
            "resultType":"matrix",
            "result":[
              {"metric":{"detected_level":"info"},"values":[[60, "5"]]},
              {"metric":{"detected_level":"warn"},"values":[[60, "3"]]},
              {"metric":{"detected_level":"error"},"values":[[60, "2"]]},
              {"metric":{"detected_level":"unknown"},"values":[[60, "4"]]}
            ]
          }
        }"#;
        let parsed: LokiMetricQueryResponse =
            serde_json::from_str(payload).expect("response should parse");
        let series: Vec<(String, BTreeMap<i64, u64>)> = parsed
            .data
            .result
            .into_iter()
            .map(|s| {
                let label = s.metric.get("detected_level").cloned().unwrap_or_default();
                let mut points: BTreeMap<i64, u64> = BTreeMap::new();
                for p in s.points() {
                    points.insert(p.timestamp_nanos, p.count);
                }
                (label, points)
            })
            .collect();

        // [0, 60] @ step=60 → exactly one bucket at ts=60.
        let start = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let end = DateTime::<Utc>::from_timestamp(60, 0).unwrap();
        let buckets = LokiLogBackend::build_count_buckets(&series, start, end, 60);
        assert_eq!(buckets.len(), 1);
        let b = &buckets[0];
        assert_eq!(b.timestamp, "1970-01-01T00:01:00+00:00");
        assert_eq!(b.by_level.get("info").copied(), Some(5));
        assert_eq!(b.by_level.get("warn").copied(), Some(3));
        assert_eq!(b.by_level.get("error").copied(), Some(2));
        assert_eq!(b.by_level.get("unknown").copied(), Some(4));
        assert_eq!(b.total, 5 + 3 + 2 + 4);
    }

    #[test]
    fn loki_metric_response_accepts_numeric_count() {
        // Lock in the numeric-count contract (item 4): even though Loki normally
        // returns counts as JSON strings, a numeric value must not fail parsing.
        let payload = r#"{
          "status":"success",
          "data":{
            "resultType":"matrix",
            "result":[{"metric":{},"values":[[1700000000, 5]]}]
          }
        }"#;
        let parsed: LokiMetricQueryResponse =
            serde_json::from_str(payload).expect("numeric count should parse");
        let series = &parsed.data.result[0];
        assert_eq!(series.values[0].0, 1700000000.0);
        assert_eq!(series.points()[0].count, 5);
    }

    #[test]
    fn parse_duration_hint_supports_all_units_including_weeks() {
        // Cover every supported unit; `w` is the new one and must equal 7d.
        assert_eq!(parse_duration_hint("30s"), Some(Duration::seconds(30)));
        assert_eq!(parse_duration_hint("5m"), Some(Duration::minutes(5)));
        assert_eq!(parse_duration_hint("2h"), Some(Duration::hours(2)));
        assert_eq!(parse_duration_hint("7d"), Some(Duration::days(7)));
        assert_eq!(parse_duration_hint("2w"), Some(Duration::weeks(2)));
        assert_eq!(parse_duration_hint("2w"), Some(Duration::days(14)));
        // Negative weeks pass through (chrono::Duration accepts negatives) — a
        // retention hint of "-1w" is nonsensical but the parser shouldn't
        // crash on it. We only assert the unit math here.
        assert_eq!(parse_duration_hint("0w"), Some(Duration::zero()));

        // Unsupported unit / malformed input.
        assert_eq!(parse_duration_hint("5y"), None);
        assert_eq!(parse_duration_hint("xw"), None);
        // Whitespace is trimmed.
        assert_eq!(parse_duration_hint("  3w  "), Some(Duration::weeks(3)));
    }

    #[test]
    fn loki_value_deserialize_accepts_two_and_three_tuples() {
        // Legacy 2-tuple: no structured metadata → detected_level is None.
        let v2: LokiValue =
            serde_json::from_str(r#"["1700000000000000000","hello"]"#).expect("2-tuple must parse");
        assert_eq!(v2.line, "hello");
        assert!(v2.structured_metadata.is_none());

        // Loki 3.x 3-tuple with structured metadata carrying detected_level.
        let v3: LokiValue = serde_json::from_str(
            r#"["1700000000000000000","boom",{"detected_level":"error","trace_id":"abc"}]"#,
        )
        .expect("3-tuple must parse");
        let md = v3
            .structured_metadata
            .as_ref()
            .expect("metadata must be populated");
        assert_eq!(md.get("detected_level").map(String::as_str), Some("error"));
    }

    #[test]
    fn classified_level_passes_detected_level_through_verbatim() {
        // Whatever Loki's auto-detection emits, we surface it as-is. No
        // remapping into a Rise-specific enum; the frontend's palette is
        // driven by `/api/v1/logs/capabilities`.
        for raw in ["info", "warn", "error", "critical", "fatal", "trace"] {
            let l = LogLine {
                timestamp_nanos: 0,
                line: "anything".into(),
                detected_level: Some(raw.into()),
            };
            assert_eq!(l.classified_level(), raw);
        }

        // Whitespace is trimmed.
        let l = LogLine {
            timestamp_nanos: 0,
            line: "anything".into(),
            detected_level: Some("  warn  ".into()),
        };
        assert_eq!(l.classified_level(), "warn");

        // Loki's explicit "unknown" is still trusted verbatim — when Loki
        // says "I can't classify this", we don't second-guess.
        let l = LogLine {
            timestamp_nanos: 0,
            line: "ERROR something exploded".into(),
            detected_level: Some("unknown".into()),
        };
        assert_eq!(l.classified_level(), "unknown");
    }

    #[test]
    fn classified_level_falls_back_to_regex_when_label_is_missing() {
        // Loki's WS tail emits in-flight entries before the classifier has
        // run on them, so the `detected_level` label is missing entirely.
        // Fall back to the K8s line-regex so the live tail isn't dim by
        // default. Empty / whitespace-only labels are treated the same as
        // absent — they signal "not classified" rather than "unknown".
        let cases = [
            ("info line that says hello", "info"),
            ("WARN connection retry", "warn"),
            ("ERROR boom", "error"),
        ];
        for (line, expected) in cases {
            for raw in [None, Some(String::new()), Some("   ".into())] {
                let l = LogLine {
                    timestamp_nanos: 0,
                    line: line.into(),
                    detected_level: raw,
                };
                assert_eq!(l.classified_level(), expected, "input {line:?}");
            }
        }
    }

    #[test]
    fn classify_k8s_line_returns_one_of_three_levels() {
        // The K8s backend has no upstream classifier; its regex catch-all
        // promises every line lands in exactly one of `KUBERNETES_LEVELS`.
        assert_eq!(classify_k8s_line("plain hello world"), "info");
        assert_eq!(classify_k8s_line("WARN connection retry"), "warn");
        assert_eq!(classify_k8s_line("ERROR: failed to connect"), "error");
        // Error wins over warn when both match.
        assert_eq!(classify_k8s_line("WARN: fatal error in handler"), "error");
    }

    #[test]
    fn validate_loki_label_name_rejects_bad_inputs() {
        assert!(validate_loki_label_name("project", "rise_project").is_ok());
        assert!(validate_loki_label_name("project", "_internal").is_ok());
        // Leading digit, hyphen, dot, brace — all invalid in Loki/Prom label
        // names and would break the LogQL selector if interpolated.
        assert!(validate_loki_label_name("project", "1bad").is_err());
        assert!(validate_loki_label_name("project", "with-hyphen").is_err());
        assert!(validate_loki_label_name("project", "a.b").is_err());
        assert!(validate_loki_label_name("project", "x}\"=\"y").is_err());
        assert!(validate_loki_label_name("project", "").is_err());
    }
}
