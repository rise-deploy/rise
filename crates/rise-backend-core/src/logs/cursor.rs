//! Opaque pagination cursors, stable line identity, and page selection.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use futures::StreamExt;
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use super::{LogEvent, LogEventStream, LogQuery, LogStatus};
use crate::logs::merge::distinct_log_id;
use crate::models::{Deployment, DeploymentStatus, Project};

pub fn encode_log_cursor<T: Serialize>(cursor: &T) -> Result<String> {
    let bytes = serde_json::to_vec(cursor).context("Failed to encode log cursor")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn decode_log_cursor<T: DeserializeOwned>(cursor: &str) -> Result<T> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .context("invalid log cursor encoding")?;
    serde_json::from_slice(&bytes).context("invalid log cursor payload")
}

pub fn log_cursor_signature(
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

pub fn stable_log_id<'a>(backend: &str, parts: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut digest = Sha256::new();
    digest.update(backend.as_bytes());
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    URL_SAFE_NO_PAD.encode(digest.finalize())
}

pub fn select_recent_page<T>(
    items: Vec<T>,
    page_size: usize,
    skip_recent: usize,
) -> (Vec<T>, bool) {
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

pub fn distinct_log_ids_from_newest(base_ids: &[String]) -> Vec<String> {
    let mut ids = vec![String::new(); base_ids.len()];
    let mut seen = HashMap::new();
    for (index, base_id) in base_ids.iter().enumerate().rev() {
        ids[index] = distinct_log_id(&mut seen, base_id.clone());
    }
    ids
}

pub fn is_followable_status(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Deploying
            | DeploymentStatus::Healthy
            | DeploymentStatus::Unhealthy
            | DeploymentStatus::Cancelling
            | DeploymentStatus::Terminating
    )
}

pub fn status_stream(status: LogStatus) -> LogEventStream {
    futures::stream::once(async move { Ok(LogEvent::Status(status)) }).boxed()
}
