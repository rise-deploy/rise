//! Device login (`rise login --device`): RFC 8628 with Rise as the
//! authorization server.
//!
//! The CLI starts a login at `POST /auth/authorize {flow: "device"}` and
//! polls `POST /auth/device/exchange` with the device code it received. The
//! user opens Rise's own `/device` page, signs in through the ordinary browser
//! login, and confirms the user code shown in their terminal. The next poll
//! then receives a session for the same `User` *and* the same `UserIdentity`
//! as the approving session, so disabling either ends the device session too
//! (ADR-0001 §1, §7).
//!
//! No IdP token is ever presented here: the only way in is a live,
//! identity-bound session confirming a single-use code. Approval also demands
//! a recent sign-in ([`APPROVAL_MAX_SESSION_AGE`]), so every CLI session
//! traces back to a fresh IdP login rather than to however old a browser
//! session happens to be.

use crate::db::device_authorizations::{
    self, DeviceAuthorization, DeviceAuthorizationStatus, NewDeviceAuthorization,
};
use crate::db::User;
use crate::server::auth::context::{AuthContext, SessionDetails};
use crate::server::auth::user_identity::{SessionRejection, UserLogins, UserPrincipal};
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::state::AppState;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    Extension, Json,
};
use base64::Engine;
use chrono::{DateTime, Utc};
use rand::{Rng, RngExt};
use rise_backend_auth::{RiseTokenSigner, SessionUser};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::time::Duration;
use tracing::instrument;
use uuid::Uuid;

/// How long a device login waits for confirmation.
pub const DEVICE_CODE_TTL: Duration = Duration::from_secs(600);
/// The polling interval handed to the CLI.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How much a `slow_down` answer adds to the interval (RFC 8628 §3.5).
const SLOW_DOWN_STEP_SECONDS: i32 = 5;
/// The interval `slow_down` never raises beyond.
const MAX_POLL_INTERVAL_SECONDS: i32 = 60;
/// A poll this much earlier than the interval still counts as on time, so
/// scheduling jitter on the client does not earn a `slow_down`.
const POLL_TOLERANCE_SECONDS: i64 = 1;
/// The oldest session that may approve a device login.
pub const APPROVAL_MAX_SESSION_AGE: Duration = Duration::from_secs(600);

/// The RFC 8628 §6.1 user-code alphabet: consonants only, so a code never
/// spells a word, and without easily confused characters.
const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
const USER_CODE_LEN: usize = 8;
/// Draws before giving up on finding a free user code.
const USER_CODE_ATTEMPTS: usize = 5;

/// A fresh device code: 256 random bits, base64url.
pub fn generate_device_code() -> String {
    let mut random_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut random_bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes)
}

/// A fresh user code, formatted `XXXX-XXXX`.
pub fn generate_user_code() -> String {
    let mut rng = rand::rng();
    let raw: String = (0..USER_CODE_LEN)
        .map(|_| USER_CODE_ALPHABET[rng.random_range(0..USER_CODE_ALPHABET.len())] as char)
        .collect();
    format_user_code(&raw)
}

fn format_user_code(raw: &str) -> String {
    let (head, tail) = raw.split_at(USER_CODE_LEN / 2);
    format!("{head}-{tail}")
}

/// Canonicalize what a user typed: case, spaces and dashes don't matter.
/// Returns `None` for anything that cannot be a user code.
pub fn normalize_user_code(input: &str) -> Option<String> {
    let raw: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let valid = raw.len() == USER_CODE_LEN && raw.bytes().all(|b| USER_CODE_ALPHABET.contains(&b));
    valid.then(|| format_user_code(&raw))
}

/// Only the hash of a device code is stored.
fn hash_device_code(device_code: &str) -> Vec<u8> {
    Sha256::digest(device_code.as_bytes()).to_vec()
}

/// A newly started device login, as the CLI needs it.
pub struct StartedDeviceLogin {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// The approving session, recorded on approval and turned into the device
/// session on the next poll.
#[derive(Debug, Serialize, Deserialize)]
struct ApprovedSession {
    /// Canonical `user:<name>` subject.
    subject: String,
    rise_uid: Uuid,
    identity_uid: Uuid,
    email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeviceExchangeRequest {
    pub device_code: String,
}

/// RFC 8628 §3.5 token response. Always HTTP 200: `error` carries the
/// standard codes (`authorization_pending`, `slow_down`, `access_denied`,
/// `expired_token`), plus `server_error` for a failure worth retrying.
#[derive(Debug, Serialize)]
pub struct DeviceExchangeResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

impl DeviceExchangeResponse {
    fn token(token: String) -> Self {
        Self {
            token: Some(token),
            error: None,
            error_description: None,
        }
    }

    fn error(error: &str, description: Option<&str>) -> Self {
        Self {
            token: None,
            error: Some(error.to_string()),
            error_description: description.map(str::to_string),
        }
    }

    fn server_error() -> Self {
        Self::error("server_error", Some("Internal error, try again"))
    }
}

#[derive(Debug, Serialize)]
pub struct DeviceLookupResponse {
    pub user_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_ip: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// The caller's session may not approve: it is too old, or predates
    /// identity-bound sessions. Sign in again, then approve.
    pub reauth_required: bool,
}

/// The device flow over exactly the server state it needs.
pub struct DeviceFlow<'a> {
    pub pool: &'a PgPool,
    pub user_logins: &'a UserLogins,
    pub signer: &'a RiseTokenSigner,
    pub public_url: &'a str,
}

impl<'a> DeviceFlow<'a> {
    pub fn new(state: &'a AppState) -> Self {
        Self {
            pool: &state.db_pool,
            user_logins: &state.user_logins,
            signer: &state.jwt_signer,
            public_url: &state.public_url,
        }
    }

    /// Start a device login. `client_name` and `client_ip` are only displayed
    /// on the confirmation page.
    pub async fn start(
        &self,
        client_name: Option<&str>,
        client_ip: Option<&str>,
    ) -> anyhow::Result<StartedDeviceLogin> {
        let device_code = generate_device_code();
        let device_code_hash = hash_device_code(&device_code);
        let expires_at = Utc::now() + DEVICE_CODE_TTL;
        // The name is requester-chosen display text: keep it short.
        let client_name = client_name.map(|name| name.chars().take(64).collect::<String>());

        for _ in 0..USER_CODE_ATTEMPTS {
            let user_code = generate_user_code();
            let created = device_authorizations::create(
                self.pool,
                &NewDeviceAuthorization {
                    device_code_hash: &device_code_hash,
                    user_code: &user_code,
                    client_name: client_name.as_deref(),
                    client_ip,
                    interval_seconds: POLL_INTERVAL.as_secs() as i32,
                    expires_at,
                },
            )
            .await?;
            if created {
                let verification_uri = format!("{}/device", self.public_url.trim_end_matches('/'));
                let verification_uri_complete = format!("{verification_uri}?user_code={user_code}");
                return Ok(StartedDeviceLogin {
                    device_code,
                    user_code,
                    verification_uri,
                    verification_uri_complete,
                    expires_in: DEVICE_CODE_TTL.as_secs(),
                    interval: POLL_INTERVAL.as_secs(),
                });
            }
        }
        anyhow::bail!("no free user code after {USER_CODE_ATTEMPTS} attempts")
    }

    /// Answer one poll by the CLI holding `device_code`.
    pub async fn exchange(&self, device_code: &str) -> DeviceExchangeResponse {
        let hash = hash_device_code(device_code);
        let row = match device_authorizations::find_by_device_code_hash(self.pool, &hash).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                return DeviceExchangeResponse::error(
                    "expired_token",
                    Some("Unknown or expired device code"),
                )
            }
            Err(e) => {
                tracing::error!("Failed to look up device authorization: {:?}", e);
                return DeviceExchangeResponse::server_error();
            }
        };

        if row.expires_at <= Utc::now() {
            self.discard(&row).await;
            return DeviceExchangeResponse::error("expired_token", Some("The device code expired"));
        }

        match row.status {
            DeviceAuthorizationStatus::Pending => self.poll_pending(&row).await,
            DeviceAuthorizationStatus::Denied => {
                self.discard(&row).await;
                DeviceExchangeResponse::error(
                    "access_denied",
                    Some("The login was denied in the browser"),
                )
            }
            DeviceAuthorizationStatus::Approved => self.redeem(&row).await,
        }
    }

    async fn poll_pending(&self, row: &DeviceAuthorization) -> DeviceExchangeResponse {
        let too_fast = row.last_polled_at.is_some_and(|last| {
            let elapsed = (Utc::now() - last).num_seconds();
            elapsed < i64::from(row.interval_seconds) - POLL_TOLERANCE_SECONDS
        });
        let interval = if too_fast {
            (row.interval_seconds + SLOW_DOWN_STEP_SECONDS).min(MAX_POLL_INTERVAL_SECONDS)
        } else {
            row.interval_seconds
        };
        if let Err(e) = device_authorizations::record_poll(self.pool, row.id, interval).await {
            tracing::error!("Failed to record device authorization poll: {:?}", e);
            return DeviceExchangeResponse::server_error();
        }
        if too_fast {
            DeviceExchangeResponse::error("slow_down", None)
        } else {
            DeviceExchangeResponse::error("authorization_pending", None)
        }
    }

    /// Turn an approved device login into a session, at most once.
    async fn redeem(&self, row: &DeviceAuthorization) -> DeviceExchangeResponse {
        let approved = row
            .approved_session
            .clone()
            .map(serde_json::from_value::<ApprovedSession>)
            .zip(row.approved_user_id);
        let (approved, user_id) = match approved {
            Some((Ok(approved), user_id)) => (approved, user_id),
            other => {
                tracing::error!(
                    id = %row.id,
                    "Approved device authorization has an unreadable approval: {:?}",
                    other.map(|(session, _)| session.err())
                );
                self.discard(row).await;
                return DeviceExchangeResponse::server_error();
            }
        };

        // The approval was checked when it was given; check again that its
        // User and identity are still active now that a session is minted from
        // it. A store failure keeps the row so the next poll can retry.
        match self
            .user_logins
            .resolve_session(&approved.subject, approved.rise_uid, approved.identity_uid)
            .await
        {
            Ok(_) => {}
            Err(SessionRejection::Store(e)) => {
                tracing::error!("Failed to re-resolve device login's User: {:?}", e);
                return DeviceExchangeResponse::server_error();
            }
            Err(rejection) => {
                tracing::warn!(
                    subject = %approved.subject,
                    "Device login refused at redemption: {rejection}"
                );
                self.discard(row).await;
                return DeviceExchangeResponse::error(
                    "access_denied",
                    Some("This account is disabled. Contact your Rise administrator."),
                );
            }
        }

        match device_authorizations::consume_approved(self.pool, row.id).await {
            Ok(true) => {}
            // A concurrent poll redeemed it first.
            Ok(false) => {
                return DeviceExchangeResponse::error(
                    "expired_token",
                    Some("The device code was already used"),
                )
            }
            Err(e) => {
                tracing::error!("Failed to consume device authorization: {:?}", e);
                return DeviceExchangeResponse::server_error();
            }
        }

        // On a DB error, fall back to no groups rather than failing the login,
        // as the other login flows do.
        let groups = crate::db::teams::get_team_names_for_user(self.pool, user_id)
            .await
            .ok();
        let claims = serde_json::json!({
            "sub": approved.subject,
            "email": approved.email,
            "name": approved.name,
        });
        let session_user = SessionUser {
            subject: approved.subject.clone(),
            rise_uid: approved.rise_uid,
            identity_uid: approved.identity_uid,
        };
        match self
            .signer
            .sign_user_jwt(&claims, &session_user, groups, self.public_url, None)
        {
            Ok(token) => {
                tracing::info!(
                    "CLI device login successful for user {} - issued Rise JWT",
                    approved.email
                );
                DeviceExchangeResponse::token(token)
            }
            Err(e) => {
                tracing::error!("Failed to sign Rise JWT for device login: {:#}", e);
                DeviceExchangeResponse::server_error()
            }
        }
    }

    /// Drop a device login that can no longer succeed. Failure only delays the
    /// hourly cleanup, so it is logged, not surfaced.
    async fn discard(&self, row: &DeviceAuthorization) {
        if let Err(e) = device_authorizations::delete(self.pool, row.id).await {
            tracing::warn!(id = %row.id, "Failed to delete device authorization: {:?}", e);
        }
    }

    async fn find_pending(&self, user_code: &str) -> Result<DeviceAuthorization, ServerError> {
        device_authorizations::find_pending_by_user_code(self.pool, user_code)
            .await
            .internal_err("Failed to look up device login")?
            .ok_or_else(|| ServerError::not_found("Unknown or expired code"))
    }

    /// Describe a pending device login to the session about to confirm it.
    pub async fn lookup(
        &self,
        principal: Option<&UserPrincipal>,
        details: Option<&SessionDetails>,
        user_code: &str,
    ) -> Result<DeviceLookupResponse, ServerError> {
        let user_code = parse_user_code(user_code)?;
        let row = self.find_pending(&user_code).await?;
        Ok(DeviceLookupResponse {
            user_code: row.user_code,
            client_name: row.client_name,
            client_ip: row.client_ip,
            created_at: row.created_at,
            expires_at: row.expires_at,
            reauth_required: principal.is_none() || fresh_session(details).is_none(),
        })
    }

    /// Approve a pending device login for the caller's session.
    pub async fn approve(
        &self,
        user: &User,
        principal: Option<&UserPrincipal>,
        details: Option<&SessionDetails>,
        user_code: &str,
    ) -> Result<(), ServerError> {
        let user_code = parse_user_code(user_code)?;
        let (Some(principal), Some(details)) = (principal, fresh_session(details)) else {
            // 401, like a missing session: either way the answer is to sign in.
            return Err(ServerError::unauthorized(
                "Sign in again to approve this device login",
            ));
        };

        let session = serde_json::to_value(ApprovedSession {
            subject: principal.subject().to_string(),
            rise_uid: principal.uid,
            identity_uid: details.identity_uid,
            email: user.email.clone(),
            name: details.name.clone(),
        })
        .internal_err("Failed to record device login approval")?;
        let approved = device_authorizations::approve(self.pool, &user_code, user.id, &session)
            .await
            .internal_err("Failed to approve device login")?;
        if !approved {
            return Err(ServerError::not_found("Unknown or expired code"));
        }
        tracing::info!(user = %user.email, "Device login approved");
        Ok(())
    }

    /// Deny a pending device login.
    pub async fn deny(&self, user: &User, user_code: &str) -> Result<(), ServerError> {
        let user_code = parse_user_code(user_code)?;
        let denied = device_authorizations::deny(self.pool, &user_code)
            .await
            .internal_err("Failed to deny device login")?;
        if !denied {
            return Err(ServerError::not_found("Unknown or expired code"));
        }
        tracing::info!(user = %user.email, "Device login denied");
        Ok(())
    }
}

/// Whether this session may approve a device login right now: only one issued
/// recently may.
fn fresh_session(details: Option<&SessionDetails>) -> Option<&SessionDetails> {
    let details = details?;
    let now = Utc::now().timestamp().max(0) as u64;
    (now.saturating_sub(details.issued_at) <= APPROVAL_MAX_SESSION_AGE.as_secs()).then_some(details)
}

fn parse_user_code(input: &str) -> Result<String, ServerError> {
    normalize_user_code(input).ok_or_else(|| {
        ServerError::bad_request("That is not a valid code. Codes look like BCDF-GHJK.")
    })
}

/// Poll a device login (`POST /auth/device/exchange`).
#[instrument(skip(state, payload))]
pub async fn exchange(
    State(state): State<AppState>,
    Json(payload): Json<DeviceExchangeRequest>,
) -> Json<DeviceExchangeResponse> {
    Json(DeviceFlow::new(&state).exchange(&payload.device_code).await)
}

#[derive(Debug, Deserialize)]
pub struct DeviceLookupQuery {
    pub user_code: String,
}

/// Show a pending device login to the user about to confirm it
/// (`GET /auth/device?user_code=`).
#[instrument(skip(state, auth, details))]
pub async fn lookup(
    State(state): State<AppState>,
    auth: AuthContext,
    details: Option<Extension<SessionDetails>>,
    Query(query): Query<DeviceLookupQuery>,
) -> Result<Json<DeviceLookupResponse>, ServerError> {
    auth.user()?;
    let details = details.as_ref().map(|Extension(details)| details);
    DeviceFlow::new(&state)
        .lookup(auth.user_principal(), details, &query.user_code)
        .await
        .map(Json)
}

#[derive(Debug, Deserialize)]
pub struct DeviceDecisionRequest {
    pub user_code: String,
}

/// Approve a device login for the caller (`POST /auth/device/approve`).
#[instrument(skip(state, auth, details, payload))]
pub async fn approve(
    State(state): State<AppState>,
    auth: AuthContext,
    details: Option<Extension<SessionDetails>>,
    Json(payload): Json<DeviceDecisionRequest>,
) -> Result<StatusCode, ServerError> {
    let user = auth.user()?;
    let details = details.as_ref().map(|Extension(details)| details);
    DeviceFlow::new(&state)
        .approve(user, auth.user_principal(), details, &payload.user_code)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deny a device login (`POST /auth/device/deny`).
#[instrument(skip(state, auth, payload))]
pub async fn deny(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(payload): Json<DeviceDecisionRequest>,
) -> Result<StatusCode, ServerError> {
    let user = auth.user()?;
    DeviceFlow::new(&state)
        .deny(user, &payload.user_code)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_codes_are_formatted_from_the_alphabet() {
        for _ in 0..100 {
            let code = generate_user_code();
            assert_eq!(code.len(), USER_CODE_LEN + 1);
            assert_eq!(&code[4..5], "-");
            assert!(code
                .bytes()
                .filter(|b| *b != b'-')
                .all(|b| USER_CODE_ALPHABET.contains(&b)));
            assert_eq!(normalize_user_code(&code).as_deref(), Some(code.as_str()));
        }
    }

    #[test]
    fn normalization_ignores_case_spaces_and_dashes() {
        let expected = Some("BCDF-GHJK".to_string());
        assert_eq!(normalize_user_code("bcdf-ghjk"), expected);
        assert_eq!(normalize_user_code("BCDFGHJK"), expected);
        assert_eq!(normalize_user_code(" bcdf ghjk "), expected);
        assert_eq!(normalize_user_code("B-C-D-F-G-H-J-K"), expected);
    }

    #[test]
    fn normalization_rejects_non_codes() {
        assert_eq!(normalize_user_code(""), None);
        assert_eq!(normalize_user_code("BCDF-GHJ"), None);
        assert_eq!(normalize_user_code("BCDF-GHJKL"), None);
        // Vowels and digits are not in the alphabet.
        assert_eq!(normalize_user_code("ABCD-EFGH"), None);
        assert_eq!(normalize_user_code("BCDF-1234"), None);
    }

    #[test]
    fn device_codes_are_distinct_and_hashed() {
        let a = generate_device_code();
        let b = generate_device_code();
        assert_ne!(a, b);
        assert_eq!(hash_device_code(&a), hash_device_code(&a));
        assert_ne!(hash_device_code(&a), hash_device_code(&b));
    }

    fn details(issued_at: u64) -> SessionDetails {
        SessionDetails {
            identity_uid: Uuid::nil(),
            name: None,
            issued_at,
        }
    }

    #[test]
    fn only_recent_sessions_may_approve() {
        let now = Utc::now().timestamp() as u64;
        assert!(fresh_session(Some(&details(now))).is_some());
        assert!(fresh_session(Some(&details(now - 60))).is_some());
        assert!(fresh_session(Some(&details(
            now - APPROVAL_MAX_SESSION_AGE.as_secs() - 60
        )))
        .is_none());
        assert!(fresh_session(None).is_none());
    }

    // ---- the flow, against Postgres ----------------------------------------

    use crate::server::auth::user_identity::{LoginProfile, ResolvedIdentity};
    use rise_resource_store_postgres::PgResourceStore;
    use std::sync::Arc;

    const ISSUER: &str = "https://idp.example.com";
    const RISE_URL: &str = "https://rise.test";

    struct Fixture {
        pool: PgPool,
        store: Arc<PgResourceStore>,
        logins: UserLogins,
        signer: RiseTokenSigner,
    }

    impl Fixture {
        async fn new(pool: PgPool) -> Self {
            rise_resource_store_postgres::run_migrations(&pool)
                .await
                .expect("resource store migrations");
            let store = Arc::new(PgResourceStore::new(pool.clone()));
            let logins =
                UserLogins::new(store.clone(), pool.clone(), ISSUER).expect("a canonical issuer");
            let signer = RiseTokenSigner::new(
                &base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
                RISE_URL.to_string(),
                3600,
                vec!["sub".into(), "email".into(), "name".into()],
                None,
                None,
            )
            .expect("test signer");
            Self {
                pool,
                store,
                logins,
                signer,
            }
        }

        fn flow(&self) -> DeviceFlow<'_> {
            DeviceFlow {
                pool: &self.pool,
                user_logins: &self.logins,
                signer: &self.signer,
                public_url: RISE_URL,
            }
        }

        /// A signed-in user: the typed row, and the User + identity their
        /// browser session names.
        async fn sign_in(&self, subject: &str, email: &str) -> (User, ResolvedIdentity) {
            let identity = self
                .logins
                .resolve_or_provision(
                    subject,
                    &LoginProfile {
                        email: Some(email.to_string()),
                        display_name: Some("Ada".to_string()),
                    },
                )
                .await
                .expect("login resolves");
            let user = crate::db::users::create(&self.pool, email)
                .await
                .expect("typed user");
            (user, identity)
        }

        /// Replace a resource's `spec.active`.
        async fn set_active(&self, uid: Uuid, active: bool) {
            use rise_resource_api::ResourceApi;
            let row = self.store.get(uid).await.unwrap().expect("row");
            let mut spec = row.spec.clone();
            spec["active"] = serde_json::json!(active);
            self.store
                .update(
                    uid,
                    rise_resource_api::UpdateResourceParams {
                        api_version: None,
                        revision: row.revision,
                        labels: row.labels.clone(),
                        annotations: Default::default(),
                        finalizers: row.finalizers.clone(),
                        owner_references: Vec::new(),
                        spec,
                        validator: None,
                    },
                )
                .await
                .expect("update active");
        }

        /// Pretend the last poll happened long enough ago.
        async fn age_last_poll(&self, user_code: &str) {
            sqlx::query(
                "UPDATE device_authorizations SET last_polled_at = NOW() - INTERVAL '1 hour' \
                 WHERE user_code = $1",
            )
            .bind(user_code)
            .execute(&self.pool)
            .await
            .unwrap();
        }
    }

    fn session_details(identity: &ResolvedIdentity, issued_at: u64) -> SessionDetails {
        SessionDetails {
            identity_uid: identity.identity_uid,
            name: Some("Ada".to_string()),
            issued_at,
        }
    }

    fn now() -> u64 {
        Utc::now().timestamp() as u64
    }

    fn error_of(response: &DeviceExchangeResponse) -> Option<&str> {
        response.error.as_deref()
    }

    #[sqlx::test]
    async fn an_approved_login_yields_one_session_for_the_approvers_identity(pool: PgPool) {
        let fx = Fixture::new(pool).await;
        let flow = fx.flow();
        let (user, identity) = fx.sign_in("subject-1", "ada@example.com").await;
        let details = session_details(&identity, now());

        let started = flow.start(Some("laptop"), Some("10.0.0.1")).await.unwrap();
        assert_eq!(
            started.verification_uri_complete,
            format!("{RISE_URL}/device?user_code={}", started.user_code)
        );
        let pending = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&pending), Some("authorization_pending"));

        // Lowercase and undashed input finds the same login.
        let typed = started.user_code.replace('-', "").to_lowercase();
        let request = flow
            .lookup(Some(&identity.principal), Some(&details), &typed)
            .await
            .unwrap();
        assert_eq!(request.user_code, started.user_code);
        assert_eq!(request.client_name.as_deref(), Some("laptop"));
        assert_eq!(request.client_ip.as_deref(), Some("10.0.0.1"));
        assert!(!request.reauth_required);

        flow.approve(&user, Some(&identity.principal), Some(&details), &typed)
            .await
            .unwrap();
        let redeemed = flow.exchange(&started.device_code).await;
        let token = redeemed.token.expect("a session after approval");
        let claims = fx.signer.verify_user_jwt(&token, RISE_URL).unwrap();
        assert_eq!(claims.sub, identity.principal.subject().to_string());
        assert_eq!(claims.rise_uid, Some(identity.principal.uid));
        assert_eq!(claims.rise_identity_uid, Some(identity.identity_uid));
        assert_eq!(claims.email, "ada@example.com");
        assert_eq!(claims.name.as_deref(), Some("Ada"));

        let again = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&again), Some("expired_token"));
        assert!(again.token.is_none());
    }

    #[sqlx::test]
    async fn a_denied_login_tells_the_cli_once(pool: PgPool) {
        let fx = Fixture::new(pool).await;
        let flow = fx.flow();
        let (user, _) = fx.sign_in("subject-1", "ada@example.com").await;

        let started = flow.start(None, None).await.unwrap();
        flow.deny(&user, &started.user_code).await.unwrap();
        let denied = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&denied), Some("access_denied"));
        let after = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&after), Some("expired_token"));
        // A decided code is no longer pending.
        let err = flow.deny(&user, &started.user_code).await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn only_a_fresh_identity_bound_session_may_approve(pool: PgPool) {
        let fx = Fixture::new(pool).await;
        let flow = fx.flow();
        let (user, identity) = fx.sign_in("subject-1", "ada@example.com").await;
        let started = flow.start(None, None).await.unwrap();
        let code = &started.user_code;

        let stale = session_details(&identity, now() - APPROVAL_MAX_SESSION_AGE.as_secs() - 60);
        let request = flow
            .lookup(Some(&identity.principal), Some(&stale), code)
            .await
            .unwrap();
        assert!(request.reauth_required);
        let err = flow
            .approve(&user, Some(&identity.principal), Some(&stale), code)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);

        // A legacy session names no User or identity.
        let request = flow.lookup(None, None, code).await.unwrap();
        assert!(request.reauth_required);
        let err = flow.approve(&user, None, None, code).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);

        // Neither refusal consumed the code.
        let pending = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&pending), Some("authorization_pending"));
    }

    #[sqlx::test]
    async fn disabling_the_identity_before_redemption_refuses_the_session(pool: PgPool) {
        let fx = Fixture::new(pool).await;
        let flow = fx.flow();
        let (user, identity) = fx.sign_in("subject-1", "ada@example.com").await;
        let details = session_details(&identity, now());
        let started = flow.start(None, None).await.unwrap();
        flow.approve(
            &user,
            Some(&identity.principal),
            Some(&details),
            &started.user_code,
        )
        .await
        .unwrap();

        fx.set_active(identity.identity_uid, false).await;
        let refused = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&refused), Some("access_denied"));
        assert!(refused.token.is_none());
    }

    #[sqlx::test]
    async fn polling_faster_than_the_interval_slows_the_client_down(pool: PgPool) {
        let fx = Fixture::new(pool).await;
        let flow = fx.flow();
        let started = flow.start(None, None).await.unwrap();

        let first = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&first), Some("authorization_pending"));
        let second = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&second), Some("slow_down"));

        // The interval grew, so a poll that would have been on time before is
        // still too early.
        let row = device_authorizations::find_pending_by_user_code(&fx.pool, &started.user_code)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.interval_seconds,
            POLL_INTERVAL.as_secs() as i32 + SLOW_DOWN_STEP_SECONDS
        );
        fx.age_last_poll(&started.user_code).await;
        let later = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&later), Some("authorization_pending"));
    }

    #[sqlx::test]
    async fn an_expired_code_can_neither_be_approved_nor_redeemed(pool: PgPool) {
        let fx = Fixture::new(pool).await;
        let flow = fx.flow();
        let (user, identity) = fx.sign_in("subject-1", "ada@example.com").await;
        let details = session_details(&identity, now());
        let started = flow.start(None, None).await.unwrap();
        sqlx::query(
            "UPDATE device_authorizations SET expires_at = NOW() - INTERVAL '1 second' \
             WHERE user_code = $1",
        )
        .bind(&started.user_code)
        .execute(&fx.pool)
        .await
        .unwrap();

        let err = flow
            .approve(
                &user,
                Some(&identity.principal),
                Some(&details),
                &started.user_code,
            )
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        let expired = flow.exchange(&started.device_code).await;
        assert_eq!(error_of(&expired), Some("expired_token"));
        assert_eq!(
            device_authorizations::delete_expired(&fx.pool)
                .await
                .unwrap(),
            0,
            "redeeming an expired code already removed it"
        );
    }

    #[sqlx::test]
    async fn an_unknown_device_code_is_expired(pool: PgPool) {
        let fx = Fixture::new(pool).await;
        let unknown = fx.flow().exchange("not-a-device-code").await;
        assert_eq!(error_of(&unknown), Some("expired_token"));
    }
}
