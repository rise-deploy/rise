//! Queries over the deployment event log.
//!
//! Events are append-only: nothing here updates or deletes a row, and
//! nothing prunes them yet — the table grows with the deployments it records.

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
    /// What inside the deployment the event is about, or `None` for the
    /// deployment itself.
    pub subject: Option<String>,
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
/// `after` continues a previous page. `kinds`, `severities` and `subject`
/// filter; empty or `None` means no filter on that dimension.
///
/// `severities` is the exact set to accept, built by
/// [`EventSeverity::at_least`](rise_backend_core::events::EventSeverity::at_least)
/// — the ordering lives there and is not restated here.
pub async fn list_for_deployment(
    pool: &PgPool,
    deployment_id: Uuid,
    limit: i64,
    after: Option<EventCursor>,
    kinds: &[String],
    severities: Option<&[&str]>,
    subject: Option<&str>,
) -> Result<Vec<DeploymentEvent>> {
    let events = sqlx::query_as!(
        DeploymentEvent,
        r#"
        SELECT
            id, deployment_id, occurred_at, recorded_at,
            kind, severity, source, subject, message,
            attributes as "attributes: serde_json::Value"
        FROM deployment_events
        WHERE deployment_id = $1
          AND ($2::timestamptz IS NULL OR (recorded_at, id) < ($2, $3))
          AND ($4::text[] IS NULL OR kind = ANY($4))
          AND ($5::text[] IS NULL OR severity = ANY($5))
          AND ($6::text IS NULL OR subject = $6)
        ORDER BY recorded_at DESC, id DESC
        LIMIT $7
        "#,
        deployment_id,
        after.map(|c| c.recorded_at),
        after.map(|c| c.id).unwrap_or_default(),
        if kinds.is_empty() { None } else { Some(kinds) },
        severities as Option<&[&str]>,
        subject,
        limit,
    )
    .fetch_all(pool)
    .await
    .context("Failed to list deployment events")?;

    Ok(events)
}
