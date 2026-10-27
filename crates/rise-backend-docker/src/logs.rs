//! Reading a deployment's logs from the Docker daemon.
//!
//! One request fans out to one `docker logs` stream per container; the shared
//! merge in `rise_backend_core::logs` puts them back into a single ordered
//! stream. The Engine API returns raw bytes with no level metadata, so each line
//! is classified by the shared heuristic classifier — the same as Kubernetes.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;

use rise_backend_core::logs::{
    classify_log_line, distinct_log_id, is_followable_status, line_matches_search,
    merge_container_streams, split_timestamped_log_line, stable_log_id, status_stream, LogEvent,
    LogEventStream, LogQuery, LogStatus, LogStatusReason, LogVolumeQuery, LogVolumeResponse,
    RuntimeLogBackend, TimestampedLineStream, DOCKER_MAX_TAIL, HEURISTIC_LEVELS,
};
use rise_backend_core::models::{Deployment, DeploymentStatus, Project};

use crate::labels::{
    self, SUFFIX_CONTAINER, SUFFIX_DEPLOYMENT_ID, SUFFIX_MANAGED_BY, SUFFIX_PROJECT,
};

/// Runtime log backend that streams directly from the Docker daemon.
///
/// Resolves the deployment's container(s) by the namespaced `deployment-id`
/// label Rise stamps (using the Docker controller's configured
/// `label_namespace`), then proxies `docker logs` into the shared
/// [`LogEventStream`]. Mirrors the Kubernetes backend's regex level
/// classification and lack of historical volume support.
pub struct DockerLogBackend {
    docker: bollard::Docker,
    /// Label namespace the Docker controller stamps containers with (e.g.
    /// `rise.dev`). Used to build the `<ns>/deployment-id` filter key.
    label_namespace: String,
}

impl DockerLogBackend {
    /// `label_namespace` must be the one the Docker reconciler stamps
    /// containers with, or no container will ever resolve.
    pub fn new(docker: bollard::Docker, label_namespace: String) -> Self {
        Self {
            docker,
            label_namespace,
        }
    }

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
        use bollard::query_parameters::ListContainersOptions;

        let ns = &self.label_namespace;
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
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
                filters: Some(filters),
                ..Default::default()
            }))
            .await?;

        // Docker ANDs repeated `label` filters, so the container filter — an
        // OR over names — is applied here rather than in the daemon query.
        let container_key = labels::ns_key(ns, SUFFIX_CONTAINER);
        let container_name = |summary: &bollard::models::ContainerSummary| {
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
            .partition(|c| c.state == Some(bollard::models::ContainerSummaryStateEnum::RUNNING));
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
        use bollard::query_parameters::LogsOptions;

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
        // The Docker API takes 32-bit epoch seconds; saturate rather than wrap.
        let since = i32::try_from(since).unwrap_or(i32::MAX);

        let options = LogsOptions {
            follow: query.follow && is_followable_status(&deployment.status),
            stdout: true,
            stderr: true,
            since,
            until: query
                .end_time
                .map(|end| i32::try_from(end.timestamp().saturating_add(1)).unwrap_or(i32::MAX))
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
