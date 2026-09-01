//! The runtime log-reading contract shared by every deployment backend.
//!
//! A backend reads lines from its own runtime — the Kubernetes API, the Docker
//! daemon, CloudWatch, Loki — but the shape of a query, the merge across a
//! deployment's containers, the cursor encoding and the level classification are
//! the same everywhere. They live here so the backends do not depend on each
//! other to share them.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;

use crate::models::{Deployment, Project};

pub mod classify;
pub mod cursor;
pub mod merge;

pub use classify::{
    classify_log_line, line_matches_search, normalize_search, parse_duration_hint,
    truncate_for_error,
};
pub use cursor::{
    decode_log_cursor, distinct_log_ids_from_newest, encode_log_cursor, is_followable_status,
    log_cursor_signature, select_recent_page, stable_log_id, status_stream,
};
pub use merge::{
    distinct_log_id, merge_container_streams, split_timestamped_log_line, BoundedNewest, MergeKey,
};

/// Loki 3.x's documented `detected_level` value set. Passed through verbatim
/// to clients; the frontend renders each via its own palette entry.
pub const LOKI_LEVELS: &[&str] = &[
    "unknown", "trace", "debug", "info", "warn", "error", "critical", "fatal",
];

/// The three levels [`classify_log_line`] can emit. Each line classified by the
/// heuristic classifier lands in exactly one of these.
pub const HEURISTIC_LEVELS: &[&str] = &["info", "warn", "error"];

/// Server-side cap on `?tail=` passed to Loki's `query_range`. Advertised via
/// `LogsCapabilities::max_tail` so the frontend can mirror the limit.
pub const LOKI_MAX_TAIL: i64 = 5000;

/// Ceiling on `?tail=` for the Docker backend, and on any merge buffer built
/// without one.
///
/// The runtime backends fan out over a deployment's containers and merge the
/// results here, in this process, so the buffer is the server's memory and the
/// request that sizes it is a tenant's. Without a ceiling one request can name
/// a tail large enough that the daemon returns whole retained logs for every
/// container at once.
pub const DOCKER_MAX_TAIL: i64 = 100_000;

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

/// One container's lines, each carrying the timestamp the merge orders on.
pub type TimestampedLineStream =
    futures::stream::BoxStream<'static, Result<(DateTime<Utc>, LogEvent)>>;

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
