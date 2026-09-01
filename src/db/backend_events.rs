//! Backend-originated events, forwarded into the deployment log.
//!
//! These are backend-owned details about a deployment — an ECS service saying it
//! cannot place a task, Kubernetes saying a pod will not schedule, or a
//! controller explaining that it represented a requested resource with a
//! different effective value. They explain things the periodic observation
//! cannot.
//!
//! Forwarded rather than trusted as the source of truth. A stream can drop and a
//! poll can overlap, so these enrich the level-triggered observations rather
//! than replacing them.

use anyhow::{Context, Result};
use rise_backend_core::events::{EventKind, EventSource, ForwardedEvent};
use sqlx::PgPool;
use uuid::Uuid;

/// Forward events, skipping any already recorded.
///
/// Returns how many were new. Conflicts are silently skipped rather than
/// counted as failures: overlapping polls re-reporting the same occurrence is
/// the expected case, not an error, and is exactly what `dedupe_key` exists to
/// absorb.
pub async fn forward(
    pool: &PgPool,
    deployment_id: Uuid,
    source: EventSource,
    events: &[ForwardedEvent],
) -> Result<u64> {
    let mut inserted = 0;

    for event in events {
        let affected = sqlx::query!(
            r#"
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, subject,
                message, attributes, dedupe_key
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (deployment_id, kind, dedupe_key)
                WHERE dedupe_key IS NOT NULL
                DO NOTHING
            "#,
            deployment_id,
            event.occurred_at,
            EventKind::BackendEvent.as_str(),
            event.severity.as_str(),
            source.as_str(),
            event.subject,
            // The runtime's own wording, bounded by the column's CHECK. Kept as
            // the message rather than an attribute: it is the event, not detail
            // about it.
            truncate(&event.message, 4096),
            event.attributes,
            event.dedupe_key,
        )
        .execute(pool)
        .await
        .context("Failed to forward backend event")?
        .rows_affected();

        inserted += affected;
    }

    Ok(inserted)
}

/// Runtime messages are unbounded and the column is not. Cut on a character
/// boundary so a multi-byte message cannot produce invalid UTF-8.
fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rise_backend_core::events::EventSeverity;

    async fn seed(pool: &PgPool) -> Uuid {
        let user: Uuid =
            sqlx::query_scalar("INSERT INTO users (email) VALUES ('be@test.local') RETURNING id")
                .fetch_one(pool)
                .await
                .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects (name, status, access_class, owner_user_id)
             VALUES ('be-test', 'Stopped', 'public', $1) RETURNING id",
        )
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO deployments (deployment_id, project_id, created_by_id, status)
             VALUES ('20260831-000002', $1, $2, 'Healthy') RETURNING id",
        )
        .bind(project)
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn event(key: &str, message: &str) -> ForwardedEvent {
        ForwardedEvent {
            dedupe_key: key.to_string(),
            occurred_at: chrono::Utc::now(),
            severity: EventSeverity::Warning,
            message: message.to_string(),
            subject: Some("web-abc".to_string()),
            attributes: serde_json::json!({ "reason": "Unhealthy" }),
        }
    }

    /// The guarantee everything else rests on: every backend re-reads the same
    /// window on every tick, so forwarding must be idempotent or the timeline
    /// fills with copies.
    #[sqlx::test]
    async fn re_forwarding_the_same_event_records_it_once(pool: PgPool) {
        let id = seed(&pool).await;
        let batch = vec![event("ecs-evt-1", "unable to place a task")];

        assert_eq!(
            forward(&pool, id, EventSource::Ecs, &batch).await.unwrap(),
            1,
        );
        // The next tick offers the same window again.
        assert_eq!(
            forward(&pool, id, EventSource::Ecs, &batch).await.unwrap(),
            0,
        );

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM deployment_events WHERE deployment_id = $1 AND kind = 'backend_event'",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    /// Distinct occurrences must not be collapsed — the dedupe is on the key,
    /// not on the message, so a recurring condition still shows every time
    /// Kubernetes counts it afresh.
    #[sqlx::test]
    async fn different_keys_are_different_events(pool: PgPool) {
        let id = seed(&pool).await;
        forward(
            &pool,
            id,
            EventSource::Kubernetes,
            &[event("u1:1", "probe failed"), event("u1:7", "probe failed")],
        )
        .await
        .unwrap();

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM deployment_events WHERE deployment_id = $1 AND kind = 'backend_event'",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 2);
    }

    /// Runtime messages are unbounded and the column is not. A message that
    /// would breach the CHECK must be cut rather than fail the forward — and
    /// cut on a character boundary, or the cut itself fails.
    #[sqlx::test]
    async fn an_oversized_message_is_truncated_not_rejected(pool: PgPool) {
        let id = seed(&pool).await;
        let mut e = event("big", &"é".repeat(4000));
        e.message.push_str(&"é".repeat(1000));

        forward(&pool, id, EventSource::Ecs, &[e]).await.unwrap();

        let stored: String = sqlx::query_scalar(
            "SELECT message FROM deployment_events WHERE deployment_id = $1 AND kind = 'backend_event'",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(stored.len() <= 4096);
        assert!(!stored.is_empty());
    }
}
