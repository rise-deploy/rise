use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::db::models::{Deployment, DeploymentStatus, Project};
use crate::server::deployment::resource_builder::{ResourceBuilder, LABEL_CONTAINER};
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
    /// Containers of the deployment the caller wants to see. Empty means
    /// "all". Names are the deployment's own container names (the implicit
    /// `app` for a single-container deployment); each backend maps them onto
    /// whatever carries the attribution in its own store.
    pub containers: Vec<String>,
    /// Opaque continuation returned by the configured backend. Its contents
    /// are private to that backend and bound to the deployment and filters.
    pub cursor: Option<String>,
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
    /// Containers the caller wants counted. Empty means "all".
    pub containers: Vec<String>,
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
    /// of `KUBERNETES_LEVELS`). `id` remains stable when the same stored event
    /// appears in a retried or adjacent request. `container` names the
    /// deployment container the line came from, when the backend can
    /// attribute it.
    Line {
        id: String,
        text: String,
        level: String,
        container: Option<String>,
    },
    Status(LogStatus),
    /// Sent once the initial backlog phase of a streaming request has been
    /// fully emitted, before the live-tail loop begins. `count` reports the
    /// emitted backlog size and `next_cursor` continues toward older entries.
    BacklogLoaded {
        count: usize,
        next_cursor: Option<String>,
    },
    /// Completes a finite historical page. A cursor is present exactly when
    /// the backend can continue paging toward older entries.
    PageLoaded {
        next_cursor: Option<String>,
    },
    /// Makes a continuation available before a combined backlog/follow source
    /// reaches an explicit backlog boundary.
    CursorUpdated {
        next_cursor: String,
    },
}

pub type LogEventStream = futures::stream::BoxStream<'static, Result<LogEvent>>;

pub(super) fn encode_log_cursor<T: Serialize>(cursor: &T) -> Result<String> {
    let bytes = serde_json::to_vec(cursor).context("Failed to encode log cursor")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn decode_log_cursor<T: DeserializeOwned>(cursor: &str) -> Result<T> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .context("invalid log cursor encoding")?;
    serde_json::from_slice(&bytes).context("invalid log cursor payload")
}

pub(super) fn log_cursor_signature(
    backend: &str,
    deployment: &Deployment,
    project: &Project,
    query: &LogQuery,
) -> String {
    let mut levels = query.levels.clone();
    levels.sort();
    levels.dedup();
    let mut containers = query.containers.clone();
    containers.sort();
    containers.dedup();
    let deployment_id = deployment.id.to_string();
    let project_id = project.id.to_string();
    let levels = levels.join("\0");
    let containers = containers.join("\0");

    let mut digest = Sha256::new();
    for part in [
        backend.as_bytes(),
        deployment_id.as_bytes(),
        project_id.as_bytes(),
        levels.as_bytes(),
        query.search.as_deref().unwrap_or_default().as_bytes(),
        containers.as_bytes(),
    ] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    URL_SAFE_NO_PAD.encode(digest.finalize())
}

pub(super) fn stable_log_id<'a>(
    backend: &str,
    parts: impl IntoIterator<Item = &'a [u8]>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(backend.as_bytes());
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    URL_SAFE_NO_PAD.encode(digest.finalize())
}

fn distinct_log_id(seen: &mut HashMap<String, u64>, base_id: String) -> String {
    let occurrence = seen.entry(base_id.clone()).or_default();
    let id = if *occurrence == 0 {
        base_id.clone()
    } else {
        let occurrence_bytes = occurrence.to_be_bytes();
        stable_log_id(
            "occurrence",
            [base_id.as_bytes(), occurrence_bytes.as_slice()],
        )
    };
    *occurrence = occurrence.saturating_add(1);
    id
}

fn split_timestamped_log_line(line: &str) -> Option<(DateTime<Utc>, &str, &str)> {
    let (timestamp_text, content) = line.split_once(' ')?;
    let timestamp = DateTime::parse_from_rfc3339(timestamp_text)
        .ok()?
        .with_timezone(&Utc);
    Some((timestamp, content, timestamp_text))
}

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
    /// Identifier surfaced for diagnostics in the capabilities response.
    /// Query and pagination behavior remains behind the trait.
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
            validate_loki_label_name("container", &labels.container)?;
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

#[derive(Debug, Serialize, Deserialize)]
struct TailLogCursor {
    version: u8,
    signature: String,
    offset: usize,
    raw_tail: usize,
    start_nanos: Option<i64>,
    end_nanos: Option<i64>,
}

fn select_recent_page<T>(items: Vec<T>, page_size: usize, skip_recent: usize) -> (Vec<T>, bool) {
    let end = items.len().saturating_sub(skip_recent);
    let start = end.saturating_sub(page_size);
    let has_older = start > 0;
    let page = items
        .into_iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    (page, has_older)
}

fn kubernetes_tail_lines(has_end_time: bool, follow: bool, effective_tail: usize) -> Option<i64> {
    (!has_end_time || follow).then_some(effective_tail as i64)
}

fn next_kubernetes_raw_tail(
    previous_raw_tail: Option<usize>,
    page_size: usize,
    max_tail: usize,
) -> usize {
    previous_raw_tail
        .unwrap_or_default()
        .saturating_add(page_size)
        .clamp(1, max_tail)
}

fn kubernetes_page_has_more(
    has_older_in_window: bool,
    has_end_time: bool,
    requested_tail: Option<i64>,
    raw_count: usize,
    raw_tail: usize,
    max_tail: usize,
) -> bool {
    let can_expand_window = !has_end_time
        && requested_tail.is_some_and(|tail| raw_count >= tail as usize)
        && raw_tail < max_tail;
    has_older_in_window || can_expand_window
}

fn kubernetes_since_seconds(now: DateTime<Utc>, start: DateTime<Utc>) -> Option<i64> {
    let delta = now - start;
    if delta <= Duration::zero() {
        return None;
    }
    let whole_seconds = delta.num_seconds();
    Some(whole_seconds.saturating_add(i64::from(delta > Duration::seconds(whole_seconds))))
}

fn distinct_log_ids_from_newest(base_ids: &[String]) -> Vec<String> {
    let mut ids = vec![String::new(); base_ids.len()];
    let mut seen = HashMap::new();
    for (index, base_id) in base_ids.iter().enumerate().rev() {
        ids[index] = distinct_log_id(&mut seen, base_id.clone());
    }
    ids
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
        let mut selector = format!("rise.dev/deployment-id={}", deployment.deployment_id);
        if !query.containers.is_empty() {
            // Every container of a deployment gets its own Pod carrying
            // `rise.dev/container` (single-container apps included, under the
            // implicit `app`), so the filter is a set-based label selector.
            selector.push_str(&format!(
                ",{} in ({})",
                LABEL_CONTAINER,
                query.containers.join(",")
            ));
        }
        let pods = pod_api
            .list(&ListParams::default().labels(&selector))
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
        // One Pod carries exactly one deployment container, so its label
        // attributes every line the kubelet returns below.
        let container = pod
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(LABEL_CONTAINER))
            .cloned();

        let signature = log_cursor_signature("kubernetes", deployment, project, &query);
        let cursor = query
            .cursor
            .as_deref()
            .map(decode_log_cursor::<TailLogCursor>)
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.version != 3 || cursor.signature != signature)
        {
            anyhow::bail!("invalid log cursor for this deployment or filter");
        }
        let skip_recent = cursor.as_ref().map_or(0, |cursor| cursor.offset);
        let page_size = query.tail_lines.unwrap_or(1_000).max(1) as usize;
        // The configured ceiling bounds both expanding tail requests and the
        // qualifying history retained while scanning an end-bounded stream.
        let max_tail = self.config.max_tail_lines.max(1) as usize;
        if skip_recent > max_tail
            || cursor
                .as_ref()
                .is_some_and(|cursor| cursor.raw_tail == 0 || cursor.raw_tail > max_tail)
        {
            anyhow::bail!("invalid log cursor offset");
        }
        let follow = query.follow && is_followable_status(&deployment.status);
        let request_now = Utc::now();
        let start_time = cursor
            .as_ref()
            .and_then(|cursor| cursor.start_nanos)
            .map(DateTime::<Utc>::from_timestamp_nanos)
            .or(query.start_time)
            .or_else(|| {
                query
                    .since_seconds
                    .map(|seconds| request_now - Duration::seconds(seconds))
            });
        let end_time = cursor
            .as_ref()
            .and_then(|cursor| cursor.end_nanos)
            .map(DateTime::<Utc>::from_timestamp_nanos)
            .or(query.end_time);
        if cursor.is_some()
            && start_time
                .zip(end_time)
                .is_some_and(|(start, end)| start >= end)
        {
            anyhow::bail!("invalid log cursor time range");
        }
        let start_nanos = start_time
            .map(|start| {
                start
                    .timestamp_nanos_opt()
                    .context("Kubernetes log start is outside the supported range")
            })
            .transpose()?;
        let end_nanos = end_time
            .map(|end| {
                end.timestamp_nanos_opt()
                    .context("Kubernetes log end is outside the supported range")
            })
            .transpose()?;
        let raw_tail = next_kubernetes_raw_tail(
            cursor.as_ref().map(|cursor| cursor.raw_tail),
            page_size,
            max_tail,
        );
        let requested_tail = kubernetes_tail_lines(end_time.is_some(), follow, raw_tail);

        let mut log_params = LogParams {
            follow,
            // Event timestamps provide both range enforcement and identity,
            // even when the caller does not render them.
            timestamps: true,
            ..Default::default()
        };
        log_params.tail_lines = requested_tail;
        if let Some(start_time) = start_time {
            if let Some(since_seconds) = kubernetes_since_seconds(request_now, start_time) {
                log_params.since_seconds = Some(since_seconds);
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
        let render_timestamps = query.timestamps;
        let pod_name_for_id = pod_name.clone();
        let namespace_for_id = namespace.clone();
        let stream = async_stream::stream! {
            use futures::AsyncBufReadExt;
            use std::collections::VecDeque;
            let mut emitted_count = 0usize;
            let mut raw_count = 0usize;
            let mut finite_lines = VecDeque::with_capacity(max_tail);
            let mut seen_ids = HashMap::new();
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
                raw_count = raw_count.saturating_add(1);
                let Some((timestamp, content, timestamp_text)) = split_timestamped_log_line(&line) else {
                    continue;
                };
                let event_end_nanos = timestamp
                    .timestamp_nanos_opt()
                    .and_then(|timestamp| timestamp.checked_add(1));
                if end_time.is_some_and(|end| timestamp >= end) {
                    break;
                }
                if content.is_empty() { continue; }
                if start_time.is_some_and(|start| timestamp < start) {
                    continue;
                }
                let level = classify_k8s_line(content);
                if !levels.is_empty() && !levels.iter().any(|l| l == level) { continue; }
                if !line_matches_search(content, search.as_deref()) { continue; }
                let base_id = stable_log_id(
                    "kubernetes",
                    [
                        namespace_for_id.as_bytes(),
                        pod_name_for_id.as_bytes(),
                        timestamp_text.as_bytes(),
                        content.as_bytes(),
                    ],
                );
                let rendered = if render_timestamps { line } else { content.to_string() };
                if follow {
                    let id = distinct_log_id(&mut seen_ids, base_id);
                    emitted_count = emitted_count.saturating_add(1);
                    yield Ok(LogEvent::Line {
                        id,
                        text: rendered,
                        level: level.to_string(),
                        container: container.clone(),
                    });
                    match encode_log_cursor(&TailLogCursor {
                        version: 3,
                        signature: signature.clone(),
                        offset: skip_recent.saturating_add(emitted_count).min(max_tail),
                        raw_tail: max_tail,
                        start_nanos,
                        end_nanos: event_end_nanos,
                    }) {
                        Ok(next_cursor) => yield Ok(LogEvent::CursorUpdated { next_cursor }),
                        Err(error) => {
                            yield Err(error);
                            return;
                        }
                    }
                } else {
                    if finite_lines.len() == max_tail {
                        finite_lines.pop_front();
                    }
                    finite_lines.push_back((rendered, level, base_id));
                }
            }
            if !follow {
                let mut finite_lines = finite_lines.into_iter().collect::<Vec<_>>();
                let base_ids = finite_lines
                    .iter()
                    .map(|(_, _, base_id)| base_id.clone())
                    .collect::<Vec<_>>();
                for ((_, _, id), distinct_id) in finite_lines
                    .iter_mut()
                    .zip(distinct_log_ids_from_newest(&base_ids))
                {
                    *id = distinct_id;
                }
                let (page, has_older_in_window) =
                    select_recent_page(finite_lines, page_size, skip_recent);
                emitted_count = page.len();
                for (line, level, id) in page {
                    yield Ok(LogEvent::Line {
                        id,
                        text: line,
                        level: level.to_string(),
                        container: container.clone(),
                    });
                }
                let has_more = kubernetes_page_has_more(
                    has_older_in_window,
                    end_time.is_some(),
                    requested_tail,
                    raw_count,
                    raw_tail,
                    max_tail,
                );
                let next_cursor = if has_more {
                    encode_log_cursor(&TailLogCursor {
                        version: 3,
                        signature,
                        offset: skip_recent.saturating_add(emitted_count),
                        raw_tail: requested_tail.map_or(max_tail, |tail| tail as usize),
                        start_nanos,
                        end_nanos,
                    })
                } else {
                    Ok(String::new())
                };
                match next_cursor {
                    Ok(next_cursor) => yield Ok(LogEvent::PageLoaded {
                        next_cursor: has_more.then_some(next_cursor),
                    }),
                    Err(error) => yield Err(error),
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
        query: LogQuery,
    ) -> Result<LogEventStream> {
        if query.cursor.is_some() {
            anyhow::bail!("invalid log cursor for the configured backend");
        }
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
    /// Find the most relevant container for a deployment by its
    /// `<label_namespace>/deployment-id` label, scoped to the owning project,
    /// and return its id alongside the deployment container name it runs.
    /// Prefers a running container. `wanted` restricts the choice to those
    /// deployment containers; empty means "any".
    ///
    /// `deployment_id` is a `YYYYMMDD-HHMMSS` timestamp that is unique only
    /// *per project* (DB constraint `UNIQUE (deployment_id, project_id)`), so
    /// filtering on it alone could resolve to another project's container —
    /// a tenant-isolation breach. We therefore also scope by the `project`
    /// label (matching `project.name`, exactly as the reconciler stamps it)
    /// plus `managed-by=rise` for defense-in-depth, mirroring
    /// `list_actual_containers`.
    async fn resolve_container(
        &self,
        deployment: &Deployment,
        project: &Project,
        wanted: &[String],
    ) -> Result<Option<(String, Option<String>)>> {
        use crate::server::deployment::controller::docker::labels::{
            self, SUFFIX_CONTAINER, SUFFIX_DEPLOYMENT_ID, SUFFIX_MANAGED_BY, SUFFIX_PROJECT,
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

        // Docker ANDs repeated `label` filters, so the container filter — an
        // OR over names — is applied here rather than in the daemon query.
        let container_key = labels::ns_key(ns, SUFFIX_CONTAINER);
        let container_name = |summary: &bollard::secret::ContainerSummary| {
            summary
                .labels
                .as_ref()
                .and_then(|labels| labels.get(&container_key))
                .cloned()
        };
        let candidates = summaries
            .into_iter()
            .filter(|summary| {
                wanted.is_empty()
                    || container_name(summary).is_some_and(|name| wanted.contains(&name))
            })
            .collect::<Vec<_>>();

        // Prefer a running container; fall back to any.
        let chosen = candidates
            .iter()
            .find(|c| c.state.as_deref() == Some("running"))
            .or_else(|| candidates.first());
        Ok(chosen.and_then(|summary| summary.id.clone().map(|id| (id, container_name(summary)))))
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
        if query.cursor.is_some() {
            anyhow::bail!("invalid log cursor for the configured backend");
        }

        let Some((container_id, container)) = self
            .resolve_container(deployment, project, &query.containers)
            .await?
        else {
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
            until: query
                .end_time
                .map(|end| end.timestamp().saturating_add(1))
                .unwrap_or_default(),
            // Event timestamps provide both range enforcement and identity,
            // even when the caller does not render them.
            timestamps: true,
            tail,
        };

        let log_stream = self.docker.logs(&container_id, Some(options));
        let levels = query.levels.clone();
        let search = query.search.clone();
        let follow = query.follow && is_followable_status(&deployment.status);
        let render_timestamps = query.timestamps;
        let start_time = query.start_time;
        let end_time = query.end_time;
        let container_id_for_lines = container_id.clone();
        let stream = async_stream::stream! {
            let mut seen_ids = HashMap::new();
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
                    let Some((timestamp, content, timestamp_text)) = split_timestamped_log_line(line) else {
                        continue;
                    };
                    if content.is_empty() { continue; }
                    if start_time.is_some_and(|start| timestamp < start)
                        || end_time.is_some_and(|end| timestamp >= end)
                    {
                        continue;
                    }
                    let level = classify_k8s_line(content);
                    if !levels.is_empty() && !levels.iter().any(|l| l == level) {
                        continue;
                    }
                    if !line_matches_search(content, search.as_deref()) {
                        continue;
                    }
                    let base_id = stable_log_id(
                        "docker",
                        [
                            container_id_for_lines.as_bytes(),
                            timestamp_text.as_bytes(),
                            content.as_bytes(),
                        ],
                    );
                    let id = distinct_log_id(&mut seen_ids, base_id);
                    yield Ok(LogEvent::Line {
                        id,
                        text: if render_timestamps { line.to_string() } else { content.to_string() },
                        level: level.to_string(),
                        container: container.clone(),
                    });
                }
            }
            if !follow {
                yield Ok(LogEvent::PageLoaded { next_cursor: None });
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

#[derive(Debug, Serialize, Deserialize)]
struct LokiLogCursor {
    version: u8,
    signature: String,
    end_nanos: i64,
    stream_offsets: BTreeMap<String, usize>,
}

/// The window a Loki continuation query covers: from the deployment's creation
/// up to the cursor's boundary.
///
/// `None` when the boundary sits at or before that floor, which would ask Loki
/// for an empty or inverted range — it rejects those outright, and without this
/// the rejection reaches the client as a 500 rather than as the stale cursor it
/// is. The Kubernetes and CloudWatch backends make the same check on their own
/// cursor shapes.
fn loki_continuation_window(
    created_at: DateTime<Utc>,
    boundary_nanos: i64,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let boundary = DateTime::<Utc>::from_timestamp_nanos(boundary_nanos);
    (created_at < boundary).then_some((created_at, boundary))
}

struct LokiLogPage {
    lines: Vec<LogLine>,
    next_cursor: Option<String>,
}

fn select_loki_page(
    lines_ascending: Vec<LogLine>,
    page_size: usize,
    boundary_nanos: Option<i128>,
    stream_offsets: &BTreeMap<String, usize>,
) -> Vec<LogLine> {
    let mut remaining_boundary = stream_offsets.clone();
    let mut lines = Vec::with_capacity(page_size);
    for line in lines_ascending.into_iter().rev() {
        if boundary_nanos == Some(line.timestamp_nanos) {
            if let Some(remaining) = remaining_boundary.get_mut(&line.stream_key) {
                if *remaining > 0 {
                    *remaining -= 1;
                    continue;
                }
            }
        }
        lines.push(line);
        if lines.len() == page_size {
            break;
        }
    }
    sort_loki_lines(&mut lines);
    lines
}

fn loki_boundary_request_limit(
    page_size: usize,
    stream_offsets: &BTreeMap<String, usize>,
) -> Option<usize> {
    let boundary_offset = stream_offsets
        .values()
        .copied()
        .fold(0usize, usize::saturating_add);
    if boundary_offset >= LOKI_MAX_TAIL as usize {
        return None;
    }
    let limit = page_size
        .saturating_add(boundary_offset)
        .min(LOKI_MAX_TAIL as usize);
    (limit > boundary_offset).then_some(limit)
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

    fn base_selector(
        &self,
        deployment: &Deployment,
        project: &Project,
        containers: &[String],
    ) -> String {
        loki_base_selector(
            &self.labels,
            &project.name,
            &deployment.deployment_id,
            containers,
        )
    }

    fn selector(
        &self,
        deployment: &Deployment,
        project: &Project,
        levels: &[String],
        search: Option<&str>,
        containers: &[String],
    ) -> String {
        let base = self.base_selector(deployment, project, containers);
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
        let selector = self.selector(
            deployment,
            project,
            &query.levels,
            query.search.as_deref(),
            &query.containers,
        );
        let end = self.effective_end_time(query);
        // Cursor boundaries are inclusive so every occurrence at the oldest
        // timestamp remains reachable. Caller-supplied range ends are
        // exclusive, matching the public API contract.
        let end = if query.end_time.is_some() && query.cursor.is_none() {
            end.checked_sub_signed(Duration::nanoseconds(1))
                .context("Loki query end is outside the supported range")?
        } else {
            end
        };
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
                let container = stream.stream.get(&self.labels.container).cloned();
                let stream_key = canonical_loki_stream(&stream.stream);
                stream
                    .values
                    .into_iter()
                    .enumerate()
                    .map(move |(order, v)| {
                        (
                            v,
                            level.clone(),
                            container.clone(),
                            stream_key.clone(),
                            order,
                        )
                    })
            })
            .filter_map(|(value, level, container, stream_key, stream_order)| {
                LogLine::from_loki_value(
                    value,
                    level.as_deref(),
                    container,
                    &stream_key,
                    stream_order,
                )
            })
            .collect::<Vec<_>>();
        assign_distinct_loki_ids(&mut lines);
        sort_loki_lines(&mut lines);
        Ok(lines)
    }

    async fn query_page(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: &LogQuery,
    ) -> Result<LokiLogPage> {
        let page_size = query.tail_lines.unwrap_or(1_000).clamp(1, LOKI_MAX_TAIL) as usize;
        let signature = log_cursor_signature("loki", deployment, project, query);
        let cursor = query
            .cursor
            .as_deref()
            .map(decode_log_cursor::<LokiLogCursor>)
            .transpose()?;
        if cursor.as_ref().is_some_and(|cursor| {
            cursor.version != 2
                || cursor.signature != signature
                || cursor
                    .stream_offsets
                    .values()
                    .copied()
                    .fold(0usize, usize::saturating_add)
                    >= LOKI_MAX_TAIL as usize
        }) {
            anyhow::bail!("invalid log cursor for this deployment or filter");
        }

        let mut page_query = query.clone();
        page_query.follow = false;
        let (boundary_nanos, stream_offsets) = if let Some(cursor) = &cursor {
            let (start, end) = loki_continuation_window(deployment.created_at, cursor.end_nanos)
                .context("invalid log cursor time range")?;
            page_query.start_time = Some(start);
            page_query.end_time = Some(end);
            (
                Some(cursor.end_nanos as i128),
                cursor.stream_offsets.clone(),
            )
        } else {
            (None, BTreeMap::new())
        };
        let request_limit = loki_boundary_request_limit(page_size, &stream_offsets)
            .context("invalid log cursor boundary for the Loki query limit")?;
        page_query.tail_lines = Some(request_limit as i64);

        let queried_lines = self.query_range(deployment, project, &page_query).await?;
        let query_limit_reached = queried_lines.len() == request_limit;
        let lines = select_loki_page(queried_lines, page_size, boundary_nanos, &stream_offsets);

        let has_more = !lines.is_empty() && (lines.len() == page_size || query_limit_reached);
        let next_cursor = if has_more {
            let oldest_nanos = lines
                .first()
                .map(|line| line.timestamp_nanos)
                .context("Loki page cursor requires an oldest line")?;
            let mut next_stream_offsets = if boundary_nanos == Some(oldest_nanos) {
                stream_offsets
            } else {
                BTreeMap::new()
            };
            for line in lines
                .iter()
                .take_while(|line| line.timestamp_nanos == oldest_nanos)
            {
                let offset = next_stream_offsets
                    .entry(line.stream_key.clone())
                    .or_default();
                *offset = offset.saturating_add(1);
            }
            let boundary_offset = next_stream_offsets
                .values()
                .copied()
                .fold(0usize, usize::saturating_add);
            if boundary_offset >= LOKI_MAX_TAIL as usize {
                None
            } else {
                Some(encode_log_cursor(&LokiLogCursor {
                    version: 2,
                    signature,
                    end_nanos: i64::try_from(oldest_nanos)
                        .context("Loki cursor timestamp is outside the supported range")?,
                    stream_offsets: next_stream_offsets,
                })?)
            }
        } else {
            None
        };

        Ok(LokiLogPage { lines, next_cursor })
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
            LokiLogPage {
                lines: Vec::new(),
                next_cursor: None,
            }
        } else {
            self.query_page(&deployment, &project, &query).await?
        };
        let selector = self.selector(
            &deployment,
            &project,
            &query.levels,
            query.search.as_deref(),
            &query.containers,
        );
        let url = websocket_url(&self.tail_url, &selector);
        let tenant_id = self.tenant_id.clone();
        let bearer_token = self.bearer_token.clone();
        let container_label = self.labels.container.clone();

        let stream = async_stream::try_stream! {
            let backlog_count = initial.lines.len();
            for line in initial.lines {
                let level = line.classified_level();
                yield LogEvent::Line {
                    id: line.id.clone(),
                    text: line.render(query.timestamps),
                    level,
                    container: line.container.clone(),
                };
            }
            yield LogEvent::BacklogLoaded { count: backlog_count, next_cursor: initial.next_cursor };

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
            let mut seen_ids = HashMap::new();
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
                            let stream_container = stream.stream.get(&container_label).cloned();
                            let stream_key = canonical_loki_stream(&stream.stream);
                            for (stream_order, value) in stream.values.into_iter().enumerate() {
                                if let Some(mut line) = LogLine::from_loki_value(value, stream_level.as_deref(), stream_container.clone(), &stream_key, stream_order) {
                                    line.id = distinct_log_id(&mut seen_ids, line.id);
                                    let level = line.classified_level();
                                    yield LogEvent::Line {
                                        id: line.id.clone(),
                                        text: line.render(query.timestamps),
                                        level,
                                        container: line.container.clone(),
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

        let page = self.query_page(deployment, project, &query).await?;
        let timestamps = query.timestamps;
        let mut events = page
            .lines
            .into_iter()
            .map(move |line| {
                let level = line.classified_level();
                Ok(LogEvent::Line {
                    id: line.id.clone(),
                    text: line.render(timestamps),
                    level,
                    container: line.container.clone(),
                })
            })
            .collect::<Vec<_>>();
        if events.is_empty() {
            events.push(Ok(LogEvent::Status(self.empty_status(deployment))));
        }
        events.push(Ok(LogEvent::PageLoaded {
            next_cursor: page.next_cursor,
        }));
        Ok(futures::stream::iter(events).boxed())
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

        let base = self.base_selector(deployment, project, &query.containers);
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

fn canonical_loki_stream(stream: &HashMap<String, String>) -> String {
    let mut labels = stream
        .iter()
        .filter(|(key, _)| key.as_str() != "detected_level")
        .collect::<Vec<_>>();
    labels.sort_by_key(|(key, _)| *key);
    labels
        .into_iter()
        .map(|(key, value)| format!("{}={}", key, value))
        .collect::<Vec<_>>()
        .join("\0")
}

#[derive(Debug)]
struct LogLine {
    id: String,
    timestamp_nanos: i128,
    stream_key: String,
    /// Zero is the newest entry returned for this stream by a BACKWARD query.
    stream_order: usize,
    line: String,
    /// `detected_level` extracted from the Loki entry's structured metadata
    /// (Loki 3.x query_range / tail responses emit a third array element
    /// alongside `[ts, line]` carrying structured metadata key/value pairs).
    /// `None` means the entry didn't include a `detected_level`; the line is
    /// then classified by regex (matching the K8s backend's behavior).
    detected_level: Option<String>,
    /// Deployment container the stream is labelled with, when the shipper
    /// emits the configured container label.
    container: Option<String>,
}

impl LogLine {
    fn from_loki_value(
        value: LokiValue,
        stream_level: Option<&str>,
        container: Option<String>,
        stream_key: &str,
        stream_order: usize,
    ) -> Option<Self> {
        let LokiValue {
            timestamp,
            line,
            structured_metadata,
        } = value;
        let timestamp_nanos: i128 = timestamp.parse().ok()?;
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
        let timestamp_bytes = timestamp_nanos.to_be_bytes();
        let id = stable_log_id(
            "loki",
            [
                timestamp_bytes.as_slice(),
                stream_key.as_bytes(),
                line.as_bytes(),
            ],
        );
        Some(Self {
            id,
            timestamp_nanos,
            stream_key: stream_key.to_string(),
            stream_order,
            line,
            detected_level,
            container,
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

    fn render(&self, timestamps: bool) -> String {
        if !timestamps {
            return self.line.clone();
        }
        let ts = DateTime::<Utc>::from_timestamp_nanos(self.timestamp_nanos as i64);
        format!("{} {}", ts.to_rfc3339(), self.line)
    }
}

fn assign_distinct_loki_ids(lines: &mut [LogLine]) {
    lines.sort_by(|a, b| {
        a.timestamp_nanos
            .cmp(&b.timestamp_nanos)
            .then_with(|| a.stream_key.cmp(&b.stream_key))
            .then_with(|| a.stream_order.cmp(&b.stream_order))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut seen = HashMap::new();
    for line in lines {
        line.id = distinct_log_id(&mut seen, line.id.clone());
    }
}

fn sort_loki_lines(lines: &mut [LogLine]) {
    lines.sort_by(|a, b| {
        a.timestamp_nanos
            .cmp(&b.timestamp_nanos)
            .then_with(|| a.stream_key.cmp(&b.stream_key))
            .then_with(|| b.stream_order.cmp(&a.stream_order))
            .then_with(|| a.id.cmp(&b.id))
    });
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

/// Stream selector scoping a query to one deployment, optionally narrowed to
/// some of its containers.
///
/// {project, deployment_id} is enough to uniquely scope to a single
/// deployment's log stream — deployment_id is generated to be unique within a
/// project, and Rise enforces project-level authz upstream. The container
/// filter narrows within that scope, on the stream label the shipper
/// attributes each line with.
fn loki_base_selector(
    labels: &LokiLabels,
    project_name: &str,
    deployment_id: &str,
    containers: &[String],
) -> String {
    let container = match container_label_pattern(containers) {
        Some(pattern) => format!(",{}=~\"{}\"", labels.container, pattern),
        None => String::new(),
    };
    format!(
        "{{{}=\"{}\",{}=\"{}\"{}}}",
        labels.project,
        escape_logql_label_value(project_name),
        labels.deployment_id,
        escape_logql_label_value(deployment_id),
        container,
    )
}

/// Regex alternation over the requested container names, for a `=~` label
/// matcher. `None` when no container filter was requested. Each name is
/// regex-escaped so it matches literally.
fn container_label_pattern(containers: &[String]) -> Option<String> {
    let cleaned: Vec<String> = containers
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .map(regex::escape)
        .collect();
    (!cleaned.is_empty()).then(|| cleaned.join("|"))
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
    fn opaque_log_cursor_round_trips() {
        let encoded = encode_log_cursor(&TailLogCursor {
            version: 3,
            signature: "deployment-and-filters".into(),
            offset: 400,
            raw_tail: 500,
            start_nanos: Some(100),
            end_nanos: Some(200),
        })
        .expect("cursor encodes");
        assert!(!encoded.contains('{'));

        let decoded: TailLogCursor = decode_log_cursor(&encoded).expect("cursor decodes");
        assert_eq!(decoded.version, 3);
        assert_eq!(decoded.signature, "deployment-and-filters");
        assert_eq!(decoded.offset, 400);
        assert_eq!(decoded.raw_tail, 500);
        assert_eq!(decoded.start_nanos, Some(100));
        assert_eq!(decoded.end_nanos, Some(200));
    }

    #[test]
    fn bounded_kubernetes_page_selects_older_rows_after_newer_rows_are_filtered() {
        assert_eq!(kubernetes_tail_lines(true, false, 200), None);
        assert_eq!(kubernetes_tail_lines(false, false, 200), Some(200));
        let start = DateTime::parse_from_rfc3339("2026-08-29T12:00:00.500Z")
            .unwrap()
            .with_timezone(&Utc);
        let now = DateTime::parse_from_rfc3339("2026-08-29T12:00:10.900Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(kubernetes_since_seconds(now, start), Some(11));

        let end = 200usize;
        let raw = (0..2_200).collect::<Vec<_>>();
        let qualifying = raw
            .into_iter()
            .filter(|timestamp| *timestamp < end)
            .collect::<Vec<_>>();
        let (page, has_older) = select_recent_page(qualifying, 200, 0);
        assert_eq!(page, (0..200).collect::<Vec<_>>());
        assert!(!has_older);

        let (older_page, has_still_older) = select_recent_page((0..2_000).collect(), 200, 200);
        assert_eq!(older_page, (1_600..1_800).collect::<Vec<_>>());
        assert!(has_still_older);
    }

    #[test]
    fn finite_kubernetes_ids_stay_stable_when_the_tail_expands() {
        let duplicate = "same-timestamp-stream-and-content".to_string();
        let recent = vec![duplicate.clone(), duplicate.clone()];
        let expanded = vec![duplicate.clone(), duplicate.clone(), duplicate];

        let recent_ids = distinct_log_ids_from_newest(&recent);
        let expanded_ids = distinct_log_ids_from_newest(&expanded);
        assert_eq!(recent_ids, expanded_ids[1..]);
        assert_ne!(recent_ids[0], recent_ids[1]);
    }

    #[test]
    fn sparse_kubernetes_pages_advance_the_raw_window_without_matches() {
        let page_size = 200;
        let max_tail = 600;

        let first_raw_tail = next_kubernetes_raw_tail(None, page_size, max_tail);
        let (first, first_has_older) = select_recent_page(vec!["recent"], page_size, 0);
        assert_eq!(first, vec!["recent"]);
        assert!(kubernetes_page_has_more(
            first_has_older,
            false,
            Some(first_raw_tail as i64),
            first_raw_tail,
            first_raw_tail,
            max_tail,
        ));

        let second_raw_tail = next_kubernetes_raw_tail(Some(first_raw_tail), page_size, max_tail);
        let (second, second_has_older) = select_recent_page(vec!["recent"], page_size, first.len());
        assert!(second.is_empty());
        assert!(kubernetes_page_has_more(
            second_has_older,
            false,
            Some(second_raw_tail as i64),
            second_raw_tail,
            second_raw_tail,
            max_tail,
        ));

        let third_raw_tail = next_kubernetes_raw_tail(Some(second_raw_tail), page_size, max_tail);
        let (third, third_has_older) =
            select_recent_page(vec!["older", "recent"], page_size, first.len());
        assert_eq!(third, vec!["older"]);
        assert!(!kubernetes_page_has_more(
            third_has_older,
            false,
            Some(third_raw_tail as i64),
            third_raw_tail,
            third_raw_tail,
            max_tail,
        ));

        let first_cursor = encode_log_cursor(&TailLogCursor {
            version: 3,
            signature: "sparse".into(),
            offset: 1,
            raw_tail: first_raw_tail,
            start_nanos: None,
            end_nanos: None,
        })
        .unwrap();
        let second_cursor = encode_log_cursor(&TailLogCursor {
            version: 3,
            signature: "sparse".into(),
            offset: 1,
            raw_tail: second_raw_tail,
            start_nanos: None,
            end_nanos: None,
        })
        .unwrap();
        assert_ne!(first_cursor, second_cursor);
    }

    #[test]
    fn follow_and_finite_duplicate_ids_have_the_same_overlap_set() {
        use std::collections::HashSet;

        let base_ids = vec!["duplicate".to_string(); 3];
        let mut follow_seen = HashMap::new();
        let follow_ids = base_ids
            .iter()
            .cloned()
            .map(|base_id| distinct_log_id(&mut follow_seen, base_id))
            .collect::<HashSet<_>>();
        let finite_ids = distinct_log_ids_from_newest(&base_ids)
            .into_iter()
            .collect::<HashSet<_>>();

        assert_eq!(follow_ids, finite_ids);
        assert_eq!(follow_ids.len(), base_ids.len());
    }

    #[test]
    fn timestamped_line_identity_is_distinct_per_occurrence() {
        let line = "2026-08-29T12:34:56.123456789Z repeated";
        let (timestamp, content, timestamp_text) = split_timestamped_log_line(line).unwrap();
        assert_eq!(
            timestamp.timestamp_nanos_opt(),
            Some(1_788_006_896_123_456_789)
        );
        assert_eq!(content, "repeated");

        let base_id = stable_log_id(
            "kubernetes",
            [timestamp_text.as_bytes(), content.as_bytes()],
        );
        let mut seen = HashMap::new();
        let first = distinct_log_id(&mut seen, base_id.clone());
        let second = distinct_log_id(&mut seen, base_id.clone());
        assert_ne!(first, second);

        let mut retry = HashMap::new();
        assert_eq!(first, distinct_log_id(&mut retry, base_id.clone()));
        assert_eq!(second, distinct_log_id(&mut retry, base_id));
    }

    #[test]
    fn loki_continuation_window_spans_creation_to_the_cursor_boundary() {
        let created = DateTime::<Utc>::from_timestamp_nanos(1_000);
        let window = loki_continuation_window(created, 5_000).expect("boundary is after creation");
        assert_eq!(window.0, created);
        assert_eq!(window.1, DateTime::<Utc>::from_timestamp_nanos(5_000));
    }

    #[test]
    fn loki_continuation_window_rejects_a_boundary_at_or_before_creation() {
        let created = DateTime::<Utc>::from_timestamp_nanos(5_000);
        // Loki refuses an end at or before the start, so neither may be asked
        // for: a cursor minted against entries older than the deployment (or
        // one already walked back to its floor) is stale, not a server fault.
        assert!(loki_continuation_window(created, 5_000).is_none());
        assert!(loki_continuation_window(created, 1_000).is_none());
    }

    #[test]
    fn loki_cursor_advances_within_an_equal_timestamp_boundary() {
        let line = |timestamp_nanos, stream_key: &str, id: &str| LogLine {
            id: id.into(),
            timestamp_nanos,
            stream_key: stream_key.into(),
            stream_order: 0,
            line: id.into(),
            detected_level: None,
            container: None,
        };
        let first = select_loki_page(
            vec![
                line(1, "one", "a"),
                line(2, "one", "b"),
                line(2, "one", "c"),
                line(2, "two", "d"),
                line(3, "one", "e"),
            ],
            2,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(
            first
                .iter()
                .map(|line| line.id.as_str())
                .collect::<Vec<_>>(),
            vec!["d", "e"]
        );

        let second = select_loki_page(
            vec![
                line(1, "one", "a"),
                line(2, "one", "b"),
                line(2, "one", "c"),
                line(2, "two", "d"),
            ],
            2,
            Some(2),
            &BTreeMap::from([("two".into(), 1)]),
        );
        assert_eq!(
            second
                .iter()
                .map(|line| line.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }

    #[test]
    fn loki_boundary_offsets_do_not_skip_newly_visible_streams() {
        let line = |stream_key: &str, id: &str| LogLine {
            id: id.into(),
            timestamp_nanos: 2,
            stream_key: stream_key.into(),
            stream_order: 0,
            line: id.into(),
            detected_level: None,
            container: None,
        };
        let page = select_loki_page(
            vec![
                line("one", "a"),
                line("one", "b"),
                line("two", "c"),
                line("three", "d"),
            ],
            2,
            Some(2),
            &BTreeMap::from([("one".into(), 2)]),
        );
        assert_eq!(
            page.iter().map(|line| line.id.as_str()).collect::<Vec<_>>(),
            vec!["d", "c"]
        );
    }

    #[test]
    fn loki_boundary_offsets_keep_backend_order_within_one_stream() {
        let line = |stream_order, id: &str| LogLine {
            id: id.into(),
            timestamp_nanos: 2,
            stream_key: "one".into(),
            stream_order,
            line: id.into(),
            detected_level: None,
            container: None,
        };
        let mut expanded = vec![
            line(0, "newest"),
            line(1, "newer"),
            line(2, "older"),
            line(3, "oldest"),
        ];
        sort_loki_lines(&mut expanded);
        let page = select_loki_page(expanded, 2, Some(2), &BTreeMap::from([("one".into(), 2)]));
        assert_eq!(
            page.iter().map(|line| line.id.as_str()).collect::<Vec<_>>(),
            vec!["oldest", "older"]
        );
    }

    #[test]
    fn loki_boundary_scan_stays_within_the_public_page_cap() {
        let offsets = BTreeMap::from([("dense-stream".into(), 4_500)]);
        assert_eq!(loki_boundary_request_limit(1_000, &offsets), Some(5_000));

        let exhausted = BTreeMap::from([("dense-stream".into(), 5_000)]);
        assert_eq!(loki_boundary_request_limit(1, &exhausted), None);
    }

    #[test]
    fn duplicate_loki_entries_receive_stable_distinct_ids() {
        let value = || LokiValue {
            timestamp: "1000000000".into(),
            line: "same".into(),
            structured_metadata: None,
        };
        let mut first = vec![
            LogLine::from_loki_value(value(), None, None, "stream", 0).unwrap(),
            LogLine::from_loki_value(value(), None, None, "stream", 1).unwrap(),
        ];
        let mut retry = vec![
            LogLine::from_loki_value(value(), None, None, "stream", 0).unwrap(),
            LogLine::from_loki_value(value(), None, None, "stream", 1).unwrap(),
        ];
        assign_distinct_loki_ids(&mut first);
        assign_distinct_loki_ids(&mut retry);
        assert_ne!(first[0].id, first[1].id);
        assert_eq!(
            first.iter().map(|line| &line.id).collect::<Vec<_>>(),
            retry.iter().map(|line| &line.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn loki_event_identity_ignores_classification_label() {
        let mut live = HashMap::from([
            ("project".into(), "demo".into()),
            ("deployment_id".into(), "deploy-1".into()),
        ]);
        let without_level = canonical_loki_stream(&live);
        live.insert("detected_level".into(), "error".into());
        assert_eq!(canonical_loki_stream(&live), without_level);
    }

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
        let parsed = LogLine::from_loki_value(value, Some("warn"), None, "stream", 0)
            .expect("valid loki value");
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
        let parsed = LogLine::from_loki_value(value, Some("  "), None, "stream", 0)
            .expect("valid loki value");
        assert_eq!(parsed.classified_level(), "info");

        // Per-entry structured metadata wins over the stream-level label.
        let mut md = std::collections::HashMap::new();
        md.insert("detected_level".to_string(), "error".to_string());
        let value = LokiValue {
            timestamp: "1000000000".to_string(),
            line: "anything".to_string(),
            structured_metadata: Some(md),
        };
        let parsed = LogLine::from_loki_value(value, Some("warn"), None, "stream", 0)
            .expect("valid loki value");
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
            let parsed =
                LogLine::from_loki_value(value, None, None, "stream", 0).expect("valid loki value");
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
    fn loki_base_selector_narrows_to_the_requested_containers() {
        let labels = LokiLabels::default();
        assert_eq!(
            loki_base_selector(&labels, "shop", "20260830-101500", &[]),
            "{rise_project=\"shop\",rise_deployment_id=\"20260830-101500\"}"
        );
        assert_eq!(
            loki_base_selector(
                &labels,
                "shop",
                "20260830-101500",
                &["web".to_string(), "api".to_string()]
            ),
            "{rise_project=\"shop\",rise_deployment_id=\"20260830-101500\",\
             container=~\"web|api\"}"
        );
    }

    #[test]
    fn container_label_pattern_escapes_and_drops_blanks() {
        assert_eq!(container_label_pattern(&[]), None);
        assert_eq!(container_label_pattern(&["  ".to_string()]), None);
        // Regex metacharacters are matched literally, so a name can never
        // widen the selector past the container it names.
        assert_eq!(
            container_label_pattern(&["web".to_string(), "a.b".to_string()]),
            Some("web|a\\.b".to_string())
        );
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
                id: "id".into(),
                timestamp_nanos: 0,
                stream_key: "stream".into(),
                stream_order: 0,
                line: "anything".into(),
                detected_level: Some(raw.into()),
                container: None,
            };
            assert_eq!(l.classified_level(), raw);
        }

        // Whitespace is trimmed.
        let l = LogLine {
            id: "id".into(),
            timestamp_nanos: 0,
            stream_key: "stream".into(),
            stream_order: 0,
            line: "anything".into(),
            detected_level: Some("  warn  ".into()),
            container: None,
        };
        assert_eq!(l.classified_level(), "warn");

        // Loki's explicit "unknown" is still trusted verbatim — when Loki
        // says "I can't classify this", we don't second-guess.
        let l = LogLine {
            id: "id".into(),
            timestamp_nanos: 0,
            stream_key: "stream".into(),
            stream_order: 0,
            line: "ERROR something exploded".into(),
            detected_level: Some("unknown".into()),
            container: None,
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
                    id: "id".into(),
                    timestamp_nanos: 0,
                    stream_key: "stream".into(),
                    stream_order: 0,
                    line: line.into(),
                    detected_level: raw,
                    container: None,
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
