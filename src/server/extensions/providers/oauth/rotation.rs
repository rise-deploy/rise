//! Rotating refresh-token handles (`rrt2.`).
//!
//! In `rotating` mode the app's refresh token is single use. Every refresh
//! consumes the presented handle and returns a successor in the same *family*
//! (the chain of handles descending from one login); the upstream refresh
//! token is kept encrypted in PostgreSQL and never leaves Rise.
//!
//! A handle is `rrt2.` plus 32 random bytes, base64url. It carries no data and
//! only its SHA-256 is stored. Everything stored for a handle is encrypted
//! under a context bound to the project, the extension and that handle's hash.
//!
//! A family is started for one [`UpstreamIdentity`] (the extension's
//! `issuer_url`, `token_endpoint` override and `client_id`) and refuses to
//! refresh once the extension is configured differently, before anything is
//! claimed or sent upstream.
//!
//! Refreshing, in short (the SQL is in `db::oauth_refresh_tokens`):
//! 1. One request atomically *claims* the handle and calls the upstream.
//! 2. On success it records the successor and the exact response it returns:
//!    the upstream's own response with only `refresh_token` replaced by the
//!    successor. Until the grace period is over, the same handle returns that
//!    same response, so parallel tabs and retried requests agree on one
//!    successor, and nothing is asked of the upstream (or of anything the
//!    upstream call needs, such as endpoint discovery or the client secret).
//! 3. A handle presented after its grace period is reuse: the whole family is
//!    revoked and its stored secrets are erased.
//!
//! The upstream call is a parameter ([`Rotation::refresh`] takes a closure),
//! so the state machine runs without an HTTP server.

use super::models::RefreshTokenSettings;
use super::provider::UpstreamReply;
use super::refresh_handle::{
    classify_decrypt_error, parse_response_body, HandleError, ResponseBody, TokenReply,
    UnsealedRefreshToken, UpstreamIdentity,
};
use crate::db::oauth_refresh_tokens::{self as store, HandleState, RotationOutcome};
use crate::server::encryption::{EncryptionContext, EncryptionProvider};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::future::Future;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

/// Scheme prefix of the server-side-state handle.
pub const ROTATING_HANDLE_PREFIX: &str = "rrt2.";

/// How long a request waits for a concurrent refresh of the same handle to
/// record its successor before giving up with a retryable error.
const WAIT_FOR_CONCURRENT_REFRESH: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Upper bound for one upstream refresh call. It is well inside the claim
/// lease, so a claim cannot be taken over while its holder is still waiting.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether `token` is a `rotating`-mode handle.
pub fn is_rotating_handle(token: &str) -> bool {
    token.starts_with(ROTATING_HANDLE_PREFIX)
}

fn new_handle() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("{ROTATING_HANDLE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn hash_handle(handle: &str) -> [u8; 32] {
    Sha256::digest(handle.as_bytes()).into()
}

/// Which stored value of a handle an encryption context protects.
#[derive(Clone, Copy)]
enum Slot {
    UpstreamToken,
    Response,
}

fn context(project: Uuid, extension: &str, handle_hash: &[u8], slot: Slot) -> EncryptionContext {
    let slot = match slot {
        Slot::UpstreamToken => "upstream",
        Slot::Response => "response",
    };
    EncryptionContext::extension(
        project,
        "oauth",
        extension,
        &format!(
            "/tokens/refresh/{}/{slot}",
            URL_SAFE_NO_PAD.encode(handle_hash)
        ),
    )
}

/// A token response as kept for the grace window.
#[derive(Serialize, Deserialize)]
struct StoredReply {
    status: u16,
    content_type: Option<String>,
    body: String,
}

/// Why the upstream call could not produce an answer. Raised by the closure
/// given to [`Rotation::refresh`]; the claim on the handle is released.
#[derive(Debug)]
pub enum UpstreamFailure {
    /// The request failed or timed out.
    Unreachable(anyhow::Error),
    /// Rise could not set the call up (unresolvable or forbidden endpoint, an
    /// unreadable client secret): a configuration problem.
    Misconfigured(anyhow::Error),
    /// A transient encryption-provider failure while setting the call up.
    Unavailable(anyhow::Error),
}

/// Why a rotating-mode request did not produce a token response.
#[derive(Debug)]
pub enum RotationError {
    /// The handle is unknown, expired, revoked, reused, unreadable, or was
    /// issued for another upstream than the extension's current one.
    InvalidGrant,
    /// A concurrent refresh of the same handle did not finish in time.
    TemporarilyUnavailable,
    /// The upstream answered with an error that is no verdict on the token;
    /// it is passed on as it is. The handle stays usable.
    Passthrough(UpstreamReply),
    /// The upstream could not be reached or timed out; the handle stays usable.
    Unreachable(anyhow::Error),
    /// The upstream answered 2xx with something that is not a JSON object token
    /// response; the handle stays usable.
    Malformed(anyhow::Error),
    /// The extension could not be called (endpoint or credential problem).
    Misconfigured(anyhow::Error),
    /// The store or the encryption provider failed.
    Internal(anyhow::Error),
}

fn internal(error: impl Into<anyhow::Error>) -> RotationError {
    RotationError::Internal(error.into())
}

/// Rotating-mode refresh-token handling for one project extension.
pub struct Rotation<'a> {
    pub pool: &'a PgPool,
    pub encryption: &'a dyn EncryptionProvider,
    pub project: Uuid,
    pub extension: &'a str,
    pub settings: RefreshTokenSettings,
    /// The upstream families are started for, and the one they are checked
    /// against when refreshed.
    pub upstream: UpstreamIdentity,
    /// How long to wait for a concurrent refresh of the same handle.
    pub wait: Duration,
    /// Upper bound for one upstream refresh call.
    pub upstream_timeout: Duration,
}

impl<'a> Rotation<'a> {
    pub fn new(
        pool: &'a PgPool,
        encryption: &'a dyn EncryptionProvider,
        project: Uuid,
        extension: &'a str,
        settings: RefreshTokenSettings,
        upstream: UpstreamIdentity,
    ) -> Self {
        Self {
            pool,
            encryption,
            project,
            extension,
            settings,
            upstream,
            wait: WAIT_FOR_CONCURRENT_REFRESH,
            upstream_timeout: UPSTREAM_TIMEOUT,
        }
    }

    /// Start a family for `upstream_refresh_token` and return its first handle.
    async fn start_family(&self, upstream_refresh_token: &str) -> Result<(String, Uuid)> {
        let handle = new_handle();
        let hash = hash_handle(&handle);
        let encrypted = self
            .encryption
            .encrypt(
                upstream_refresh_token,
                &context(self.project, self.extension, &hash, Slot::UpstreamToken),
            )
            .await?;
        let family = store::create_family(
            self.pool,
            self.project,
            self.extension,
            self.upstream.fingerprint(),
            self.settings.max_lifetime(),
            &hash,
            &encrypted,
        )
        .await?;
        Ok((handle, family))
    }

    /// Replace the upstream `refresh_token` of a code-grant response by the
    /// first handle of a new family.
    ///
    /// The body rules are those of the sealed mode, except that a 2xx response
    /// must end up carrying a handle: a body that is not a JSON object with a
    /// refresh token is passed on only if it shows no sign of one.
    pub async fn issue_in_token_response(
        &self,
        status: u16,
        body: String,
    ) -> Result<String, super::refresh_handle::ReplyError> {
        use super::refresh_handle::ReplyError;
        if !(200..300).contains(&status) {
            return Ok(body);
        }
        match parse_response_body(body.as_bytes()) {
            ResponseBody::Opaque => Ok(body),
            ResponseBody::Unparseable => Err(ReplyError::Unparseable),
            ResponseBody::Object(mut members) => {
                let Some(upstream) = members.refresh_token() else {
                    return Ok(body);
                };
                let unavailable = |e: anyhow::Error| ReplyError::Seal(classify_decrypt_error(e));
                let (handle, family) = self.start_family(&upstream).await.map_err(unavailable)?;
                info!(
                    family_id = %family,
                    project_id = %self.project,
                    extension = self.extension,
                    outcome = "family_started",
                    "rotating refresh token family issued"
                );
                members
                    .set_refresh_token(&handle)
                    .and_then(|()| members.to_body())
                    .map_err(|e| ReplyError::Seal(HandleError::Unavailable(e)))
                    .map(|body| String::from_utf8(body).expect("serde_json writes UTF-8"))
            }
        }
    }

    /// Refresh with a handle previously issued by this mode.
    ///
    /// Returns the token response for the app. `upstream` is called with the
    /// upstream refresh token when this request wins the claim on the handle,
    /// and not otherwise: replays and requests waiting for the winner do no
    /// upstream work, including whatever the closure sets up.
    pub async fn refresh<F, Fut>(
        &self,
        handle: &str,
        upstream: F,
    ) -> Result<TokenReply, RotationError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<UpstreamReply, UpstreamFailure>>,
    {
        let hash = hash_handle(handle);
        let claimant = Uuid::new_v4();
        let deadline = Instant::now() + self.wait;
        let mut upstream = Some(upstream);
        loop {
            let claimed = store::claim(
                self.pool,
                &hash,
                self.project,
                self.extension,
                self.upstream.fingerprint(),
                claimant,
                store::CLAIM_LEASE,
            )
            .await
            .map_err(internal)?;
            if let Some(claimed) = claimed {
                let upstream = upstream.take().expect("the claim is won at most once");
                return self.rotate(&hash, claimant, claimed, upstream).await;
            }

            match store::state(
                self.pool,
                &hash,
                self.project,
                self.extension,
                self.upstream.fingerprint(),
            )
            .await
            .map_err(internal)?
            {
                HandleState::Unknown | HandleState::FamilyEnded => {
                    return Err(RotationError::InvalidGrant)
                }
                HandleState::UpstreamChanged => {
                    warn!(
                        project_id = %self.project,
                        extension = self.extension,
                        outcome = "upstream_changed",
                        "rotating refresh token was issued for another upstream than the extension's current one"
                    );
                    return Err(RotationError::InvalidGrant);
                }
                HandleState::Rotated {
                    within_grace: true,
                    response_encrypted,
                    family_id,
                } => {
                    let encrypted = response_encrypted
                        .ok_or_else(|| internal(anyhow!("recorded refresh response is missing")))?;
                    let stored = self
                        .encryption
                        .decrypt(
                            &encrypted,
                            &context(self.project, self.extension, &hash, Slot::Response),
                        )
                        .await
                        .map_err(|e| self.unreadable(family_id, e))?;
                    let reply = serde_json::from_str::<StoredReply>(&stored)
                        .map_err(|e| self.unreadable(family_id, e.into()))?;
                    info!(
                        family_id = %family_id,
                        project_id = %self.project,
                        extension = self.extension,
                        outcome = "grace_replay",
                        "rotating refresh token replayed within grace period"
                    );
                    return Ok(TokenReply {
                        status: reply.status,
                        content_type: reply.content_type,
                        body: reply.body.into_bytes(),
                    });
                }
                HandleState::Rotated { family_id, .. } => {
                    self.revoke(family_id, "reuse_detected").await;
                    return Err(RotationError::InvalidGrant);
                }
                HandleState::InFlight | HandleState::Available => {
                    if Instant::now() >= deadline {
                        warn!(
                            project_id = %self.project,
                            extension = self.extension,
                            outcome = "concurrent_refresh_timeout",
                            "concurrent refresh of a rotating refresh token did not finish in time"
                        );
                        return Err(RotationError::TemporarilyUnavailable);
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// The winner of a claim: refresh upstream and record the successor.
    async fn rotate<F, Fut>(
        &self,
        hash: &[u8; 32],
        claimant: Uuid,
        claimed: store::ClaimedHandle,
        upstream: F,
    ) -> Result<TokenReply, RotationError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<UpstreamReply, UpstreamFailure>>,
    {
        let family_id = claimed.family_id;
        let upstream_token = match self
            .encryption
            .decrypt(
                &claimed.upstream_token_encrypted,
                &context(self.project, self.extension, hash, Slot::UpstreamToken),
            )
            .await
        {
            Ok(token) => token,
            Err(e) => {
                self.release(hash, claimant).await;
                return Err(self.unreadable(family_id, e));
            }
        };

        let reply = match self.call_upstream(upstream, upstream_token.clone()).await {
            Ok(reply) => reply,
            Err(failure) => {
                self.release(hash, claimant).await;
                return Err(match failure {
                    UpstreamFailure::Unreachable(e) => RotationError::Unreachable(e),
                    UpstreamFailure::Misconfigured(e) => RotationError::Misconfigured(e),
                    UpstreamFailure::Unavailable(e) => RotationError::Internal(e),
                });
            }
        };
        if reply.is_invalid_grant() {
            self.revoke(family_id, "upstream_invalid_grant").await;
            return Err(RotationError::InvalidGrant);
        }
        if !reply.is_success() {
            // No verdict on the token: pass the answer on and let a retry use it.
            self.release(hash, claimant).await;
            return Err(RotationError::Passthrough(reply));
        }
        let ResponseBody::Object(mut members) = parse_response_body(&reply.body) else {
            self.release(hash, claimant).await;
            return Err(RotationError::Malformed(anyhow!(
                "upstream 2xx token response is not a JSON object"
            )));
        };

        // The upstream's new token if it rotated its own, otherwise the one it
        // just accepted, which stays valid.
        let next_upstream = members.refresh_token().unwrap_or(upstream_token);
        let successor = new_handle();
        let successor_hash = hash_handle(&successor);
        let built = members
            .set_refresh_token(&successor)
            .and_then(|()| members.to_body())
            .map(|body| TokenReply {
                status: reply.status,
                content_type: reply.content_type.clone(),
                body,
            });
        let reply = match built {
            Ok(reply) => reply,
            Err(e) => {
                self.release(hash, claimant).await;
                return Err(internal(e));
            }
        };

        match self
            .record_successor(
                hash,
                claimant,
                family_id,
                &next_upstream,
                &successor_hash,
                &reply,
            )
            .await
        {
            Ok(RotationOutcome::Recorded) => {
                info!(
                    family_id = %family_id,
                    project_id = %self.project,
                    extension = self.extension,
                    outcome = "rotated",
                    "rotating refresh token rotated"
                );
                Ok(reply)
            }
            // Revoked while the upstream request ran: no tokens for a dead login.
            Ok(RotationOutcome::FamilyEnded) => Err(RotationError::InvalidGrant),
            Ok(RotationOutcome::ClaimLost) => {
                warn!(
                    family_id = %family_id,
                    project_id = %self.project,
                    extension = self.extension,
                    outcome = "claim_lost",
                    "claim on a rotating refresh token expired during the upstream request"
                );
                Err(RotationError::TemporarilyUnavailable)
            }
            Err(e) => {
                self.release(hash, claimant).await;
                Err(internal(e))
            }
        }
    }

    async fn record_successor(
        &self,
        hash: &[u8; 32],
        claimant: Uuid,
        family_id: Uuid,
        next_upstream: &str,
        successor_hash: &[u8; 32],
        reply: &TokenReply,
    ) -> Result<RotationOutcome> {
        let stored = serde_json::to_string(&StoredReply {
            status: reply.status,
            content_type: reply.content_type.clone(),
            body: String::from_utf8(reply.body.clone()).context("Token response is not UTF-8")?,
        })?;
        let upstream_encrypted = self
            .encryption
            .encrypt(
                next_upstream,
                &context(
                    self.project,
                    self.extension,
                    successor_hash,
                    Slot::UpstreamToken,
                ),
            )
            .await?;
        let response_encrypted = self
            .encryption
            .encrypt(
                &stored,
                &context(self.project, self.extension, hash, Slot::Response),
            )
            .await?;
        store::record_rotation(
            self.pool,
            &store::Rotation {
                claimed_handle: hash,
                claimant,
                family_id,
                grace: self.settings.grace_period(),
                successor_handle: successor_hash,
                successor_upstream_token_encrypted: &upstream_encrypted,
                response_encrypted: &response_encrypted,
            },
        )
        .await
    }

    /// Refresh with an `rrt1.` handle that has been unsealed by the caller, and
    /// start a family from the result. This is how an extension moves from
    /// `sealed` to `rotating` without signing anyone out.
    ///
    /// The login's absolute lifetime counts from the handle's issue time: one
    /// that is older than `max_lifetime_seconds` at `now` (Unix seconds) is not
    /// accepted.
    pub async fn refresh_sealed<F, Fut>(
        &self,
        sealed: &UnsealedRefreshToken,
        now: i64,
        upstream: F,
    ) -> Result<TokenReply, RotationError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<UpstreamReply, UpstreamFailure>>,
    {
        let lifetime = i64::try_from(self.settings.max_lifetime().as_secs()).unwrap_or(i64::MAX);
        if sealed.issued_at.saturating_add(lifetime) <= now {
            info!(
                project_id = %self.project,
                extension = self.extension,
                outcome = "sealed_handle_past_max_lifetime",
                "sealed refresh handle is older than the maximum login lifetime"
            );
            return Err(RotationError::InvalidGrant);
        }
        let reply = match self
            .call_upstream(upstream, sealed.upstream_refresh_token.clone())
            .await
        {
            Ok(reply) => reply,
            Err(UpstreamFailure::Unreachable(e)) => return Err(RotationError::Unreachable(e)),
            Err(UpstreamFailure::Misconfigured(e)) => return Err(RotationError::Misconfigured(e)),
            Err(UpstreamFailure::Unavailable(e)) => return Err(RotationError::Internal(e)),
        };
        if reply.is_invalid_grant() {
            return Err(RotationError::InvalidGrant);
        }
        if !reply.is_success() {
            return Err(RotationError::Passthrough(reply));
        }
        let ResponseBody::Object(mut members) = parse_response_body(&reply.body) else {
            return Err(RotationError::Malformed(anyhow!(
                "upstream 2xx token response is not a JSON object"
            )));
        };
        let next_upstream = members
            .refresh_token()
            .unwrap_or_else(|| sealed.upstream_refresh_token.clone());
        let (handle, family) = self.start_family(&next_upstream).await.map_err(|e| {
            match classify_decrypt_error(e) {
                HandleError::Rejected(e) | HandleError::Unavailable(e) => internal(e),
            }
        })?;
        info!(
            family_id = %family,
            project_id = %self.project,
            extension = self.extension,
            outcome = "family_started_from_sealed",
            "rotating refresh token family started from a sealed handle"
        );
        members
            .set_refresh_token(&handle)
            .and_then(|()| members.to_body())
            .map_err(internal)
            .map(|body| TokenReply {
                status: reply.status,
                content_type: reply.content_type,
                body,
            })
    }

    async fn call_upstream<F, Fut>(
        &self,
        upstream: F,
        upstream_refresh_token: String,
    ) -> Result<UpstreamReply, UpstreamFailure>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<UpstreamReply, UpstreamFailure>>,
    {
        tokio::time::timeout(self.upstream_timeout, upstream(upstream_refresh_token))
            .await
            .unwrap_or_else(|_| {
                Err(UpstreamFailure::Unreachable(anyhow!(
                    "upstream refresh timed out after {:?}",
                    self.upstream_timeout
                )))
            })
    }

    /// A stored value that does not decrypt for its handle. If the ciphertext
    /// was rejected the record was tampered with (or moved between rows): the
    /// handle is not usable. Anything else says nothing about the record and
    /// is a retryable failure.
    fn unreadable(&self, family_id: Uuid, error: anyhow::Error) -> RotationError {
        if rise_backend_core::encryption::is_ciphertext_rejection(&error)
            || error.downcast_ref::<serde_json::Error>().is_some()
        {
            error!(
                security_event = "refresh_token_record_unreadable",
                family_id = %family_id,
                project_id = %self.project,
                extension = self.extension,
                "stored rotating refresh token record is unreadable: {:?}",
                error
            );
            RotationError::InvalidGrant
        } else {
            internal(error)
        }
    }

    async fn revoke(&self, family_id: Uuid, reason: &'static str) {
        match store::revoke_family(self.pool, family_id).await {
            Ok(_) => warn!(
                security_event = "refresh_token_family_revoked",
                reason,
                family_id = %family_id,
                project_id = %self.project,
                extension = self.extension,
                "rotating refresh token family revoked"
            ),
            Err(e) => error!(
                security_event = "refresh_token_family_revoke_failed",
                reason,
                family_id = %family_id,
                project_id = %self.project,
                extension = self.extension,
                "failed to revoke rotating refresh token family: {:?}",
                e
            ),
        }
    }

    async fn release(&self, hash: &[u8; 32], claimant: Uuid) {
        if let Err(e) = store::release_claim(self.pool, hash, claimant).await {
            error!(
                project_id = %self.project,
                extension = self.extension,
                "failed to release claim on a rotating refresh token (it expires on its own): {:?}",
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{extensions, projects, users, ProjectStatus};
    use crate::server::encryption::providers::local::LocalEncryptionProvider;
    use crate::server::extensions::providers::oauth::refresh_handle;
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn encryption() -> LocalEncryptionProvider {
        LocalEncryptionProvider::new(&base64::engine::general_purpose::STANDARD.encode([9u8; 32]))
            .unwrap()
    }

    async fn project(pool: &PgPool, name: &str, extensions_: &[&str]) -> Uuid {
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
        for extension in extensions_ {
            extensions::create(pool, project.id, extension, "oauth", &json!({}))
                .await
                .unwrap();
        }
        project.id
    }

    fn settings() -> RefreshTokenSettings {
        RefreshTokenSettings {
            mode: super::super::models::RefreshTokenMode::Rotating,
            grace_period_seconds: Some(30),
            max_lifetime_seconds: Some(3600),
        }
    }

    /// The upstream identity of the extension in most tests.
    pub(super) fn identity() -> UpstreamIdentity {
        UpstreamIdentity::from_parts(
            "https://idp.example.com",
            Some("https://idp.example.com/token"),
            "upstream-client",
        )
    }

    /// The scope `rrt1.` handles are sealed under in the tests.
    fn handle_scope(project: Uuid) -> refresh_handle::HandleScope<'static> {
        refresh_handle::HandleScope {
            project,
            extension: "login",
            extension_created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            upstream: identity(),
        }
    }

    fn rotation<'a>(
        pool: &'a PgPool,
        encryption: &'a LocalEncryptionProvider,
        project: Uuid,
        extension: &'a str,
    ) -> Rotation<'a> {
        let mut rotation =
            Rotation::new(pool, encryption, project, extension, settings(), identity());
        rotation.wait = Duration::from_millis(400);
        rotation
    }

    fn upstream_json(status: u16, body: Value) -> UpstreamReply {
        UpstreamReply {
            status,
            content_type: Some("application/json".into()),
            body: body.to_string().into_bytes(),
        }
    }

    /// A 2xx token response, with the upstream's own refresh token if it rotated.
    fn ok(access_token: &str, new_upstream: Option<&str>) -> UpstreamReply {
        let mut body =
            json!({"access_token": access_token, "token_type": "Bearer", "scope": "upstream"});
        if let Some(token) = new_upstream {
            body["refresh_token"] = json!(token);
        }
        upstream_json(200, body)
    }

    /// Records the upstream refresh tokens it is called with.
    #[derive(Clone, Default)]
    struct FakeUpstream {
        calls: Arc<std::sync::Mutex<Vec<String>>>,
        delay: Duration,
    }

    type Answer =
        std::pin::Pin<Box<dyn Future<Output = Result<UpstreamReply, UpstreamFailure>> + Send>>;

    impl FakeUpstream {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn answer(
            &self,
            reply: Result<UpstreamReply, UpstreamFailure>,
        ) -> impl FnOnce(String) -> Answer {
            let this = self.clone();
            move |token| {
                Box::pin(async move {
                    this.calls.lock().unwrap().push(token);
                    tokio::time::sleep(this.delay).await;
                    reply
                })
            }
        }
    }

    fn token(reply: &TokenReply) -> (Value, String) {
        let value: Value = serde_json::from_slice(&reply.body).unwrap();
        let handle = value["refresh_token"].as_str().unwrap().to_string();
        (value, handle)
    }

    /// Log in: issue a family for `upstream_rt` and return its first handle.
    async fn login(rotation: &Rotation<'_>, upstream_rt: &str) -> String {
        let body = rotation
            .issue_in_token_response(
                200,
                json!({"access_token": "at0", "token_type": "Bearer", "refresh_token": upstream_rt})
                    .to_string(),
            )
            .await
            .unwrap();
        serde_json::from_str::<Value>(&body).unwrap()["refresh_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn expire_grace(pool: &PgPool) {
        sqlx::query("UPDATE oauth_refresh_tokens SET grace_until = NOW() - INTERVAL '1 second' WHERE rotated_at IS NOT NULL")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn stored_secrets(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM oauth_refresh_tokens WHERE upstream_token_encrypted IS NOT NULL OR successor_response_encrypted IS NOT NULL",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn code_grant_issues_a_stored_handle_without_leaking_the_upstream_token(pool: PgPool) {
        let project = project(&pool, "issue", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let body = rotation
            .issue_in_token_response(
                200,
                r#"{"zeta":1,"access_token":"at","token_type":"Bearer","refresh_token":"upstream-rt","scope":"a b","extra":[1]}"#.into(),
            )
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&body).unwrap();
        let handle = value["refresh_token"].as_str().unwrap().to_string();
        assert!(is_rotating_handle(&handle));
        assert_eq!(handle.len(), "rrt2.".len() + 43);
        assert!(!body.contains("upstream-rt"));
        assert_eq!(value["scope"], "a b");
        assert_eq!(value["extra"], json!([1]));
        assert!(body.find("zeta").unwrap() < body.find("access_token").unwrap());
        assert!(body.find("access_token").unwrap() < body.find("refresh_token").unwrap());

        // Only the hash of the handle is stored, and the stored token is ciphertext.
        let (hash, stored): (Vec<u8>, String) = sqlx::query_as(
            "SELECT handle_hash, upstream_token_encrypted FROM oauth_refresh_tokens",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(hash, hash_handle(&handle).to_vec());
        assert!(!stored.contains("upstream-rt"));
        let expires_in_range: bool = sqlx::query_scalar(
            "SELECT expires_at BETWEEN NOW() + INTERVAL '3590 seconds' AND NOW() + INTERVAL '3600 seconds' FROM oauth_refresh_families",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            expires_in_range,
            "family lifetime follows max_lifetime_seconds"
        );

        // Responses without a usable refresh token and failures pass through.
        for (status, body) in [
            (200, r#"{"access_token":"at"}"#),
            (200, r#"{"access_token":"at","refresh_token":""}"#),
            (400, r#"{"error":"invalid_grant","refresh_token":"rt"}"#),
            (200, "not json"),
        ] {
            assert_eq!(
                rotation
                    .issue_in_token_response(status, body.into())
                    .await
                    .unwrap(),
                body
            );
        }
        // A body that is not an object but carries a refresh token is refused.
        assert!(matches!(
            rotation
                .issue_in_token_response(200, "access_token=at&refresh_token=rt".into())
                .await,
            Err(super::super::refresh_handle::ReplyError::Unparseable)
        ));
        let families: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_families")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(families, 1);
    }

    #[sqlx::test]
    async fn rotation_without_upstream_rotation_keeps_the_upstream_token(pool: PgPool) {
        let project = project(&pool, "plain", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;

        let upstream = FakeUpstream::default();
        let reply = rotation
            .refresh(&first, upstream.answer(Ok(ok("at1", None))))
            .await
            .unwrap();
        let (value, second) = token(&reply);
        assert_eq!(value["access_token"], "at1");
        assert_eq!(
            value["scope"], "upstream",
            "the upstream's members are kept"
        );
        assert!(is_rotating_handle(&second));
        assert_ne!(
            second, first,
            "a successor is issued even if upstream did not rotate"
        );
        assert!(!String::from_utf8_lossy(&reply.body).contains("upstream-1"));
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type.as_deref(), Some("application/json"));
        assert_eq!(upstream.calls(), ["upstream-1"]);

        // The successor still wraps the same upstream token.
        let reply = rotation
            .refresh(&second, upstream.answer(Ok(ok("at2", None))))
            .await
            .unwrap();
        assert_ne!(token(&reply).1, second);
        assert_eq!(upstream.calls(), ["upstream-1", "upstream-1"]);
    }

    #[sqlx::test]
    async fn rotation_with_upstream_rotation_wraps_the_new_upstream_token(pool: PgPool) {
        let project = project(&pool, "upstream-rotates", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;

        let upstream = FakeUpstream::default();
        let reply = rotation
            .refresh(&first, upstream.answer(Ok(ok("at1", Some("upstream-2")))))
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&reply.body).contains("upstream-2"));
        let second = token(&reply).1;
        let reply = rotation
            .refresh(&second, upstream.answer(Ok(ok("at2", Some("")))))
            .await
            .unwrap();
        let third = token(&reply).1;
        // An empty upstream token counts as "not rotated".
        rotation
            .refresh(&third, upstream.answer(Ok(ok("at3", None))))
            .await
            .unwrap();
        assert_eq!(upstream.calls(), ["upstream-1", "upstream-2", "upstream-2"]);
    }

    #[sqlx::test]
    async fn the_upstream_answer_is_relayed_with_only_the_refresh_token_replaced(pool: PgPool) {
        let project = project(&pool, "relay", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();
        let body = r#"{"id_token":"idt","access_token":"at","extra":{"b":[1,2.50],"a":null},"refresh_token":"upstream-2","expires_in":60}"#;
        let reply = rotation
            .refresh(
                &first,
                upstream.answer(Ok(UpstreamReply {
                    status: 200,
                    content_type: Some("application/json;charset=UTF-8".into()),
                    body: body.as_bytes().to_vec(),
                })),
            )
            .await
            .unwrap();
        assert_eq!(
            reply.content_type.as_deref(),
            Some("application/json;charset=UTF-8")
        );
        let text = String::from_utf8(reply.body.clone()).unwrap();
        let positions: Vec<usize> = [
            "id_token",
            "access_token",
            "extra",
            "refresh_token",
            "expires_in",
        ]
        .iter()
        .map(|name| text.find(&format!("\"{name}\"")).unwrap())
        .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{text}");
        assert!(
            text.contains(r#""extra":{"b":[1,2.50],"a":null}"#),
            "{text}"
        );

        // A response without a refresh token gets the successor appended.
        let (_, second) = token(&reply);
        let reply = rotation
            .refresh(&second, upstream.answer(Ok(ok("at2", None))))
            .await
            .unwrap();
        let text = String::from_utf8(reply.body).unwrap();
        assert!(text.rfind("refresh_token").unwrap() > text.find("scope").unwrap());
    }

    #[sqlx::test]
    async fn a_duplicate_within_grace_gets_the_same_successor_without_calling_upstream(
        pool: PgPool,
    ) {
        let project = project(&pool, "dup", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();

        let a = rotation
            .refresh(&first, upstream.answer(Ok(ok("at1", None))))
            .await
            .unwrap();
        let b = rotation
            .refresh(&first, upstream.answer(Ok(ok("never", None))))
            .await
            .unwrap();
        assert_eq!(a, b, "the exact response is replayed");
        assert_eq!(upstream.calls().len(), 1);

        // The replayed successor is a working handle, and spent handles stay
        // replayable for the whole grace window.
        let (_, second) = token(&a);
        rotation
            .refresh(&second, upstream.answer(Ok(ok("at2", None))))
            .await
            .unwrap();
        assert_eq!(
            rotation
                .refresh(&first, upstream.answer(Ok(ok("never", None))))
                .await
                .unwrap(),
            a
        );
    }

    #[sqlx::test]
    async fn replays_and_waiting_requests_do_not_run_the_upstream_setup(pool: PgPool) {
        let project = project(&pool, "setup", &["login"]).await;
        let provider = encryption();
        let mut rotation = rotation(&pool, &provider, project, "login");
        rotation.wait = Duration::from_secs(5);
        let first = login(&rotation, "upstream-1").await;
        let setups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slow = |setups: Arc<std::sync::atomic::AtomicUsize>| {
            move |_token: String| {
                setups.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Ok::<_, UpstreamFailure>(ok("at1", None))
                }
            }
        };
        let results = futures::future::join_all(
            (0..3).map(|_| rotation.refresh(&first, slow(setups.clone()))),
        )
        .await;
        assert!(results.iter().all(Result::is_ok));
        // A replay after the winner finished does not set anything up either.
        rotation
            .refresh(&first, slow(setups.clone()))
            .await
            .unwrap();
        assert_eq!(setups.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[sqlx::test]
    async fn concurrent_requests_agree_on_one_successor_and_one_upstream_call(pool: PgPool) {
        let project = project(&pool, "concurrent", &["login"]).await;
        let provider = encryption();
        let mut rotation = rotation(&pool, &provider, project, "login");
        rotation.wait = Duration::from_secs(5);
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream {
            delay: Duration::from_millis(400),
            ..Default::default()
        };

        let attempts = (0..4)
            .map(|n| rotation.refresh(&first, upstream.answer(Ok(ok(&format!("at{n}"), None)))));
        let replies: Vec<TokenReply> = futures::future::join_all(attempts)
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert!(replies.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(upstream.calls().len(), 1);
    }

    #[sqlx::test]
    async fn a_request_that_waits_too_long_for_the_winner_gets_a_retryable_error(pool: PgPool) {
        let project = project(&pool, "slow", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        // Another request holds the claim and never finishes.
        crate::db::oauth_refresh_tokens::claim(
            &pool,
            &hash_handle(&first),
            project,
            "login",
            identity().fingerprint(),
            Uuid::new_v4(),
            crate::db::oauth_refresh_tokens::CLAIM_LEASE,
        )
        .await
        .unwrap()
        .unwrap();

        let upstream = FakeUpstream::default();
        let result = rotation
            .refresh(&first, upstream.answer(Ok(ok("never", None))))
            .await;
        assert!(matches!(result, Err(RotationError::TemporarilyUnavailable)));
        assert!(upstream.calls().is_empty());
    }

    #[sqlx::test]
    async fn reuse_after_the_grace_period_revokes_the_family_and_erases_its_secrets(pool: PgPool) {
        let project = project(&pool, "reuse", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();
        let second = token(
            &rotation
                .refresh(&first, upstream.answer(Ok(ok("at1", None))))
                .await
                .unwrap(),
        )
        .1;
        let third = token(
            &rotation
                .refresh(&second, upstream.answer(Ok(ok("at2", None))))
                .await
                .unwrap(),
        )
        .1;
        assert!(stored_secrets(&pool).await > 0);
        expire_grace(&pool).await;

        // Replaying the oldest handle is reuse.
        assert!(matches!(
            rotation
                .refresh(&first, upstream.answer(Ok(ok("never", None))))
                .await,
            Err(RotationError::InvalidGrant)
        ));
        // It ended the login: the current handle is dead as well, and nothing
        // of the family's secrets is left.
        assert!(matches!(
            rotation
                .refresh(&third, upstream.answer(Ok(ok("never", None))))
                .await,
            Err(RotationError::InvalidGrant)
        ));
        assert_eq!(stored_secrets(&pool).await, 0);
        assert_eq!(upstream.calls().len(), 2, "upstream was not called again");
        let revoked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM oauth_refresh_families WHERE revoked_at IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(revoked, 1);
    }

    #[sqlx::test]
    async fn the_family_lifetime_is_absolute(pool: PgPool) {
        let project = project(&pool, "lifetime", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();
        let second = token(
            &rotation
                .refresh(&first, upstream.answer(Ok(ok("at1", None))))
                .await
                .unwrap(),
        )
        .1;
        // Refreshing does not extend the family: it ends when it was set to.
        let not_extended: bool = sqlx::query_scalar(
            "SELECT expires_at <= NOW() + INTERVAL '3600 seconds' FROM oauth_refresh_families",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(not_extended);

        sqlx::query("UPDATE oauth_refresh_families SET expires_at = NOW() - INTERVAL '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            rotation
                .refresh(&second, upstream.answer(Ok(ok("never", None))))
                .await,
            Err(RotationError::InvalidGrant)
        ));
        assert_eq!(upstream.calls().len(), 1);
    }

    #[sqlx::test]
    async fn failures_that_are_no_verdict_release_the_claim_for_a_retry(pool: PgPool) {
        let project = project(&pool, "failure", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();

        let outage = upstream_json(503, json!({"error": "temporarily_unavailable"}));
        let result = rotation.refresh(&first, upstream.answer(Ok(outage))).await;
        let Err(RotationError::Passthrough(passed)) = result else {
            panic!("an upstream 503 is passed through");
        };
        assert_eq!(passed.status, 503);
        // A 4xx that is not invalid_grant, and invalid_grant with a 5xx status or
        // a non-JSON body, are no verdict either.
        for answer in [
            upstream_json(400, json!({"error": "invalid_client"})),
            upstream_json(500, json!({"error": "invalid_grant"})),
            UpstreamReply {
                status: 400,
                content_type: Some("text/plain".into()),
                body: b"error=invalid_grant".to_vec(),
            },
        ] {
            assert!(matches!(
                rotation.refresh(&first, upstream.answer(Ok(answer))).await,
                Err(RotationError::Passthrough(_))
            ));
        }
        for failure in [
            UpstreamFailure::Unreachable(anyhow!("connection refused")),
            UpstreamFailure::Misconfigured(anyhow!("no endpoint")),
            UpstreamFailure::Unavailable(anyhow!("throttled")),
        ] {
            assert!(rotation
                .refresh(&first, upstream.answer(Err(failure)))
                .await
                .is_err());
        }

        // Same handle, immediately usable again, without waiting for a lease.
        let reply = rotation
            .refresh(&first, upstream.answer(Ok(ok("at1", None))))
            .await
            .unwrap();
        assert_eq!(token(&reply).0["access_token"], "at1");
        assert_eq!(upstream.calls().len(), 8);
    }

    #[sqlx::test]
    async fn a_malformed_2xx_answer_releases_the_claim(pool: PgPool) {
        let project = project(&pool, "malformed", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();
        for body in [
            "access_token=at&refresh_token=leaky&token_type=bearer",
            "ok",
            "[1]",
        ] {
            let result = rotation
                .refresh(
                    &first,
                    upstream.answer(Ok(UpstreamReply {
                        status: 200,
                        content_type: Some("application/x-www-form-urlencoded".into()),
                        body: body.as_bytes().to_vec(),
                    })),
                )
                .await;
            assert!(matches!(result, Err(RotationError::Malformed(_))), "{body}");
        }
        rotation
            .refresh(&first, upstream.answer(Ok(ok("at1", None))))
            .await
            .unwrap();
        assert_eq!(upstream.calls().len(), 4);
    }

    #[sqlx::test]
    async fn an_upstream_call_that_times_out_releases_the_claim(pool: PgPool) {
        let project = project(&pool, "timeout", &["login"]).await;
        let provider = encryption();
        let mut rotation = rotation(&pool, &provider, project, "login");
        rotation.upstream_timeout = Duration::from_millis(100);
        let first = login(&rotation, "upstream-1").await;
        let slow = FakeUpstream {
            delay: Duration::from_millis(500),
            ..Default::default()
        };
        let result = rotation
            .refresh(&first, slow.answer(Ok(ok("late", None))))
            .await;
        assert!(matches!(result, Err(RotationError::Unreachable(_))));
        let fast = FakeUpstream::default();
        rotation
            .refresh(&first, fast.answer(Ok(ok("at1", None))))
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn a_stale_claim_is_taken_over_and_the_late_claimant_cannot_overwrite_the_result(
        pool: PgPool,
    ) {
        let project = project(&pool, "lease", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let slow = FakeUpstream {
            delay: Duration::from_millis(600),
            ..Default::default()
        };
        let fast = FakeUpstream::default();

        let original = rotation.refresh(&first, slow.answer(Ok(ok("original", None))));
        let takeover = async {
            // The original claimant is frozen past its lease.
            tokio::time::sleep(Duration::from_millis(150)).await;
            sqlx::query("UPDATE oauth_refresh_tokens SET claim_expires_at = NOW() - INTERVAL '1 second' WHERE claimed_by IS NOT NULL")
                .execute(&pool)
                .await
                .unwrap();
            rotation
                .refresh(&first, fast.answer(Ok(ok("takeover", None))))
                .await
        };
        let (original, takeover) = tokio::join!(original, takeover);

        let takeover = takeover.unwrap();
        assert_eq!(token(&takeover).0["access_token"], "takeover");
        assert!(matches!(
            original,
            Err(RotationError::TemporarilyUnavailable)
        ));
        // The takeover's response is what the handle replays, and only its
        // successor exists.
        let replay = rotation
            .refresh(&first, fast.answer(Ok(ok("never", None))))
            .await
            .unwrap();
        assert_eq!(replay, takeover);
        assert_eq!(slow.calls().len(), 1);
        assert_eq!(fast.calls().len(), 1);
        let handles: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(handles, 2);
    }

    #[sqlx::test]
    async fn only_an_upstream_4xx_invalid_grant_revokes_the_family_and_erases_it(pool: PgPool) {
        let project = project(&pool, "dead", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();
        let second = token(
            &rotation
                .refresh(&first, upstream.answer(Ok(ok("at1", None))))
                .await
                .unwrap(),
        )
        .1;

        let dead = upstream_json(400, json!({"error": "invalid_grant"}));
        assert!(matches!(
            rotation.refresh(&second, upstream.answer(Ok(dead))).await,
            Err(RotationError::InvalidGrant)
        ));
        assert_eq!(stored_secrets(&pool).await, 0);
        for handle in [&first, &second] {
            assert!(matches!(
                rotation
                    .refresh(handle, upstream.answer(Ok(ok("never", None))))
                    .await,
                Err(RotationError::InvalidGrant)
            ));
        }
        assert_eq!(upstream.calls().len(), 2);
    }

    #[sqlx::test]
    async fn a_login_revoked_while_the_refresh_is_in_flight_answers_invalid_grant(pool: PgPool) {
        let project = project(&pool, "inflight", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let family: Uuid = sqlx::query_scalar("SELECT family_id FROM oauth_refresh_families")
            .fetch_one(&pool)
            .await
            .unwrap();
        let slow = FakeUpstream {
            delay: Duration::from_millis(300),
            ..Default::default()
        };
        let refresh = rotation.refresh(&first, slow.answer(Ok(ok("at1", None))));
        let revoke = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            crate::db::oauth_refresh_tokens::revoke_family(&pool, family)
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(refresh, revoke);
        assert!(matches!(result, Err(RotationError::InvalidGrant)));
        assert_eq!(
            stored_secrets(&pool).await,
            0,
            "no successor survives the revocation"
        );
    }

    #[sqlx::test]
    async fn handles_do_not_cross_extensions_or_projects_and_foreign_use_revokes_nothing(
        pool: PgPool,
    ) {
        let project_a = project(&pool, "scope-a", &["login", "other"]).await;
        let project_b = project(&pool, "scope-b", &["login"]).await;
        let provider = encryption();
        let rotation_a = rotation(&pool, &provider, project_a, "login");
        let first = login(&rotation_a, "upstream-1").await;
        let upstream = FakeUpstream::default();

        for foreign in [
            rotation(&pool, &provider, project_a, "other"),
            rotation(&pool, &provider, project_b, "login"),
            rotation(&pool, &provider, project_a, "missing"),
        ] {
            assert!(matches!(
                foreign
                    .refresh(&first, upstream.answer(Ok(ok("never", None))))
                    .await,
                Err(RotationError::InvalidGrant)
            ));
        }
        assert!(upstream.calls().is_empty());

        // The owner can still use it: a foreign presenter cannot end the login.
        rotation_a
            .refresh(&first, upstream.answer(Ok(ok("at1", None))))
            .await
            .unwrap();
        assert_eq!(upstream.calls(), ["upstream-1"]);
    }

    #[sqlx::test]
    async fn unknown_and_malformed_handles_are_invalid_grant(pool: PgPool) {
        let project = project(&pool, "unknown", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let upstream = FakeUpstream::default();
        for handle in [new_handle().as_str(), "rrt2.", "rrt2.!!!", "rrt2.AAAA"] {
            assert!(matches!(
                rotation
                    .refresh(handle, upstream.answer(Ok(ok("never", None))))
                    .await,
                Err(RotationError::InvalidGrant)
            ));
        }
        assert!(upstream.calls().is_empty());
    }

    async fn sealed(
        provider: &LocalEncryptionProvider,
        project: Uuid,
        issued_at: i64,
    ) -> UnsealedRefreshToken {
        let handle = refresh_handle::seal(
            provider,
            &handle_scope(project),
            "upstream-sealed",
            issued_at,
        )
        .await
        .unwrap();
        refresh_handle::unseal(provider, &handle_scope(project), &handle)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn an_rrt1_handle_starts_a_new_family(pool: PgPool) {
        let project = project(&pool, "migrate", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let upstream = FakeUpstream::default();
        let now = 1_000_000;

        let reply = rotation
            .refresh_sealed(
                &sealed(&provider, project, now - 10).await,
                now,
                upstream.answer(Ok(ok("at1", None))),
            )
            .await
            .unwrap();
        let (value, handle) = token(&reply);
        assert_eq!(value["access_token"], "at1");
        assert!(is_rotating_handle(&handle));
        assert!(!String::from_utf8_lossy(&reply.body).contains("upstream-sealed"));
        assert_eq!(upstream.calls(), ["upstream-sealed"]);

        // The new handle is a normal member of a new family wrapping the same upstream token.
        let reply = rotation
            .refresh(
                &handle,
                upstream.answer(Ok(ok("at2", Some("upstream-new")))),
            )
            .await
            .unwrap();
        assert_ne!(token(&reply).1, handle);
        assert_eq!(upstream.calls(), ["upstream-sealed", "upstream-sealed"]);

        // The upstream's rotated token is what the family wraps.
        let reply = rotation
            .refresh_sealed(
                &sealed(&provider, project, now).await,
                now,
                upstream.answer(Ok(ok("at3", Some("rotated")))),
            )
            .await
            .unwrap();
        rotation
            .refresh(&token(&reply).1, upstream.answer(Ok(ok("at4", None))))
            .await
            .unwrap();
        assert_eq!(upstream.calls()[2..], ["upstream-sealed", "rotated"]);

        let families: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_families")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(families, 2);
    }

    #[sqlx::test]
    async fn an_rrt1_handle_past_the_max_lifetime_is_refused_before_any_upstream_call(
        pool: PgPool,
    ) {
        let project = project(&pool, "old-handle", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let upstream = FakeUpstream::default();
        let now = 1_000_000;
        // max_lifetime_seconds is 3600 in these tests.
        for issued_at in [now - 3600, now - 100_000] {
            assert!(matches!(
                rotation
                    .refresh_sealed(
                        &sealed(&provider, project, issued_at).await,
                        now,
                        upstream.answer(Ok(ok("never", None))),
                    )
                    .await,
                Err(RotationError::InvalidGrant)
            ));
        }
        assert!(rotation
            .refresh_sealed(
                &sealed(&provider, project, now - 3599).await,
                now,
                upstream.answer(Ok(ok("at", None))),
            )
            .await
            .is_ok());
        assert_eq!(upstream.calls().len(), 1);
    }

    #[sqlx::test]
    async fn failed_migration_refreshes_create_nothing(pool: PgPool) {
        let project = project(&pool, "migrate-fail", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let upstream = FakeUpstream::default();
        let now = 5;
        let handle = sealed(&provider, project, now).await;
        for answer in [
            Ok(upstream_json(400, json!({"error": "invalid_grant"}))),
            Ok(upstream_json(503, json!({}))),
            Ok(UpstreamReply {
                status: 200,
                content_type: None,
                body: b"refresh_token=x".to_vec(),
            }),
            Err(UpstreamFailure::Unreachable(anyhow!("down"))),
        ] {
            assert!(rotation
                .refresh_sealed(&handle, now, upstream.answer(answer))
                .await
                .is_err());
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_families")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[sqlx::test]
    async fn the_sealed_mode_does_not_accept_rrt2_handles(pool: PgPool) {
        // `present` is what a sealed-mode extension runs on every presented token.
        let project = project(&pool, "sealed", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let handle = login(&rotation, "upstream-1").await;
        assert!(refresh_handle::is_handle(&handle));
        assert!(
            refresh_handle::present(&provider, &handle_scope(project), &handle)
                .await
                .is_err()
        );
    }

    struct Throttled;

    #[async_trait::async_trait]
    impl EncryptionProvider for Throttled {
        async fn encrypt(&self, _: &str, _: &EncryptionContext) -> Result<String> {
            Err(anyhow!("ThrottlingException"))
        }
        async fn decrypt(&self, _: &str, _: &EncryptionContext) -> Result<String> {
            Err(anyhow!("ThrottlingException"))
        }
    }

    #[sqlx::test]
    async fn transient_encryption_failures_are_retryable_and_release_the_claim(pool: PgPool) {
        let project = project(&pool, "throttled", &["login"]).await;
        let provider = encryption();
        let healthy = rotation(&pool, &provider, project, "login");
        let first = login(&healthy, "upstream-1").await;
        let throttled = Throttled;
        let degraded = Rotation::new(&pool, &throttled, project, "login", settings(), identity());
        let upstream = FakeUpstream::default();
        assert!(matches!(
            degraded
                .refresh(&first, upstream.answer(Ok(ok("never", None))))
                .await,
            Err(RotationError::Internal(_))
        ));
        assert!(upstream.calls().is_empty());
        // The claim was released: the handle is usable once the provider recovers.
        healthy
            .refresh(&first, upstream.answer(Ok(ok("at1", None))))
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn stored_values_are_bound_to_their_handle_and_tampered_ones_are_invalid_grant(
        pool: PgPool,
    ) {
        // Moving a ciphertext to another row must not let it decrypt there.
        let project = project(&pool, "bound", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let a = login(&rotation, "upstream-a").await;
        let b = login(&rotation, "upstream-b").await;
        sqlx::query("UPDATE oauth_refresh_tokens SET upstream_token_encrypted = (SELECT upstream_token_encrypted FROM oauth_refresh_tokens WHERE handle_hash = $1) WHERE handle_hash = $2")
            .bind(hash_handle(&a).to_vec())
            .bind(hash_handle(&b).to_vec())
            .execute(&pool)
            .await
            .unwrap();
        let upstream = FakeUpstream::default();
        assert!(matches!(
            rotation
                .refresh(&b, upstream.answer(Ok(ok("never", None))))
                .await,
            Err(RotationError::InvalidGrant)
        ));
        assert!(upstream.calls().is_empty());
        // The failed attempt did not leave the handle claimed.
        assert!(matches!(
            crate::db::oauth_refresh_tokens::state(
                &pool,
                &hash_handle(&b),
                project,
                "login",
                identity().fingerprint(),
            )
            .await
            .unwrap(),
            HandleState::Available
        ));
    }
}
