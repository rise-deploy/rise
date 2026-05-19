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

use crate::server::error::ServerError;
use crate::server::state::AppState;

/// A trusted external controller identity configured under `auth.controllers`.
///
/// Each entry binds a stable controller ID (the key used under
/// `status.controllers`) to an OIDC issuer plus optional claim constraints
/// (`audience`, `subject`, free-form `claims`). Wildcards in
/// `subject`/`claims` follow `JwtValidator::validate_custom_claims` rules.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ControllerIdentity {
    /// Stable controller ID written under `status.controllers`. Must be a
    /// DNS subdomain with an optional single `/name` suffix, e.g.
    /// `controller.example.com` or `controller.example.com/my-ctrl`.
    pub id: String,
    /// OIDC issuer URL. Used for JWKS discovery and `iss` validation.
    pub issuer: String,
    /// Expected `aud` claim. Exact match when set.
    #[serde(default)]
    pub audience: Option<String>,
    /// Expected `sub` claim. Wildcards supported.
    #[serde(default)]
    pub subject: Option<String>,
    /// Additional claim constraints. Wildcards supported.
    #[serde(default)]
    pub claims: HashMap<String, String>,
}

/// A JWKS-validated controller token, injected into request extensions by
/// `auth_middleware` after `JwtValidator::validate_token` succeeds AND the
/// matching `ControllerIdentity`'s claim constraints are satisfied.
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
        _state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<VerifiedControllerToken>()
            .cloned()
            .map(ControllerAuthContext)
            .ok_or_else(|| ServerError::unauthorized("Controller authentication required"))
    }
}

// DNS-1123 subdomain followed by an optional single `/name` suffix.
// - Host: one or more lowercase DNS labels joined by `.`, at least one `.` (so
//   single-word hosts like `localhost` are rejected — controller IDs must be
//   fully-qualified to act as Kubernetes annotation key prefixes).
// - Optional path segment after `/`: `[A-Za-z0-9_.-]+` (matches Kubernetes
//   annotation key name format).
static CONTROLLER_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)+(/[A-Za-z0-9_.-]+)?$",
    )
    .expect("controller id regex compiles")
});

/// Validate a controller `id` value.
///
/// Format: DNS-1123 subdomain (at least one `.`) optionally followed by a
/// single `/name` segment, max 253 chars total.
pub fn validate_controller_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("controller id is empty");
    }
    if id.len() > 253 {
        bail!("controller id too long (>{} chars)", 253);
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

/// Index a list of `ControllerIdentity` values by id and by issuer.
///
/// Validates each id, rejects duplicate ids, and returns
/// `(by_id, by_issuer)`. Multiple identities may share an issuer (they get
/// disambiguated by `audience`/`subject`/`claims` at request time).
pub fn build_controller_indexes(controllers: &[ControllerIdentity]) -> Result<ControllerIndexes> {
    let mut by_id: HashMap<String, ControllerIdentity> = HashMap::new();
    let mut by_issuer: HashMap<String, Vec<ControllerIdentity>> = HashMap::new();

    for c in controllers {
        validate_controller_id(&c.id)
            .with_context(|| format!("invalid controller id for entry {:?}", c.id))?;
        if c.issuer.is_empty() {
            return Err(anyhow!("controller {:?} has empty issuer", c.id));
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
        ] {
            assert!(
                validate_controller_id(id).is_err(),
                "expected {:?} to be rejected",
                id
            );
        }
    }

    #[test]
    fn test_validate_controller_id_rejects_too_long() {
        // Build a 254-char id: many short labels.
        let host = (0..50).map(|_| "ab").collect::<Vec<_>>().join(".");
        // host is 50 labels of 2 chars + 49 dots = 100 + 49 = 149 chars
        let mut id = host.clone();
        while id.len() <= 253 {
            id.push_str(".ab");
        }
        assert!(id.len() > 253);
        // Strip until valid form check would matter; format may also fail format check
        // but length check fires first.
        let err = validate_controller_id(&id).unwrap_err().to_string();
        assert!(err.contains("too long"), "got: {err}");
    }

    fn ident(id: &str, issuer: &str) -> ControllerIdentity {
        ControllerIdentity {
            id: id.to_string(),
            issuer: issuer.to_string(),
            audience: None,
            subject: None,
            claims: HashMap::new(),
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
}
