use anyhow::{Context, Result};
use sqlx::{AssertSqlSafe, PgPool};

/// Sanitize a PostgreSQL identifier (database name, username, etc.) to prevent SQL injection.
/// This function validates that the identifier contains only allowed characters and properly
/// escapes double quotes for safe use in SQL statements.
#[allow(dead_code)]
fn sanitize_identifier(identifier: &str) -> Result<String> {
    // Only allow alphanumeric, underscores, hyphens, and periods
    if !identifier
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        anyhow::bail!(
            "Invalid identifier \"{}\": contains illegal characters",
            identifier
        );
    }

    // Escape internal double quotes and quote the identifier to handle
    // reserved words and special characters in a PostgreSQL-safe way.
    let escaped = identifier.replace('"', "\"\"");
    Ok(format!("\"{}\"", escaped))
}

/// Check if a PostgreSQL database exists
#[allow(dead_code)]
pub async fn database_exists(pool: &PgPool, database_name: &str) -> Result<bool> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(database_name)
            .fetch_one(pool)
            .await
            .context("Failed to check if database exists")?;

    Ok(exists)
}

/// Create a PostgreSQL database with a specific owner
///
/// This function sanitizes the database and owner names internally to prevent SQL injection.
#[allow(dead_code)]
pub async fn create_database(pool: &PgPool, database_name: &str, owner: &str) -> Result<()> {
    let sanitized_db = sanitize_identifier(database_name)?;
    let sanitized_owner = sanitize_identifier(owner)?;
    let create_sql = format!("CREATE DATABASE {} OWNER {}", sanitized_db, sanitized_owner);

    sqlx::query(AssertSqlSafe(create_sql))
        .execute(pool)
        .await
        .context("Failed to create database")?;

    Ok(())
}

/// Check if a PostgreSQL role/user exists
#[allow(dead_code)]
pub async fn user_exists(pool: &PgPool, username: &str) -> Result<bool> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(username)
            .fetch_one(pool)
            .await
            .context("Failed to check if user exists")?;

    Ok(exists)
}

/// Create a PostgreSQL user with a password
///
/// This function sanitizes the username and escapes the password internally to prevent SQL injection.
#[allow(dead_code)]
pub async fn create_user(pool: &PgPool, username: &str, password: &str) -> Result<()> {
    let sanitized_username = sanitize_identifier(username)?;
    // Escape single quotes in password by doubling them (PostgreSQL standard)
    let escaped_password = password.replace('\'', "''");
    let create_sql = format!(
        "CREATE USER {} WITH PASSWORD '{}'",
        sanitized_username, escaped_password
    );

    sqlx::query(AssertSqlSafe(create_sql))
        .execute(pool)
        .await
        .context("Failed to create user")?;

    Ok(())
}

/// Update a PostgreSQL user's password
///
/// This function sanitizes the username and escapes the password internally to prevent SQL injection.
#[allow(dead_code)]
pub async fn update_user_password(pool: &PgPool, username: &str, password: &str) -> Result<()> {
    let sanitized_username = sanitize_identifier(username)?;
    // Escape single quotes in password by doubling them (PostgreSQL standard)
    let escaped_password = password.replace('\'', "''");
    let alter_sql = format!(
        "ALTER USER {} WITH PASSWORD '{}'",
        sanitized_username, escaped_password
    );

    sqlx::query(AssertSqlSafe(alter_sql))
        .execute(pool)
        .await
        .context("Failed to update user password")?;

    Ok(())
}

/// Change the owner of a database
///
/// This function sanitizes the database and owner names internally to prevent SQL injection.
#[allow(dead_code)]
pub async fn change_database_owner(
    pool: &PgPool,
    database_name: &str,
    new_owner: &str,
) -> Result<()> {
    let sanitized_db = sanitize_identifier(database_name)?;
    let sanitized_owner = sanitize_identifier(new_owner)?;
    let alter_sql = format!(
        "ALTER DATABASE {} OWNER TO {}",
        sanitized_db, sanitized_owner
    );

    sqlx::query(AssertSqlSafe(alter_sql))
        .execute(pool)
        .await
        .context("Failed to change database owner")?;

    Ok(())
}

/// Drop a PostgreSQL database
///
/// This function sanitizes the database name internally to prevent SQL injection.
#[allow(dead_code)]
pub async fn drop_database(pool: &PgPool, database_name: &str) -> Result<()> {
    let sanitized_db = sanitize_identifier(database_name)?;
    let drop_sql = format!("DROP DATABASE {}", sanitized_db);

    sqlx::query(AssertSqlSafe(drop_sql))
        .execute(pool)
        .await
        .context("Failed to drop database")?;

    Ok(())
}

/// Drop a PostgreSQL user/role
///
/// This function sanitizes the username internally to prevent SQL injection.
#[allow(dead_code)]
pub async fn drop_user(pool: &PgPool, username: &str) -> Result<()> {
    let sanitized_username = sanitize_identifier(username)?;
    let drop_sql = format!("DROP USER {}", sanitized_username);

    sqlx::query(AssertSqlSafe(drop_sql))
        .execute(pool)
        .await
        .context("Failed to drop user")?;

    Ok(())
}
