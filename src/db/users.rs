use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::models::User;

/// Find user by email address
pub async fn find_by_email<'a, E>(executor: E, email: &str) -> Result<Option<User>>
where
    E: sqlx::Executor<'a, Database = sqlx::Postgres>,
{
    let user = sqlx::query_as!(
        User,
        r#"
        SELECT id, email, created_at, updated_at
        FROM users
        WHERE email = $1
        "#,
        email
    )
    .fetch_optional(executor)
    .await
    .context("Failed to find user by email")?;

    Ok(user)
}

/// Find user by ID
pub async fn find_by_id(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    id: Uuid,
) -> Result<Option<User>> {
    let user = sqlx::query_as!(
        User,
        r#"
        SELECT id, email, created_at, updated_at
        FROM users
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(executor)
    .await
    .context("Failed to find user by ID")?;

    Ok(user)
}

/// Create a new user.
///
/// Used directly by tests; production code paths must go through
/// [`find_or_create_with_default_organization`] so the user is stamped with
/// a default-Organization membership in the same transaction.
///
/// Idempotent on the unique `email` constraint: concurrent inserts of the
/// same email return the existing row instead of erroring, so the
/// `find_by_email`-then-`create` pattern in `find_or_create*` is race-safe.
#[allow(dead_code)]
pub async fn create(pool: &PgPool, email: &str) -> Result<User> {
    let user = sqlx::query_as!(
        User,
        r#"
        INSERT INTO users (email)
        VALUES ($1)
        ON CONFLICT (email) DO UPDATE SET email = EXCLUDED.email
        RETURNING id, email, created_at, updated_at
        "#,
        email
    )
    .fetch_one(pool)
    .await
    .context("Failed to create user")?;

    Ok(user)
}

/// Find user by email, or create if not exists.
///
/// Prefer [`find_or_create_with_default_organization`] for callers that have
/// access to the default Organization UID: this version does not stamp the
/// default-Organization membership, which means the new user row is in an
/// inconsistent state until the next bootstrap or backfill pass.
#[allow(dead_code)]
pub async fn find_or_create(pool: &PgPool, email: &str) -> Result<User> {
    // Try to find existing user first
    if let Some(user) = find_by_email(pool, email).await? {
        return Ok(user);
    }

    // User doesn't exist, create new one
    create(pool, email).await
}

/// Idempotent find-or-create that runs the user insert AND the
/// default-Organization membership insert inside a single transaction.
///
/// The auth middleware must call this rather than [`find_or_create`] so a
/// user row never exists without the corresponding default-Organization
/// membership: bootstrap validation refuses to start the server in that
/// state.
pub async fn find_or_create_with_default_organization(
    pool: &PgPool,
    email: &str,
    default_organization_uid: Uuid,
) -> Result<User> {
    let mut tx = pool.begin().await.context("Failed to start transaction")?;
    let user = find_or_create_with_executor(&mut tx, email).await?;
    crate::db::organization_links::ensure_user_membership(
        &mut tx,
        user.id,
        default_organization_uid,
    )
    .await?;
    tx.commit().await.context("Failed to commit transaction")?;
    Ok(user)
}

/// Find user by email, or create if not exists (using a mutable connection for transactions)
pub async fn find_or_create_with_executor(
    conn: &mut sqlx::PgConnection,
    email: &str,
) -> Result<User> {
    // Try to insert; if the user already exists, do nothing (avoids unnecessary writes/locks)
    let inserted = sqlx::query_as!(
        User,
        r#"
        INSERT INTO users (email)
        VALUES ($1)
        ON CONFLICT (email) DO NOTHING
        RETURNING id, email, created_at, updated_at
        "#,
        email
    )
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to insert user")?;

    if let Some(user) = inserted {
        return Ok(user);
    }

    // User already exists; fetch it
    let user = sqlx::query_as!(
        User,
        r#"
        SELECT id, email, created_at, updated_at
        FROM users
        WHERE email = $1
        "#,
        email
    )
    .fetch_one(&mut *conn)
    .await
    .context("Failed to find existing user after insert conflict")?;

    Ok(user)
}

/// Batch fetch user emails by IDs
pub async fn get_emails_batch(
    pool: &PgPool,
    user_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, String>> {
    let records = sqlx::query!(
        r#"
        SELECT id, email
        FROM users
        WHERE id = ANY($1)
        "#,
        user_ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to batch fetch user emails")?;

    Ok(records.into_iter().map(|r| (r.id, r.email)).collect())
}

/// Batch fetch full user details by IDs
pub async fn get_users_batch(
    pool: &PgPool,
    user_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, User>> {
    let users = sqlx::query_as!(
        User,
        r#"
        SELECT id, email, created_at, updated_at
        FROM users
        WHERE id = ANY($1)
        "#,
        user_ids
    )
    .fetch_all(pool)
    .await
    .context("Failed to batch fetch users")?;

    Ok(users.into_iter().map(|u| (u.id, u)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn test_create_user(pool: PgPool) -> Result<()> {
        let user = create(&pool, "test@example.com").await?;
        assert_eq!(user.email, "test@example.com");
        Ok(())
    }

    #[sqlx::test]
    async fn test_find_by_email(pool: PgPool) -> Result<()> {
        let created = create(&pool, "test@example.com").await?;
        let found = find_by_email(&pool, "test@example.com").await?;
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, created.id);
        Ok(())
    }

    #[sqlx::test]
    async fn test_find_by_id(pool: PgPool) -> Result<()> {
        let created = create(&pool, "test@example.com").await?;
        let found = find_by_id(&pool, created.id).await?;
        assert!(found.is_some());
        assert_eq!(found.unwrap().email, "test@example.com");
        Ok(())
    }

    #[sqlx::test]
    async fn test_find_or_create_existing(pool: PgPool) -> Result<()> {
        let created = create(&pool, "test@example.com").await?;
        let found = find_or_create(&pool, "test@example.com").await?;
        assert_eq!(found.id, created.id);
        Ok(())
    }

    #[sqlx::test]
    async fn test_find_or_create_new(pool: PgPool) -> Result<()> {
        let user = find_or_create(&pool, "new@example.com").await?;
        assert_eq!(user.email, "new@example.com");
        Ok(())
    }
}
