use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::db::models::{Deployment, DeploymentStatus, Project};
use crate::server::settings::{DeploymentLogsSettings, LokiLabels};
use rise_backend_core::logs::distinct_log_id;
pub use rise_backend_core::logs::{
    classify_log_line, decode_log_cursor, encode_log_cursor, is_followable_status,
    line_matches_search, log_cursor_signature, merge_container_streams, normalize_search,
    parse_duration_hint, split_timestamped_log_line, stable_log_id, status_stream,
    truncate_for_error, LogEvent, LogEventStream, LogQuery, LogStatus, LogStatusReason,
    LogVolumeBucket, LogVolumeQuery, LogVolumeResponse, LogsCapabilities, RuntimeLogBackend,
    TimestampedLineStream, DOCKER_MAX_TAIL, HEURISTIC_LEVELS, LOKI_LEVELS, LOKI_MAX_TAIL,
};

pub use rise_backend_ecs::logs::EcsCloudWatchContext;

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
            Ok(Arc::new(
                rise_backend_kubernetes::KubernetesLogBackend::new(kube_client, config.clone()),
            ))
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
                rise_backend_ecs::logs::CloudWatchLogBackend::new(
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
    /// Find every container of a deployment by its
    /// `<label_namespace>/deployment-id` label, scoped to the owning project,
    /// each paired with the deployment container name it runs. Running
    /// containers come first. `wanted` restricts the set; empty means "all".
    ///
    /// `deployment_id` is a `YYYYMMDD-HHMMSS` timestamp that is unique only
    /// *per project* (DB constraint `UNIQUE (deployment_id, project_id)`), so
    /// filtering on it alone could resolve to another project's container —
    /// a tenant-isolation breach. We therefore also scope by the `project`
    /// label (matching `project.name`, exactly as the reconciler stamps it)
    /// plus `managed-by=rise` for defense-in-depth, mirroring
    /// `list_actual_containers`.
    async fn resolve_containers(
        &self,
        deployment: &Deployment,
        project: &Project,
        wanted: &[String],
    ) -> Result<Vec<(String, Option<String>)>> {
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

        // Every match, not one: returning a single container would show one
        // container's output with no sign the others exist. Running first, so a
        // deployment mid-recreate leads with what is actually serving.
        let (running, stopped): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|c| c.state.as_deref() == Some("running"));
        Ok(running
            .into_iter()
            .chain(stopped)
            .filter_map(|summary| {
                let name = container_name(&summary);
                summary.id.clone().map(|id| (id, name))
            })
            .collect())
    }
}

#[async_trait]
impl RuntimeLogBackend for DockerLogBackend {
    fn backend_kind(&self) -> &'static str {
        "docker"
    }

    fn levels(&self) -> &'static [&'static str] {
        HEURISTIC_LEVELS
    }

    fn supports_volume(&self) -> bool {
        false
    }

    fn max_tail(&self) -> Option<i64> {
        Some(DOCKER_MAX_TAIL)
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

        let containers = self
            .resolve_containers(deployment, project, &query.containers)
            .await?;
        if containers.is_empty() {
            return Ok(status_stream(LogStatus {
                reason: LogStatusReason::HistoricalBackendNotConfigured,
                message: Some(
                    "No active deployment container was found and historical logs are not \
                     configured."
                        .into(),
                ),
                retention_hint: None,
            }));
        }

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

        let levels = query.levels.clone();
        let search = query.search.clone();
        let follow = query.follow && is_followable_status(&deployment.status);
        let render_timestamps = query.timestamps;
        let start_time = query.start_time;
        let end_time = query.end_time;
        let tail_limit = query.tail_lines.map(|t| t.max(1) as usize);

        // One stream per container: the Engine API has no multi-container logs
        // endpoint, so the fan-out and the merge are ours to do — exactly what
        // `docker compose logs` does for the same reason.
        let per_container: Vec<TimestampedLineStream> = containers
            .into_iter()
            .map(|(container_id, container)| {
                let log_stream = self.docker.logs(&container_id, Some(options.clone()));
                let levels = levels.clone();
                let search = search.clone();
                async_stream::stream! {
                    let mut seen_ids = HashMap::new();
                    futures::pin_mut!(log_stream);
                    while let Some(item) = log_stream.next().await {
                        let output = match item {
                            Ok(output) => output,
                            Err(e) => {
                                // One container failing is not the deployment
                                // failing. Name it and stop this stream; the
                                // others keep going.
                                yield Err(anyhow::anyhow!(
                                    "Docker log stream error for container {}: {}",
                                    container.as_deref().unwrap_or("unknown"),
                                    e
                                ));
                                break;
                            }
                        };
                        let raw = output.to_string();
                        for line in raw.split('\n') {
                            let line = line.trim_end_matches('\r');
                            if line.is_empty() {
                                continue;
                            }
                            let Some((timestamp, content, timestamp_text)) =
                                split_timestamped_log_line(line)
                            else {
                                continue;
                            };
                            if content.is_empty() {
                                continue;
                            }
                            if start_time.is_some_and(|start| timestamp < start)
                                || end_time.is_some_and(|end| timestamp >= end)
                            {
                                continue;
                            }
                            let level = classify_log_line(content);
                            if !levels.is_empty() && !levels.iter().any(|l| l == level) {
                                continue;
                            }
                            if !line_matches_search(content, search.as_deref()) {
                                continue;
                            }
                            let base_id = stable_log_id(
                                "docker",
                                [
                                    container_id.as_bytes(),
                                    timestamp_text.as_bytes(),
                                    content.as_bytes(),
                                ],
                            );
                            let id = distinct_log_id(&mut seen_ids, base_id);
                            yield Ok((
                                timestamp,
                                LogEvent::Line {
                                    id,
                                    text: if render_timestamps {
                                        line.to_string()
                                    } else {
                                        content.to_string()
                                    },
                                    level: level.to_string(),
                                    container: container.clone(),
                                },
                            ));
                        }
                    }
                }
                .boxed()
            })
            .collect();

        Ok(merge_container_streams(per_container, follow, tail_limit))
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
        //     `HEURISTIC_LEVELS` — a subset of Loki's vocabulary, safe
        //     to render with the same palette.
        match self.detected_level.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => raw.to_string(),
            _ => classify_log_line(&self.line).to_string(),
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

#[cfg(test)]
mod tests {

    fn line_at(secs: i64, container: &str, text: &str) -> (DateTime<Utc>, LogEvent) {
        (
            DateTime::from_timestamp(secs, 0).unwrap(),
            LogEvent::Line {
                id: format!("{container}-{secs}-{text}"),
                text: text.to_string(),
                level: "info".to_string(),
                container: Some(container.to_string()),
            },
        )
    }

    fn stream_of(lines: Vec<(DateTime<Utc>, LogEvent)>) -> TimestampedLineStream {
        futures::stream::iter(lines.into_iter().map(Ok)).boxed()
    }

    async fn drain(stream: LogEventStream) -> Vec<LogEvent> {
        stream
            .filter_map(|item| async move { item.ok() })
            .collect()
            .await
    }

    fn texts(events: &[LogEvent]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                LogEvent::Line {
                    container, text, ..
                } => Some((container.clone().unwrap_or_default(), text.clone())),
                _ => None,
            })
            .collect()
    }

    /// The whole point: a deployment's containers each have their own stream,
    /// and a range query returns all of them in the order things happened.
    #[tokio::test]
    async fn a_range_query_merges_every_container_in_time_order() {
        let web = stream_of(vec![line_at(10, "web", "w1"), line_at(30, "web", "w2")]);
        let worker = stream_of(vec![
            line_at(20, "worker", "k1"),
            line_at(40, "worker", "k2"),
        ]);

        let events = drain(merge_container_streams(vec![web, worker], false, None)).await;

        assert_eq!(
            texts(&events),
            vec![
                ("web".to_string(), "w1".to_string()),
                ("worker".to_string(), "k1".to_string()),
                ("web".to_string(), "w2".to_string()),
                ("worker".to_string(), "k2".to_string()),
            ],
        );
    }

    /// Exactly one `PageLoaded`, however many containers were merged — it marks
    /// the end of the page, not the end of a stream.
    #[tokio::test]
    async fn a_merged_range_reports_one_page_end() {
        let a = stream_of(vec![line_at(10, "web", "w")]);
        let b = stream_of(vec![line_at(20, "worker", "k")]);

        let events = drain(merge_container_streams(vec![a, b], false, None)).await;

        let ends = events
            .iter()
            .filter(|e| matches!(e, LogEvent::PageLoaded { .. }))
            .count();
        assert_eq!(ends, 1);
        assert!(matches!(events.last(), Some(LogEvent::PageLoaded { .. })));
    }

    /// `tail` asks for N lines from the deployment, but each container's stream
    /// was asked for N of its own. Without trimming after the merge, a
    /// two-container deployment returns twice what was requested — which is why
    /// an unfiltered view and a filtered one could both report the same count.
    #[tokio::test]
    async fn tail_bounds_the_deployment_not_each_container() {
        let web = stream_of(vec![
            line_at(10, "web", "w1"),
            line_at(30, "web", "w2"),
            line_at(50, "web", "w3"),
        ]);
        let worker = stream_of(vec![
            line_at(20, "worker", "k1"),
            line_at(40, "worker", "k2"),
            line_at(60, "worker", "k3"),
        ]);

        let events = drain(merge_container_streams(vec![web, worker], false, Some(3))).await;

        // The newest three across both — k2(40), w3(50), k3(60) — not three
        // from each.
        assert_eq!(
            texts(&events),
            vec![
                ("worker".to_string(), "k2".to_string()),
                ("web".to_string(), "w3".to_string()),
                ("worker".to_string(), "k3".to_string()),
            ],
        );
    }

    /// One container failing is not the deployment failing: its error surfaces
    /// and the others' lines still arrive.
    #[tokio::test]
    async fn a_failing_container_does_not_take_the_others_down() {
        let good = stream_of(vec![line_at(10, "web", "w1"), line_at(30, "web", "w2")]);
        let bad: TimestampedLineStream =
            futures::stream::iter(vec![Err(anyhow::anyhow!("boom"))]).boxed();

        let merged = merge_container_streams(vec![good, bad], false, None);
        let all: Vec<_> = merged.collect().await;

        assert_eq!(all.iter().filter(|r| r.is_err()).count(), 1);
        let ok: Vec<LogEvent> = all.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(texts(&ok).len(), 2, "the healthy container still reported");
    }

    /// Following cannot be globally ordered without buffering, and buffering a
    /// follow withholds the output that is the point of it. Every line still
    /// arrives, and each names its container so a reader can reassemble them.
    #[tokio::test]
    async fn following_emits_everything_without_waiting_to_sort() {
        let web = stream_of(vec![line_at(30, "web", "w")]);
        let worker = stream_of(vec![line_at(10, "worker", "k")]);

        let events = drain(merge_container_streams(vec![web, worker], true, None)).await;

        let mut seen = texts(&events);
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("web".to_string(), "w".to_string()),
                ("worker".to_string(), "k".to_string()),
            ],
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LogEvent::PageLoaded { .. })),
            "a follow has no page to end",
        );
    }
    use super::*;

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
