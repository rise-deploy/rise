//! Queries over the deployment event log (ADR-0006).
//!
//! Events are append-only: nothing here updates or deletes an individual row.
//! Retention operates in bulk and is the only writer that removes anything.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One recorded event, as stored.
///
/// `kind`, `severity` and `source` are strings here rather than the enums in
/// `rise_backend_core::events`: a row written by a newer server must still be
/// readable by an older one, so parsing is the caller's decision.
#[derive(Debug, Clone, Serialize)]
pub struct DeploymentEvent {
    pub id: i64,
    pub deployment_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub kind: String,
    pub severity: String,
    pub source: String,
    pub message: Option<String>,
    pub attributes: serde_json::Value,
}

/// A position in the log, for keyset pagination.
///
/// Pages are cut on `(recorded_at, id)` rather than on `occurred_at`, because
/// `occurred_at` can move backwards: an event derived from a late observation
/// carries an older timestamp than one already returned, and a cursor over it
/// would step past that event permanently.
#[derive(Debug, Clone, Copy)]
pub struct EventCursor {
    pub recorded_at: DateTime<Utc>,
    pub id: i64,
}

/// Read one deployment's events, newest first by write order.
///
/// `after` continues a previous page. `min_severity` and `kinds` filter; empty
/// or `None` means no filter on that dimension.
pub async fn list_for_deployment(
    pool: &PgPool,
    deployment_id: Uuid,
    limit: i64,
    after: Option<EventCursor>,
    kinds: &[String],
    min_severity_rank: Option<i32>,
) -> Result<Vec<DeploymentEvent>> {
    // `severity_rank` keeps the ordering in one place: the CASE mirrors
    // `EventSeverity`'s ordering, and a value outside it sorts as the loudest
    // so an unrecognised severity is never silently filtered out.
    let events = sqlx::query_as!(
        DeploymentEvent,
        r#"
        SELECT
            id, deployment_id, occurred_at, recorded_at,
            kind, severity, source, message,
            attributes as "attributes: serde_json::Value"
        FROM deployment_events
        WHERE deployment_id = $1
          AND ($2::timestamptz IS NULL OR (recorded_at, id) < ($2, $3))
          AND ($4::text[] IS NULL OR kind = ANY($4))
          AND ($5::int IS NULL OR CASE severity
                WHEN 'debug' THEN 0
                WHEN 'info' THEN 1
                WHEN 'warning' THEN 2
                WHEN 'error' THEN 3
                ELSE 3
              END >= $5)
        ORDER BY recorded_at DESC, id DESC
        LIMIT $6
        "#,
        deployment_id,
        after.map(|c| c.recorded_at),
        after.map(|c| c.id).unwrap_or_default(),
        if kinds.is_empty() { None } else { Some(kinds) },
        min_severity_rank,
        limit,
    )
    .fetch_all(pool)
    .await
    .context("Failed to list deployment events")?;

    Ok(events)
}
