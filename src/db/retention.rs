//! Bounding the growth of deployment history.
//!
//! Two different problems, deliberately solved separately. Events accumulate
//! within a *single* deployment that keeps changing state, and are bounded by a
//! per-deployment cap. Deployments accumulate across a *project's* history, and
//! are bounded by age with explicit protections. Neither is a general "delete
//! old rows" sweep: what makes each safe is what it refuses to touch.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Drop a deployment's oldest events once it has more than `cap`.
///
/// The unbounded shape is one long-lived deployment that oscillates: every
/// `Healthy → Unhealthy → Healthy` writes two more rows, forever. Age cannot
/// bound that — those rows are recent — so the cap is per deployment.
///
/// The first event is always kept. It is the creation event, which carries what
/// the deployment was asked to be and anchors the start of its timeline; losing
/// it to make room for the ten-thousandth flap would be the wrong trade.
pub async fn trim_deployment_events(pool: &PgPool, cap: i64) -> Result<u64> {
    let deleted = sqlx::query!(
        r#"
        WITH over_cap AS (
            SELECT deployment_id
            FROM deployment_events
            GROUP BY deployment_id
            HAVING count(*) > $1
        ),
        cutoff AS (
            -- The id of the cap-th newest event for each over-cap deployment;
            -- everything below it goes. A lateral keeps this an index scan per
            -- deployment rather than a sort of the whole table.
            SELECT o.deployment_id, c.id AS cutoff_id, f.first_id
            FROM over_cap o
            CROSS JOIN LATERAL (
                SELECT id FROM deployment_events e
                WHERE e.deployment_id = o.deployment_id
                ORDER BY e.id DESC
                -- `OFFSET cap - 1` is the cap-th newest, and everything
                -- strictly below it goes — so exactly `cap` survive.
                OFFSET $1 - 1 LIMIT 1
            ) c
            CROSS JOIN LATERAL (
                SELECT min(id) AS first_id FROM deployment_events e
                WHERE e.deployment_id = o.deployment_id
            ) f
        )
        DELETE FROM deployment_events e
        USING cutoff
        WHERE e.deployment_id = cutoff.deployment_id
          AND e.id < cutoff.cutoff_id
          AND e.id > cutoff.first_id
        "#,
        cap,
    )
    .execute(pool)
    .await
    .context("Failed to trim deployment events")?
    .rows_affected();

    Ok(deleted)
}

/// Delete finished deployments older than `older_than`, keeping what is still
/// reachable.
///
/// Three things are never deleted, and each is a different kind of "still in
/// use":
///
/// - **Anything not terminal**, and anything flagged active. A deployment that
///   is still running is not history.
/// - **The newest `keep_primary_per_environment` *finished* deployments in each
///   environment's primary group**, regardless of age. These are what a
///   rollback reaches for, and an environment that has not deployed in a year
///   would otherwise lose the very deployment it is currently serving from.
///
/// Deleting a deployment takes its events and its environment-variable snapshot
/// with it (both `ON DELETE CASCADE`), which is what makes it un-rollback-able —
/// hence the protections rather than a bare age filter.
pub async fn delete_aged_deployments(
    pool: &PgPool,
    older_than: DateTime<Utc>,
    keep_primary_per_environment: i64,
    limit: i64,
) -> Result<u64> {
    let deleted = sqlx::query!(
        r#"
        WITH ranked_primary AS (
            -- Only deletable deployments are ranked. The active one and
            -- anything still running are protected outright below, so letting
            -- them occupy the kept slots would spend the budget on rows that
            -- were never at risk and leave no rollback history at all.
            SELECT
                d.id,
                row_number() OVER (
                    PARTITION BY d.environment_id
                    ORDER BY d.created_at DESC
                ) AS rn
            FROM deployments d
            JOIN environments e ON e.id = d.environment_id
            WHERE e.primary_deployment_group IS NOT NULL
              AND d.deployment_group = e.primary_deployment_group
              AND NOT d.is_active
              AND is_terminal(d.status)
        ),
        doomed AS (
            SELECT d.id
            FROM deployments d
            WHERE d.created_at < $1
              AND NOT d.is_active
              AND is_terminal(d.status)
              AND NOT EXISTS (
                  SELECT 1 FROM ranked_primary r
                  WHERE r.id = d.id AND r.rn <= $2
              )
            -- Oldest first, so a bounded pass makes progress from the far end
            -- rather than nibbling at whatever the planner happened to find.
            ORDER BY d.created_at
            LIMIT $3
        )
        DELETE FROM deployments d
        USING doomed
        WHERE d.id = doomed.id
        "#,
        older_than,
        keep_primary_per_environment,
        limit,
    )
    .execute(pool)
    .await
    .context("Failed to delete aged deployments")?
    .rows_affected();

    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// A project, an environment whose primary group is `default`, and a user.
    async fn seed(pool: &PgPool) -> (Uuid, Uuid) {
        let user: Uuid =
            sqlx::query_scalar("INSERT INTO users (email) VALUES ('r@test.local') RETURNING id")
                .fetch_one(pool)
                .await
                .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects (name, status, access_class, owner_user_id)
             VALUES ('retention-test', 'Stopped', 'public', $1) RETURNING id",
        )
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap();
        let env: Uuid = sqlx::query_scalar(
            "INSERT INTO environments (project_id, name, primary_deployment_group)
             VALUES ($1, 'production', 'default') RETURNING id",
        )
        .bind(project)
        .fetch_one(pool)
        .await
        .unwrap();
        (project, env)
    }

    #[allow(clippy::too_many_arguments)]
    async fn seed_deployment(
        pool: &PgPool,
        project: Uuid,
        env: Option<Uuid>,
        deployment_id: &str,
        group: &str,
        status: &str,
        is_active: bool,
        age_days: i64,
    ) -> Uuid {
        let user: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
        sqlx::query_scalar(
            "INSERT INTO deployments
                (deployment_id, project_id, created_by_id, status, deployment_group,
                 environment_id, is_active, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, NOW() - ($8 || ' days')::interval)
             RETURNING id",
        )
        .bind(deployment_id)
        .bind(project)
        .bind(user)
        .bind(status)
        .bind(group)
        .bind(env)
        .bind(is_active)
        .bind(age_days.to_string())
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn event_count(pool: &PgPool, id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM deployment_events WHERE deployment_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The cap bounds a flapping deployment without losing where it started.
    #[sqlx::test]
    async fn trimming_keeps_the_cap_and_always_the_first_event(pool: PgPool) {
        let (project, env) = seed(&pool).await;
        let id = seed_deployment(
            &pool,
            project,
            Some(env),
            "20260101-000001",
            "default",
            "Healthy",
            true,
            0,
        )
        .await;

        // The creation row is written by `create()`; here the deployment is
        // seeded directly, so stand in for it with an explicit first event.
        for i in 0..30 {
            sqlx::query(
                "INSERT INTO deployment_events
                    (deployment_id, occurred_at, kind, severity, source, attributes)
                 VALUES ($1, NOW(), 'status_changed', 'info', 'control-plane', $2)",
            )
            .bind(id)
            .bind(serde_json::json!({ "to": format!("e{i}") }))
            .execute(&pool)
            .await
            .unwrap();
        }
        let first: i64 =
            sqlx::query_scalar("SELECT min(id) FROM deployment_events WHERE deployment_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(trim_deployment_events(&pool, 10).await.unwrap(), 19);
        assert_eq!(event_count(&pool, id).await, 11, "the cap plus the first");

        let survivors: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM deployment_events WHERE deployment_id = $1 ORDER BY id",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            survivors[0], first,
            "the deployment still knows when it began"
        );

        // Idempotent: a second pass has nothing left to take.
        assert_eq!(trim_deployment_events(&pool, 10).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn trimming_leaves_deployments_under_the_cap_alone(pool: PgPool) {
        let (project, env) = seed(&pool).await;
        let id = seed_deployment(
            &pool,
            project,
            Some(env),
            "20260101-000002",
            "default",
            "Healthy",
            true,
            0,
        )
        .await;
        sqlx::query(
            "INSERT INTO deployment_events
                (deployment_id, occurred_at, kind, severity, source, attributes)
             VALUES ($1, NOW(), 'status_changed', 'info', 'control-plane', '{}'::jsonb)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(trim_deployment_events(&pool, 10).await.unwrap(), 0);
        assert_eq!(event_count(&pool, id).await, 1);
    }

    /// Age alone must not delete something still being served or rolled back to.
    #[sqlx::test]
    async fn deletion_spares_the_active_the_unfinished_and_the_recent_primaries(pool: PgPool) {
        let (project, env) = seed(&pool).await;
        let old = 365;

        let active = seed_deployment(
            &pool,
            project,
            Some(env),
            "a",
            "default",
            "Healthy",
            true,
            old,
        )
        .await;
        let unfinished = seed_deployment(
            &pool,
            project,
            Some(env),
            "u",
            "default",
            "Deploying",
            false,
            old,
        )
        .await;
        // Three finished primaries, all ancient. Keeping two means the oldest
        // is the only one eligible.
        let p1 = seed_deployment(
            &pool,
            project,
            Some(env),
            "p1",
            "default",
            "Superseded",
            false,
            old + 3,
        )
        .await;
        let p2 = seed_deployment(
            &pool,
            project,
            Some(env),
            "p2",
            "default",
            "Superseded",
            false,
            old + 2,
        )
        .await;
        let p3 = seed_deployment(
            &pool,
            project,
            Some(env),
            "p3",
            "default",
            "Superseded",
            false,
            old + 1,
        )
        .await;
        // A finished deployment outside the primary group has only age to
        // protect it — this is the shape retention exists to remove.
        let side = seed_deployment(
            &pool,
            project,
            Some(env),
            "s",
            "preview",
            "Superseded",
            false,
            old,
        )
        .await;
        // And a recent one, which age spares.
        let recent = seed_deployment(
            &pool,
            project,
            Some(env),
            "r",
            "preview",
            "Superseded",
            false,
            1,
        )
        .await;

        let cutoff = Utc::now() - chrono::Duration::days(90);
        let deleted = delete_aged_deployments(&pool, cutoff, 2, 500)
            .await
            .unwrap();
        assert_eq!(deleted, 2, "only the surplus primary and the old preview");

        let survivors: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM deployments")
            .fetch_all(&pool)
            .await
            .unwrap();
        let alive = |id: Uuid| survivors.contains(&id);

        assert!(alive(active), "the active deployment is not history");
        assert!(
            alive(unfinished),
            "a non-terminal deployment is not history"
        );
        assert!(alive(p2) && alive(p3), "the newest primaries are kept");
        assert!(alive(recent), "age spares the recent");
        assert!(!alive(p1), "the surplus primary goes");
        assert!(!alive(side), "a finished non-primary past the age goes");
    }

    /// Deleting a deployment takes its history with it, which is exactly why
    /// the protections above exist.
    #[sqlx::test]
    async fn deleting_a_deployment_takes_its_events(pool: PgPool) {
        let (project, env) = seed(&pool).await;
        let doomed = seed_deployment(
            &pool,
            project,
            Some(env),
            "d",
            "preview",
            "Superseded",
            false,
            400,
        )
        .await;
        sqlx::query(
            "INSERT INTO deployment_events
                (deployment_id, occurred_at, kind, severity, source, attributes)
             VALUES ($1, NOW(), 'status_changed', 'info', 'control-plane', '{}'::jsonb)",
        )
        .bind(doomed)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(event_count(&pool, doomed).await, 1);

        let cutoff = Utc::now() - chrono::Duration::days(90);
        assert_eq!(
            delete_aged_deployments(&pool, cutoff, 10, 500)
                .await
                .unwrap(),
            1
        );
        assert_eq!(event_count(&pool, doomed).await, 0);
    }
}
