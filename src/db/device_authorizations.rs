//! Pending device logins (`rise login --device`). See the
//! `device_authorizations` migration for the table's shape and purpose.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Where a device login stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAuthorizationStatus {
    /// Waiting for the user to confirm the code in the browser.
    Pending,
    /// Confirmed; the next poll receives a session.
    Approved,
    /// Refused in the browser.
    Denied,
}

impl DeviceAuthorizationStatus {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "denied" => Ok(Self::Denied),
            other => anyhow::bail!("unknown device authorization status '{other}'"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    pub id: Uuid,
    pub user_code: String,
    pub status: DeviceAuthorizationStatus,
    pub approved_user_id: Option<Uuid>,
    pub approved_session: Option<serde_json::Value>,
    pub client_name: Option<String>,
    pub client_ip: Option<String>,
    pub interval_seconds: i32,
    pub last_polled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// The columns every read returns, in [`DeviceAuthorization`] order.
struct Row {
    id: Uuid,
    user_code: String,
    status: String,
    approved_user_id: Option<Uuid>,
    approved_session: Option<serde_json::Value>,
    client_name: Option<String>,
    client_ip: Option<String>,
    interval_seconds: i32,
    last_polled_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl TryFrom<Row> for DeviceAuthorization {
    type Error = anyhow::Error;

    fn try_from(row: Row) -> Result<Self> {
        Ok(Self {
            id: row.id,
            user_code: row.user_code,
            status: DeviceAuthorizationStatus::parse(&row.status)?,
            approved_user_id: row.approved_user_id,
            approved_session: row.approved_session,
            client_name: row.client_name,
            client_ip: row.client_ip,
            interval_seconds: row.interval_seconds,
            last_polled_at: row.last_polled_at,
            created_at: row.created_at,
            expires_at: row.expires_at,
        })
    }
}

/// Parameters of a new pending device login.
pub struct NewDeviceAuthorization<'a> {
    pub device_code_hash: &'a [u8],
    pub user_code: &'a str,
    pub client_name: Option<&'a str>,
    pub client_ip: Option<&'a str>,
    pub interval_seconds: i32,
    pub expires_at: DateTime<Utc>,
}

/// Insert a pending device login.
///
/// Returns `Ok(false)` when the user code is already taken (by a live or a
/// not-yet-cleaned-up row), so the caller can draw another one.
pub async fn create(pool: &PgPool, new: &NewDeviceAuthorization<'_>) -> Result<bool> {
    let result = sqlx::query!(
        r#"
        INSERT INTO device_authorizations
            (device_code_hash, user_code, client_name, client_ip, interval_seconds, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (user_code) DO NOTHING
        "#,
        new.device_code_hash,
        new.user_code,
        new.client_name,
        new.client_ip,
        new.interval_seconds,
        new.expires_at,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// The pending, unexpired device login with this user code.
pub async fn find_pending_by_user_code(
    pool: &PgPool,
    user_code: &str,
) -> Result<Option<DeviceAuthorization>> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT id, user_code, status, approved_user_id, approved_session, client_name,
               client_ip, interval_seconds, last_polled_at, created_at, expires_at
        FROM device_authorizations
        WHERE user_code = $1 AND status = 'pending' AND expires_at > NOW()
        "#,
        user_code,
    )
    .fetch_optional(pool)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// The device login a polling CLI holds, whatever its status or expiry.
pub async fn find_by_device_code_hash(
    pool: &PgPool,
    device_code_hash: &[u8],
) -> Result<Option<DeviceAuthorization>> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT id, user_code, status, approved_user_id, approved_session, client_name,
               client_ip, interval_seconds, last_polled_at, created_at, expires_at
        FROM device_authorizations
        WHERE device_code_hash = $1
        "#,
        device_code_hash,
    )
    .fetch_optional(pool)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Approve a pending, unexpired device login for `user_id`, recording the
/// approving session. Returns `false` if the code is no longer pending.
pub async fn approve(
    pool: &PgPool,
    user_code: &str,
    user_id: Uuid,
    session: &serde_json::Value,
) -> Result<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE device_authorizations
        SET status = 'approved', approved_user_id = $2, approved_session = $3
        WHERE user_code = $1 AND status = 'pending' AND expires_at > NOW()
        "#,
        user_code,
        user_id,
        session,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Deny a pending, unexpired device login. Returns `false` if the code is no
/// longer pending.
pub async fn deny(pool: &PgPool, user_code: &str) -> Result<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE device_authorizations
        SET status = 'denied'
        WHERE user_code = $1 AND status = 'pending' AND expires_at > NOW()
        "#,
        user_code,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Record a poll of a pending device login, with the interval the client must
/// keep from now on.
pub async fn record_poll(pool: &PgPool, id: Uuid, interval_seconds: i32) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE device_authorizations
        SET last_polled_at = NOW(), interval_seconds = $2
        WHERE id = $1
        "#,
        id,
        interval_seconds,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove an approved device login, returning whether this call removed it.
///
/// Exactly one concurrent poll wins, so an approval yields at most one
/// session.
pub async fn consume_approved(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query!(
        "DELETE FROM device_authorizations WHERE id = $1 AND status = 'approved'",
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn delete(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query!("DELETE FROM device_authorizations WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Remove expired device logins; returns how many were removed.
pub async fn delete_expired(pool: &PgPool) -> Result<u64> {
    let result = sqlx::query!("DELETE FROM device_authorizations WHERE expires_at <= NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
