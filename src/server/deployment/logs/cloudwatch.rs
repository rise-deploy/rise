use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::db::models::{Deployment, DeploymentStatus, Project};

use super::{
    classify_k8s_line, decode_log_cursor, encode_log_cursor, is_followable_status,
    line_matches_search, log_cursor_signature, parse_duration_hint, stable_log_id, status_stream,
    LogEvent, LogEventStream, LogQuery, LogStatus, LogStatusReason, LogVolumeBucket,
    LogVolumeQuery, LogVolumeResponse, RuntimeLogBackend, KUBERNETES_LEVELS,
};

const MAX_TAIL: i64 = 5_000;
const MAX_SCAN_EVENTS: usize = 100_000;
const MAX_SCAN_PAGES: usize = 100;
const PAGE_SIZE: i32 = 10_000;
const LIVE_TAIL_BUFFER_SIZE: usize = 16;
// CloudWatch rejects descending FilterLogEvents requests whose start precedes
// 2024-01-01. No stream matching this backend's UUID prefix contains earlier
// events.
const DESCENDING_START_MIN_MILLIS: i64 = 1_704_067_200_000;

pub(super) struct CloudWatchLogBackend {
    client: aws_sdk_cloudwatchlogs::Client,
    log_group: String,
    log_group_arn: String,
    resource_prefix: String,
    retention_hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CloudWatchLine {
    timestamp_millis: i64,
    ingestion_time_millis: i64,
    log_stream: String,
    message: String,
    id: String,
    // FilterLogEvents exposes an event ID for pagination ordering; Live Tail does not.
    pagination_id: String,
    /// Deployment container the line came from, resolved from the log stream
    /// name. `None` when the stream doesn't sit under the deployment prefix.
    container: Option<String>,
}

impl CloudWatchLine {
    fn occurrence_key(&self) -> CloudWatchOccurrenceKey {
        CloudWatchOccurrenceKey {
            timestamp_millis: self.timestamp_millis,
            ingestion_time_millis: self.ingestion_time_millis,
            log_stream: self.log_stream.clone(),
            message: self.message.clone(),
        }
    }

    fn from_filtered(event: &aws_sdk_cloudwatchlogs::types::FilteredLogEvent) -> Option<Self> {
        let timestamp_millis = event.timestamp()?;
        let ingestion_time_millis = event.ingestion_time().unwrap_or_default();
        let log_stream = event.log_stream_name().unwrap_or_default().to_string();
        let message = event
            .message()
            .unwrap_or_default()
            .trim_end_matches(['\r', '\n'])
            .to_string();
        let pagination_id = event.event_id().map(str::to_string).unwrap_or_else(|| {
            stable_cloudwatch_source_id(
                timestamp_millis,
                ingestion_time_millis,
                &log_stream,
                &message,
            )
        });
        Some(Self {
            id: String::new(),
            pagination_id,
            timestamp_millis,
            ingestion_time_millis,
            log_stream,
            message,
            container: None,
        })
    }

    fn from_live(event: &aws_sdk_cloudwatchlogs::types::LiveTailSessionLogEvent) -> Option<Self> {
        let timestamp_millis = event.timestamp()?;
        let ingestion_time_millis = event.ingestion_time().unwrap_or_default();
        let log_stream = event.log_stream_name().unwrap_or_default().to_string();
        let message = event
            .message()
            .unwrap_or_default()
            .trim_end_matches(['\r', '\n'])
            .to_string();
        Some(Self {
            id: String::new(),
            pagination_id: String::new(),
            timestamp_millis,
            ingestion_time_millis,
            log_stream,
            message,
            container: None,
        })
    }

    fn assign_occurrence_id(&mut self, occurrence: u64) {
        let timestamp = self.timestamp_millis.to_be_bytes();
        let ingestion = self.ingestion_time_millis.to_be_bytes();
        let occurrence = occurrence.to_be_bytes();
        self.id = stable_log_id(
            "cloudwatch",
            [
                timestamp.as_slice(),
                ingestion.as_slice(),
                self.log_stream.as_bytes(),
                self.message.as_bytes(),
                occurrence.as_slice(),
            ],
        );
    }

    fn pagination_key(&self) -> CloudWatchKey {
        CloudWatchKey {
            timestamp_millis: self.timestamp_millis,
            ingestion_time_millis: self.ingestion_time_millis,
            log_stream: self.log_stream.clone(),
            id: self.pagination_id.clone(),
        }
    }

    fn pagination_primary(&self) -> (i64, i64) {
        (self.timestamp_millis, self.ingestion_time_millis)
    }

    fn level(&self) -> &'static str {
        classify_k8s_line(&self.message)
    }

    fn matches(&self, levels: &[String], search: Option<&str>, containers: &[String]) -> bool {
        let level = self.level();
        (levels.is_empty() || levels.iter().any(|candidate| candidate == level))
            && (containers.is_empty()
                || self
                    .container
                    .as_ref()
                    .is_some_and(|container| containers.contains(container)))
            && line_matches_search(&self.message, search)
    }

    /// Resolve the deployment container from the log stream name. The ECS
    /// awslogs driver appends `{container}/{task-id}` to the configured
    /// stream prefix, which is this backend's per-deployment prefix.
    fn attribute_container(&mut self, prefix: &str) {
        self.container = container_from_stream(prefix, &self.log_stream);
    }

    fn render(&self, timestamps: bool) -> String {
        if !timestamps {
            return self.message.clone();
        }
        let timestamp = DateTime::<Utc>::from_timestamp_millis(self.timestamp_millis)
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
        format!("{} {}", timestamp.to_rfc3339(), self.message)
    }
}

/// The container segment of an ECS awslogs stream name — the first path
/// segment after the deployment's stream prefix.
fn container_from_stream(prefix: &str, log_stream: &str) -> Option<String> {
    log_stream
        .strip_prefix(prefix)?
        .split('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
}

fn stable_cloudwatch_source_id(
    timestamp_millis: i64,
    ingestion_time_millis: i64,
    log_stream: &str,
    message: &str,
) -> String {
    let timestamp = timestamp_millis.to_be_bytes();
    let ingestion = ingestion_time_millis.to_be_bytes();
    stable_log_id(
        "cloudwatch",
        [
            timestamp.as_slice(),
            ingestion.as_slice(),
            log_stream.as_bytes(),
            message.as_bytes(),
        ],
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CloudWatchOccurrenceKey {
    timestamp_millis: i64,
    ingestion_time_millis: i64,
    log_stream: String,
    message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct CloudWatchKey {
    timestamp_millis: i64,
    ingestion_time_millis: i64,
    log_stream: String,
    id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CloudWatchCursor {
    version: u8,
    signature: String,
    start_millis: i64,
    end_millis: i64,
    before: Option<CloudWatchKey>,
}

struct BacklogPage {
    lines: Vec<CloudWatchLine>,
    next_cursor: Option<String>,
}

struct ScanResult {
    lines: Vec<CloudWatchLine>,
    complete: bool,
}

struct CloudWatchScan<'a> {
    /// Per-deployment stream prefix, also the anchor for container attribution.
    prefix: &'a str,
    /// Prefix actually sent to CloudWatch. Narrowed to a single container's
    /// streams when the filter names exactly one; otherwise the deployment
    /// prefix, with the filter applied per line.
    request_prefix: &'a str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    levels: &'a [String],
    search: Option<&'a str>,
    containers: &'a [String],
    stop_after: Option<usize>,
    before: Option<&'a CloudWatchKey>,
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn volume_bucket_index(
    timestamp_millis: i64,
    aligned_start_seconds: i64,
    step_seconds: i64,
    bucket_count: usize,
) -> Option<usize> {
    let start_millis = aligned_start_seconds.saturating_mul(1_000);
    let step_millis = step_seconds.max(1).saturating_mul(1_000);
    let offset = timestamp_millis.saturating_sub(start_millis);
    if offset <= 0 {
        return None;
    }
    let index = (offset - 1).div_euclid(step_millis) as usize;
    (index < bucket_count).then_some(index)
}

fn cloudwatch_inclusive_end_millis(exclusive_end: DateTime<Utc>) -> i64 {
    let end_millis = exclusive_end.timestamp_millis();
    if exclusive_end
        .timestamp_subsec_nanos()
        .is_multiple_of(1_000_000)
    {
        end_millis.saturating_sub(1)
    } else {
        end_millis
    }
}

fn next_occurrence(
    occurrences: &mut HashMap<CloudWatchOccurrenceKey, u64>,
    key: CloudWatchOccurrenceKey,
) -> Result<u64> {
    let next = occurrences.entry(key).or_insert(0);
    let occurrence = *next;
    *next = next
        .checked_add(1)
        .context("CloudWatch log occurrence counter overflowed")?;
    Ok(occurrence)
}

fn consume_backlog_overlap(
    remaining: &mut HashMap<CloudWatchOccurrenceKey, u64>,
    key: &CloudWatchOccurrenceKey,
) -> bool {
    remaining.get_mut(key).is_some_and(|count| {
        if *count == 0 {
            return false;
        }
        *count -= 1;
        true
    })
}

fn select_backlog_page(
    mut lines: Vec<CloudWatchLine>,
    tail: usize,
) -> (Vec<CloudWatchLine>, Option<CloudWatchKey>) {
    lines.sort_by_key(|line| std::cmp::Reverse(line.pagination_key()));
    lines.truncate(tail);
    let boundary = lines.last().map(CloudWatchLine::pagination_key);
    lines.sort();
    (lines, boundary)
}

impl CloudWatchLogBackend {
    pub(super) async fn new(
        sdk_config: aws_config::SdkConfig,
        region: String,
        log_group: String,
        resource_prefix: String,
        retention_hint: Option<String>,
    ) -> Result<Self> {
        let identity = aws_sdk_sts::Client::new(&sdk_config)
            .get_caller_identity()
            .send()
            .await
            .context("CloudWatch log backend could not resolve its AWS account")?;
        let account = identity
            .account()
            .context("sts:GetCallerIdentity returned no AWS account")?;
        let caller_arn = identity
            .arn()
            .context("sts:GetCallerIdentity returned no caller ARN")?;
        let partition = caller_arn
            .split(':')
            .nth(1)
            .filter(|value| !value.is_empty())
            .context("sts:GetCallerIdentity returned an invalid caller ARN")?;
        let log_group_arn =
            format!("arn:{partition}:logs:{region}:{account}:log-group:{log_group}");

        Ok(Self {
            client: aws_sdk_cloudwatchlogs::Client::new(&sdk_config),
            log_group,
            log_group_arn,
            resource_prefix,
            retention_hint,
        })
    }

    fn stream_prefix(&self, deployment: &Deployment, project: &Project) -> String {
        format!(
            "{}/{}/{}/",
            rise_backend_core::naming::sanitize_ecs_name(&self.resource_prefix),
            project.id,
            deployment.id
        )
    }

    /// Stream-name prefixes to ask CloudWatch for. One per requested
    /// container so the daemon does the narrowing where it can; the
    /// deployment-wide prefix when no container filter is set.
    fn request_prefixes(prefix: &str, containers: &[String]) -> Vec<String> {
        if containers.is_empty() {
            return vec![prefix.to_string()];
        }
        containers
            .iter()
            .map(|container| format!("{prefix}{container}/"))
            .collect()
    }

    fn effective_start(deployment: &Deployment, query: &LogQuery) -> DateTime<Utc> {
        query.start_time.unwrap_or_else(|| {
            query
                .since_seconds
                .map(|seconds| Utc::now() - Duration::seconds(seconds))
                .unwrap_or(deployment.created_at)
        })
    }

    fn empty_status(&self, deployment: &Deployment) -> LogStatus {
        let retention_expired = self
            .retention_hint
            .as_deref()
            .and_then(parse_duration_hint)
            .is_some_and(|retention| Utc::now() - deployment.created_at > retention);
        LogStatus {
            reason: if retention_expired {
                LogStatusReason::RetentionExpiredPossible
            } else {
                LogStatusReason::NoLogsFound
            },
            message: None,
            retention_hint: self.retention_hint.clone(),
        }
    }

    fn deployment_not_ready() -> LogStatus {
        LogStatus {
            reason: LogStatusReason::DeploymentNotReady,
            message: Some("Deployment is not ready yet - no runtime logs are available.".into()),
            retention_hint: None,
        }
    }

    async fn scan(&self, query: CloudWatchScan<'_>) -> Result<ScanResult> {
        let mut next_token: Option<String> = None;
        let mut seen_tokens = HashSet::new();
        let mut scanned = 0usize;
        let mut lines = Vec::new();
        let mut stop_primary = None;
        let mut occurrence_primary = None;
        let mut occurrences = HashMap::new();

        for _ in 0..MAX_SCAN_PAGES {
            let mut request = self
                .client
                .filter_log_events()
                .log_group_name(&self.log_group)
                .log_stream_name_prefix(query.request_prefix)
                .start_time(
                    query
                        .start
                        .timestamp_millis()
                        .max(DESCENDING_START_MIN_MILLIS),
                )
                .end_time(query.end.timestamp_millis().max(0))
                .start_from_head(false)
                .limit(PAGE_SIZE);
            if let Some(token) = next_token.as_deref() {
                request = request.next_token(token);
            }

            let output = request.send().await.with_context(|| {
                format!("Failed to read CloudWatch log group {:?}", self.log_group)
            })?;
            for event in output.events() {
                let mut line = CloudWatchLine::from_filtered(event);
                if line.as_ref().is_some_and(|line| {
                    stop_primary.is_some_and(|primary| line.pagination_primary() < primary)
                }) {
                    return Ok(ScanResult {
                        lines,
                        complete: false,
                    });
                }
                scanned += 1;
                if scanned > MAX_SCAN_EVENTS {
                    anyhow::bail!(
                        "CloudWatch log scan exceeds the limit of {MAX_SCAN_EVENTS} events"
                    );
                }
                let Some(mut line) = line.take() else {
                    continue;
                };
                let primary = line.pagination_primary();
                if occurrence_primary != Some(primary) {
                    occurrences.clear();
                    occurrence_primary = Some(primary);
                }
                let occurrence = next_occurrence(&mut occurrences, line.occurrence_key())?;
                line.assign_occurrence_id(occurrence);
                line.attribute_container(query.prefix);
                if query
                    .before
                    .is_some_and(|boundary| line.pagination_key() >= *boundary)
                {
                    continue;
                }
                if line.matches(query.levels, query.search, query.containers) {
                    lines.push(line);
                    if stop_primary.is_none()
                        && query.stop_after.is_some_and(|limit| lines.len() >= limit)
                    {
                        // FilterLogEvents does not expose its request-id tie-breaker.
                        // The cursor can order an entire timestamp/ingestion group by event ID.
                        stop_primary = Some(primary);
                    }
                }
            }

            let token = output.next_token().map(str::to_string);
            if token.is_none() {
                return Ok(ScanResult {
                    lines,
                    complete: true,
                });
            }
            let token = token.expect("checked above");
            if !seen_tokens.insert(token.clone()) {
                anyhow::bail!("CloudWatch log pagination returned a repeated next token");
            }
            next_token = Some(token);
        }

        anyhow::bail!("CloudWatch log scan exceeds the limit of {MAX_SCAN_PAGES} pages")
    }

    async fn backlog(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: &LogQuery,
    ) -> Result<BacklogPage> {
        let tail = query.tail_lines.unwrap_or(1_000).clamp(1, MAX_TAIL) as usize;
        let signature = log_cursor_signature("cloudwatch", deployment, project, query);
        let cursor = query
            .cursor
            .as_deref()
            .map(decode_log_cursor::<CloudWatchCursor>)
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.version != 2 || cursor.signature != signature)
        {
            anyhow::bail!("invalid log cursor for this deployment or filter");
        }
        let prefix = self.stream_prefix(deployment, project);
        let end = if let Some(cursor) = &cursor {
            DateTime::<Utc>::from_timestamp_millis(cursor.end_millis)
                .context("invalid log cursor end timestamp")?
        } else if let Some(exclusive_end) = query.end_time {
            // FilterLogEvents endTime is inclusive; LogQuery end_time is exclusive.
            DateTime::<Utc>::from_timestamp_millis(cloudwatch_inclusive_end_millis(exclusive_end))
                .context("CloudWatch log end is outside the supported timestamp range")?
        } else {
            Utc::now()
        };
        let start = if let Some(cursor) = &cursor {
            DateTime::<Utc>::from_timestamp_millis(cursor.start_millis)
                .context("invalid log cursor start timestamp")?
        } else {
            Self::effective_start(deployment, query)
        };
        if cursor.is_none() && start > end {
            return Ok(BacklogPage {
                lines: Vec::new(),
                next_cursor: None,
            });
        }
        if start > end || (cursor.is_some() && start < deployment.created_at) {
            anyhow::bail!("invalid log cursor time range");
        }
        if cursor
            .as_ref()
            .and_then(|cursor| cursor.before.as_ref())
            .is_some_and(|boundary| {
                boundary.timestamp_millis < start.timestamp_millis()
                    || boundary.timestamp_millis > end.timestamp_millis()
            })
        {
            anyhow::bail!("invalid log cursor boundary");
        }
        // FilterLogEvents takes a single prefix, so only a one-container
        // filter can be pushed down; wider filters narrow per line.
        let request_prefixes = Self::request_prefixes(&prefix, &query.containers);
        let request_prefix = match request_prefixes.as_slice() {
            [single] => single.clone(),
            _ => prefix.clone(),
        };
        let result = self
            .scan(CloudWatchScan {
                prefix: &prefix,
                request_prefix: &request_prefix,
                start,
                end,
                levels: &query.levels,
                search: query.search.as_deref(),
                containers: &query.containers,
                stop_after: Some(tail),
                before: cursor.as_ref().and_then(|cursor| cursor.before.as_ref()),
            })
            .await?;
        let has_more_in_segment = !result.complete || result.lines.len() > tail;
        let (lines, boundary) = select_backlog_page(result.lines, tail);
        let next_segment = if has_more_in_segment {
            boundary.map(|boundary| (start, end, Some(boundary)))
        } else if start > deployment.created_at {
            let older_end =
                DateTime::<Utc>::from_timestamp_millis(start.timestamp_millis().saturating_sub(1))
                    .context("invalid log cursor time range")?;
            (deployment.created_at <= older_end).then_some((deployment.created_at, older_end, None))
        } else {
            None
        };
        let next_cursor = next_segment
            .map(|(start, end, before)| {
                encode_log_cursor(&CloudWatchCursor {
                    version: 2,
                    signature,
                    start_millis: start.timestamp_millis(),
                    end_millis: end.timestamp_millis(),
                    before,
                })
            })
            .transpose()?;
        Ok(BacklogPage { lines, next_cursor })
    }

    async fn live_tail(
        &self,
        deployment: Deployment,
        project: Project,
        query: LogQuery,
    ) -> Result<LogEventStream> {
        use aws_sdk_cloudwatchlogs::types::StartLiveTailResponseStream;

        let prefix = self.stream_prefix(&deployment, &project);
        // Unlike FilterLogEvents, Live Tail takes a prefix list, so every
        // requested container is pushed down to the session.
        let mut request = self
            .client
            .start_live_tail()
            .log_group_identifiers(&self.log_group_arn);
        for request_prefix in Self::request_prefixes(&prefix, &query.containers) {
            request = request.log_stream_name_prefixes(request_prefix);
        }
        let response = request
            .send()
            .await
            .context("Failed to start CloudWatch Live Tail")?;
        let mut response_stream = response.response_stream;
        let (live_tx, mut live_rx) = tokio::sync::mpsc::channel(LIVE_TAIL_BUFFER_SIZE);
        let live_task = AbortOnDrop(tokio::spawn(async move {
            loop {
                let event = match response_stream.recv().await {
                    Ok(Some(event)) => Ok(event),
                    Ok(None) => Err(anyhow::anyhow!(
                        "CloudWatch Live Tail stream ended; reconnect to resume streaming"
                    )),
                    Err(error)
                        if error
                            .as_service_error()
                            .is_some_and(|service| service.is_session_timeout_exception()) =>
                    {
                        Err(anyhow::anyhow!(
                            "CloudWatch Live Tail session expired; reconnect to resume streaming"
                        ))
                    }
                    Err(error) => Err(anyhow::anyhow!(
                        "CloudWatch Live Tail stream failed: {error}"
                    )),
                };
                let failed = event.is_err();
                if live_tx.send(event).await.is_err() || failed {
                    break;
                }
            }
        }));
        let initial = self.backlog(&deployment, &project, &query).await?;
        let levels = query.levels.clone();
        let search = query.search.clone();
        let containers = query.containers.clone();
        let timestamps = query.timestamps;

        let log_group = self.log_group.clone();
        let stream = async_stream::try_stream! {
            let _live_task = live_task;
            let backlog_count = initial.lines.len();
            let mut backlog_occurrences: HashMap<CloudWatchOccurrenceKey, u64> = HashMap::new();
            let mut live_occurrences = HashMap::new();
            let mut occurrence_primary = None;
            for line in &initial.lines {
                *backlog_occurrences
                    .entry(line.occurrence_key())
                    .or_insert(0) += 1;
            }
            for line in initial.lines {
                let level = line.level().to_string();
                yield LogEvent::Line {
                    id: line.id.clone(),
                    text: line.render(timestamps),
                    level,
                    container: line.container.clone(),
                };
            }
            yield LogEvent::BacklogLoaded { count: backlog_count, next_cursor: initial.next_cursor };

            while let Some(event) = live_rx.recv().await {
                let event = event?;
                if let StartLiveTailResponseStream::SessionUpdate(update) = event {
                    if update
                        .session_metadata()
                        .is_some_and(|metadata| metadata.sampled())
                    {
                        warn!(log_group = %log_group, "CloudWatch Live Tail sampled log events");
                    }
                    for event in update.session_results() {
                        let Some(mut line) = CloudWatchLine::from_live(event) else {
                            continue;
                        };
                        let primary = line.pagination_primary();
                        if occurrence_primary != Some(primary) {
                            live_occurrences.clear();
                            occurrence_primary = Some(primary);
                        }
                        let key = line.occurrence_key();
                        let occurrence = next_occurrence(&mut live_occurrences, key.clone())?;
                        if consume_backlog_overlap(&mut backlog_occurrences, &key) {
                            continue;
                        }
                        line.attribute_container(&prefix);
                        if !line.matches(&levels, search.as_deref(), &containers) {
                            continue;
                        }
                        line.assign_occurrence_id(occurrence);
                        let level = line.level().to_string();
                        yield LogEvent::Line {
                            id: line.id.clone(),
                            text: line.render(timestamps),
                            level,
                            container: line.container.clone(),
                        };
                    }
                }
            }
        };
        Ok(stream.boxed())
    }
}

#[async_trait]
impl RuntimeLogBackend for CloudWatchLogBackend {
    fn backend_kind(&self) -> &'static str {
        "cloudwatch"
    }

    fn levels(&self) -> &'static [&'static str] {
        KUBERNETES_LEVELS
    }

    fn supports_volume(&self) -> bool {
        true
    }

    fn max_tail(&self) -> Option<i64> {
        Some(MAX_TAIL)
    }

    async fn stream_logs(
        &self,
        deployment: &Deployment,
        project: &Project,
        query: LogQuery,
    ) -> Result<LogEventStream> {
        if matches!(
            deployment.status,
            DeploymentStatus::Pending
                | DeploymentStatus::Building
                | DeploymentStatus::Pushing
                | DeploymentStatus::Pushed
        ) {
            return Ok(status_stream(Self::deployment_not_ready()));
        }
        if query.follow && is_followable_status(&deployment.status) {
            return self
                .live_tail(deployment.clone(), project.clone(), query)
                .await;
        }

        let page = self.backlog(deployment, project, &query).await?;
        let timestamps = query.timestamps;
        let mut events = page
            .lines
            .into_iter()
            .map(move |line| {
                let level = line.level().to_string();
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
                status: Some(Self::deployment_not_ready()),
                start_time: query.start_time.to_rfc3339(),
                end_time: query.end_time.to_rfc3339(),
                step_seconds: query.step_seconds,
                buckets: vec![],
            });
        }

        let step = query.step_seconds.max(1);
        let start_seconds = query.start_time.timestamp();
        let end_seconds = query.end_time.timestamp();
        let aligned_start_seconds = start_seconds.div_euclid(step) * step;
        let aligned_end_seconds = if end_seconds.rem_euclid(step) == 0 {
            end_seconds
        } else {
            (end_seconds.div_euclid(step) + 1) * step
        };
        let aligned_start = DateTime::<Utc>::from_timestamp(aligned_start_seconds, 0)
            .context("CloudWatch volume start is outside the supported timestamp range")?;
        let aligned_end = DateTime::<Utc>::from_timestamp(aligned_end_seconds, 0)
            .context("CloudWatch volume end is outside the supported timestamp range")?;

        let prefix = self.stream_prefix(deployment, project);
        let request_prefixes = Self::request_prefixes(&prefix, &query.containers);
        let request_prefix = match request_prefixes.as_slice() {
            [single] => single.clone(),
            _ => prefix.clone(),
        };
        let result = self
            .scan(CloudWatchScan {
                prefix: &prefix,
                request_prefix: &request_prefix,
                start: query.start_time,
                end: query.end_time,
                levels: &query.levels,
                search: query.search.as_deref(),
                containers: &query.containers,
                stop_after: None,
                before: None,
            })
            .await?;
        debug_assert!(result.complete);

        let bucket_count = ((aligned_end_seconds - aligned_start_seconds) / step).max(0) as usize;
        let mut buckets = Vec::with_capacity(bucket_count);
        for index in 0..bucket_count {
            let edge = aligned_start_seconds + ((index as i64 + 1) * step);
            buckets.push(LogVolumeBucket {
                timestamp: DateTime::<Utc>::from_timestamp(edge, 0)
                    .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
                    .to_rfc3339(),
                total: 0,
                by_level: HashMap::new(),
            });
        }
        for line in result.lines {
            let Some(index) = volume_bucket_index(
                line.timestamp_millis,
                aligned_start_seconds,
                step,
                bucket_count,
            ) else {
                continue;
            };
            let Some(bucket) = buckets.get_mut(index) else {
                continue;
            };
            bucket.total += 1;
            *bucket.by_level.entry(line.level().to_string()).or_insert(0) += 1;
        }

        let empty = buckets.iter().all(|bucket| bucket.total == 0);
        Ok(LogVolumeResponse {
            status: empty.then(|| self.empty_status(deployment)),
            start_time: aligned_start.to_rfc3339(),
            end_time: aligned_end.to_rfc3339(),
            step_seconds: step,
            buckets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_is_read_from_the_stream_name_after_the_deployment_prefix() {
        let prefix = "rise/project-uuid/deployment-uuid/";
        assert_eq!(
            container_from_stream(prefix, &format!("{prefix}web/task-1")),
            Some("web".to_string())
        );
        // A stream from another deployment shares the log group but not the
        // prefix, and must never be attributed to this one.
        assert_eq!(
            container_from_stream(prefix, "rise/project-uuid/other-uuid/web/task-1"),
            None
        );
        assert_eq!(container_from_stream(prefix, prefix), None);
    }

    #[test]
    fn request_prefixes_narrow_to_one_stream_prefix_per_container() {
        let prefix = "rise/project-uuid/deployment-uuid/";
        assert_eq!(
            CloudWatchLogBackend::request_prefixes(prefix, &[]),
            vec![prefix.to_string()]
        );
        assert_eq!(
            CloudWatchLogBackend::request_prefixes(prefix, &["web".to_string(), "api".to_string()]),
            vec![format!("{prefix}web/"), format!("{prefix}api/"),]
        );
    }

    #[test]
    fn container_filter_keeps_only_the_named_containers() {
        let line = |container: &str| CloudWatchLine {
            id: "event-1".into(),
            pagination_id: "event-1".into(),
            timestamp_millis: 1_700_000_000_000,
            ingestion_time_millis: 1_700_000_000_100,
            log_stream: format!("rise/project/deployment/{container}/task"),
            message: "hello".into(),
            container: Some(container.to_string()),
        };
        assert!(line("web").matches(&[], None, &["web".to_string()]));
        assert!(!line("api").matches(&[], None, &["web".to_string()]));
        // An unattributable line (a stream outside the deployment prefix)
        // is dropped by a container filter rather than passed through.
        let mut orphan = line("web");
        orphan.container = None;
        assert!(!orphan.matches(&[], None, &["web".to_string()]));
        assert!(orphan.matches(&[], None, &[]));
    }

    #[test]
    fn filtered_line_uses_the_shared_classifier_and_search() {
        let line = CloudWatchLine {
            id: "event-1".into(),
            pagination_id: "event-1".into(),
            timestamp_millis: 1_700_000_000_000,
            ingestion_time_millis: 1_700_000_000_100,
            log_stream: "rise/project/deployment/app/task".into(),
            message: "ERROR Database Unavailable".into(),
            container: None,
        };
        assert_eq!(line.level(), "error");
        assert!(line.matches(&["error".into()], Some("database"), &[]));
        assert!(!line.matches(&["warn".into()], Some("database"), &[]));
        assert!(!line.matches(&[], Some("missing"), &[]));
    }

    #[test]
    fn render_uses_the_cloudwatch_event_timestamp() {
        let line = CloudWatchLine {
            id: "event-1".into(),
            pagination_id: "event-1".into(),
            timestamp_millis: 1_700_000_000_000,
            ingestion_time_millis: 1_700_000_000_100,
            log_stream: "stream".into(),
            message: "hello".into(),
            container: None,
        };
        assert_eq!(line.clone().render(false), "hello");
        assert_eq!(line.render(true), "2023-11-14T22:13:20+00:00 hello");
    }

    #[test]
    fn volume_buckets_are_left_open_and_right_closed_at_millisecond_precision() {
        let start = 1_700_000_000;
        assert_eq!(volume_bucket_index(start * 1_000, start, 60, 2), None);
        assert_eq!(
            volume_bucket_index(start * 1_000 + 1, start, 60, 2),
            Some(0)
        );
        assert_eq!(
            volume_bucket_index(start * 1_000 + 60_000, start, 60, 2),
            Some(0)
        );
        assert_eq!(
            volume_bucket_index(start * 1_000 + 60_001, start, 60, 2),
            Some(1)
        );
        assert_eq!(
            volume_bucket_index(start * 1_000 + 120_001, start, 60, 2),
            None
        );
    }

    #[test]
    fn explicit_log_end_is_converted_to_an_inclusive_cloudwatch_bound() {
        let exclusive_end = DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap();
        assert_eq!(
            cloudwatch_inclusive_end_millis(exclusive_end),
            1_699_999_999_999
        );

        let sub_millisecond_end =
            DateTime::<Utc>::from_timestamp(1_700_000_000, 123_456_789).unwrap();
        assert_eq!(
            cloudwatch_inclusive_end_millis(sub_millisecond_end),
            1_700_000_000_123
        );
    }

    #[test]
    fn backlog_pagination_uses_the_oldest_returned_event_as_its_boundary() {
        let line = |timestamp_millis, id: &str| CloudWatchLine {
            id: id.into(),
            pagination_id: id.into(),
            timestamp_millis,
            ingestion_time_millis: timestamp_millis,
            log_stream: "stream".into(),
            message: id.into(),
            container: None,
        };
        let all = vec![
            line(500, "fifth"),
            line(400, "fourth"),
            line(300, "third"),
            line(200, "second"),
            line(100, "first"),
        ];

        let (page, boundary) = select_backlog_page(all.clone(), 2);
        assert_eq!(
            page.into_iter()
                .map(|line| line.message)
                .collect::<Vec<_>>(),
            vec!["fourth", "fifth"]
        );

        let boundary = boundary.expect("a non-empty page has a boundary");
        let mut next_candidates = all;
        next_candidates.push(line(600, "late-newer"));
        next_candidates.push(line(350, "late-older"));
        next_candidates.retain(|line| line.pagination_key() < boundary);
        let (next_page, _) = select_backlog_page(next_candidates, 2);
        assert_eq!(
            next_page
                .into_iter()
                .map(|line| line.message)
                .collect::<Vec<_>>(),
            vec!["third", "late-older"]
        );
    }

    #[test]
    fn backlog_boundary_orders_events_with_identical_cloudwatch_timestamps() {
        let line = |id: &str| CloudWatchLine {
            id: id.into(),
            pagination_id: id.into(),
            timestamp_millis: 500,
            ingestion_time_millis: 600,
            log_stream: "stream".into(),
            message: id.into(),
            container: None,
        };
        let all = vec![line("a"), line("b"), line("c")];

        let (page, boundary) = select_backlog_page(all.clone(), 2);
        assert_eq!(
            page.into_iter().map(|line| line.id).collect::<Vec<_>>(),
            vec!["b", "c"]
        );

        let boundary = boundary.expect("a non-empty page has a boundary");
        let remainder = all
            .into_iter()
            .filter(|line| line.pagination_key() < boundary)
            .collect::<Vec<_>>();
        let (page, _) = select_backlog_page(remainder, 2);
        assert_eq!(
            page.into_iter().map(|line| line.id).collect::<Vec<_>>(),
            vec!["a"]
        );
    }

    #[test]
    fn filtered_and_live_occurrence_ids_match_and_repeats_differ() {
        let line = || CloudWatchLine {
            id: String::new(),
            pagination_id: String::new(),
            timestamp_millis: 500,
            ingestion_time_millis: 600,
            log_stream: "stream".into(),
            message: "repeated".into(),
            container: None,
        };
        let mut first = line();
        let mut second = line();
        let mut repeated_first = line();

        first.pagination_id = "filtered-event-id".into();
        first.assign_occurrence_id(0);
        second.assign_occurrence_id(1);
        repeated_first.assign_occurrence_id(0);

        assert_ne!(first.id, second.id);
        assert_eq!(first.id, repeated_first.id);
    }

    #[test]
    fn backlog_overlap_consumes_its_occurrence_before_a_repeat_is_emitted() {
        let key = CloudWatchOccurrenceKey {
            timestamp_millis: 500,
            ingestion_time_millis: 600,
            log_stream: "stream".into(),
            message: "repeated".into(),
        };
        let mut backlog_remaining = HashMap::from([(key.clone(), 1)]);
        let mut live_occurrences = HashMap::new();

        let overlap = next_occurrence(&mut live_occurrences, key.clone()).unwrap();
        assert_eq!(overlap, 0);
        assert!(consume_backlog_overlap(&mut backlog_remaining, &key));

        let repeated = next_occurrence(&mut live_occurrences, key.clone()).unwrap();
        assert_eq!(repeated, 1);
        assert!(!consume_backlog_overlap(&mut backlog_remaining, &key));
    }

    #[tokio::test]
    async fn dropping_abort_guard_cancels_the_live_tail_reader() {
        struct NotifyOnDrop(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for NotifyOnDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _notify = NotifyOnDrop(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        }));
        started_rx.await.expect("reader task starts");

        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("reader task is cancelled")
            .expect("reader task drop is observed");
    }
}
