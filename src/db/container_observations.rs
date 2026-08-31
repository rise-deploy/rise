//! The last observation of each replica, and the events implied by the next one.
//!
//! Reads and writes are a pair: a reconcile tick loads what was seen last time,
//! compares it against what it sees now (`rise_backend_core::observation`), and
//! writes both the resulting events and the new observation in one transaction.
//! Splitting those would let a crash between them either lose events or replay
//! them.

use anyhow::{Context, Result};
use rise_backend_core::events::EventSource;
use rise_backend_core::observation::{ContainerObservation, DerivedEvent, ObservedState};
use sqlx::PgPool;
use uuid::Uuid;

/// Every replica of a deployment as the backend last saw it.
pub async fn list_for_deployment(
    pool: &PgPool,
    deployment_id: Uuid,
) -> Result<Vec<ContainerObservation>> {
    let rows = sqlx::query!(
        r#"
        SELECT subject, container, instance, replica, state, started_at, finished_at,
               exit_code, restart_count, health, reason, image
        FROM deployment_container_observations
        WHERE deployment_id = $1
        ORDER BY container, replica NULLS LAST, subject
        "#,
        deployment_id,
    )
    .fetch_all(pool)
    .await
    .context("Failed to list container observations")?;

    Ok(rows
        .into_iter()
        .map(|r| ContainerObservation {
            subject: r.subject,
            container: r.container,
            instance: r.instance,
            replica: r.replica.map(|v| v as u32),
            state: parse_state(&r.state),
            started_at: r.started_at,
            finished_at: r.finished_at,
            exit_code: r.exit_code,
            restart_count: r.restart_count,
            health: r.health,
            reason: r.reason,
            image: r.image,
        })
        .collect())
}

/// A state the database holds but this build does not know is `Unknown`, not an
/// error: a row written by a newer server must stay readable, and `Unknown`
/// already means "observed, but not something we can reason about".
fn parse_state(value: &str) -> ObservedState {
    match value {
        "pending" => ObservedState::Pending,
        "running" => ObservedState::Running,
        "exited" => ObservedState::Exited,
        _ => ObservedState::Unknown,
    }
}

/// Replace the recorded observations and append the events they imply.
///
/// One transaction, because the two are the same fact seen twice: the events say
/// what changed, the observations become the baseline the *next* tick compares
/// against. A crash between them would either lose the events or re-derive them
/// from a stale baseline and write them twice.
///
/// `observations` is the complete current set for the deployment — replicas
/// absent from it are deleted, which is what makes a scale-down or a replaced
/// task disappear from the baseline as well as from the timeline.
///
/// `source` is the recording backend. The derivation does not carry it: what
/// changed is a property of the observations, while who saw it is a property of
/// whoever is writing them down.
pub async fn record_observations(
    pool: &PgPool,
    deployment_id: Uuid,
    source: EventSource,
    observations: &[ContainerObservation],
    events: &[DerivedEvent],
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("Failed to begin observation tx")?;

    for event in events {
        let attributes =
            serde_json::to_value(&event.attributes).unwrap_or_else(|_| serde_json::json!({}));
        sqlx::query!(
            r#"
            INSERT INTO deployment_events (
                deployment_id, occurred_at, kind, severity, source, subject, attributes
            )
            VALUES ($1, COALESCE($2, NOW()), $3, $4, $5, $6, $7)
            "#,
            deployment_id,
            event.occurred_at,
            event.kind.as_str(),
            event.severity.as_str(),
            source.as_str(),
            event.subject,
            attributes,
        )
        .execute(&mut *tx)
        .await
        .context("Failed to record derived deployment event")?;
    }

    // Subjects the backend no longer reports are gone: their termination has
    // just been recorded above, and keeping them would make the next tick
    // rediscover the same disappearance forever.
    let live: Vec<String> = observations.iter().map(|o| o.subject.clone()).collect();
    sqlx::query!(
        "DELETE FROM deployment_container_observations
         WHERE deployment_id = $1 AND subject <> ALL($2)",
        deployment_id,
        &live,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to prune vanished container observations")?;

    for o in observations {
        sqlx::query!(
            r#"
            INSERT INTO deployment_container_observations (
                deployment_id, subject, container, instance, replica, state,
                started_at, finished_at, exit_code, restart_count, health, reason,
                image, observed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, NOW())
            ON CONFLICT (deployment_id, subject) DO UPDATE SET
                container = EXCLUDED.container,
                instance = EXCLUDED.instance,
                replica = EXCLUDED.replica,
                state = EXCLUDED.state,
                started_at = EXCLUDED.started_at,
                finished_at = EXCLUDED.finished_at,
                exit_code = EXCLUDED.exit_code,
                restart_count = EXCLUDED.restart_count,
                health = EXCLUDED.health,
                reason = EXCLUDED.reason,
                image = EXCLUDED.image,
                observed_at = NOW()
            "#,
            deployment_id,
            o.subject,
            o.container,
            o.instance,
            o.replica.map(|v| v as i32),
            o.state.as_str(),
            o.started_at,
            o.finished_at,
            o.exit_code,
            o.restart_count,
            o.health,
            o.reason,
            o.image,
        )
        .execute(&mut *tx)
        .await
        .context("Failed to record container observation")?;
    }

    tx.commit().await.context("Failed to commit observations")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rise_backend_core::events::{EventKind, EventSeverity};
    use rise_backend_core::observation::derive_events;

    async fn seed_deployment(pool: &PgPool) -> Uuid {
        let user: Uuid =
            sqlx::query_scalar("INSERT INTO users (email) VALUES ('obs@test.local') RETURNING id")
                .fetch_one(pool)
                .await
                .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects (name, status, access_class, owner_user_id)
             VALUES ('obs-test', 'Stopped', 'public', $1) RETURNING id",
        )
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO deployments (deployment_id, project_id, created_by_id, status)
             VALUES ('20260831-000001', $1, $2, 'Healthy') RETURNING id",
        )
        .bind(project)
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn running(subject: &str, restarts: i64) -> ContainerObservation {
        let mut o = ContainerObservation::new(subject, "web", ObservedState::Running);
        o.restart_count = Some(restarts);
        o.replica = Some(0);
        o.image = Some("registry.test/web:v1".to_string());
        o
    }

    async fn events_for(pool: &PgPool, id: Uuid) -> Vec<(String, Option<String>)> {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT kind, subject FROM deployment_events
             WHERE deployment_id = $1 AND kind <> 'status_changed' ORDER BY id",
        )
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// The baseline survives a round trip, so the next tick compares against
    /// what was actually seen rather than a lossy copy of it.
    #[sqlx::test]
    async fn an_observation_round_trips(pool: PgPool) {
        let id = seed_deployment(&pool).await;
        let mut seen = running("web[0]", 2);
        seen.health = Some("healthy".to_string());
        seen.reason = Some("started".to_string());

        record_observations(&pool, id, EventSource::Docker, &[seen.clone()], &[])
            .await
            .unwrap();

        assert_eq!(list_for_deployment(&pool, id).await.unwrap(), vec![seen]);
    }

    /// The whole point of storing a baseline: two ticks apart, the counter
    /// advancing is the only evidence of an in-place restart.
    #[sqlx::test]
    async fn the_stored_baseline_is_what_the_next_tick_compares_against(pool: PgPool) {
        let id = seed_deployment(&pool).await;
        let now = chrono::Utc::now();

        record_observations(&pool, id, EventSource::Docker, &[running("web[0]", 0)], &[])
            .await
            .unwrap();

        let previous = list_for_deployment(&pool, id).await.unwrap();
        let current = vec![running("web[0]", 1)];
        let derived = derive_events(&previous, &current, now);
        assert_eq!(derived.len(), 1, "{derived:?}");
        assert_eq!(derived[0].kind, EventKind::ReplicaRestarted);

        record_observations(&pool, id, EventSource::Docker, &current, &derived)
            .await
            .unwrap();

        assert_eq!(
            events_for(&pool, id).await,
            vec![("replica_restarted".to_string(), Some("web[0]".to_string()))],
        );
        // And the baseline moved, so the same restart is not rediscovered.
        let again = derive_events(
            &list_for_deployment(&pool, id).await.unwrap(),
            &current,
            now,
        );
        assert!(again.is_empty(), "{again:?}");
    }

    /// A replica that stops being reported is deleted, not left behind — a
    /// lingering row would make every later tick rediscover the same
    /// disappearance.
    #[sqlx::test]
    async fn a_vanished_replica_leaves_the_baseline(pool: PgPool) {
        let id = seed_deployment(&pool).await;
        record_observations(
            &pool,
            id,
            EventSource::Ecs,
            &[running("task-a", 0), running("task-b", 0)],
            &[],
        )
        .await
        .unwrap();

        let previous = list_for_deployment(&pool, id).await.unwrap();
        let current = vec![running("task-b", 0)];
        let derived = derive_events(&previous, &current, chrono::Utc::now());

        record_observations(&pool, id, EventSource::Ecs, &current, &derived)
            .await
            .unwrap();

        let subjects: Vec<String> = list_for_deployment(&pool, id)
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.subject)
            .collect();
        assert_eq!(subjects, vec!["task-b".to_string()]);
        assert_eq!(
            events_for(&pool, id).await,
            vec![("replica_terminated".to_string(), Some("task-a".to_string()))],
        );
    }

    /// Severity rides along with the event: a non-zero exit is a fault, and the
    /// timeline colours it accordingly.
    #[sqlx::test]
    async fn a_derived_events_severity_is_stored(pool: PgPool) {
        let id = seed_deployment(&pool).await;
        let mut gone = ContainerObservation::new("web[0]", "web", ObservedState::Exited);
        gone.exit_code = Some(137);

        let derived = derive_events(&[running("web[0]", 0)], &[gone.clone()], chrono::Utc::now());
        assert_eq!(derived[0].severity, EventSeverity::Warning);

        record_observations(&pool, id, EventSource::Docker, &[gone], &derived)
            .await
            .unwrap();

        let severity: String = sqlx::query_scalar(
            "SELECT severity FROM deployment_events WHERE deployment_id = $1 AND kind <> 'status_changed'",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(severity, "warning");
    }
}
