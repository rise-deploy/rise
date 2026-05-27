use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::db::models::{Deployment, DeploymentStatus, Project};
use crate::server::deployment::resource_builder::ResourceBuilder;
use crate::server::settings::DeploymentLogsSettings;

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
}

#[derive(Debug, Clone)]
pub struct LogCountsQuery {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub step_seconds: i64,
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
    resource_builder: Option<Arc<ResourceBuilder>>,
) -> Result<Arc<dyn RuntimeLogBackend>> {
    match settings {
        DeploymentLogsSettings::Kubernetes => {
            let kube_client = kube_client.context("Kubernetes log backend requires kube client")?;
            let resource_builder =
                resource_builder.context("Kubernetes log backend requires resource builder")?;
            Ok(Arc::new(KubernetesLogBackend {
                kube_client,
                resource_builder,
            }))
        }
        DeploymentLogsSettings::Loki {
            url,
            tenant_id,
            bearer_token_env,
            timeout_secs,
            retention_hint,
        } => {
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
            )?))
        }
    }
}

struct KubernetesLogBackend {
    kube_client: kube::Client,
    resource_builder: Arc<ResourceBuilder>,
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
        use futures::AsyncReadExt;
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

        let namespace = self.resource_builder.namespace_name(project);
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

        let mut log_params = LogParams {
            follow: query.follow && is_followable_status(&deployment.status),
            timestamps: query.timestamps,
            ..Default::default()
        };
        if let Some(tail) = query.tail_lines {
            log_params.tail_lines = Some(tail);
        }
        if let Some(since) = query.since_seconds {
            log_params.since_seconds = Some(since);
        } else if let Some(start_time) = query.start_time {
            log_params.since_seconds = Some((Utc::now() - start_time).num_seconds().max(1));
        }

        let mut log_stream = pod_api.log_stream(&pod_name, &log_params).await?;
        let level = query.level;
        let stream = async_stream::stream! {
            let mut buffer = vec![0u8; 8192];
            loop {
                match log_stream.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(n) => {
                        for line in String::from_utf8_lossy(&buffer[..n]).lines() {
                            if line.is_empty() { continue; }
                            if !line_matches_level(line, level) { continue; }
                            yield Ok(LogEvent::Line(line.to_string()));
                        }
                    }
                    Err(e) => {
                        yield Err(anyhow::anyhow!("Log stream error: {}", e));
                        break;
                    }
                }
            }
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
                    "Historical log counts require the Loki backend to be enabled.".into(),
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
    http_client: reqwest::Client,
}

impl LokiLogBackend {
    fn new(
        url: String,
        tenant_id: Option<String>,
        bearer_token: Option<String>,
        timeout_secs: u64,
        retention_hint: Option<String>,
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
            http_client,
        })
    }

    fn base_selector(&self, deployment: &Deployment, project: &Project) -> String {
        format!(
            "{{rise_project=\"{}\",rise_deployment_id=\"{}\",rise_deployment_uuid=\"{}\"}}",
            escape_logql_label_value(&project.name),
            escape_logql_label_value(&deployment.deployment_id),
            deployment.id
        )
    }

    fn selector(
        &self,
        deployment: &Deployment,
        project: &Project,
        level: LogLevelFilter,
    ) -> String {
        let base = self.base_selector(deployment, project);
        level_filtered_selector(&base, level)
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
        let selector = self.selector(deployment, project, query.level);
        let end = self.effective_end_time(query);
        let start = self.effective_start_time(deployment, query);
        let tail = if query.follow {
            query.tail_lines.unwrap_or(1)
        } else {
            query.tail_lines.unwrap_or(1000)
        };

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
        let initial = self.query_range(&deployment, &project, &query).await?;
        let selector = self.selector(&deployment, &project, query.level);
        let url = websocket_url(&self.tail_url, &selector);
        let tenant_id = self.tenant_id.clone();
        let bearer_token = self.bearer_token.clone();

        let stream = async_stream::try_stream! {
            for line in initial {
                yield LogEvent::Line(line.render(query.timestamps));
            }

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
            let (_write, mut read) = ws_stream.split();

            while let Some(message) = read.next().await {
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
        };

        Ok(stream.boxed())
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
        while current <= end_time {
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

        let base = self.base_selector(deployment, project);
        let range = format!("[{}s]", query.step_seconds.max(1));
        let total_query = format!("sum(count_over_time({base}{range}))");
        let warn_selector = level_filtered_selector(&base, LogLevelFilter::Warn);
        let error_selector = level_filtered_selector(&base, LogLevelFilter::Error);
        let info_selector = level_filtered_selector(&base, LogLevelFilter::Info);
        let warn_query = format!("sum(count_over_time(({warn_selector}){range}))");
        let error_query = format!("sum(count_over_time(({error_selector}){range}))");
        let info_query = format!("sum(count_over_time(({info_selector}){range}))");

        let (total, warn, error, info) = futures::try_join!(
            self.query_counts_series(
                total_query,
                query.start_time,
                query.end_time,
                query.step_seconds,
            ),
            self.query_counts_series(
                warn_query,
                query.start_time,
                query.end_time,
                query.step_seconds,
            ),
            self.query_counts_series(
                error_query,
                query.start_time,
                query.end_time,
                query.step_seconds,
            ),
            self.query_counts_series(
                info_query,
                query.start_time,
                query.end_time,
                query.step_seconds,
            ),
        )?;

        let is_empty = total.is_empty() && warn.is_empty() && error.is_empty() && info.is_empty();
        let buckets = Self::build_count_buckets(
            total,
            info,
            warn,
            error,
            query.start_time,
            query.end_time,
            query.step_seconds,
        );

        Ok(LogCountsResponse {
            status: if is_empty {
                Some(self.empty_status(deployment))
            } else {
                None
            },
            start_time: query.start_time.to_rfc3339(),
            end_time: query.end_time.to_rfc3339(),
            step_seconds: query.step_seconds,
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

#[derive(Deserialize, Clone)]
struct MetricSample(f64, String);

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
            .map(|MetricSample(ts_seconds, count_str)| {
                let timestamp_nanos = (ts_seconds * 1_000_000_000.0) as i64;
                let count = count_str
                    .parse::<f64>()
                    .ok()
                    .map(|v| v.round().max(0.0) as u64)
                    .unwrap_or(0);
                CountPoint {
                    timestamp_nanos,
                    count,
                }
            })
            .collect()
    }
}

fn escape_logql_label_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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
