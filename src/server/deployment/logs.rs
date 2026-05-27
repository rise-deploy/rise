use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::db::models::{Deployment, DeploymentStatus, Project};
use crate::server::deployment::resource_builder::ResourceBuilder;
use crate::server::settings::{DeploymentLogsSettings, LokiLabels};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevelFilter {
    All,
    Info,
    Warn,
    Error,
}

impl LogLevelFilter {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.to_ascii_lowercase()).as_deref() {
            Some("info") => Self::Info,
            Some("warn") => Self::Warn,
            Some("error") => Self::Error,
            _ => Self::All,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogQuery {
    pub follow: bool,
    pub tail_lines: Option<i64>,
    pub timestamps: bool,
    pub since_seconds: Option<i64>,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub level: LogLevelFilter,
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
pub struct LogCountsQuery {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub step_seconds: i64,
    pub level: LogLevelFilter,
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
pub struct LogCountsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<LogStatus>,
    pub start_time: String,
    pub end_time: String,
    pub step_seconds: i64,
    pub buckets: Vec<LogCountBucket>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogCountBucket {
    pub timestamp: String,
    pub total: u64,
    pub info: u64,
    pub warn: u64,
    pub error: u64,
}

#[derive(Debug, Clone)]
pub enum LogEvent {
    Line(String),
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

#[async_trait]
pub trait RuntimeLogBackend: Send + Sync {
    async fn stream_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogQuery,
    ) -> Result<LogEventStream>;

    async fn count_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogCountsQuery,
    ) -> Result<LogCountsResponse>;
}

pub async fn init_runtime_log_backend(
    settings: &DeploymentLogsSettings,
    kube_client: Option<kube::Client>,
) -> Result<Arc<dyn RuntimeLogBackend>> {
    match settings {
        DeploymentLogsSettings::Kubernetes => {
            let kube_client = kube_client.context("Kubernetes log backend requires kube client")?;
            Ok(Arc::new(KubernetesLogBackend { kube_client }))
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
            let bearer_token = bearer_token_env
                .as_ref()
                .map(|name| {
                    std::env::var(name)
                        .with_context(|| format!("Loki bearer_token_env '{}' is not set", name))
                })
                .transpose()?;
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
        let effective_tail = query
            .tail_lines
            .map(|t| t.saturating_add(skip_recent).max(1));

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
            // The Kubernetes pods/log API only supports a since-anchored window;
            // a future-dated start (and any explicit end_time) cannot be honored.
            let delta = (Utc::now() - start_time).num_seconds();
            if delta > 0 {
                log_params.since_seconds = Some(delta);
            }
        }

        let log_stream = pod_api.log_stream(&pod_name, &log_params).await?;
        let level = query.level;
        let search = query.search.clone();
        let stream = async_stream::stream! {
            use futures::AsyncBufReadExt;
            use std::collections::VecDeque;
            let skip = skip_recent as usize;
            // Buffer the trailing `skip` qualifying lines so we can drop them
            // once the stream ends. While the buffer is full, evict the oldest
            // and yield it — that's a line the frontend doesn't already have.
            let mut trailing: VecDeque<String> = VecDeque::with_capacity(skip.saturating_add(1));
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
                if !line_matches_level(&line, level) { continue; }
                if !line_matches_search(&line, search.as_deref()) { continue; }
                if skip == 0 {
                    yield Ok(LogEvent::Line(line));
                } else {
                    trailing.push_back(line);
                    if trailing.len() > skip {
                        if let Some(out) = trailing.pop_front() {
                            yield Ok(LogEvent::Line(out));
                        }
                    }
                }
            }
            // Anything left in the buffer is in the trailing `skip` window and
            // intentionally dropped — those are the lines the frontend already has.
        };

        Ok(stream.boxed())
    }

    async fn count_logs(
        &self,
        _deployment: &Deployment,
        _project: &Project,
        query: LogCountsQuery,
    ) -> Result<LogCountsResponse> {
        Ok(LogCountsResponse {
            status: Some(LogStatus {
                reason: LogStatusReason::HistoricalBackendNotConfigured,
                message: Some(
                    "Historical log counts aren't supported by the configured log backend.".into(),
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

fn status_stream(status: LogStatus) -> LogEventStream {
    futures::stream::once(async move { Ok(LogEvent::Status(status)) }).boxed()
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
        level: LogLevelFilter,
        search: Option<&str>,
    ) -> String {
        let base = self.base_selector(deployment, project);
        let with_level = level_filtered_selector(&base, level);
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
        let selector = self.selector(deployment, project, query.level, query.search.as_deref());
        let end = self.effective_end_time(query);
        let start = self.effective_start_time(deployment, query);
        let tail = if query.follow {
            query.tail_lines.unwrap_or(1)
        } else {
            query.tail_lines.unwrap_or(1000)
        };
        let tail = tail.clamp(1, 5000);

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
            anyhow::bail!("Loki query failed ({}): {}", status, body);
        }

        let body: LokiQueryResponse = response.json().await.context("Invalid Loki response")?;
        let mut lines = body
            .data
            .result
            .into_iter()
            .flat_map(|stream| stream.values.into_iter())
            .filter_map(LogLine::from_loki_value)
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
        let selector = self.selector(&deployment, &project, query.level, query.search.as_deref());
        let url = websocket_url(&self.tail_url, &selector);
        let tenant_id = self.tenant_id.clone();
        let bearer_token = self.bearer_token.clone();

        let stream = async_stream::try_stream! {
            let backlog_count = initial.len();
            for line in initial {
                yield LogEvent::Line(line.render(query.timestamps));
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
                            for value in stream.values {
                                if let Some(line) = LogLine::from_loki_value(value) {
                                    yield LogEvent::Line(line.render(query.timestamps));
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
        };

        Ok(stream.boxed())
    }

    async fn query_counts_or_empty(
        &self,
        query: Option<String>,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        step_seconds: i64,
    ) -> Result<BTreeMap<i64, u64>> {
        match query {
            Some(q) => {
                self.query_counts_series(q, start_time, end_time, step_seconds)
                    .await
            }
            None => Ok(BTreeMap::new()),
        }
    }

    async fn query_counts_series(
        &self,
        query: String,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        step_seconds: i64,
    ) -> Result<BTreeMap<i64, u64>> {
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
            anyhow::bail!("Loki counts query failed ({}): {}", status, body);
        }

        let body: LokiMetricQueryResponse = response
            .json()
            .await
            .context("Invalid Loki counts response")?;

        let mut counts = BTreeMap::new();
        for series in body.data.result {
            for point in series.points() {
                counts
                    .entry(point.timestamp_nanos)
                    .and_modify(|count| *count += point.count)
                    .or_insert(point.count);
            }
        }
        Ok(counts)
    }

    fn build_count_buckets(
        total: BTreeMap<i64, u64>,
        info: BTreeMap<i64, u64>,
        warn: BTreeMap<i64, u64>,
        error: BTreeMap<i64, u64>,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        step_seconds: i64,
    ) -> Vec<LogCountBucket> {
        // Emit a contiguous row of buckets spanning [start_time, end_time] at
        // step_seconds intervals. For each bucket we sum counts whose timestamp
        // falls in [bucket_start, bucket_start + step). Loki returns timestamps
        // that are already aligned to the step boundary, so an exact-match lookup
        // is sufficient when present, but we also accept any data point that
        // falls inside the bucket window — defensive against slight drift.
        let step = Duration::seconds(step_seconds.max(1));
        let step_nanos = step.num_nanoseconds().unwrap_or(i64::MAX);

        let lookup = |source: &BTreeMap<i64, u64>, bucket_nanos: i64| -> u64 {
            // Common case: Loki point lands exactly on the bucket boundary.
            if let Some(v) = source.get(&bucket_nanos) {
                return *v;
            }
            // Fallback: any point within [bucket, bucket + step).
            source
                .range(bucket_nanos..bucket_nanos.saturating_add(step_nanos))
                .map(|(_, v)| *v)
                .sum()
        };

        let mut buckets = Vec::new();
        let mut current = start_time;
        // Half-open: each bucket represents [current, current + step), so we stop
        // once `current` reaches the aligned end — including it would emit one
        // extra empty bucket past the requested range.
        while current < end_time {
            let bucket_nanos = current.timestamp_nanos_opt().unwrap_or_default();
            buckets.push(LogCountBucket {
                timestamp: current.to_rfc3339(),
                total: lookup(&total, bucket_nanos),
                info: lookup(&info, bucket_nanos),
                warn: lookup(&warn, bucket_nanos),
                error: lookup(&error, bucket_nanos),
            });
            current += step;
        }
        buckets
    }
}

#[async_trait]
impl RuntimeLogBackend for LokiLogBackend {
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
        Ok(futures::stream::iter(
            lines
                .into_iter()
                .map(move |line| Ok(LogEvent::Line(line.render(timestamps)))),
        )
        .boxed())
    }

    async fn count_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogCountsQuery,
    ) -> Result<LogCountsResponse> {
        if matches!(
            deployment.status,
            DeploymentStatus::Pending
                | DeploymentStatus::Building
                | DeploymentStatus::Pushing
                | DeploymentStatus::Pushed
        ) {
            return Ok(LogCountsResponse {
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
        // The chart should reflect the same filters as the log list. We layer
        // the search filter onto each per-level sub-query and skip levels that
        // the user filtered out (their bars would always read zero).
        let search = query.search.as_deref();
        let total_selector = level_filtered_selector(&base, query.level);
        let total_selector = append_search_filter(&total_selector, search);
        let total_query = format!("sum(count_over_time(({total_selector}){range}))");
        let build_segment = |level: LogLevelFilter| -> Option<String> {
            if query.level != LogLevelFilter::All && query.level != level {
                return None;
            }
            let selector = level_filtered_selector(&base, level);
            let selector = append_search_filter(&selector, search);
            Some(format!("sum(count_over_time(({selector}){range}))"))
        };
        let warn_query = build_segment(LogLevelFilter::Warn);
        let error_query = build_segment(LogLevelFilter::Error);
        let info_query = build_segment(LogLevelFilter::Info);

        let (total, warn, error, info) = futures::try_join!(
            self.query_counts_series(total_query, aligned_start, aligned_end, step_seconds,),
            self.query_counts_or_empty(warn_query, aligned_start, aligned_end, step_seconds,),
            self.query_counts_or_empty(error_query, aligned_start, aligned_end, step_seconds,),
            self.query_counts_or_empty(info_query, aligned_start, aligned_end, step_seconds,),
        )?;

        let is_empty = total.is_empty() && warn.is_empty() && error.is_empty() && info.is_empty();
        let buckets = Self::build_count_buckets(
            total,
            info,
            warn,
            error,
            aligned_start,
            aligned_end,
            step_seconds,
        );

        Ok(LogCountsResponse {
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
}

impl LogLine {
    fn from_loki_value(value: Vec<String>) -> Option<Self> {
        if value.len() != 2 {
            return None;
        }
        let timestamp_nanos = value[0].parse().ok()?;
        Some(Self {
            timestamp_nanos,
            line: value[1].clone(),
        })
    }

    fn render(self, timestamps: bool) -> String {
        if !timestamps {
            return self.line;
        }
        let ts = DateTime::<Utc>::from_timestamp_nanos(self.timestamp_nanos as i64);
        format!("{} {}", ts.to_rfc3339(), self.line)
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
    values: Vec<Vec<String>>,
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

// LogQL filter expressions for each level. The same regex set is used for the
// log counts chart so the chart segments stay in sync with the filtered view.
const LEVEL_REGEX_ERROR: &str = r"(?i)\b(error|err|fatal|panic|exception|failed)\b";
const LEVEL_REGEX_WARN: &str = r"(?i)\b(warn|warning)\b";
const LEVEL_REGEX_INFO_EXCLUDE: &str =
    r"(?i)\b(error|err|fatal|panic|exception|failed|warn|warning)\b";

fn level_filtered_selector(base: &str, level: LogLevelFilter) -> String {
    match level {
        LogLevelFilter::All => base.to_string(),
        LogLevelFilter::Error => format!("{base} |~ `{LEVEL_REGEX_ERROR}`"),
        LogLevelFilter::Warn => {
            format!("{base} |~ `{LEVEL_REGEX_WARN}` !~ `{LEVEL_REGEX_ERROR}`")
        }
        LogLevelFilter::Info => format!("{base} !~ `{LEVEL_REGEX_INFO_EXCLUDE}`"),
    }
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

/// Match a raw log line against the same regex set used by the LogQL filters.
/// Used by the Kubernetes log backend which has to filter line-by-line.
pub(crate) fn line_matches_level(line: &str, level: LogLevelFilter) -> bool {
    use std::sync::OnceLock;
    static ERROR_RE: OnceLock<regex::Regex> = OnceLock::new();
    static WARN_RE: OnceLock<regex::Regex> = OnceLock::new();
    let err = ERROR_RE.get_or_init(|| regex::Regex::new(LEVEL_REGEX_ERROR).unwrap());
    let warn = WARN_RE.get_or_init(|| regex::Regex::new(LEVEL_REGEX_WARN).unwrap());
    match level {
        LogLevelFilter::All => true,
        LogLevelFilter::Error => err.is_match(line),
        LogLevelFilter::Warn => warn.is_match(line) && !err.is_match(line),
        LogLevelFilter::Info => !err.is_match(line) && !warn.is_match(line),
    }
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

/// Parse a short retention hint like `"7d"`. Supported units: `s`, `m`, `h`, `d`.
fn parse_duration_hint(value: &str) -> Option<Duration> {
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
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_count_buckets_is_half_open_at_end() {
        // [0s, 300s) at step=60s should yield exactly 5 buckets at 0, 60, 120,
        // 180, 240 — not 6. The aligned-end bucket itself represents [300, 360)
        // and would be empty by construction; emitting it pads every chart.
        let start = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let end = DateTime::<Utc>::from_timestamp(300, 0).unwrap();
        let buckets = LokiLogBackend::build_count_buckets(
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            start,
            end,
            60,
        );
        assert_eq!(buckets.len(), 5);
    }

    #[test]
    fn build_count_buckets_sums_samples_into_their_bucket() {
        // A Loki point at the bucket boundary belongs to that bucket (exact-match
        // path). A point inside the bucket window also counts; nothing should
        // double-count into the next bucket.
        let step = 60;
        let start = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let end = DateTime::<Utc>::from_timestamp(180, 0).unwrap();
        let mut total = BTreeMap::new();
        total.insert(0i64, 3);
        total.insert(60i64 * 1_000_000_000, 7);
        let buckets = LokiLogBackend::build_count_buckets(
            total,
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            start,
            end,
            step,
        );
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].total, 3);
        assert_eq!(buckets[1].total, 7);
        assert_eq!(buckets[2].total, 0);
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
