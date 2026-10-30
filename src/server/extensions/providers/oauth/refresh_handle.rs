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
//! The leading `rrt<N>.` is the handle *scheme*. This module only unseals
//! `rrt1.`; any other scheme is recognised by [`is_handle`] and rejected, so a
//! handle of a scheme this extension does not accept is never mistaken for a
//! raw upstream token and forwarded. A token that carries no scheme is not a
//! handle.
//!
//! Responses are the upstream's own with only the `refresh_token` replaced:
//! status, content type, the other members and their order are kept.

use super::provider::UpstreamReply;
use crate::server::encryption::{EncryptionContext, EncryptionProvider};
use anyhow::{anyhow, Context, Result};
use rise_backend_core::encryption::{is_ciphertext_rejection, ENVELOPE_PREFIX};
use serde::{
    de::{MapAccess, Visitor},
    ser::SerializeMap,
    Deserialize, Deserializer, Serialize, Serializer,
};
use serde_json::value::RawValue;
use uuid::Uuid;

/// Scheme prefix of the stateless sealed handle.
pub const SEALED_HANDLE_PREFIX: &str = "rrt1.";

const REFRESH_TOKEN_MEMBER: &str = "refresh_token";

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

/// Whether `token` carries a Rise handle scheme, `rrt<N>.` (it may still fail
/// to unseal, or belong to a scheme this module does not accept).
pub fn is_handle(token: &str) -> bool {
    token
        .strip_prefix("rrt")
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(scheme, _)| !scheme.is_empty() && scheme.bytes().all(|b| b.is_ascii_digit()))
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

/// Why a presented refresh token cannot be used.
#[derive(Debug)]
pub enum HandleError {
    /// Not a handle this extension accepts: another scheme, malformed,
    /// tampered with, or issued for another project or extension.
    Rejected(anyhow::Error),
    /// The encryption provider could not answer (throttling, network, ...);
    /// says nothing about the handle.
    Unavailable(anyhow::Error),
}

impl std::fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(e) => write!(f, "refresh token handle rejected: {e:#}"),
            Self::Unavailable(e) => write!(f, "refresh token handle could not be checked: {e:#}"),
        }
    }
}

impl std::error::Error for HandleError {}

/// Classify a decryption failure of a stored or presented value.
pub fn classify_decrypt_error(error: anyhow::Error) -> HandleError {
    if is_ciphertext_rejection(&error) {
        HandleError::Rejected(error)
    } else {
        HandleError::Unavailable(error)
    }
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
) -> Result<UnsealedRefreshToken, HandleError> {
    let Some(body) = handle.strip_prefix(SEALED_HANDLE_PREFIX) else {
        return Err(HandleError::Rejected(anyhow!(
            "Not a Rise refresh token handle"
        )));
    };
    let plaintext = provider
        .decrypt(
            &format!("{ENVELOPE_PREFIX}{body}"),
            &context(project, extension),
        )
        .await
        .map_err(classify_decrypt_error)?;
    let payload: Payload = serde_json::from_str(&plaintext)
        .context("Invalid refresh handle payload")
        .map_err(HandleError::Rejected)?;
    if payload.v != PAYLOAD_VERSION {
        return Err(HandleError::Rejected(anyhow!(
            "Unsupported refresh handle payload version {}",
            payload.v
        )));
    }
    Ok(UnsealedRefreshToken {
        upstream_refresh_token: payload.upstream,
        issued_at: payload.iat,
    })
}

/// Interpret the `refresh_token` a client sent to the token endpoint.
///
/// Anything carrying a handle scheme must unseal, otherwise it is rejected
/// (it is never forwarded upstream). Anything else is treated as a legacy raw
/// upstream token.
pub async fn present(
    provider: &dyn EncryptionProvider,
    project: Uuid,
    extension: &str,
    token: &str,
) -> Result<PresentedRefreshToken, HandleError> {
    if is_handle(token) {
        unseal(provider, project, extension, token)
            .await
            .map(PresentedRefreshToken::Handle)
    } else {
        Ok(PresentedRefreshToken::Legacy(token.to_string()))
    }
}

/// A token-endpoint response as returned to the client: the upstream's status,
/// content type and body, with only the `refresh_token` replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenReply {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// The members of a JSON object in document order, values kept verbatim.
///
/// Keeping the raw values (rather than a `Map`) preserves member order without
/// enabling `serde_json`'s `preserve_order` for the whole workspace; only
/// inter-member whitespace is normalized when the object is written back.
#[derive(Debug)]
pub(super) struct Members(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Members {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MembersVisitor;
        impl<'de> Visitor<'de> for MembersVisitor {
            type Value = Members;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Members, A::Error> {
                let mut members = Vec::new();
                while let Some(entry) = map.next_entry::<String, Box<RawValue>>()? {
                    members.push(entry);
                }
                Ok(Members(members))
            }
        }
        deserializer.deserialize_map(MembersVisitor)
    }
}

impl Serialize for Members {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl Members {
    /// The non-empty string `refresh_token` member, if there is one.
    fn refresh_token(&self) -> Option<String> {
        self.0
            .iter()
            .filter(|(name, _)| name == REFRESH_TOKEN_MEMBER)
            .find_map(|(_, value)| {
                serde_json::from_str::<String>(value.get())
                    .ok()
                    .filter(|token| !token.is_empty())
            })
    }

    /// Set `refresh_token` to `handle`: every existing member of that name is
    /// replaced in place, and one is appended if there was none.
    fn set_refresh_token(&mut self, handle: &str) -> Result<()> {
        let value = RawValue::from_string(serde_json::to_string(handle)?)?;
        let mut replaced = false;
        for (name, existing) in &mut self.0 {
            if name == REFRESH_TOKEN_MEMBER {
                *existing = value.clone();
                replaced = true;
            }
        }
        if !replaced {
            self.0.push((REFRESH_TOKEN_MEMBER.into(), value));
        }
        Ok(())
    }

    fn to_body(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("Failed to serialize token response")
    }
}

/// What a 2xx token response body is, as far as refresh tokens go.
#[derive(Debug)]
pub(super) enum ResponseBody {
    /// A JSON object (whatever the content type says).
    Object(Members),
    /// Not a JSON object, and no sign of a refresh token: relayed untouched.
    Opaque,
    /// Not a JSON object, but it appears to carry a refresh token (for example
    /// a form-encoded `refresh_token=` pair). It cannot be rewritten, so it
    /// must not be returned at all.
    Unparseable,
}

pub(super) fn parse_response_body(body: &[u8]) -> ResponseBody {
    if let Ok(members) = serde_json::from_slice::<Members>(body) {
        return ResponseBody::Object(members);
    }
    if String::from_utf8_lossy(body).contains(REFRESH_TOKEN_MEMBER) {
        ResponseBody::Unparseable
    } else {
        ResponseBody::Opaque
    }
}

/// Failure to turn an upstream 2xx response into the client's response.
#[derive(Debug)]
pub enum ReplyError {
    /// The upstream body carries a refresh token that cannot be replaced.
    /// The response must not be returned.
    Unparseable,
    /// Sealing failed (encryption provider error).
    Seal(HandleError),
}

/// Whether a 2xx status.
fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Replace the upstream `refresh_token` in a cached code-grant response by a
/// sealed handle.
///
/// Only a 2xx response is touched. A JSON object body with a non-empty string
/// `refresh_token` has it replaced in place (member order and the raw values
/// of the other members are kept); a body that is not a JSON object but looks
/// like it carries a refresh token is refused; every other body is returned
/// byte-for-byte.
pub async fn seal_in_token_response(
    provider: &dyn EncryptionProvider,
    project: Uuid,
    extension: &str,
    status: u16,
    body: String,
    issued_at: i64,
) -> Result<String, ReplyError> {
    if !is_success(status) {
        return Ok(body);
    }
    match parse_response_body(body.as_bytes()) {
        ResponseBody::Opaque => Ok(body),
        ResponseBody::Unparseable => Err(ReplyError::Unparseable),
        ResponseBody::Object(mut members) => {
            let Some(upstream) = members.refresh_token() else {
                return Ok(body);
            };
            let handle = seal(provider, project, extension, &upstream, issued_at)
                .await
                .map_err(|e| ReplyError::Seal(HandleError::Unavailable(e)))?;
            members
                .set_refresh_token(&handle)
                .and_then(|()| members.to_body())
                .map_err(|e| ReplyError::Seal(HandleError::Unavailable(e)))
                .map(|body| String::from_utf8(body).expect("serde_json writes UTF-8"))
        }
    }
}

/// Turn the upstream's 2xx answer to a refresh into the client's response in
/// `sealed` mode.
///
/// The upstream status, content type and body are passed on; only the
/// `refresh_token` changes:
/// - a refresh token the upstream returned is replaced by a handle for it;
/// - a legacy raw token the client presented, when the upstream returned none,
///   is sealed into a handle and added, so the client moves to handles on its
///   first refresh;
/// - otherwise the response carries none, and the client keeps the handle it
///   presented, which still wraps a valid upstream token.
pub async fn seal_refresh_response(
    provider: &dyn EncryptionProvider,
    project: Uuid,
    extension: &str,
    presented: &PresentedRefreshToken,
    upstream: UpstreamReply,
    issued_at: i64,
) -> Result<TokenReply, ReplyError> {
    let UpstreamReply {
        status,
        content_type,
        body,
    } = upstream;
    let unavailable = |e: anyhow::Error| ReplyError::Seal(HandleError::Unavailable(e));
    let body = match parse_response_body(&body) {
        ResponseBody::Opaque => body,
        ResponseBody::Unparseable => return Err(ReplyError::Unparseable),
        ResponseBody::Object(mut members) => {
            let to_seal = match (members.refresh_token(), presented) {
                (Some(rotated), _) => Some(rotated),
                (None, PresentedRefreshToken::Legacy(token)) => Some(token.clone()),
                (None, PresentedRefreshToken::Handle(_)) => None,
            };
            match to_seal {
                None => body,
                Some(token) => {
                    let handle = seal(provider, project, extension, &token, issued_at)
                        .await
                        .map_err(unavailable)?;
                    members.set_refresh_token(&handle).map_err(unavailable)?;
                    members.to_body().map_err(unavailable)?
                }
            }
        }
    };
    Ok(TokenReply {
        status,
        content_type,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::encryption::providers::local::LocalEncryptionProvider;
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

    #[test]
    fn any_handle_scheme_is_recognised_and_raw_tokens_are_not() {
        for handle in ["rrt1.abc", "rrt2.abc", "rrt10.x", "rrt9."] {
            assert!(is_handle(handle), "{handle}");
        }
        for raw in [
            "",
            "rrt",
            "rrt.abc",
            "rrta.abc",
            "rrt1abc",
            "xrrt1.abc",
            "ghr_abc.def",
        ] {
            assert!(!is_handle(raw), "{raw}");
        }
    }

    #[tokio::test]
    async fn a_handle_of_another_scheme_is_rejected_not_forwarded() {
        let project = Uuid::new_v4();
        assert!(matches!(
            present(&provider(), project, "login", "rrt2.abcd").await,
            Err(HandleError::Rejected(_))
        ));
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

    async fn refresh(
        presented: &PresentedRefreshToken,
        status: u16,
        body: &str,
    ) -> Result<TokenReply, ReplyError> {
        seal_refresh_response(
            &provider(),
            PROJECT,
            "login",
            presented,
            UpstreamReply {
                status,
                content_type: Some("application/json".into()),
                body: body.as_bytes().to_vec(),
            },
            9,
        )
        .await
    }

    const PROJECT: Uuid = Uuid::from_u128(1);

    async fn presented_handle(upstream: &str) -> PresentedRefreshToken {
        let handle = seal(&provider(), PROJECT, "login", upstream, 1)
            .await
            .unwrap();
        present(&provider(), PROJECT, "login", &handle)
            .await
            .unwrap()
    }

    fn reply_json(reply: &TokenReply) -> Value {
        serde_json::from_slice(&reply.body).unwrap()
    }

    #[tokio::test]
    async fn refresh_response_gets_a_new_handle_only_when_upstream_rotated() {
        let handle = presented_handle("old-rt").await;
        let reply = refresh(
            &handle,
            200,
            r#"{"access_token":"at","refresh_token":"new-rt"}"#,
        )
        .await
        .unwrap();
        let new_handle = reply_json(&reply)["refresh_token"]
            .as_str()
            .unwrap()
            .to_string();
        let unsealed = unseal(&provider(), PROJECT, "login", &new_handle)
            .await
            .unwrap();
        assert_eq!(unsealed.upstream_refresh_token, "new-rt");
        assert_eq!(unsealed.issued_at, 9);

        // Not rotated: the client keeps the handle it has, so the body is untouched.
        for body in [
            r#"{ "access_token":"at" }"#,
            r#"{"access_token":"at","refresh_token":null}"#,
            r#"{"access_token":"at","refresh_token":""}"#,
        ] {
            let reply = refresh(&handle, 200, body).await.unwrap();
            assert_eq!(reply.body, body.as_bytes());
        }
    }

    #[tokio::test]
    async fn a_legacy_token_is_sealed_into_the_response_even_when_upstream_did_not_rotate() {
        let legacy = PresentedRefreshToken::Legacy("raw-rt".into());
        for body in [
            r#"{"access_token":"at","scope":"a"}"#,
            r#"{"access_token":"at","refresh_token":"","scope":"a"}"#,
        ] {
            let reply = refresh(&legacy, 200, body).await.unwrap();
            let value = reply_json(&reply);
            let handle = value["refresh_token"].as_str().unwrap();
            assert!(handle.starts_with("rrt1."));
            assert!(!String::from_utf8_lossy(&reply.body).contains("raw-rt"));
            assert_eq!(value["scope"], "a");
            let unsealed = unseal(&provider(), PROJECT, "login", handle).await.unwrap();
            assert_eq!(unsealed.upstream_refresh_token, "raw-rt");
        }
        // A token the upstream rotated wins over the legacy one.
        let reply = refresh(
            &legacy,
            200,
            r#"{"access_token":"at","refresh_token":"rotated"}"#,
        )
        .await
        .unwrap();
        let handle = reply_json(&reply)["refresh_token"]
            .as_str()
            .unwrap()
            .to_string();
        let unsealed = unseal(&provider(), PROJECT, "login", &handle)
            .await
            .unwrap();
        assert_eq!(unsealed.upstream_refresh_token, "rotated");
        // A body that is not a JSON object cannot take a handle and passes through.
        let reply = refresh(&legacy, 200, "ok").await.unwrap();
        assert_eq!(reply.body, b"ok");
    }

    #[tokio::test]
    async fn refresh_responses_keep_status_content_type_member_order_and_unknown_members() {
        let handle = presented_handle("old-rt").await;
        let body = r#"{"zeta":1,"access_token":"at","refresh_token":"new-rt","scope":"upstream scope","alpha":{"b":[1,2.50,null],"a":1},"expires_in":3600}"#;
        let reply = refresh(&handle, 200, body).await.unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type.as_deref(), Some("application/json"));
        let text = String::from_utf8(reply.body.clone()).unwrap();
        let positions: Vec<usize> = [
            "zeta",
            "access_token",
            "refresh_token",
            "scope",
            "alpha",
            "expires_in",
        ]
        .iter()
        .map(|name| text.find(&format!("\"{name}\"")).unwrap())
        .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{text}");
        assert!(
            text.contains(r#""alpha":{"b":[1,2.50,null],"a":1}"#),
            "{text}"
        );
        assert_eq!(reply_json(&reply)["scope"], "upstream scope");
    }

    #[tokio::test]
    async fn a_2xx_body_that_is_not_an_object_but_carries_a_refresh_token_fails_closed() {
        let handle = presented_handle("old-rt").await;
        let legacy = PresentedRefreshToken::Legacy("raw".into());
        for body in [
            "access_token=at&refresh_token=rt&token_type=bearer",
            r#"["refresh_token","rt"]"#,
            "refresh_token=rt",
        ] {
            for presented in [&handle, &legacy] {
                assert!(
                    matches!(
                        refresh(presented, 200, body).await,
                        Err(ReplyError::Unparseable)
                    ),
                    "{body}"
                );
            }
        }
        // The same for a code-grant response.
        let out = seal_in_token_response(
            &provider(),
            Uuid::new_v4(),
            "login",
            200,
            "access_token=at&refresh_token=rt".into(),
            1,
        )
        .await;
        assert!(matches!(out, Err(ReplyError::Unparseable)));
        // Bodies without any sign of a refresh token pass through.
        for body in ["ok", "", "access_token=at&token_type=bearer"] {
            assert_eq!(
                refresh(&handle, 200, body).await.unwrap().body,
                body.as_bytes()
            );
        }
    }

    #[tokio::test]
    async fn every_refresh_token_member_of_a_duplicated_name_is_replaced() {
        let handle = presented_handle("old-rt").await;
        let reply = refresh(
            &handle,
            200,
            r#"{"refresh_token":"first","access_token":"at","refresh_token":"second"}"#,
        )
        .await
        .unwrap();
        let text = String::from_utf8(reply.body).unwrap();
        assert!(
            !text.contains("first") && !text.contains("second"),
            "{text}"
        );
    }

    struct FailingProvider(fn() -> anyhow::Error);

    #[async_trait::async_trait]
    impl EncryptionProvider for FailingProvider {
        async fn encrypt(&self, _: &str, _: &EncryptionContext) -> Result<String> {
            Err((self.0)())
        }
        async fn decrypt(&self, _: &str, _: &EncryptionContext) -> Result<String> {
            Err((self.0)())
        }
    }

    #[tokio::test]
    async fn only_ciphertext_failures_reject_a_handle_and_other_failures_are_unavailable() {
        use rise_backend_core::encryption::CiphertextRejected;
        let project = Uuid::new_v4();
        let handle = seal(&provider(), project, "login", "rt", 1).await.unwrap();

        let rejecting =
            FailingProvider(|| anyhow!("authentication failed").context(CiphertextRejected));
        assert!(matches!(
            unseal(&rejecting, project, "login", &handle).await,
            Err(HandleError::Rejected(_))
        ));
        let throttled = FailingProvider(|| anyhow!("ThrottlingException"));
        assert!(matches!(
            unseal(&throttled, project, "login", &handle).await,
            Err(HandleError::Unavailable(_))
        ));
        assert!(matches!(
            present(&throttled, project, "login", &handle).await,
            Err(HandleError::Unavailable(_))
        ));
        // A raw token is never decrypted, so a failing provider is irrelevant.
        assert!(present(&throttled, project, "login", "raw").await.is_ok());
        // Things that are not handles at all are rejected before any provider call.
        assert!(matches!(
            unseal(&throttled, project, "login", "rrt2.abc").await,
            Err(HandleError::Rejected(_))
        ));
        // A real provider rejects tampering with the typed marker.
        let mut tampered = handle.clone();
        tampered.pop();
        tampered.push(if handle.ends_with('A') { 'B' } else { 'A' });
        let error = provider()
            .decrypt(
                &format!("{ENVELOPE_PREFIX}{}", &tampered["rrt1.".len()..]),
                &context(project, "login"),
            )
            .await
            .unwrap_err();
        assert!(is_ciphertext_rejection(&error));
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
            (200, r#"["token"]"#),
            (200, "access_token=at&token_type=bearer"),
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
