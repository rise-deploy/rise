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

/// Where a deployment's image came from: the Git state the CLI reported when
/// the image was built, and the deployment it was created from, if any.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    pub git_revision: Option<String>,
    pub git_branch: Option<String>,
    pub git_dirty: Option<bool>,
    /// `deployment_id` of the deployment this one was created from (redeploy,
    /// rollback, promote).
    pub source_deployment_id: Option<String>,
}

/// Provenance for several deployments at once.
///
/// The Git fields come from the `Building` transition the CLI reports. A
/// deployment created from another one is not built, so its Git state is
/// inherited by following `rolled_back_from_deployment_id` back to the
/// deployment that was (bounded, so a malformed chain cannot run away).
/// Deployments with nothing known are absent from the map.
pub async fn provenance_batch(
    pool: &PgPool,
    deployment_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Provenance>> {
    let mut out: std::collections::HashMap<Uuid, Provenance> = std::collections::HashMap::new();
    if deployment_ids.is_empty() {
        return Ok(out);
    }

    let git_rows = sqlx::query!(
        r#"
        WITH RECURSIVE chain AS (
            SELECT d.id AS root, d.id AS cur, d.rolled_back_from_deployment_id AS parent, 0 AS depth
            FROM deployments d
            WHERE d.id = ANY($1)
            UNION ALL
            SELECT c.root, p.id, p.rolled_back_from_deployment_id, c.depth + 1
            FROM chain c
            JOIN deployments p ON p.id = c.parent
            WHERE c.depth < 20
        )
        SELECT DISTINCT ON (c.root)
            c.root AS "deployment_id!",
            e.attributes->>'git_revision' AS git_revision,
            e.attributes->>'git_branch' AS git_branch,
            CASE jsonb_typeof(e.attributes->'git_dirty')
                WHEN 'boolean' THEN (e.attributes->>'git_dirty')::boolean
            END AS git_dirty
        FROM chain c
        JOIN deployment_events e
            ON e.deployment_id = c.cur AND e.attributes ? 'git_revision'
        ORDER BY c.root, c.depth, e.recorded_at DESC, e.id DESC
        "#,
        deployment_ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to read deployment Git provenance")?;

    for row in git_rows {
        let entry = out.entry(row.deployment_id).or_default();
        entry.git_revision = row.git_revision;
        entry.git_branch = row.git_branch;
        entry.git_dirty = row.git_dirty;
    }

    let source_rows = sqlx::query!(
        r#"
        SELECT d.id AS "id!", s.deployment_id AS "source_deployment_id!"
        FROM deployments d
        JOIN deployments s ON s.id = d.rolled_back_from_deployment_id
        WHERE d.id = ANY($1)
        "#,
        deployment_ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to read source deployments")?;

    for row in source_rows {
        out.entry(row.id).or_default().source_deployment_id = Some(row.source_deployment_id);
    }

    Ok(out)
}
