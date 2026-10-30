//! Sealed refresh-token handles.
//!
//! The OAuth extension relays an upstream provider's token response to the
//! deployed app, but the upstream `refresh_token` never leaves Rise. The app
//! receives an opaque *handle* instead (RFC 6749 §6 lets the token endpoint
//! return its own refresh-token value), and only the Rise token endpoint can
//! turn it back into the upstream token.
//!
//! A handle is `rrt1.` followed by the base64url ciphertext of the configured
//! [`EncryptionProvider`], sealed under
//! `EncryptionContext::extension(project, "oauth", extension, "/tokens/refresh")`.
//! The context is authenticated by the provider, so a handle only unseals for
//! the project and extension that issued it. The ciphertext carries a small
//! versioned JSON payload (`v`, `upstream`, `iat`).
//!
//! The leading `rrt<N>.` is the handle *scheme*. [`unseal`] dispatches on it,
//! which keeps room for further schemes (for example server-side-state
//! handles) next to this one; a token that carries no known scheme is not a
//! handle.

use crate::server::encryption::{EncryptionContext, EncryptionProvider};
use anyhow::{bail, Context, Result};
use rise_backend_core::encryption::ENVELOPE_PREFIX;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Scheme prefix of the stateless sealed handle.
pub const SEALED_HANDLE_PREFIX: &str = "rrt1.";

/// Encryption slot a refresh handle is bound to.
const SLOT: &str = "/tokens/refresh";

const PAYLOAD_VERSION: u8 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    v: u8,
    upstream: String,
    /// Unix seconds at which Rise issued the handle.
    iat: i64,
}

/// What a handle stands for once unsealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsealedRefreshToken {
    /// The refresh token to present to the upstream provider.
    pub upstream_refresh_token: String,
    /// Unix seconds at which Rise issued the handle.
    pub issued_at: i64,
}

/// A refresh token presented to the token endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentedRefreshToken {
    /// A Rise-issued handle.
    Handle(UnsealedRefreshToken),
    /// A raw upstream refresh token, issued before handles existed.
    // Transitional: removed once tokens issued before handles have aged out.
    Legacy(String),
}

impl PresentedRefreshToken {
    /// The token to present to the upstream provider.
    pub fn upstream_token(&self) -> &str {
        match self {
            Self::Handle(handle) => &handle.upstream_refresh_token,
            Self::Legacy(token) => token,
        }
    }
}

/// Whether `token` carries a Rise handle scheme (it may still fail to unseal).
pub fn is_handle(token: &str) -> bool {
    token.starts_with(SEALED_HANDLE_PREFIX)
}

fn context(project: Uuid, extension: &str) -> EncryptionContext {
    EncryptionContext::extension(project, "oauth", extension, SLOT)
}

/// Seal `upstream_refresh_token` into a handle for `project`/`extension`.
pub async fn seal(
    provider: &dyn EncryptionProvider,
    project: Uuid,
    extension: &str,
    upstream_refresh_token: &str,
    issued_at: i64,
) -> Result<String> {
    let payload = serde_json::to_string(&Payload {
        v: PAYLOAD_VERSION,
        upstream: upstream_refresh_token.to_string(),
        iat: issued_at,
    })
    .context("Failed to serialize refresh handle payload")?;
    let envelope = provider
        .encrypt(&payload, &context(project, extension))
        .await?;
    // The envelope is `rise:enc:v2:<base64url>`; the handle reuses its
    // base64url body under its own scheme prefix.
    let body = envelope
        .strip_prefix(ENVELOPE_PREFIX)
        .context("Encryption provider returned an unexpected envelope format")?;
    Ok(format!("{SEALED_HANDLE_PREFIX}{body}"))
}

/// Unseal a handle issued for `project`/`extension`.
///
/// Fails for anything that is not a handle, was tampered with, or was issued
/// for another project or extension.
pub async fn unseal(
    provider: &dyn EncryptionProvider,
    project: Uuid,
    extension: &str,
    handle: &str,
) -> Result<UnsealedRefreshToken> {
    let Some(body) = handle.strip_prefix(SEALED_HANDLE_PREFIX) else {
        bail!("Not a Rise refresh token handle");
    };
    let plaintext = provider
        .decrypt(
            &format!("{ENVELOPE_PREFIX}{body}"),
            &context(project, extension),
        )
        .await
        .context("Refresh token handle did not unseal")?;
    let payload: Payload =
        serde_json::from_str(&plaintext).context("Invalid refresh handle payload")?;
    if payload.v != PAYLOAD_VERSION {
        bail!("Unsupported refresh handle payload version {}", payload.v);
    }
    Ok(UnsealedRefreshToken {
        upstream_refresh_token: payload.upstream,
        issued_at: payload.iat,
    })
}

/// Interpret the `refresh_token` a client sent to the token endpoint.
///
/// Anything carrying the handle scheme must unseal, otherwise it is rejected
/// (it is never forwarded upstream). Anything else is treated as a legacy raw
/// upstream token.
pub async fn present(
    provider: &dyn EncryptionProvider,
    project: Uuid,
    extension: &str,
    token: &str,
) -> Result<PresentedRefreshToken> {
    if is_handle(token) {
        unseal(provider, project, extension, token)
            .await
            .map(PresentedRefreshToken::Handle)
    } else {
        Ok(PresentedRefreshToken::Legacy(token.to_string()))
    }
}

/// The handle to return for the refresh token an upstream refresh response
/// carried, if any.
///
/// `None` (no new upstream token) stays `None`: the client keeps the handle it
/// presented, which still wraps a valid upstream token.
pub async fn seal_rotated(
    provider: &dyn EncryptionProvider,
    project: Uuid,
    extension: &str,
    upstream_refresh_token: Option<&str>,
    issued_at: i64,
) -> Result<Option<String>> {
    match upstream_refresh_token {
        Some(token) if !token.is_empty() => seal(provider, project, extension, token, issued_at)
            .await
            .map(Some),
        _ => Ok(None),
    }
}

/// Replace the upstream `refresh_token` in a cached token-endpoint response
/// body with a sealed handle.
///
/// Only a 2xx response whose body is a JSON object with a non-empty string
/// `refresh_token` is rewritten; every other body is returned byte-for-byte.
/// The remaining members are kept, but re-serialized (member order and
/// whitespace are not preserved).
pub async fn seal_in_token_response(
    provider: &dyn EncryptionProvider,
    project: Uuid,
    extension: &str,
    status: u16,
    body: String,
    issued_at: i64,
) -> Result<String> {
    if !(200..300).contains(&status) {
        return Ok(body);
    }
    let Ok(serde_json::Value::Object(mut members)) = serde_json::from_str(&body) else {
        return Ok(body);
    };
    let upstream = match members.get("refresh_token") {
        Some(serde_json::Value::String(token)) if !token.is_empty() => token.clone(),
        _ => return Ok(body),
    };
    let handle = seal(provider, project, extension, &upstream, issued_at).await?;
    members.insert("refresh_token".into(), serde_json::Value::String(handle));
    serde_json::to_string(&members).context("Failed to serialize token response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::encryption::providers::local::LocalEncryptionProvider;
    use crate::server::extensions::providers::oauth::models::OAuth2TokenResponse;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde_json::{json, Value};

    fn provider() -> LocalEncryptionProvider {
        LocalEncryptionProvider::new(&STANDARD.encode([7u8; 32])).unwrap()
    }

    async fn seal_body(status: u16, body: &str) -> (Uuid, String) {
        let project = Uuid::new_v4();
        let out = seal_in_token_response(&provider(), project, "login", status, body.into(), 42)
            .await
            .unwrap();
        (project, out)
    }

    #[tokio::test]
    async fn handle_round_trips_with_issued_at() {
        let project = Uuid::new_v4();
        let handle = seal(&provider(), project, "login", "upstream-rt", 1_700_000_000)
            .await
            .unwrap();
        assert!(handle.starts_with("rrt1."));
        assert!(!handle.contains("upstream-rt"));
        assert!(!handle.contains(ENVELOPE_PREFIX));
        let unsealed = unseal(&provider(), project, "login", &handle)
            .await
            .unwrap();
        assert_eq!(unsealed.upstream_refresh_token, "upstream-rt");
        assert_eq!(unsealed.issued_at, 1_700_000_000);
    }

    #[tokio::test]
    async fn handles_are_unique_per_issue() {
        let project = Uuid::new_v4();
        let a = seal(&provider(), project, "login", "rt", 1).await.unwrap();
        let b = seal(&provider(), project, "login", "rt", 1).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn handle_is_bound_to_project_and_extension() {
        let project = Uuid::new_v4();
        let handle = seal(&provider(), project, "login", "rt", 1).await.unwrap();
        assert!(unseal(&provider(), project, "other", &handle)
            .await
            .is_err());
        assert!(unseal(&provider(), Uuid::new_v4(), "login", &handle)
            .await
            .is_err());
        assert!(present(&provider(), project, "other", &handle)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn tampered_or_foreign_handles_do_not_unseal() {
        let project = Uuid::new_v4();
        let handle = seal(&provider(), project, "login", "rt", 1).await.unwrap();
        let mut tampered = handle.clone();
        tampered.pop();
        tampered.push(if handle.ends_with('A') { 'B' } else { 'A' });
        assert!(unseal(&provider(), project, "login", &tampered)
            .await
            .is_err());
        assert!(unseal(&provider(), project, "login", "rrt1.%%%")
            .await
            .is_err());
        assert!(unseal(&provider(), project, "login", "rrt2.abcd")
            .await
            .is_err());
        // A ciphertext sealed for another slot is not a refresh handle.
        let other_slot = provider()
            .encrypt(
                r#"{"v":1,"upstream":"rt","iat":1}"#,
                &EncryptionContext::extension(project, "oauth", "login", "/tokens/other"),
            )
            .await
            .unwrap();
        let forged = format!("rrt1.{}", other_slot.strip_prefix(ENVELOPE_PREFIX).unwrap());
        assert!(unseal(&provider(), project, "login", &forged)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn legacy_raw_token_is_forwarded_unchanged() {
        let project = Uuid::new_v4();
        let presented = present(&provider(), project, "login", "raw-upstream-rt")
            .await
            .unwrap();
        assert_eq!(
            presented,
            PresentedRefreshToken::Legacy("raw-upstream-rt".into())
        );
        assert_eq!(presented.upstream_token(), "raw-upstream-rt");
    }

    #[tokio::test]
    async fn presented_handle_yields_upstream_token() {
        let project = Uuid::new_v4();
        let handle = seal(&provider(), project, "login", "rt", 5).await.unwrap();
        let presented = present(&provider(), project, "login", &handle)
            .await
            .unwrap();
        assert_eq!(presented.upstream_token(), "rt");
        assert!(matches!(presented, PresentedRefreshToken::Handle(_)));
    }

    #[tokio::test]
    async fn refresh_response_gets_a_new_handle_only_when_upstream_rotated() {
        let project = Uuid::new_v4();
        let rotated = seal_rotated(&provider(), project, "login", Some("new-rt"), 9)
            .await
            .unwrap()
            .unwrap();
        let unsealed = unseal(&provider(), project, "login", &rotated)
            .await
            .unwrap();
        assert_eq!(unsealed.upstream_refresh_token, "new-rt");
        assert_eq!(unsealed.issued_at, 9);

        for none in [None, Some("")] {
            assert_eq!(
                seal_rotated(&provider(), project, "login", none, 9)
                    .await
                    .unwrap(),
                None
            );
        }

        // Without a refresh token the serialized response has no member for it.
        let body = serde_json::to_value(OAuth2TokenResponse {
            access_token: "at".into(),
            token_type: "Bearer".into(),
            expires_in: Some(60),
            refresh_token: None,
            scope: None,
            id_token: None,
        })
        .unwrap();
        assert!(body.get("refresh_token").is_none());
    }

    #[tokio::test]
    async fn code_grant_response_gets_a_handle_and_keeps_other_members() {
        let (project, out) = seal_body(
            200,
            r#"{"access_token":"at","token_type":"Bearer","expires_in":3600,"refresh_token":"upstream-rt","scope":"a b","id_token":"idt","extra":{"n":[1,2.5,null]}}"#,
        )
        .await;
        let value: Value = serde_json::from_str(&out).unwrap();
        let handle = value["refresh_token"].as_str().unwrap();
        assert!(is_handle(handle));
        assert!(!out.contains("upstream-rt"));
        let unsealed = unseal(&provider(), project, "login", handle).await.unwrap();
        assert_eq!(unsealed.upstream_refresh_token, "upstream-rt");
        assert_eq!(unsealed.issued_at, 42);
        let mut expected = json!({
            "access_token": "at", "token_type": "Bearer", "expires_in": 3600,
            "scope": "a b", "id_token": "idt", "extra": {"n": [1, 2.5, null]}
        });
        expected["refresh_token"] = Value::String(handle.into());
        assert_eq!(value, expected);
    }

    #[tokio::test]
    async fn responses_without_a_usable_refresh_token_pass_through_untouched() {
        for (status, body) in [
            (200, r#"{ "access_token":"at",  "token_type":"Bearer" }"#),
            (200, r#"{"access_token":"at","refresh_token":null}"#),
            (200, r#"{"access_token":"at","refresh_token":""}"#),
            (200, r#"{"access_token":"at","refresh_token":42}"#),
            (200, r#"["refresh_token"]"#),
            (200, "access_token=at&refresh_token=rt&token_type=bearer"),
            (200, "not json at all"),
            (200, ""),
            (400, r#"{"error":"invalid_grant","refresh_token":"rt"}"#),
            (500, r#"{"refresh_token":"rt"}"#),
        ] {
            let (_, out) = seal_body(status, body).await;
            assert_eq!(out, body, "status {status}: {body}");
        }
    }
}
