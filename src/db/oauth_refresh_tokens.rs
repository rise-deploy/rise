//! Server-side state of OAuth extensions in `rotating` refresh-token mode.
//!
//! Two tables hold it (see the migration for the model): a *family* per login
//! and one row per issued handle, keyed by the SHA-256 of the handle. This
//! module only stores and moves opaque, already-encrypted values; the rotation
//! protocol that drives it lives in
//! `server::extensions::providers::oauth::rotation`.
//!
//! All time comparisons use the database clock, so replicas agree on them.

use anyhow::{Context, Result};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// How long a winner may hold a claim before another request may take it over.
/// It has to outlast the upstream token request.
pub const CLAIM_LEASE: Duration = Duration::from_secs(60);

/// A handle claimed for rotation.
#[derive(Debug)]
pub struct ClaimedHandle {
    pub family_id: Uuid,
    /// The encrypted upstream refresh token the handle stands for.
    pub upstream_token_encrypted: String,
}

/// What the store knows about a handle that could not be claimed.
#[derive(Debug, PartialEq, Eq)]
pub enum HandleState {
    /// No such handle for this project and extension.
    Unknown,
    /// The family is revoked or past its maximum lifetime.
    FamilyEnded,
    /// The family was started for another upstream than the extension's
    /// current one.
    UpstreamChanged,
    /// The handle was exchanged for a successor.
    Rotated {
        family_id: Uuid,
        /// The rotation happened less than the grace period ago.
        within_grace: bool,
        /// The token response recorded for the rotation, while it is kept.
        response_encrypted: Option<String>,
    },
    /// Another request holds the claim and has not recorded a successor yet.
    InFlight,
    /// The handle is claimable (the claim was released between the two reads).
    Available,
}

/// The values a rotation records.
#[derive(Debug)]
pub struct Rotation<'a> {
    pub claimed_handle: &'a [u8],
    pub claimant: Uuid,
    pub family_id: Uuid,
    pub grace: Duration,
    pub successor_handle: &'a [u8],
    pub successor_upstream_token_encrypted: &'a str,
    pub response_encrypted: &'a str,
}

/// Outcome of recording a rotation.
#[derive(Debug, PartialEq, Eq)]
pub enum RotationOutcome {
    Recorded,
    /// The claim expired and was taken over, or the handle is gone.
    ClaimLost,
    /// The family was revoked or ended while the upstream request ran.
    FamilyEnded,
}

/// Start a family whose only handle is `handle_hash`, for the upstream
/// identified by `upstream_identity`.
pub async fn create_family(
    pool: &PgPool,
    project_id: Uuid,
    extension: &str,
    upstream_identity: &str,
    max_lifetime: Duration,
    handle_hash: &[u8],
    upstream_token_encrypted: &str,
) -> Result<Uuid> {
    let family_id = Uuid::new_v4();
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO oauth_refresh_families (family_id, project_id, extension, upstream_identity, expires_at)
         VALUES ($1, $2, $3, $4, NOW() + ($5 * INTERVAL '1 second'))",
        family_id,
        project_id,
        extension,
        upstream_identity,
        max_lifetime.as_secs_f64(),
    )
    .execute(&mut *tx)
    .await
    .context("Failed to create refresh token family")?;
    sqlx::query!(
        "INSERT INTO oauth_refresh_tokens (handle_hash, family_id, project_id, extension, upstream_token_encrypted)
         VALUES ($1, $2, $3, $4, $5)",
        handle_hash,
        family_id,
        project_id,
        extension,
        upstream_token_encrypted,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to store refresh token handle")?;
    tx.commit().await?;
    Ok(family_id)
}

/// Atomically claim a live, unrotated handle of `project_id`/`extension`.
///
/// One conditional `UPDATE ... RETURNING`: of any number of concurrent callers
/// exactly one gets the row, and only while the family is live, was started for
/// `upstream_identity`, the handle has not been rotated, and no other claim
/// lease is running.
pub async fn claim(
    pool: &PgPool,
    handle_hash: &[u8],
    project_id: Uuid,
    extension: &str,
    upstream_identity: &str,
    claimant: Uuid,
    lease: Duration,
) -> Result<Option<ClaimedHandle>> {
    let row = sqlx::query!(
        r#"UPDATE oauth_refresh_tokens t
           SET claimed_by = $5,
               claim_expires_at = NOW() + ($6 * INTERVAL '1 second')
           FROM oauth_refresh_families f
           WHERE t.handle_hash = $1
             AND t.project_id = $2
             AND t.extension = $3
             AND f.family_id = t.family_id
             AND f.revoked_at IS NULL
             AND f.expires_at > NOW()
             AND f.upstream_identity = $4
             AND t.rotated_at IS NULL
             AND t.upstream_token_encrypted IS NOT NULL
             AND (t.claimed_by IS NULL OR t.claim_expires_at < NOW())
           RETURNING t.family_id, t.upstream_token_encrypted AS "upstream_token_encrypted!""#,
        handle_hash,
        project_id,
        extension,
        upstream_identity,
        claimant,
        lease.as_secs_f64(),
    )
    .fetch_optional(pool)
    .await
    .context("Failed to claim refresh token handle")?;
    Ok(row.map(|row| ClaimedHandle {
        family_id: row.family_id,
        upstream_token_encrypted: row.upstream_token_encrypted,
    }))
}

/// Read why a handle could not be claimed, given the extension's current
/// `upstream_identity`.
pub async fn state(
    pool: &PgPool,
    handle_hash: &[u8],
    project_id: Uuid,
    extension: &str,
    upstream_identity: &str,
) -> Result<HandleState> {
    let row = sqlx::query!(
        r#"SELECT t.family_id,
                  (f.revoked_at IS NOT NULL OR f.expires_at <= NOW()) AS "family_ended!",
                  f.upstream_identity <> $4 AS "upstream_changed!",
                  t.rotated_at IS NOT NULL AS "rotated!",
                  COALESCE(t.grace_until >= NOW(), FALSE) AS "within_grace!",
                  t.successor_response_encrypted,
                  (t.claimed_by IS NOT NULL AND t.claim_expires_at >= NOW()) AS "claimed!"
           FROM oauth_refresh_tokens t
           JOIN oauth_refresh_families f ON f.family_id = t.family_id
           WHERE t.handle_hash = $1 AND t.project_id = $2 AND t.extension = $3"#,
        handle_hash,
        project_id,
        extension,
        upstream_identity,
    )
    .fetch_optional(pool)
    .await
    .context("Failed to read refresh token handle")?;
    Ok(match row {
        None => HandleState::Unknown,
        Some(row) if row.family_ended => HandleState::FamilyEnded,
        Some(row) if row.upstream_changed => HandleState::UpstreamChanged,
        Some(row) if row.rotated => HandleState::Rotated {
            family_id: row.family_id,
            within_grace: row.within_grace,
            response_encrypted: row.successor_response_encrypted,
        },
        Some(row) if row.claimed => HandleState::InFlight,
        Some(_) => HandleState::Available,
    })
}

/// Give a claim back so the handle can be presented again.
pub async fn release_claim(pool: &PgPool, handle_hash: &[u8], claimant: Uuid) -> Result<bool> {
    let result = sqlx::query!(
        "UPDATE oauth_refresh_tokens
         SET claimed_by = NULL, claim_expires_at = NULL
         WHERE handle_hash = $1 AND claimed_by = $2 AND rotated_at IS NULL",
        handle_hash,
        claimant,
    )
    .execute(pool)
    .await
    .context("Failed to release refresh token claim")?;
    Ok(result.rows_affected() == 1)
}

/// Record the rotation of a claimed handle and store its successor, in one
/// transaction. Responses recorded by earlier rotations of the family that are
/// past their grace window are dropped on the way.
///
/// The family row is locked `FOR SHARE` first, which orders this against a
/// concurrent [`revoke_family`]: the revocation either lands before the
/// successor exists (this call reports `FamilyEnded` and stores nothing) or
/// after it, and then covers the successor too.
pub async fn record_rotation(pool: &PgPool, rotation: &Rotation<'_>) -> Result<RotationOutcome> {
    let mut tx = pool.begin().await?;
    let family = sqlx::query!(
        r#"SELECT project_id, extension
           FROM oauth_refresh_families
           WHERE family_id = $1 AND revoked_at IS NULL AND expires_at > NOW()
           FOR SHARE"#,
        rotation.family_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(family) = family else {
        return Ok(RotationOutcome::FamilyEnded);
    };
    // Insert the successor before pointing the old row at it. The old row is
    // only updated if the claim is still ours.
    sqlx::query!(
        "INSERT INTO oauth_refresh_tokens (handle_hash, family_id, project_id, extension, upstream_token_encrypted)
         VALUES ($1, $2, $3, $4, $5)",
        rotation.successor_handle,
        rotation.family_id,
        family.project_id,
        family.extension,
        rotation.successor_upstream_token_encrypted,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to store successor refresh token handle")?;
    let updated = sqlx::query!(
        "UPDATE oauth_refresh_tokens
         SET rotated_at = NOW(),
             grace_until = NOW() + ($3 * INTERVAL '1 second'),
             successor_response_encrypted = $4,
             upstream_token_encrypted = NULL,
             claimed_by = NULL,
             claim_expires_at = NULL
         WHERE handle_hash = $1 AND claimed_by = $2 AND rotated_at IS NULL AND family_id = $5",
        rotation.claimed_handle,
        rotation.claimant,
        rotation.grace.as_secs_f64(),
        rotation.response_encrypted,
        rotation.family_id,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to record refresh token rotation")?;
    if updated.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(RotationOutcome::ClaimLost);
    }
    // Responses of earlier rotations in this family are past their grace window.
    sqlx::query!(
        "UPDATE oauth_refresh_tokens SET successor_response_encrypted = NULL
         WHERE family_id = $1 AND successor_response_encrypted IS NOT NULL AND grace_until < NOW()",
        rotation.family_id,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to drop expired refresh token responses")?;
    tx.commit().await?;
    Ok(RotationOutcome::Recorded)
}

/// Revoke a family: none of its handles can be refreshed any more, and what
/// they stored is erased (the upstream tokens and the recorded responses).
///
/// The family flag is set first and the erasure runs as a separate statement,
/// after any `record_rotation` holding the family row has committed, so a
/// successor stored by that rotation is erased too.
pub async fn revoke_family(pool: &PgPool, family_id: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let result = sqlx::query!(
        "UPDATE oauth_refresh_families SET revoked_at = NOW()
         WHERE family_id = $1 AND revoked_at IS NULL",
        family_id,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to revoke refresh token family")?;
    sqlx::query!(
        "UPDATE oauth_refresh_tokens
         SET upstream_token_encrypted = NULL, successor_response_encrypted = NULL
         WHERE family_id = $1",
        family_id,
    )
    .execute(&mut *tx)
    .await
    .context("Failed to erase revoked refresh tokens")?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

/// Delete every family (and so every handle) of an extension.
pub async fn delete_extension_families(
    pool: &PgPool,
    project_id: Uuid,
    extension: &str,
) -> Result<u64> {
    let result = sqlx::query!(
        "DELETE FROM oauth_refresh_families WHERE project_id = $1 AND extension = $2",
        project_id,
        extension,
    )
    .execute(pool)
    .await
    .context("Failed to delete refresh token families")?;
    Ok(result.rows_affected())
}

/// Housekeeping, to be called periodically; returns the number of families
/// deleted.
///
/// Recorded responses are dropped once their grace window has passed. The
/// emptied rows stay until their family ends: they are what identifies a
/// replayed old handle as reuse.
pub async fn delete_expired(pool: &PgPool) -> Result<u64> {
    sqlx::query!(
        "UPDATE oauth_refresh_tokens
         SET successor_response_encrypted = NULL
         WHERE successor_response_encrypted IS NOT NULL AND grace_until < NOW()",
    )
    .execute(pool)
    .await
    .context("Failed to drop expired refresh token responses")?;
    let result = sqlx::query!("DELETE FROM oauth_refresh_families WHERE expires_at <= NOW()")
        .execute(pool)
        .await
        .context("Failed to delete expired refresh token families")?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{extensions, projects, users, ProjectStatus};

    async fn project_with_extension(pool: &PgPool, name: &str, extension: &str) -> Uuid {
        let user = users::create(pool, &format!("{name}@example.com"))
            .await
            .unwrap();
        let project = projects::create(
            pool,
            name,
            ProjectStatus::Stopped,
            "default".into(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();
        extensions::create(pool, project.id, extension, "oauth", &serde_json::json!({}))
            .await
            .unwrap();
        project.id
    }

    /// The upstream identity of the families the tests start.
    const IDENTITY: &str = "identity-a";

    fn hash(n: u8) -> Vec<u8> {
        vec![n; 32]
    }

    async fn rotate(pool: &PgPool, project: Uuid, from: u8, to: u8, grace: Duration) -> Uuid {
        let claimant = Uuid::new_v4();
        let claimed = claim(
            pool,
            &hash(from),
            project,
            "login",
            IDENTITY,
            claimant,
            CLAIM_LEASE,
        )
        .await
        .unwrap()
        .expect("claimable");
        let outcome = record_rotation(
            pool,
            &Rotation {
                claimed_handle: &hash(from),
                claimant,
                family_id: claimed.family_id,
                grace,
                successor_handle: &hash(to),
                successor_upstream_token_encrypted: "enc-next",
                response_encrypted: "enc-response",
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome, RotationOutcome::Recorded);
        claimed.family_id
    }

    #[sqlx::test]
    async fn only_one_concurrent_claimant_wins(pool: PgPool) {
        let project = project_with_extension(&pool, "claims", "login").await;
        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(1),
            "enc",
        )
        .await
        .unwrap();
        let attempts = (0..8).map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move {
                claim(
                    &pool,
                    &hash(1),
                    project,
                    "login",
                    IDENTITY,
                    Uuid::new_v4(),
                    CLAIM_LEASE,
                )
                .await
                .unwrap()
                .is_some()
            })
        });
        let wins = futures::future::join_all(attempts)
            .await
            .into_iter()
            .filter(|won| *won.as_ref().unwrap())
            .count();
        assert_eq!(wins, 1);
        assert_eq!(
            state(&pool, &hash(1), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::InFlight
        );
    }

    #[sqlx::test]
    async fn released_and_expired_claims_can_be_taken_again(pool: PgPool) {
        let project = project_with_extension(&pool, "release", "login").await;
        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(1),
            "enc",
        )
        .await
        .unwrap();
        let first = Uuid::new_v4();
        claim(
            &pool,
            &hash(1),
            project,
            "login",
            IDENTITY,
            first,
            CLAIM_LEASE,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(release_claim(&pool, &hash(1), first).await.unwrap());
        assert!(!release_claim(&pool, &hash(1), first).await.unwrap());
        assert_eq!(
            state(&pool, &hash(1), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Available
        );

        claim(
            &pool,
            &hash(1),
            project,
            "login",
            IDENTITY,
            Uuid::new_v4(),
            CLAIM_LEASE,
        )
        .await
        .unwrap()
        .unwrap();
        sqlx::query(
            "UPDATE oauth_refresh_tokens SET claim_expires_at = NOW() - INTERVAL '1 second'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(claim(
            &pool,
            &hash(1),
            project,
            "login",
            IDENTITY,
            Uuid::new_v4(),
            CLAIM_LEASE
        )
        .await
        .unwrap()
        .is_some());
    }

    #[sqlx::test]
    async fn rotation_replaces_the_handle_and_keeps_the_response_for_the_grace_window(
        pool: PgPool,
    ) {
        let project = project_with_extension(&pool, "rotate", "login").await;
        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(1),
            "enc",
        )
        .await
        .unwrap();
        rotate(&pool, project, 1, 2, Duration::from_secs(30)).await;

        match state(&pool, &hash(1), project, "login", IDENTITY)
            .await
            .unwrap()
        {
            HandleState::Rotated {
                within_grace: true,
                response_encrypted: Some(response),
                ..
            } => assert_eq!(response, "enc-response"),
            other => panic!("unexpected state {other:?}"),
        }
        assert_eq!(
            state(&pool, &hash(2), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Available
        );
        // The old handle cannot be claimed again, and no longer holds the
        // upstream token.
        assert!(claim(
            &pool,
            &hash(1),
            project,
            "login",
            IDENTITY,
            Uuid::new_v4(),
            CLAIM_LEASE
        )
        .await
        .unwrap()
        .is_none());
        let kept: Option<String> = sqlx::query_scalar(
            "SELECT upstream_token_encrypted FROM oauth_refresh_tokens WHERE handle_hash = $1",
        )
        .bind(hash(1))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept, None);
        let claimed = claim(
            &pool,
            &hash(2),
            project,
            "login",
            IDENTITY,
            Uuid::new_v4(),
            CLAIM_LEASE,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(claimed.upstream_token_encrypted, "enc-next");
    }

    #[sqlx::test]
    async fn grace_ends_and_housekeeping_drops_the_response_but_keeps_the_tombstone(pool: PgPool) {
        let project = project_with_extension(&pool, "grace", "login").await;
        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(1),
            "enc",
        )
        .await
        .unwrap();
        rotate(&pool, project, 1, 2, Duration::from_secs(30)).await;
        sqlx::query("UPDATE oauth_refresh_tokens SET grace_until = NOW() - INTERVAL '1 second' WHERE handle_hash = $1")
            .bind(hash(1))
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            state(&pool, &hash(1), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Rotated {
                within_grace: false,
                response_encrypted: Some(_),
                ..
            }
        ));

        assert_eq!(delete_expired(&pool).await.unwrap(), 0);
        assert!(matches!(
            state(&pool, &hash(1), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Rotated {
                within_grace: false,
                response_encrypted: None,
                ..
            }
        ));
    }

    #[sqlx::test]
    async fn revoked_and_expired_families_end_every_handle(pool: PgPool) {
        let project = project_with_extension(&pool, "revoke", "login").await;
        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(1),
            "enc",
        )
        .await
        .unwrap();
        let family = rotate(&pool, project, 1, 2, Duration::from_secs(30)).await;
        let stored = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM oauth_refresh_tokens
                 WHERE upstream_token_encrypted IS NOT NULL OR successor_response_encrypted IS NOT NULL",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        assert_eq!(
            stored().await,
            2,
            "the live successor's token and the recorded response"
        );
        assert!(revoke_family(&pool, family).await.unwrap());
        assert!(!revoke_family(&pool, family).await.unwrap());
        assert_eq!(
            stored().await,
            0,
            "revocation erases every stored secret of the family"
        );
        for handle in [1, 2] {
            assert_eq!(
                state(&pool, &hash(handle), project, "login", IDENTITY)
                    .await
                    .unwrap(),
                HandleState::FamilyEnded
            );
            assert!(claim(
                &pool,
                &hash(handle),
                project,
                "login",
                IDENTITY,
                Uuid::new_v4(),
                CLAIM_LEASE
            )
            .await
            .unwrap()
            .is_none());
        }

        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(3),
            "enc",
        )
        .await
        .unwrap();
        sqlx::query("UPDATE oauth_refresh_families SET expires_at = NOW() - INTERVAL '1 second' WHERE revoked_at IS NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            state(&pool, &hash(3), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::FamilyEnded
        );
        assert!(claim(
            &pool,
            &hash(3),
            project,
            "login",
            IDENTITY,
            Uuid::new_v4(),
            CLAIM_LEASE
        )
        .await
        .unwrap()
        .is_none());
        // Housekeeping deletes ended-by-lifetime families and their handles.
        assert_eq!(delete_expired(&pool).await.unwrap(), 1);
        assert_eq!(
            state(&pool, &hash(3), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Unknown
        );
    }

    #[sqlx::test]
    async fn revocation_during_the_upstream_request_stores_no_successor(pool: PgPool) {
        let project = project_with_extension(&pool, "race", "login").await;
        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(1),
            "enc",
        )
        .await
        .unwrap();
        let claimant = Uuid::new_v4();
        let claimed = claim(
            &pool,
            &hash(1),
            project,
            "login",
            IDENTITY,
            claimant,
            CLAIM_LEASE,
        )
        .await
        .unwrap()
        .unwrap();
        revoke_family(&pool, claimed.family_id).await.unwrap();
        let outcome = record_rotation(
            &pool,
            &Rotation {
                claimed_handle: &hash(1),
                claimant,
                family_id: claimed.family_id,
                grace: Duration::from_secs(30),
                successor_handle: &hash(2),
                successor_upstream_token_encrypted: "enc-next",
                response_encrypted: "enc-response",
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome, RotationOutcome::FamilyEnded);
        assert_eq!(
            state(&pool, &hash(2), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Unknown
        );
    }

    #[sqlx::test]
    async fn a_lost_claim_cannot_record_a_rotation(pool: PgPool) {
        let project = project_with_extension(&pool, "lost", "login").await;
        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(1),
            "enc",
        )
        .await
        .unwrap();
        let slow = Uuid::new_v4();
        let claimed = claim(
            &pool,
            &hash(1),
            project,
            "login",
            IDENTITY,
            slow,
            CLAIM_LEASE,
        )
        .await
        .unwrap()
        .unwrap();
        sqlx::query(
            "UPDATE oauth_refresh_tokens SET claim_expires_at = NOW() - INTERVAL '1 second'",
        )
        .execute(&pool)
        .await
        .unwrap();
        claim(
            &pool,
            &hash(1),
            project,
            "login",
            IDENTITY,
            Uuid::new_v4(),
            CLAIM_LEASE,
        )
        .await
        .unwrap()
        .unwrap();
        let outcome = record_rotation(
            &pool,
            &Rotation {
                claimed_handle: &hash(1),
                claimant: slow,
                family_id: claimed.family_id,
                grace: Duration::from_secs(30),
                successor_handle: &hash(2),
                successor_upstream_token_encrypted: "enc-next",
                response_encrypted: "enc-response",
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome, RotationOutcome::ClaimLost);
        assert_eq!(
            state(&pool, &hash(2), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Unknown
        );
    }

    #[sqlx::test]
    async fn handles_are_scoped_to_their_project_and_extension(pool: PgPool) {
        let project = project_with_extension(&pool, "scope-a", "login").await;
        let other_project = project_with_extension(&pool, "scope-b", "login").await;
        extensions::create(&pool, project, "other", "oauth", &serde_json::json!({}))
            .await
            .unwrap();
        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(1),
            "enc",
        )
        .await
        .unwrap();
        assert_eq!(
            state(&pool, &hash(1), project, "other", IDENTITY)
                .await
                .unwrap(),
            HandleState::Unknown
        );
        assert_eq!(
            state(&pool, &hash(1), other_project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Unknown
        );
        for (p, e) in [(project, "other"), (other_project, "login")] {
            assert!(
                claim(&pool, &hash(1), p, e, IDENTITY, Uuid::new_v4(), CLAIM_LEASE)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[sqlx::test]
    async fn deleting_an_extension_removes_its_families(pool: PgPool) {
        let project = project_with_extension(&pool, "cascade", "login").await;
        extensions::create(&pool, project, "other", "oauth", &serde_json::json!({}))
            .await
            .unwrap();
        for (n, ext) in [(1, "login"), (2, "other")] {
            create_family(
                &pool,
                project,
                ext,
                IDENTITY,
                Duration::from_secs(600),
                &hash(n),
                "enc",
            )
            .await
            .unwrap();
        }
        extensions::delete_permanently(&pool, project, "login")
            .await
            .unwrap();
        assert_eq!(
            state(&pool, &hash(1), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Unknown
        );
        assert_eq!(
            state(&pool, &hash(2), project, "other", IDENTITY)
                .await
                .unwrap(),
            HandleState::Available
        );

        assert_eq!(
            delete_extension_families(&pool, project, "other")
                .await
                .unwrap(),
            1
        );
        create_family(
            &pool,
            project,
            "other",
            IDENTITY,
            Duration::from_secs(600),
            &hash(3),
            "enc",
        )
        .await
        .unwrap();
        extensions::delete_permanently(&pool, project, "other")
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[sqlx::test]
    async fn a_family_is_only_claimable_for_the_upstream_it_was_started_for(pool: PgPool) {
        let project = project_with_extension(&pool, "identity", "login").await;
        create_family(
            &pool,
            project,
            "login",
            IDENTITY,
            Duration::from_secs(600),
            &hash(1),
            "enc",
        )
        .await
        .unwrap();

        let claimant = Uuid::new_v4();
        assert!(
            claim(
                &pool,
                &hash(1),
                project,
                "login",
                "identity-b",
                claimant,
                CLAIM_LEASE
            )
            .await
            .unwrap()
            .is_none(),
            "a different upstream identity never claims"
        );
        assert_eq!(
            state(&pool, &hash(1), project, "login", "identity-b")
                .await
                .unwrap(),
            HandleState::UpstreamChanged
        );
        // Nothing was claimed or consumed: the original identity still works.
        assert_eq!(
            state(&pool, &hash(1), project, "login", IDENTITY)
                .await
                .unwrap(),
            HandleState::Available
        );
        assert!(claim(
            &pool,
            &hash(1),
            project,
            "login",
            IDENTITY,
            claimant,
            CLAIM_LEASE
        )
        .await
        .unwrap()
        .is_some());
    }
}
