//! Controller identity authentication context.
//!
//! Defines the configuration type, the request-extension token type, and the
//! Axum extractor used by generic-resource controller endpoints. The controller
//! auth context is intentionally separate from `AuthContext` (which covers user
//! JWTs and project-scoped service-account JWTs) so the type system rules out
//! mixing controller tokens with user/SA flows.
//!
//! Wiring is added in PR3 but no HTTP route consumes the extractor yet — that
//! lands with PR4 (the generic resource API).
use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Context, Result};
use axum::{extract::FromRequestParts, http::request::Parts};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::server::auth::context::VerifiedExternalToken;
use crate::server::error::ServerError;
use crate::server::state::AppState;

/// A trusted external controller identity configured under `auth.controllers`.
///
/// Each entry binds a stable controller ID (the key used under
/// `status.controllers`) to an OIDC issuer plus required claim constraints.
/// `claims.aud` is mandatory. Wildcards follow `JwtValidator::validate_custom_claims`
/// glob rules (`*` matches any sequence of characters).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ControllerIdentity {
    /// Stable controller ID written under `status.controllers`. Must be a
    /// DNS subdomain with an optional single `/name` suffix, e.g.
    /// `controller.example.com` or `controller.example.com/my-ctrl`.
    pub id: String,
    /// OIDC issuer URL. Used for JWKS discovery and `iss` validation.
    pub issuer: String,
    /// Expected string-valued claim constraints. `aud` is required and may
    /// match either a string JWT `aud` or one member of an array JWT `aud`.
    /// Put `sub` here when a subject constraint is needed. Glob `*` supported.
    #[serde(default)]
    pub claims: HashMap<String, String>,
}

/// A JWKS-validated controller token, produced by `ControllerAuthContext` after
/// `auth_middleware` verifies the external JWT and the matching
/// `ControllerIdentity`'s claim constraints are satisfied.
///
/// `#[allow(dead_code)]` until the generic resource controller endpoints
/// (a later PR) consume the fields.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct VerifiedControllerToken {
    pub identity_id: String,
    pub issuer: String,
    pub claims: serde_json::Value,
}

/// Axum extractor — yields the verified controller token or 401.
///
/// `#[allow(dead_code)]` until the generic resource controller endpoints
/// (a later PR) take it as a handler argument.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ControllerAuthContext(pub VerifiedControllerToken);

impl FromRequestParts<AppState> for ControllerAuthContext {
    type Rejection = ServerError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let token = parts
            .extensions
            .get::<VerifiedExternalToken>()
            .cloned()
            .ok_or_else(|| ServerError::unauthorized("Controller authentication required"))?;

        let candidates = state
            .controllers_by_issuer
            .get(&token.issuer)
            .ok_or_else(|| ServerError::unauthorized("Controller authentication required"))?;

        match match_controller_identity(&token.claims, candidates) {
            ControllerMatch::Single(ident) => Ok(ControllerAuthContext(VerifiedControllerToken {
                identity_id: ident.id.clone(),
                issuer: token.issuer,
                claims: token.claims,
            })),
            ControllerMatch::None(detail) => {
                tracing::warn!(
                    "Controller JWT for issuer '{}' did not match any configured identity: {}",
                    token.issuer,
                    detail
                );
                Err(ServerError::unauthorized(
                    "Token did not match any configured controller identity",
                ))
            }
            ControllerMatch::Multiple(matched) => {
                let ids: Vec<&str> = matched.iter().map(|i| i.id.as_str()).collect();
                tracing::error!(
                    "Multiple controller identities matched JWT from issuer '{}': {:?}",
                    token.issuer,
                    ids
                );
                Err(ServerError::conflict(
                    "Token matched multiple controller identities; configuration is ambiguous",
                ))
            }
        }
    }
}

// DNS-1123 subdomain followed by an optional `/name` suffix where `name`
// follows Kubernetes annotation key name rules.
//
// Host portion: one or more lowercase DNS labels joined by `.`, at least one
// `.` (so single-word hosts like `localhost` are rejected — controller IDs
// must be fully-qualified to act as Kubernetes annotation key prefixes).
//
// Name portion: matches the rule from
// <https://kubernetes.io/docs/concepts/overview/working-with-objects/labels/#syntax-and-character-set>:
// "must be 63 characters or less, beginning and ending with an alphanumeric
// character ([a-z0-9A-Z]) with dashes (-), underscores (_), dots (.), and
// alphanumerics between". Length is enforced separately in
// `validate_controller_id` so the regex stays readable.
static CONTROLLER_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)+(/[A-Za-z0-9]([A-Za-z0-9_.-]*[A-Za-z0-9])?)?$",
    )
    .expect("controller id regex compiles")
});

/// Maximum length of the `/name` portion of a controller id, per Kubernetes
/// annotation key name rules.
const CONTROLLER_ID_NAME_MAX_LEN: usize = 63;

/// Validate a controller `id` value.
///
/// Format: DNS-1123 subdomain (at least one `.`) optionally followed by a
/// single `/name` segment matching the Kubernetes annotation key name rules
/// (alphanumeric start/end, `-`/`_`/`.` allowed in between, max 63 chars).
/// Whole id is capped at 253 chars.
pub fn validate_controller_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("controller id is empty");
    }
    if id.len() > 253 {
        bail!("controller id too long (>{} chars)", 253);
    }
    if let Some(slash_pos) = id.find('/') {
        let name = &id[slash_pos + 1..];
        if name.len() > CONTROLLER_ID_NAME_MAX_LEN {
            bail!(
                "controller id name portion too long ({} > {} chars)",
                name.len(),
                CONTROLLER_ID_NAME_MAX_LEN
            );
        }
    }
    if !CONTROLLER_ID_RE.is_match(id) {
        bail!(
            "invalid controller id {:?}: expected DNS subdomain with optional /name suffix",
            id
        );
    }
    Ok(())
}

/// Maps used by middleware to look up controller identities at request time.
pub type ControllerIndexes = (
    HashMap<String, ControllerIdentity>,
    HashMap<String, Vec<ControllerIdentity>>,
);

/// Outcome of matching JWT claims against a set of controller identity
/// candidates that share the token's issuer.
#[derive(Debug)]
pub enum ControllerMatch<'a> {
    /// Exactly one identity matched the token's claim constraints.
    Single(&'a ControllerIdentity),
    /// No identity matched. The contained string explains the most recent
    /// failure (for diagnostics / 401 detail).
    None(String),
    /// Two or more identities matched — configuration is ambiguous.
    Multiple(Vec<&'a ControllerIdentity>),
}

/// Check whether a JWT `aud` claim matches the expected audience.
///
/// `aud` may be a single string or an array of strings per RFC 7519 §4.1.3
/// (`aud` is "a single case-sensitive string or a JSON array of case-sensitive
/// strings"). When the expected audience contains a `*` it is matched as a
/// glob pattern using the same rules as `JwtValidator::validate_custom_claims`.
fn audience_matches(claim: &serde_json::Value, expected: &str) -> bool {
    let check = |actual: &str| -> bool {
        if expected.contains('*') {
            crate::server::auth::jwt::JwtValidator::matches_wildcard_pattern(expected, actual)
        } else {
            actual == expected
        }
    };
    match claim {
        serde_json::Value::String(s) => check(s),
        serde_json::Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).any(check),
        _ => false,
    }
}

/// Match a verified JWT's claims against a list of candidate identities.
///
/// Pure helper extracted from `auth_middleware` so it can be unit-tested
/// without spinning up JWKS. Caller is responsible for verifying that the
/// JWT signature and `iss` are correct before invoking.
///
/// Each identity's `claims` constraints are applied via
/// `validate_custom_claims` (string-valued claims with glob `*`), except `aud`,
/// which accepts either a string or an array of strings per RFC 7519 §4.1.3.
///
/// Returns `Single` when exactly one identity satisfies all constraints,
/// `Multiple` when two or more do (ambiguous configuration), and `None` with
/// the most recent failure detail when none match.
pub fn match_controller_identity<'a>(
    token_claims: &serde_json::Value,
    candidates: &'a [ControllerIdentity],
) -> ControllerMatch<'a> {
    let mut matched: Vec<&'a ControllerIdentity> = Vec::new();
    let mut last_err: Option<String> = None;

    for ident in candidates {
        let Some(expected_aud) = ident.claims.get("aud") else {
            last_err = Some(format!(
                "identity {:?}: missing required claims.aud constraint",
                ident.id
            ));
            continue;
        };
        let aud_claim = token_claims.get("aud").unwrap_or(&serde_json::Value::Null);
        if !audience_matches(aud_claim, expected_aud) {
            last_err = Some(format!(
                "identity {:?}: aud claim does not match expected audience {:?}",
                ident.id, expected_aud
            ));
            continue;
        }

        let expected_string_claims: HashMap<String, String> = ident
            .claims
            .iter()
            .filter(|(key, _)| key.as_str() != "aud")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        match crate::server::auth::jwt::JwtValidator::validate_custom_claims(
            token_claims,
            &expected_string_claims,
        ) {
            Ok(()) => matched.push(ident),
            Err(e) => {
                last_err = Some(format!("identity {:?}: {}", ident.id, e));
            }
        }
    }

    match matched.len() {
        1 => ControllerMatch::Single(matched.into_iter().next().unwrap()),
        0 => ControllerMatch::None(last_err.unwrap_or_else(|| "no candidates".to_string())),
        _ => ControllerMatch::Multiple(matched),
    }
}

/// Index a list of `ControllerIdentity` values by id and by issuer.
///
/// Validates each id, rejects duplicate ids, and returns
/// `(by_id, by_issuer)`. Multiple identities may share an issuer (they get
/// disambiguated by claim constraints at request time).
pub fn build_controller_indexes(controllers: &[ControllerIdentity]) -> Result<ControllerIndexes> {
    let mut by_id: HashMap<String, ControllerIdentity> = HashMap::new();
    let mut by_issuer: HashMap<String, Vec<ControllerIdentity>> = HashMap::new();

    for c in controllers {
        validate_controller_id(&c.id)
            .with_context(|| format!("invalid controller id for entry {:?}", c.id))?;
        if c.issuer.is_empty() {
            return Err(anyhow!("controller {:?} has empty issuer", c.id));
        }
        let Some(aud) = c.claims.get("aud") else {
            return Err(anyhow!("controller {:?} must configure claims.aud", c.id));
        };
        if aud.is_empty() {
            return Err(anyhow!("controller {:?} has empty claims.aud", c.id));
        }
        if by_id.insert(c.id.clone(), c.clone()).is_some() {
            return Err(anyhow!("duplicate controller id: {:?}", c.id));
        }
        by_issuer
            .entry(c.issuer.clone())
            .or_default()
            .push(c.clone());
    }

    Ok((by_id, by_issuer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_controller_id_accepts_valid_forms() {
        for id in [
            "controller.example.com",
            "controller.example.com/my-ctrl",
            "k8s.rise.dev/default",
            "a.b",
            "a.b/c",
            "deep.sub.domain.example.com",
            "deep.sub.domain.example.com/Ctrl_1.0",
        ] {
            validate_controller_id(id)
                .unwrap_or_else(|e| panic!("expected {:?} to be valid: {}", id, e));
        }
    }

    #[test]
    fn test_validate_controller_id_rejects_invalid_forms() {
        for id in [
            "",
            "controller",                        // no dot — not a subdomain
            "Controller.example.com",            // uppercase in host
            "-bad.example.com",                  // leading hyphen
            "bad-.example.com",                  // trailing hyphen on label
            "controller.example.com/",           // trailing slash
            "/controller",                       // leading slash, no host
            "controller.example.com/a/b",        // nested path segments not allowed
            "controller.example.com//x",         // empty path segment
            "controller.example.com/with space", // invalid path char
            "controller.example.com/-leading",   // leading hyphen in name
            "controller.example.com/trailing-",  // trailing hyphen in name
            "controller.example.com/.dot",       // leading dot in name
            "controller.example.com/dot.",       // trailing dot in name
            "controller.example.com/_under",     // leading underscore in name
            "controller.example.com/under_",     // trailing underscore in name
        ] {
            assert!(
                validate_controller_id(id).is_err(),
                "expected {:?} to be rejected",
                id
            );
        }
    }

    #[test]
    fn test_validate_controller_id_rejects_too_long_total() {
        // Build a host >253 chars so the whole-id length check fires.
        let host = (0..50).map(|_| "ab").collect::<Vec<_>>().join(".");
        let mut id = host;
        while id.len() <= 253 {
            id.push_str(".ab");
        }
        assert!(id.len() > 253);
        let err = validate_controller_id(&id).unwrap_err().to_string();
        assert!(err.contains("too long"), "got: {err}");
    }

    #[test]
    fn test_validate_controller_id_rejects_name_over_63_chars() {
        // 64-character name segment (alphanumeric so the format check would pass,
        // forcing the length check to fire first).
        let id = format!("controller.example.com/{}", "a".repeat(64));
        let err = validate_controller_id(&id).unwrap_err().to_string();
        assert!(
            err.contains("name portion too long"),
            "expected name length error, got: {err}"
        );
    }

    #[test]
    fn test_validate_controller_id_accepts_name_exactly_63_chars() {
        let id = format!("controller.example.com/{}", "a".repeat(63));
        validate_controller_id(&id).expect("63-char name should be accepted");
    }

    fn ident(id: &str, issuer: &str) -> ControllerIdentity {
        ControllerIdentity {
            id: id.to_string(),
            issuer: issuer.to_string(),
            claims: HashMap::from([("aud".to_string(), "rise".to_string())]),
        }
    }

    #[test]
    fn test_build_indexes_happy_path() {
        let c1 = ident("a.example.com", "https://issuer.example.com");
        let c2 = ident("b.example.com/x", "https://issuer.example.com");
        let c3 = ident("c.example.com", "https://other.example.com");

        let (by_id, by_iss) = build_controller_indexes(&[c1, c2, c3]).unwrap();

        assert_eq!(by_id.len(), 3);
        assert!(by_id.contains_key("a.example.com"));
        assert!(by_id.contains_key("b.example.com/x"));
        assert!(by_id.contains_key("c.example.com"));

        assert_eq!(by_iss.len(), 2);
        assert_eq!(by_iss["https://issuer.example.com"].len(), 2);
        assert_eq!(by_iss["https://other.example.com"].len(), 1);
    }

    #[test]
    fn test_build_indexes_rejects_duplicate_id() {
        let c = ident("a.example.com", "https://issuer.example.com");
        let err = build_controller_indexes(&[c.clone(), c])
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate controller id"), "got: {err}");
    }

    #[test]
    fn test_build_indexes_rejects_invalid_id() {
        let c = ident("not-a-subdomain", "https://issuer.example.com");
        let err = build_controller_indexes(&[c]).unwrap_err().to_string();
        assert!(err.contains("invalid controller id"), "got: {err}");
    }

    #[test]
    fn test_build_indexes_rejects_empty_issuer() {
        let c = ident("a.example.com", "");
        let err = build_controller_indexes(&[c]).unwrap_err().to_string();
        assert!(err.contains("empty issuer"), "got: {err}");
    }

    #[test]
    fn test_build_indexes_rejects_missing_audience_claim() {
        let c = ControllerIdentity {
            id: "a.example.com".to_string(),
            issuer: "https://issuer.example.com".to_string(),
            claims: HashMap::new(),
        };
        let err = build_controller_indexes(&[c]).unwrap_err().to_string();
        assert!(err.contains("claims.aud"), "got: {err}");
    }

    #[test]
    fn test_build_indexes_rejects_empty_audience_claim() {
        let c = ControllerIdentity {
            id: "a.example.com".to_string(),
            issuer: "https://issuer.example.com".to_string(),
            claims: HashMap::from([("aud".to_string(), "".to_string())]),
        };
        let err = build_controller_indexes(&[c]).unwrap_err().to_string();
        assert!(err.contains("empty claims.aud"), "got: {err}");
    }

    // --- audience_matches ---

    #[test]
    fn test_audience_matches_string_exact() {
        assert!(audience_matches(&serde_json::json!("rise"), "rise"));
        assert!(!audience_matches(&serde_json::json!("rise"), "other"));
    }

    #[test]
    fn test_audience_matches_string_glob() {
        assert!(audience_matches(&serde_json::json!("rise-prod"), "rise-*"));
        assert!(audience_matches(&serde_json::json!("rise"), "*"));
        assert!(!audience_matches(&serde_json::json!("other"), "rise-*"));
    }

    #[test]
    fn test_audience_matches_array_contains_expected() {
        let claim = serde_json::json!(["other-aud", "rise", "extra"]);
        assert!(audience_matches(&claim, "rise"));
    }

    #[test]
    fn test_audience_matches_array_no_match() {
        let claim = serde_json::json!(["a", "b", "c"]);
        assert!(!audience_matches(&claim, "rise"));
    }

    #[test]
    fn test_audience_matches_array_glob() {
        let claim = serde_json::json!(["api-prod", "web-dev"]);
        assert!(audience_matches(&claim, "api-*"));
        assert!(audience_matches(&claim, "*-dev"));
        assert!(!audience_matches(&claim, "api-dev"));
    }

    #[test]
    fn test_audience_matches_array_ignores_non_string_entries() {
        let claim = serde_json::json!([42, true, "rise"]);
        assert!(audience_matches(&claim, "rise"));
    }

    #[test]
    fn test_audience_matches_missing_claim() {
        assert!(!audience_matches(&serde_json::Value::Null, "rise"));
        assert!(!audience_matches(&serde_json::json!(42), "rise"));
    }

    // --- match_controller_identity ---

    fn ident_full(id: &str, claims: &[(&str, &str)]) -> ControllerIdentity {
        ControllerIdentity {
            id: id.to_string(),
            issuer: "https://issuer.example.com".to_string(),
            claims: claims
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn test_match_none_without_audience_constraint() {
        let candidates = [ident_full("a.example.com", &[])];
        let claims = serde_json::json!({"sub": "anyone", "aud": "anything"});
        let m = match_controller_identity(&claims, &candidates);
        match m {
            ControllerMatch::None(detail) => assert!(detail.contains("claims.aud"), "{detail}"),
            other => panic!("expected None, got {other:?}"),
        }
    }

    #[test]
    fn test_match_single_with_constraints() {
        let candidates = [ident_full(
            "a.example.com",
            &[("aud", "rise"), ("sub", "ctrl-*"), ("scope", "controller")],
        )];
        let claims = serde_json::json!({
            "sub": "ctrl-abc",
            "aud": "rise",
            "scope": "controller",
        });
        let m = match_controller_identity(&claims, &candidates);
        assert!(matches!(m, ControllerMatch::Single(_)), "got {m:?}");
    }

    #[test]
    fn test_match_audience_array() {
        let candidates = [ident_full("a.example.com", &[("aud", "rise")])];
        let claims = serde_json::json!({
            "sub": "x",
            "aud": ["other", "rise"],
        });
        let m = match_controller_identity(&claims, &candidates);
        assert!(matches!(m, ControllerMatch::Single(_)), "got {m:?}");
    }

    #[test]
    fn test_match_audience_array_no_match() {
        let candidates = [ident_full("a.example.com", &[("aud", "rise")])];
        let claims = serde_json::json!({"sub": "x", "aud": ["other"]});
        let m = match_controller_identity(&claims, &candidates);
        match m {
            ControllerMatch::None(detail) => {
                assert!(detail.contains("aud claim does not match"), "got: {detail}")
            }
            other => panic!("expected None, got {other:?}"),
        }
    }

    #[test]
    fn test_match_none_when_subject_mismatches() {
        let candidates = [ident_full(
            "a.example.com",
            &[("aud", "rise"), ("sub", "ctrl-*")],
        )];
        let claims = serde_json::json!({"aud": "rise", "sub": "other-bot"});
        let m = match_controller_identity(&claims, &candidates);
        match m {
            ControllerMatch::None(detail) => assert!(detail.contains("a.example.com"), "{detail}"),
            other => panic!("expected None, got {other:?}"),
        }
    }

    #[test]
    fn test_match_none_when_extra_claim_missing() {
        let candidates = [ident_full(
            "a.example.com",
            &[("aud", "rise"), ("scope", "controller")],
        )];
        let claims = serde_json::json!({"aud": "rise", "sub": "x"});
        let m = match_controller_identity(&claims, &candidates);
        assert!(matches!(m, ControllerMatch::None(_)), "got {m:?}");
    }

    #[test]
    fn test_match_multiple_when_constraints_ambiguous() {
        // Two identities with the same audience both match the token.
        let candidates = [
            ident_full("a.example.com", &[("aud", "rise")]),
            ident_full("b.example.com", &[("aud", "rise")]),
        ];
        let claims = serde_json::json!({"aud": "rise", "sub": "x"});
        let m = match_controller_identity(&claims, &candidates);
        match m {
            ControllerMatch::Multiple(matches) => {
                let ids: Vec<&str> = matches.iter().map(|i| i.id.as_str()).collect();
                assert_eq!(ids.len(), 2);
                assert!(ids.contains(&"a.example.com"));
                assert!(ids.contains(&"b.example.com"));
            }
            other => panic!("expected Multiple, got {other:?}"),
        }
    }

    #[test]
    fn test_match_disambiguates_by_audience() {
        // Same issuer, different audiences; only one should match.
        let candidates = [
            ident_full("a.example.com", &[("aud", "rise-a")]),
            ident_full("b.example.com", &[("aud", "rise-b")]),
        ];
        let claims = serde_json::json!({"sub": "x", "aud": "rise-b"});
        let m = match_controller_identity(&claims, &candidates);
        match m {
            ControllerMatch::Single(i) => assert_eq!(i.id, "b.example.com"),
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[test]
    fn test_match_disambiguates_by_subject() {
        let candidates = [
            ident_full("a.example.com", &[("aud", "rise"), ("sub", "ctrl-a")]),
            ident_full("b.example.com", &[("aud", "rise"), ("sub", "ctrl-b")]),
        ];
        let claims = serde_json::json!({"aud": "rise", "sub": "ctrl-b"});
        let m = match_controller_identity(&claims, &candidates);
        match m {
            ControllerMatch::Single(i) => assert_eq!(i.id, "b.example.com"),
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[test]
    fn test_match_empty_candidates() {
        let m = match_controller_identity(&serde_json::json!({}), &[]);
        assert!(matches!(m, ControllerMatch::None(_)));
    }
}
