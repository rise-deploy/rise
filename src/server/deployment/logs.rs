use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::db::models::{Deployment, DeploymentStatus, Project};
use crate::server::deployment::resource_builder::ResourceBuilder;
use crate::server::settings::DeploymentLogsSettings;

#[derive(Debug, Clone)]
pub struct LogQuery {
    pub follow: bool,
    pub tail_lines: Option<i64>,
    pub timestamps: bool,
    pub since_seconds: Option<i64>,
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
                message: Some("Deployment is not ready yet - no runtime logs are available.".into()),
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
            follow: query.follow,
            timestamps: query.timestamps,
            ..Default::default()
        };
        if let Some(tail) = query.tail_lines {
            log_params.tail_lines = Some(tail);
        }
        if let Some(since) = query.since_seconds {
            log_params.since_seconds = Some(since);
        }

        let mut log_stream = pod_api.log_stream(&pod_name, &log_params).await?;
        let stream = async_stream::stream! {
            let mut buffer = vec![0u8; 8192];
            loop {
                match log_stream.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(n) => {
                        for line in String::from_utf8_lossy(&buffer[..n]).lines() {
                            if !line.is_empty() {
                                yield Ok(LogEvent::Line(line.to_string()));
                            }
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

    fn selector(&self, deployment: &Deployment, project: &Project) -> String {
        format!(
            "{{rise_project=\"{}\",rise_deployment_id=\"{}\",rise_deployment_uuid=\"{}\"}}",
            escape_logql_label_value(&project.name),
            escape_logql_label_value(&deployment.deployment_id),
            deployment.id
        )
    }

    async fn query_range(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: &LogQuery,
    ) -> Result<Vec<LogLine>> {
        let selector = self.selector(deployment, project);
        let now = Utc::now();
        let start = query
            .since_seconds
            .map(|s| now - Duration::seconds(s))
            .unwrap_or(deployment.created_at);

        let url = format!(
            "{}?query={}&direction=BACKWARD&start={}&end={}&limit={}",
            self.query_url,
            urlencoding::encode(&selector),
            to_loki_nanos(start),
            to_loki_nanos(now),
            query.tail_lines.unwrap_or(1000).max(1)
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
        let selector = self.selector(&deployment, &project);
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
}

#[async_trait]
impl RuntimeLogBackend for LokiLogBackend {
    async fn stream_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogQuery,
    ) -> Result<LogEventStream> {
        if query.follow {
            return self
                .tail_stream(deployment.clone(), project.clone(), query)
                .await;
        }

        let lines = self.query_range(deployment, project, &query).await?;
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
        match DateTime::<Utc>::from_timestamp_nanos(self.timestamp_nanos as i64) {
            ts => format!("{} {}", ts.to_rfc3339(), self.line),
        }
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

fn escape_logql_label_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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
