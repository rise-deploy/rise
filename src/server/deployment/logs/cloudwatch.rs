use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use tracing::warn;

use crate::db::models::{Deployment, DeploymentStatus, Project};

use super::{
    classify_k8s_line, is_followable_status, line_matches_search, parse_duration_hint,
    status_stream, LogEvent, LogEventStream, LogQuery, LogStatus, LogStatusReason, LogVolumeBucket,
    LogVolumeQuery, LogVolumeResponse, RuntimeLogBackend, KUBERNETES_LEVELS,
};

const MAX_TAIL: i64 = 5_000;
const MAX_SCAN_EVENTS: usize = 100_000;
const MAX_SCAN_PAGES: usize = 100;
const PAGE_SIZE: i32 = 10_000;
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
}

impl CloudWatchLine {
    fn from_filtered(event: &aws_sdk_cloudwatchlogs::types::FilteredLogEvent) -> Option<Self> {
        Some(Self {
            timestamp_millis: event.timestamp()?,
            ingestion_time_millis: event.ingestion_time().unwrap_or_default(),
            log_stream: event.log_stream_name().unwrap_or_default().to_string(),
            message: event
                .message()
                .unwrap_or_default()
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        })
    }

    fn from_live(event: &aws_sdk_cloudwatchlogs::types::LiveTailSessionLogEvent) -> Option<Self> {
        Some(Self {
            timestamp_millis: event.timestamp()?,
            ingestion_time_millis: event.ingestion_time().unwrap_or_default(),
            log_stream: event.log_stream_name().unwrap_or_default().to_string(),
            message: event
                .message()
                .unwrap_or_default()
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        })
    }

    fn level(&self) -> &'static str {
        classify_k8s_line(&self.message)
    }

    fn matches(&self, levels: &[String], search: Option<&str>) -> bool {
        let level = self.level();
        (levels.is_empty() || levels.iter().any(|candidate| candidate == level))
            && line_matches_search(&self.message, search)
    }

    fn render(self, timestamps: bool) -> String {
        if !timestamps {
            return self.message;
        }
        let timestamp = DateTime::<Utc>::from_timestamp_millis(self.timestamp_millis)
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
        format!("{} {}", timestamp.to_rfc3339(), self.message)
    }
}

struct ScanResult {
    lines: Vec<CloudWatchLine>,
    complete: bool,
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

fn select_backlog_page(
    lines_newest_first: Vec<CloudWatchLine>,
    skip_recent: usize,
    tail: usize,
) -> Vec<CloudWatchLine> {
    let mut page = lines_newest_first
        .into_iter()
        .skip(skip_recent)
        .take(tail)
        .collect::<Vec<_>>();
    page.sort();
    page
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

    async fn scan(
        &self,
        prefix: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        levels: &[String],
        search: Option<&str>,
        stop_after: Option<usize>,
    ) -> Result<ScanResult> {
        let mut next_token: Option<String> = None;
        let mut seen_tokens = HashSet::new();
        let mut scanned = 0usize;
        let mut lines = Vec::new();

        for _ in 0..MAX_SCAN_PAGES {
            let mut request = self
                .client
                .filter_log_events()
                .log_group_name(&self.log_group)
                .log_stream_name_prefix(prefix)
                .start_time(start.timestamp_millis().max(DESCENDING_START_MIN_MILLIS))
                .end_time(end.timestamp_millis().max(0))
                .start_from_head(false)
                .limit(PAGE_SIZE);
            if let Some(token) = next_token.as_deref() {
                request = request.next_token(token);
            }

            let output = request.send().await.with_context(|| {
                format!("Failed to read CloudWatch log group {:?}", self.log_group)
            })?;
            for event in output.events() {
                scanned += 1;
                if scanned > MAX_SCAN_EVENTS {
                    anyhow::bail!(
                        "CloudWatch log scan exceeds the limit of {MAX_SCAN_EVENTS} events"
                    );
                }
                let Some(line) = CloudWatchLine::from_filtered(event) else {
                    continue;
                };
                if line.matches(levels, search) {
                    lines.push(line);
                }
            }

            let token = output.next_token().map(str::to_string);
            let complete = token.is_none();
            if complete || stop_after.is_some_and(|limit| lines.len() >= limit) {
                return Ok(ScanResult { lines, complete });
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
    ) -> Result<Vec<CloudWatchLine>> {
        let tail = query.tail_lines.unwrap_or(1_000).clamp(1, MAX_TAIL) as usize;
        let skip_recent = query.skip_recent.unwrap_or(0).max(0) as usize;
        let prefix = self.stream_prefix(deployment, project);
        // Timestamp-only pagination can strand events when several ECS streams
        // share the oldest loaded millisecond. Count-based pagination scans the
        // same newest-first result set and skips exactly what the caller has.
        let end = if skip_recent > 0 {
            Utc::now()
        } else {
            query.end_time.unwrap_or_else(Utc::now)
        };
        let result = self
            .scan(
                &prefix,
                Self::effective_start(deployment, query),
                end,
                &query.levels,
                query.search.as_deref(),
                Some(tail.saturating_add(skip_recent)),
            )
            .await?;
        Ok(select_backlog_page(result.lines, skip_recent, tail))
    }

    async fn live_tail(
        &self,
        deployment: Deployment,
        project: Project,
        query: LogQuery,
    ) -> Result<LogEventStream> {
        use aws_sdk_cloudwatchlogs::types::StartLiveTailResponseStream;

        let prefix = self.stream_prefix(&deployment, &project);
        let response = self
            .client
            .start_live_tail()
            .log_group_identifiers(&self.log_group_arn)
            .log_stream_name_prefixes(&prefix)
            .send()
            .await
            .context("Failed to start CloudWatch Live Tail")?;
        let mut response_stream = response.response_stream;
        let (live_tx, mut live_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let event = match response_stream.recv().await {
                    Ok(Some(event)) => Ok(event),
                    Ok(None) => break,
                    Err(error)
                        if error
                            .as_service_error()
                            .is_some_and(|service| service.is_session_timeout_exception()) =>
                    {
                        break;
                    }
                    Err(error) => Err(anyhow::anyhow!(
                        "CloudWatch Live Tail stream failed: {error}"
                    )),
                };
                let failed = event.is_err();
                if live_tx.send(event).is_err() || failed {
                    break;
                }
            }
        });
        let initial = self.backlog(&deployment, &project, &query).await?;
        let levels = query.levels.clone();
        let search = query.search.clone();
        let timestamps = query.timestamps;

        let log_group = self.log_group.clone();
        let stream = async_stream::try_stream! {
            let backlog_count = initial.len();
            let mut backlog_occurrences: HashMap<CloudWatchLine, usize> = HashMap::new();
            for line in &initial {
                *backlog_occurrences.entry(line.clone()).or_insert(0) += 1;
            }
            for line in initial {
                let level = line.level().to_string();
                yield LogEvent::Line { text: line.render(timestamps), level };
            }
            yield LogEvent::BacklogLoaded { count: backlog_count };

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
                        let Some(line) = CloudWatchLine::from_live(event) else {
                            continue;
                        };
                        if let Some(remaining) = backlog_occurrences.get_mut(&line) {
                            if *remaining > 0 {
                                *remaining -= 1;
                                continue;
                            }
                        }
                        if !line.matches(&levels, search.as_deref()) {
                            continue;
                        }
                        let level = line.level().to_string();
                        yield LogEvent::Line { text: line.render(timestamps), level };
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

        let lines = self.backlog(deployment, project, &query).await?;
        if lines.is_empty() {
            return Ok(status_stream(self.empty_status(deployment)));
        }
        let timestamps = query.timestamps;
        Ok(futures::stream::iter(lines.into_iter().map(move |line| {
            let level = line.level().to_string();
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
        let result = self
            .scan(
                &prefix,
                query.start_time,
                query.end_time,
                &query.levels,
                query.search.as_deref(),
                None,
            )
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
    fn filtered_line_uses_the_shared_classifier_and_search() {
        let line = CloudWatchLine {
            timestamp_millis: 1_700_000_000_000,
            ingestion_time_millis: 1_700_000_000_100,
            log_stream: "rise/project/deployment/app/task".into(),
            message: "ERROR Database Unavailable".into(),
        };
        assert_eq!(line.level(), "error");
        assert!(line.matches(&["error".into()], Some("database")));
        assert!(!line.matches(&["warn".into()], Some("database")));
        assert!(!line.matches(&[], Some("missing")));
    }

    #[test]
    fn render_uses_the_cloudwatch_event_timestamp() {
        let line = CloudWatchLine {
            timestamp_millis: 1_700_000_000_000,
            ingestion_time_millis: 1_700_000_000_100,
            log_stream: "stream".into(),
            message: "hello".into(),
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
    fn backlog_pagination_skips_newest_events_before_sorting_the_page() {
        let line = |timestamp_millis, message: &str| CloudWatchLine {
            timestamp_millis,
            ingestion_time_millis: timestamp_millis,
            log_stream: "stream".into(),
            message: message.into(),
        };
        let newest_first = vec![
            line(3, "third"),
            line(2, "second-b"),
            line(2, "second-a"),
            line(1, "first"),
        ];

        let page = select_backlog_page(newest_first, 1, 2);
        assert_eq!(
            page.into_iter()
                .map(|line| line.message)
                .collect::<Vec<_>>(),
            vec!["second-a", "second-b"]
        );
    }
}
