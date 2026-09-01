//! Reading a deployment's logs from the Kubernetes API.
//!
//! One HTTP request fans out to one log stream per Pod; the shared merge in
//! `rise_backend_core::logs` puts them back into a single ordered stream. The
//! kubelet returns raw bytes with no level metadata, so each line is classified
//! by the shared heuristic classifier.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use rise_backend_core::logs::{
    classify_log_line, decode_log_cursor, distinct_log_id, distinct_log_ids_from_newest,
    encode_log_cursor, is_followable_status, line_matches_search, log_cursor_signature,
    select_recent_page, split_timestamped_log_line, stable_log_id, status_stream, BoundedNewest,
    LogEvent, LogEventStream, LogQuery, LogStatus, LogStatusReason, LogVolumeQuery,
    LogVolumeResponse, MergeKey, RuntimeLogBackend, TimestampedLineStream, HEURISTIC_LEVELS,
};
use rise_backend_core::models::{Deployment, DeploymentStatus, Project};

use crate::config::KubernetesLogBackendSettings;
use crate::resource_builder::{ResourceBuilder, LABEL_CONTAINER};

/// How many of a deployment's Pods the Kubernetes backend reads at once.
///
/// One HTTP request fans out to one log stream per Pod. Uncapped, a request
/// against a large deployment opens that many simultaneous streams against the
/// API server, and concurrent requests multiply it.
const KUBERNETES_POD_READ_FANOUT: usize = 8;

pub struct KubernetesLogBackend {
    kube_client: kube::Client,
    config: KubernetesLogBackendSettings,
}

impl KubernetesLogBackend {
    pub fn new(kube_client: kube::Client, config: KubernetesLogBackendSettings) -> Self {
        Self {
            kube_client,
            config,
        }
    }
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

/// One kubelet line that survived filtering, carrying what the merge, the page
/// and the rendered event each need from it.
struct KubernetesLine {
    timestamp: DateTime<Utc>,
    id: String,
    rendered: String,
    level: &'static str,
    container: Option<String>,
}

/// What a reader does with one raw kubelet line.
enum KubernetesLineOutcome {
    /// At or past the requested end: this Pod has nothing further to offer.
    EndOfRange,
    Skip,
    Keep(KubernetesLine),
}

/// The per-line work that is identical whether a Pod is being followed or read
/// as a finite page, factored out so the two paths cannot drift apart.
#[derive(Clone)]
struct KubernetesLineFilter {
    namespace: String,
    levels: Vec<String>,
    search: Option<String>,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    render_timestamps: bool,
}

impl KubernetesLineFilter {
    fn apply(
        &self,
        pod_name: &str,
        container: &Option<String>,
        line: String,
    ) -> KubernetesLineOutcome {
        let Some((timestamp, content, timestamp_text)) = split_timestamped_log_line(&line) else {
            return KubernetesLineOutcome::Skip;
        };
        if self.end_time.is_some_and(|end| timestamp >= end) {
            return KubernetesLineOutcome::EndOfRange;
        }
        if content.is_empty() {
            return KubernetesLineOutcome::Skip;
        }
        if self.start_time.is_some_and(|start| timestamp < start) {
            return KubernetesLineOutcome::Skip;
        }
        let level = classify_log_line(content);
        if !self.levels.is_empty() && !self.levels.iter().any(|wanted| wanted == level) {
            return KubernetesLineOutcome::Skip;
        }
        if !line_matches_search(content, self.search.as_deref()) {
            return KubernetesLineOutcome::Skip;
        }
        // The Pod name is part of the identity: two Pods can emit the same text
        // in the same nanosecond and remain two distinct lines.
        let id = stable_log_id(
            "kubernetes",
            [
                self.namespace.as_bytes(),
                pod_name.as_bytes(),
                timestamp_text.as_bytes(),
                content.as_bytes(),
            ],
        );
        let content = content.to_string();
        let rendered = if self.render_timestamps {
            line
        } else {
            content
        };
        KubernetesLineOutcome::Keep(KubernetesLine {
            timestamp,
            id,
            rendered,
            level,
            container: container.clone(),
        })
    }
}

/// One Pod's lines, filtered, in the order the kubelet returned them.
///
/// `raw_high_water` collects the largest number of raw lines any one Pod
/// returned. That is what tells the pager whether a wider window would find
/// anything: the Pod that filled the tail it was given is the one still holding
/// history behind the page.
fn kubernetes_pod_lines(
    log_stream: impl futures::io::AsyncBufRead + Send + Unpin + 'static,
    pod_name: String,
    container: Option<String>,
    filter: KubernetesLineFilter,
    raw_high_water: Arc<AtomicUsize>,
) -> futures::stream::BoxStream<'static, Result<KubernetesLine>> {
    async_stream::stream! {
        use futures::AsyncBufReadExt;
        let mut raw_count = 0usize;
        let mut lines = futures::io::BufReader::new(log_stream).lines();
        while let Some(line) = lines.next().await {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    yield Err(anyhow::anyhow!(
                        "Log stream error for pod {pod_name}: {error}"
                    ));
                    break;
                }
            };
            if line.is_empty() {
                continue;
            }
            raw_count = raw_count.saturating_add(1);
            raw_high_water.fetch_max(raw_count, Ordering::Relaxed);
            match filter.apply(&pod_name, &container, line) {
                KubernetesLineOutcome::EndOfRange => break,
                KubernetesLineOutcome::Skip => continue,
                KubernetesLineOutcome::Keep(kept) => yield Ok(kept),
            }
        }
    }
    .boxed()
}

/// One Pod's lines as follow events, each stamped with the timestamp the live
/// merge orders on.
fn kubernetes_pod_line_stream(
    log_stream: impl futures::io::AsyncBufRead + Send + Unpin + 'static,
    pod_name: String,
    container: Option<String>,
    filter: KubernetesLineFilter,
) -> TimestampedLineStream {
    kubernetes_pod_lines(
        log_stream,
        pod_name,
        container,
        filter,
        Arc::new(AtomicUsize::new(0)),
    )
    .scan(HashMap::new(), |seen_ids, item| {
        let mapped = item.map(|line| {
            let id = distinct_log_id(seen_ids, line.id);
            (
                line.timestamp,
                LogEvent::Line {
                    id,
                    text: line.rendered,
                    level: line.level.to_string(),
                    container: line.container,
                },
            )
        });
        futures::future::ready(Some(mapped))
    })
    .boxed()
}

/// A Pod that could not be opened surfaces against its own name and leaves the
/// rest of the deployment streaming.
fn kubernetes_source_error(pod_name: &str, error: anyhow::Error) -> TimestampedLineStream {
    let message = format!("Log stream error for pod {pod_name}: {error:?}");
    futures::stream::once(async move { Err(anyhow::anyhow!(message)) }).boxed()
}

/// Attach a continuation cursor to each followed line.
///
/// `offset` counts what has been shown and `end_nanos` bounds the range it was
/// drawn from, so the cursor tracks the newest timestamp seen across every Pod
/// rather than whichever Pod happened to deliver last — otherwise a slow Pod
/// catching up would walk the boundary backwards.
fn kubernetes_follow_cursors(
    merged: impl Stream<Item = Result<(DateTime<Utc>, LogEvent)>> + Send + 'static,
    signature: String,
    skip_recent: usize,
    max_tail: usize,
    start_nanos: Option<i64>,
) -> LogEventStream {
    async_stream::stream! {
        let mut merged = std::pin::pin!(merged);
        let mut emitted_count = 0usize;
        let mut newest_nanos: Option<i64> = None;
        while let Some(item) = merged.next().await {
            let (timestamp, event) = match item {
                Ok(item) => item,
                // One Pod failing is not the request failing: report it and let
                // the others keep streaming.
                Err(error) => {
                    yield Err(error);
                    continue;
                }
            };
            emitted_count = emitted_count.saturating_add(1);
            newest_nanos = timestamp
                .timestamp_nanos_opt()
                .and_then(|nanos| nanos.checked_add(1))
                .max(newest_nanos);
            yield Ok(event);
            match encode_log_cursor(&TailLogCursor {
                version: 3,
                signature: signature.clone(),
                offset: skip_recent.saturating_add(emitted_count).min(max_tail),
                raw_tail: max_tail,
                start_nanos,
                end_nanos: newest_nanos,
            }) {
                Ok(next_cursor) => yield Ok(LogEvent::CursorUpdated { next_cursor }),
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }
    }
    .boxed()
}

/// Cut one page out of the lines every Pod contributed.
///
/// `skip_recent` counts lines already shown across the whole deployment, so the
/// page is taken from the merged sequence and not from any one Pod's tail —
/// that is what keeps paging back from repeating a line it already showed or
/// stepping over one it has not.
///
/// `max_tail` is the ceiling on how far back paging can reach, applied to the
/// merged sequence so a deployment's reach does not grow with its Pod count.
fn kubernetes_merged_page(
    mut merged: Vec<KubernetesLine>,
    page_size: usize,
    skip_recent: usize,
    max_tail: usize,
) -> (Vec<KubernetesLine>, bool) {
    // Stable, so lines sharing a timestamp keep the order their own Pod emitted
    // them in.
    merged.sort_by_key(|line| line.timestamp);
    if merged.len() > max_tail {
        merged.drain(..merged.len() - max_tail);
    }

    let base_ids = merged
        .iter()
        .map(|line| line.id.clone())
        .collect::<Vec<_>>();
    for (line, distinct_id) in merged
        .iter_mut()
        .zip(distinct_log_ids_from_newest(&base_ids))
    {
        line.id = distinct_id;
    }

    select_recent_page(merged, page_size, skip_recent)
}

/// Read every Pod of a deployment and keep the newest `max_tail` lines across
/// all of them.
///
/// The bound is global, not per Pod. Holding `max_tail` lines for each Pod and
/// merging afterwards would make one request's memory scale with the
/// deployment's replica count — a fifteen-Pod deployment asking for a date
/// range would buffer fifteen times what it can return.
///
/// Reads run concurrently but capped, so one request cannot open an unbounded
/// number of simultaneous log streams against the API server.
///
/// Returns the kept lines in chronological order, the largest raw line count
/// any single Pod returned, and any Pod that could not be read in full.
async fn read_kubernetes_pods(
    pod_api: &kube::Api<k8s_openapi::api::core::v1::Pod>,
    sources: Vec<(String, Option<String>)>,
    log_params: &kube::api::LogParams,
    filter: &KubernetesLineFilter,
    max_tail: usize,
) -> (Vec<KubernetesLine>, usize, Vec<anyhow::Error>) {
    let raw_high_water = Arc::new(AtomicUsize::new(0));
    let readers = sources
        .into_iter()
        .enumerate()
        .map(|(pod_index, (pod_name, container))| {
            let pod_api = pod_api.clone();
            let log_params = log_params.clone();
            let filter = filter.clone();
            let raw_high_water = Arc::clone(&raw_high_water);
            async_stream::stream! {
                let log_stream = match pod_api.log_stream(&pod_name, &log_params).await {
                    Ok(log_stream) => log_stream,
                    Err(error) => {
                        yield Err(anyhow::Error::new(error)
                            .context(format!("reading logs for pod {pod_name}")));
                        return;
                    }
                };
                let mut sequence = 0usize;
                let lines =
                    kubernetes_pod_lines(log_stream, pod_name, container, filter, raw_high_water);
                futures::pin_mut!(lines);
                while let Some(line) = lines.next().await {
                    match line {
                        Ok(line) => {
                            let key = MergeKey {
                                timestamp: line.timestamp,
                                source: pod_index,
                                sequence,
                            };
                            sequence = sequence.saturating_add(1);
                            yield Ok((key, line));
                        }
                        Err(error) => yield Err(error),
                    }
                }
            }
            .boxed()
        })
        .collect::<Vec<_>>();

    let mut newest = BoundedNewest::new(max_tail);
    let mut errors = Vec::new();
    let mut merged = futures::stream::iter(readers).flatten_unordered(KUBERNETES_POD_READ_FANOUT);
    while let Some(item) = merged.next().await {
        match item {
            Ok((key, line)) => newest.push(key, line),
            // A Pod that could not be read says so against its own name; the
            // lines the others produced are still worth returning.
            Err(error) => errors.push(error),
        }
    }

    (
        newest.into_chronological(),
        raw_high_water.load(Ordering::Relaxed),
        errors,
    )
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

#[async_trait]
impl RuntimeLogBackend for KubernetesLogBackend {
    fn backend_kind(&self) -> &'static str {
        "kubernetes"
    }

    fn levels(&self) -> &'static [&'static str] {
        HEURISTIC_LEVELS
    }

    fn supports_volume(&self) -> bool {
        false
    }

    fn max_tail(&self) -> Option<i64> {
        Some(self.config.max_tail_lines.max(1))
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

        // Each container of a deployment gets its own Pod, so a deployment with
        // more than one container is more than one log source. A Pod's
        // `rise.dev/container` label attributes every line the kubelet returns
        // for it.
        let sources = pods
            .items
            .iter()
            .map(|pod| {
                let pod_name = pod.metadata.name.clone().context("Pod name not found")?;
                let container = pod
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(LABEL_CONTAINER))
                    .cloned();
                Ok((pod_name, container))
            })
            .collect::<Result<Vec<_>>>()?;

        if sources.is_empty() {
            return Ok(status_stream(LogStatus {
                reason: LogStatusReason::HistoricalBackendNotConfigured,
                message: Some(
                    "No active deployment pod was found and historical logs are not configured."
                        .into(),
                ),
                retention_hint: None,
            }));
        }

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

        let filter = KubernetesLineFilter {
            namespace: namespace.clone(),
            levels: query.levels.clone(),
            search: query.search.clone(),
            start_time,
            end_time,
            render_timestamps: query.timestamps,
        };

        if follow {
            // Arrival order, for the reason `merge_container_streams`
            // documents: a live merge cannot be globally ordered without
            // holding lines back, and holding them back defeats following.
            let mut streams = Vec::with_capacity(sources.len());
            for (pod_name, container) in sources {
                match pod_api.log_stream(&pod_name, &log_params).await {
                    Ok(log_stream) => streams.push(kubernetes_pod_line_stream(
                        log_stream,
                        pod_name,
                        container,
                        filter.clone(),
                    )),
                    Err(error) => streams.push(kubernetes_source_error(&pod_name, error.into())),
                }
            }
            return Ok(kubernetes_follow_cursors(
                futures::stream::select_all(streams),
                signature,
                skip_recent,
                max_tail,
                start_nanos,
            ));
        }

        let (merged, raw_count, errors) =
            read_kubernetes_pods(&pod_api, sources, &log_params, &filter, max_tail).await;
        let (page, has_older_in_window) =
            kubernetes_merged_page(merged, page_size, skip_recent, max_tail);
        let has_more = kubernetes_page_has_more(
            has_older_in_window,
            end_time.is_some(),
            requested_tail,
            raw_count,
            raw_tail,
            max_tail,
        );
        let next_cursor = has_more
            .then(|| {
                encode_log_cursor(&TailLogCursor {
                    version: 3,
                    signature,
                    offset: skip_recent.saturating_add(page.len()),
                    raw_tail: requested_tail.map_or(max_tail, |tail| tail as usize),
                    start_nanos,
                    end_nanos,
                })
            })
            .transpose()?;

        Ok(async_stream::stream! {
            for error in errors {
                yield Err(error);
            }
            for line in page {
                yield Ok(LogEvent::Line {
                    id: line.id,
                    text: line.rendered,
                    level: line.level.to_string(),
                    container: line.container,
                });
            }
            yield Ok(LogEvent::PageLoaded { next_cursor });
        }
        .boxed())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A kubelet line as the Pod hands it over: RFC3339 timestamp, a space,
    /// then the content.
    fn kubelet_line(second: u32, text: &str) -> String {
        format!("2026-08-31T12:00:{second:02}.000000000Z {text}")
    }

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

    fn test_filter() -> KubernetesLineFilter {
        KubernetesLineFilter {
            namespace: "rise-demo".into(),
            levels: vec![],
            search: None,
            start_time: None,
            end_time: None,
            render_timestamps: false,
        }
    }

    /// Keep whatever the filter kept, or panic — every caller here feeds it a
    /// line it expects to survive.
    fn kept_line(
        filter: &KubernetesLineFilter,
        pod: &str,
        second: u32,
        text: &str,
    ) -> KubernetesLine {
        match filter.apply(pod, &Some("web".into()), kubelet_line(second, text)) {
            KubernetesLineOutcome::Keep(line) => line,
            _ => panic!("expected {text:?} to be kept"),
        }
    }

    fn rendered(page: &[KubernetesLine]) -> Vec<String> {
        page.iter().map(|line| line.rendered.clone()).collect()
    }

    #[test]
    fn merged_page_orders_across_pods_not_within_them() {
        // Two Pods, each ordered on its own, whose lines interleave in time.
        // Reading either Pod alone tells you nothing about the other's timing.
        let filter = test_filter();
        let merged = vec![
            kept_line(&filter, "web-a", 1, "a-first"),
            kept_line(&filter, "web-a", 3, "a-second"),
            kept_line(&filter, "web-b", 2, "b-first"),
            kept_line(&filter, "web-b", 4, "b-second"),
        ];

        let (page, has_older) = kubernetes_merged_page(merged, 10, 0, 1_000);

        assert_eq!(
            rendered(&page),
            vec!["a-first", "b-first", "a-second", "b-second"]
        );
        assert!(!has_older);
    }

    #[test]
    fn paging_back_across_pods_neither_repeats_nor_skips_a_line() {
        // The failure this guards against is silent: an offset that means
        // "lines of this Pod" rather than "lines of this deployment" drops or
        // doubles lines at every page boundary, and the result still looks
        // like a plausible log.
        let filter = test_filter();
        // Nine lines alternating between two Pods. Each page is served from a
        // fresh read, exactly as a real continuation request is.
        let deployment_lines = || {
            (0..9)
                .map(|n| {
                    let pod = if n % 2 == 0 { "web-a" } else { "web-b" };
                    kept_line(&filter, pod, n, &format!("line-{n}"))
                })
                .collect::<Vec<_>>()
        };

        // Walk backwards the way the cursor does: each page skips everything
        // the pages before it emitted.
        let mut seen_from_newest: Vec<String> = Vec::new();
        let mut skip_recent = 0usize;
        loop {
            let (page, has_older) =
                kubernetes_merged_page(deployment_lines(), 4, skip_recent, 1_000);
            skip_recent += page.len();
            let mut page = rendered(&page);
            page.extend(seen_from_newest);
            seen_from_newest = page;
            if !has_older {
                break;
            }
        }

        assert_eq!(
            seen_from_newest,
            (0..9).map(|n| format!("line-{n}")).collect::<Vec<_>>(),
            "every line exactly once, in order"
        );
    }

    #[test]
    fn merged_page_reach_does_not_grow_with_pod_count() {
        // `max_tail` bounds how far back a deployment can be paged. Applying it
        // per Pod would let a ten-Pod deployment reach ten times as far.
        let filter = test_filter();
        let merged = (0..10)
            .map(|n| {
                let pod = if n % 2 == 0 { "web-a" } else { "web-b" };
                kept_line(&filter, pod, n, &format!("line-{n}"))
            })
            .collect();

        let (page, _) = kubernetes_merged_page(merged, 10, 0, 4);

        assert_eq!(
            rendered(&page),
            vec!["line-6", "line-7", "line-8", "line-9"],
            "the four newest across both Pods"
        );
    }

    #[test]
    fn identical_lines_from_two_pods_stay_two_lines() {
        // Same text, same nanosecond, different Pod. The Pod name is part of
        // the identity, so neither one deduplicates the other away.
        let filter = test_filter();
        let merged = vec![
            kept_line(&filter, "web-a", 7, "listening on :8080"),
            kept_line(&filter, "web-b", 7, "listening on :8080"),
        ];

        let (page, _) = kubernetes_merged_page(merged, 10, 0, 1_000);

        assert_eq!(page.len(), 2);
        assert_ne!(page[0].id, page[1].id);
    }

    #[test]
    fn filter_stops_a_pod_at_the_end_of_the_range() {
        let filter = KubernetesLineFilter {
            end_time: Some(
                DateTime::parse_from_rfc3339("2026-08-31T12:00:05Z")
                    .expect("valid timestamp")
                    .with_timezone(&Utc),
            ),
            ..test_filter()
        };

        assert!(matches!(
            filter.apply("web-a", &None, kubelet_line(5, "at the boundary")),
            KubernetesLineOutcome::EndOfRange
        ));
        assert!(matches!(
            filter.apply("web-a", &None, kubelet_line(4, "inside")),
            KubernetesLineOutcome::Keep(_)
        ));
    }

    #[test]
    fn filter_applies_level_and_search_before_the_merge() {
        // Filtering per Pod is what lets `skip_recent` count emitted lines: the
        // merged sequence must already be the one the reader sees.
        let filter = KubernetesLineFilter {
            levels: vec!["error".into()],
            ..test_filter()
        };
        assert!(matches!(
            filter.apply("web-a", &None, kubelet_line(1, "GET /health 200")),
            KubernetesLineOutcome::Skip
        ));
        assert!(matches!(
            filter.apply("web-a", &None, kubelet_line(1, "ERROR dial tcp: timeout")),
            KubernetesLineOutcome::Keep(_)
        ));

        let filter = KubernetesLineFilter {
            search: Some("timeout".into()),
            ..test_filter()
        };
        assert!(matches!(
            filter.apply("web-a", &None, kubelet_line(1, "GET /health 200")),
            KubernetesLineOutcome::Skip
        ));
        assert!(matches!(
            filter.apply("web-a", &None, kubelet_line(1, "dial tcp: timeout")),
            KubernetesLineOutcome::Keep(_)
        ));
    }

    #[test]
    fn filter_attributes_each_line_to_its_own_pods_container() {
        let filter = test_filter();
        let KubernetesLineOutcome::Keep(line) =
            filter.apply("worker-x", &Some("worker".into()), kubelet_line(1, "tick"))
        else {
            panic!("expected the line to be kept");
        };
        assert_eq!(line.container.as_deref(), Some("worker"));
        assert_eq!(line.rendered, "tick");
    }
}
