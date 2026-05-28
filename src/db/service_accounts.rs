use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use crate::db::models::{Project, ServiceAccount, User};

/// Create a new service account for a project
pub async fn create(
    pool: &PgPool,
    project_id: Uuid,
    issuer_url: &str,
    claims: &HashMap<String, String>,
) -> Result<ServiceAccount> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    // Get project for email generation
    let project = sqlx::query_as!(
        Project,
        r#"
        SELECT id, name, status as "status: _", access_class,
               owner_user_id, owner_team_id, finalizers, source_url,
               template_id, template_image,
               created_at, updated_at
        FROM projects
        WHERE id = $1
        "#,
        project_id
    )
    .fetch_one(&mut *tx)
    .await
    .context("Failed to fetch project")?;

    // Calculate next sequence number
    let sequence: Option<i32> = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(MAX(sequence), 0) + 1 as "sequence"
        FROM service_accounts
        WHERE project_id = $1
        "#,
        project_id
    )
    .fetch_one(&mut *tx)
    .await
    .context("Failed to calculate sequence")?;

    let sequence = sequence.unwrap_or(1);

    // Generate service account email
    let email = format!("{}+{}@sa.rise.local", project.name, sequence);

    // Check if user already exists with this email (from previously deleted SA)
    let existing_user = sqlx::query_as!(
        User,
        r#"
        SELECT id, email, created_at, updated_at
        FROM users
        WHERE email = $1
        "#,
        email
    )
    .fetch_optional(&mut *tx)
    .await
    .context("Failed to check for existing user")?;

    // Create or reuse user
    let user = if let Some(user) = existing_user {
        tracing::debug!("Reusing existing user for service account: {}", email);
        user
    } else {
        sqlx::query_as!(
            User,
            r#"
            INSERT INTO users (email)
            VALUES ($1)
            RETURNING id, email, created_at, updated_at
            "#,
            email
        )
        .fetch_one(&mut *tx)
        .await
        .context("Failed to create user for service account")?
    };

    // Convert claims HashMap to JSONB
    let claims_json = serde_json::to_value(claims).context("Failed to serialize claims")?;

    // Create service account
    let sa = sqlx::query_as!(
        ServiceAccount,
        r#"
        INSERT INTO service_accounts (project_id, user_id, issuer_url, claims, sequence)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, project_id, user_id, issuer_url, claims, sequence, allowed_environment_ids,
                  deleted_at, created_at, updated_at
        "#,
        project_id,
        user.id,
        issuer_url,
        claims_json,
        sequence
    )
    .fetch_one(&mut *tx)
    .await
    .context("Failed to create service account")?;

    tx.commit().await.context("Failed to commit transaction")?;

    Ok(sa)
}

/// List all active service accounts for a project
pub async fn list_by_project(pool: &PgPool, project_id: Uuid) -> Result<Vec<ServiceAccount>> {
    let sas = sqlx::query_as!(
        ServiceAccount,
        r#"
        SELECT id, project_id, user_id, issuer_url, claims, sequence, allowed_environment_ids,
               deleted_at, created_at, updated_at
        FROM service_accounts
        WHERE project_id = $1 AND deleted_at IS NULL
        ORDER BY sequence ASC
        "#,
        project_id
    )
    .fetch_all(pool)
    .await
    .context("Failed to list service accounts")?;

    Ok(sas)
}

/// Get a service account by ID
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Option<ServiceAccount>> {
    let sa = sqlx::query_as!(
        ServiceAccount,
        r#"
        SELECT id, project_id, user_id, issuer_url, claims, sequence, allowed_environment_ids,
               deleted_at, created_at, updated_at
        FROM service_accounts
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to get service account by ID")?;

    Ok(sa)
}

/// Get claims for a service account
#[allow(dead_code)]
pub async fn get_claims(
    pool: &PgPool,
    service_account_id: Uuid,
) -> Result<HashMap<String, String>> {
    let sa = sqlx::query_as!(
        ServiceAccount,
        r#"
        SELECT id, project_id, user_id, issuer_url, claims, sequence, allowed_environment_ids,
               deleted_at, created_at, updated_at
        FROM service_accounts
        WHERE id = $1
        "#,
        service_account_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to fetch service account")?;

    // Convert JSONB to HashMap<String, String>
    let claims: HashMap<String, String> =
        serde_json::from_value(sa.claims).context("Failed to deserialize claims")?;

    Ok(claims)
}

/// Check if a user is a service account
pub async fn is_service_account(pool: &PgPool, user_id: Uuid) -> Result<bool> {
    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM service_accounts
            WHERE user_id = $1 AND deleted_at IS NULL
        ) as "exists!"
        "#,
        user_id
    )
    .fetch_one(pool)
    .await
    .context("Failed to check if user is a service account")?;

    Ok(exists)
}

/// Update a service account's issuer_url, claims, and/or allowed_environment_ids.
///
/// `allowed_environment_ids`:
/// - `None` = don't change
/// - `Some(None)` = set to NULL (allow any environment)
/// - `Some(Some(ids))` = restrict to these environment IDs
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    issuer_url: Option<&str>,
    claims: Option<&HashMap<String, String>>,
    allowed_environment_ids: Option<Option<&[Uuid]>>,
) -> Result<ServiceAccount> {
    // Convert claims HashMap to JSONB if provided
    let claims_json = if let Some(c) = claims {
        Some(serde_json::to_value(c).context("Failed to serialize claims")?)
    } else {
        None
    };

    // Flatten allowed_environment_ids for the query
    let update_env_ids = allowed_environment_ids.is_some();
    let env_ids: Option<Vec<Uuid>> =
        allowed_environment_ids.and_then(|opt| opt.map(|ids| ids.to_vec()));

    let sa = sqlx::query_as!(
        ServiceAccount,
        r#"
        UPDATE service_accounts
        SET
            issuer_url = COALESCE($2, issuer_url),
            claims = COALESCE($3, claims),
            allowed_environment_ids = CASE WHEN $4 THEN $5 ELSE allowed_environment_ids END,
            updated_at = NOW()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING id, project_id, user_id, issuer_url, claims, sequence, allowed_environment_ids,
                  deleted_at, created_at, updated_at
        "#,
        id,
        issuer_url,
        claims_json,
        update_env_ids,
        env_ids.as_deref()
    )
    .fetch_one(pool)
    .await
    .context("Failed to update service account")?;

    Ok(sa)
}

/// Check if any active service account exists with this issuer URL
pub async fn issuer_exists(pool: &PgPool, issuer_url: &str) -> Result<bool> {
    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM service_accounts
            WHERE issuer_url = $1 AND deleted_at IS NULL
        ) as "exists!"
        "#,
        issuer_url
    )
    .fetch_one(pool)
    .await
    .context("Failed to check if issuer exists")?;

    Ok(exists)
}

/// Find all active service accounts for a specific project and issuer URL
pub async fn find_by_project_and_issuer(
    pool: &PgPool,
    project_id: Uuid,
    issuer_url: &str,
) -> Result<Vec<ServiceAccount>> {
    let sas = sqlx::query_as!(
        ServiceAccount,
        r#"
        SELECT id, project_id, user_id, issuer_url, claims, sequence, allowed_environment_ids,
               deleted_at, created_at, updated_at
        FROM service_accounts
        WHERE project_id = $1 AND issuer_url = $2 AND deleted_at IS NULL
        ORDER BY sequence ASC
        "#,
        project_id,
        issuer_url
    )
    .fetch_all(pool)
    .await
    .context("Failed to find service accounts by project and issuer")?;

    Ok(sas)
}

/// Create a service account with raw JSON claims (for testing malformed claims).
///
/// Unlike [`create`], this accepts arbitrary `serde_json::Value` claims,
/// allowing tests to insert non-`HashMap<String, String>` values.
#[cfg(test)]
pub async fn create_with_raw_claims(
    pool: &PgPool,
    project_id: Uuid,
    issuer_url: &str,
    raw_claims: serde_json::Value,
) -> Result<ServiceAccount> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    let project = sqlx::query_as!(
        Project,
        r#"
        SELECT id, name, status as "status: _", access_class,
               owner_user_id, owner_team_id, finalizers, source_url,
               template_id, template_image,
               created_at, updated_at
        FROM projects
        WHERE id = $1
        "#,
        project_id
    )
    .fetch_one(&mut *tx)
    .await
    .context("Failed to fetch project")?;

    let sequence: Option<i32> = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(MAX(sequence), 0) + 1 as "sequence"
        FROM service_accounts
        WHERE project_id = $1
        "#,
        project_id
    )
    .fetch_one(&mut *tx)
    .await
    .context("Failed to calculate sequence")?;

    let sequence = sequence.unwrap_or(1);
    let email = format!("{}+{}@sa.rise.local", project.name, sequence);

    let existing_user = sqlx::query_as!(
        User,
        r#"
        SELECT id, email, created_at, updated_at
        FROM users
        WHERE email = $1
        "#,
        email
    )
    .fetch_optional(&mut *tx)
    .await
    .context("Failed to check for existing user")?;

    let user = if let Some(user) = existing_user {
        user
    } else {
        sqlx::query_as!(
            User,
            r#"
            INSERT INTO users (email)
            VALUES ($1)
            RETURNING id, email, created_at, updated_at
            "#,
            email
        )
        .fetch_one(&mut *tx)
        .await
        .context("Failed to create user for service account")?
    };

    let sa = sqlx::query_as!(
        ServiceAccount,
        r#"
        INSERT INTO service_accounts (project_id, user_id, issuer_url, claims, sequence)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, project_id, user_id, issuer_url, claims, sequence, allowed_environment_ids,
                  deleted_at, created_at, updated_at
        "#,
        project_id,
        user.id,
        issuer_url,
        raw_claims,
        sequence
    )
    .fetch_one(&mut *tx)
    .await
    .context("Failed to create service account with raw claims")?;

    tx.commit().await.context("Failed to commit transaction")?;

    Ok(sa)
}

/// Find an active service account by its synthetic user ID.
///
/// Each service account has a unique synthetic user, so this returns at most one result.
pub async fn find_active_by_user_id(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<ServiceAccount>> {
    let sa = sqlx::query_as!(
        ServiceAccount,
        r#"
        SELECT id, project_id, user_id, issuer_url, claims, sequence, allowed_environment_ids,
               deleted_at, created_at, updated_at
        FROM service_accounts
        WHERE user_id = $1 AND deleted_at IS NULL
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await
    .context("Failed to find service account by user ID")?;

    Ok(sa)
}

/// Soft delete a service account
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE service_accounts
        SET deleted_at = NOW(), updated_at = NOW()
        WHERE id = $1
        "#,
        id
    )
    .execute(pool)
    .await
    .context("Failed to soft delete service account")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::ProjectStatus, projects, users};

    #[sqlx::test]
    async fn test_create_service_account(pool: PgPool) -> Result<()> {
        // Create test user and project
        let user = users::create(&pool, "owner@example.com").await?;
        let project = projects::create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        // Create service account
        let mut claims = HashMap::new();
        claims.insert("sub".to_string(), "test-subject".to_string());
        claims.insert("iss".to_string(), "https://gitlab.com".to_string());

        let sa = create(&pool, project.id, "https://gitlab.com", &claims).await?;

        assert_eq!(sa.project_id, project.id);
        assert_eq!(sa.issuer_url, "https://gitlab.com");
        assert_eq!(sa.sequence, 1);
        assert!(sa.deleted_at.is_none());

        // Verify user was created
        let sa_user = users::find_by_id(&pool, sa.user_id).await?;
        assert!(sa_user.is_some());
        assert_eq!(sa_user.unwrap().email, "test-project+1@sa.rise.local");

        Ok(())
    }

    #[sqlx::test]
    async fn test_sequence_increment(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "owner@example.com").await?;
        let project = projects::create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let claims = HashMap::new();

        // Create first service account
        let sa1 = create(&pool, project.id, "https://gitlab.com", &claims).await?;
        assert_eq!(sa1.sequence, 1);

        // Create second service account
        let sa2 = create(&pool, project.id, "https://github.com", &claims).await?;
        assert_eq!(sa2.sequence, 2);

        Ok(())
    }

    #[sqlx::test]
    async fn test_list_by_project(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "owner@example.com").await?;
        let project = projects::create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let claims = HashMap::new();

        // Create two service accounts
        create(&pool, project.id, "https://gitlab.com", &claims).await?;
        create(&pool, project.id, "https://github.com", &claims).await?;

        // List service accounts
        let sas = list_by_project(&pool, project.id).await?;
        assert_eq!(sas.len(), 2);

        Ok(())
    }

    #[sqlx::test]
    async fn test_soft_delete(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "owner@example.com").await?;
        let project = projects::create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let claims = HashMap::new();
        let sa = create(&pool, project.id, "https://gitlab.com", &claims).await?;

        // Soft delete
        soft_delete(&pool, sa.id).await?;

        // Verify not in list
        let sas = list_by_project(&pool, project.id).await?;
        assert_eq!(sas.len(), 0);

        // Verify still exists in DB
        let sa_deleted = get_by_id(&pool, sa.id).await?;
        assert!(sa_deleted.is_some());
        assert!(sa_deleted.unwrap().deleted_at.is_some());

        Ok(())
    }

    #[sqlx::test]
    async fn test_issuer_exists(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "owner@example.com").await?;
        let project = projects::create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        // No service accounts yet
        assert!(!issuer_exists(&pool, "https://gitlab.com").await?);

        // Create a service account
        let claims = HashMap::new();
        create(&pool, project.id, "https://gitlab.com", &claims).await?;

        // Now it should exist
        assert!(issuer_exists(&pool, "https://gitlab.com").await?);
        assert!(!issuer_exists(&pool, "https://github.com").await?);

        Ok(())
    }

    #[sqlx::test]
    async fn test_find_by_project_and_issuer(pool: PgPool) -> Result<()> {
        let user = users::create(&pool, "owner@example.com").await?;
        let project1 = projects::create(
            &pool,
            "project1",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;
        let project2 = projects::create(
            &pool,
            "project2",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(user.id),
            None,
            None,
        )
        .await?;

        let claims = HashMap::new();

        // Create SAs: project1 has 2 gitlab SAs, project2 has 1 gitlab SA
        create(&pool, project1.id, "https://gitlab.com", &claims).await?;
        create(&pool, project1.id, "https://gitlab.com", &claims).await?;
        create(&pool, project2.id, "https://gitlab.com", &claims).await?;

        // Should only return project1's SAs
        let sas = find_by_project_and_issuer(&pool, project1.id, "https://gitlab.com").await?;
        assert_eq!(sas.len(), 2);

        // Should only return project2's SA
        let sas = find_by_project_and_issuer(&pool, project2.id, "https://gitlab.com").await?;
        assert_eq!(sas.len(), 1);

        // No SAs for github
        let sas = find_by_project_and_issuer(&pool, project1.id, "https://github.com").await?;
        assert_eq!(sas.len(), 0);

        Ok(())
    }

    #[sqlx::test]
    async fn test_is_service_account(pool: PgPool) -> Result<()> {
        let regular_user = users::create(&pool, "regular@example.com").await?;
        let project = projects::create(
            &pool,
            "test-project",
            ProjectStatus::Stopped,
            "public".to_string(),
            Some(regular_user.id),
            None,
            None,
        )
        .await?;

        let claims = HashMap::new();
        let sa = create(&pool, project.id, "https://gitlab.com", &claims).await?;

        // Regular user should not be a service account
        assert!(!is_service_account(&pool, regular_user.id).await?);

        // SA user should be a service account
        assert!(is_service_account(&pool, sa.user_id).await?);

        Ok(())
    }
}
