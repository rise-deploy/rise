//! Pure claim- and identity-matching helpers.
//!
//! These functions operate over already-verified claims (or static config) and
//! perform no I/O. They back the controller-identity and service-account
//! matching paths in rise-deploy.

use std::collections::{BTreeMap, HashMap};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::AuthError;

/// A trusted external controller identity configured under `auth.controllers`.
///
/// Each entry binds a stable controller ID (the key used under
/// `status.controllers`) to an OIDC issuer plus required claim constraints.
/// `claims.aud` is mandatory. Wildcards follow [`validate_custom_claims`]
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

/// Indexes built at startup from the configured controller identities.
#[derive(Debug)]
pub struct ControllerIndexes {
    /// Controller identities keyed by `id`.
    pub by_id: HashMap<String, ControllerIdentity>,
    /// Controller identities keyed by issuer URL for O(1) middleware lookup.
    pub by_issuer: HashMap<String, Vec<ControllerIdentity>>,
}

/// Outcome of matching JWT claims against a set of controller identity
/// candidates that share the token's issuer.
#[derive(Debug)]
pub enum ControllerMatch<'a> {
    /// Exactly one identity matched the token's claim constraints.
    Single(&'a ControllerIdentity),
    /// No identity matched. The contained string lists all per-identity
    /// rejection reasons, joined by `"; "`, for diagnostics.
    Unmatched(String),
    /// Two or more identities matched — configuration is ambiguous.
    Multiple(Vec<&'a ControllerIdentity>),
}

/// One candidate in a trust-policy match: a borrowed label (for diagnostics)
/// and its expected claim constraints.
///
/// Shape-agnostic so this crate never depends on how a caller stores its
/// candidates — `rise-resource-api`'s `ControllerTrustPolicySpec` in
/// particular, which this crate cannot depend on without an orphan-rule
/// cycle. A caller borrows its own facts into this type, matches, and maps
/// the returned index back.
#[derive(Debug, Clone, Copy)]
pub struct TrustCandidate<'a> {
    pub label: &'a str,
    pub claims: &'a BTreeMap<String, String>,
}

/// Outcome of matching JWT claims against a list of [`TrustCandidate`]s,
/// identifying matches by their position in the input slice so a caller can
/// map back to its own fact type without this crate knowing its shape.
#[derive(Debug)]
pub enum TrustMatch {
    /// Exactly one candidate matched, at this index.
    Single(usize),
    /// No candidate matched. The contained string lists all per-candidate
    /// rejection reasons, joined by `"; "`, for diagnostics.
    Unmatched(String),
    /// Two or more candidates matched — configuration is ambiguous.
    Multiple(Vec<usize>),
}

/// Match a verified JWT's claims against a list of [`TrustCandidate`]s.
///
/// Pure helper, and the shape-agnostic twin of [`match_controller_identity`]:
/// same matching rules (mandatory `aud`, remaining claims via
/// [`validate_custom_claims`]), but over borrowed candidates rather than a
/// concrete config type, so it also backs trust-policy resources.
pub fn match_trust_candidates(
    token_claims: &serde_json::Value,
    candidates: &[TrustCandidate<'_>],
) -> TrustMatch {
    let mut matched: Vec<usize> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for (index, candidate) in candidates.iter().enumerate() {
        let Some(expected_aud) = candidate.claims.get("aud") else {
            errors.push(format!(
                "candidate {:?}: missing required claims.aud constraint",
                candidate.label
            ));
            continue;
        };
        let aud_claim = token_claims.get("aud").unwrap_or(&serde_json::Value::Null);
        if !audience_matches(aud_claim, expected_aud) {
            errors.push(format!(
                "candidate {:?}: aud claim does not match expected audience {:?}",
                candidate.label, expected_aud
            ));
            continue;
        }

        let expected_string_claims: HashMap<String, String> = candidate
            .claims
            .iter()
            .filter(|(key, _)| key.as_str() != "aud")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        match validate_custom_claims(token_claims, &expected_string_claims) {
            Ok(()) => matched.push(index),
            Err(e) => {
                errors.push(format!("candidate {:?}: {}", candidate.label, e));
            }
        }
    }

    match matched.len() {
        1 => TrustMatch::Single(matched[0]),
        0 => TrustMatch::Unmatched(if errors.is_empty() {
            "no candidates".to_string()
        } else {
            errors.join("; ")
        }),
        _ => TrustMatch::Multiple(matched),
    }
}

/// Validate custom claims (supports exact matching and wildcard patterns)
///
/// Claims can use wildcard patterns with `*`:
/// - `app*` matches `app`, `app-mr/6`, `app-staging`, etc.
/// - `*-prod` matches `api-prod`, `web-prod`, etc.
/// - `app-*-prod` matches `app-staging-prod`, `app-test-prod`, etc.
///
/// If no wildcard is present, exact matching is performed (backward compatible).
pub fn validate_custom_claims(
    jwt_claims: &serde_json::Value,
    expected_claims: &HashMap<String, String>,
) -> Result<(), AuthError> {
    let claims_obj = jwt_claims
        .as_object()
        .ok_or_else(|| AuthError::ClaimMismatch("JWT claims is not an object".to_string()))?;

    for (key, expected_value) in expected_claims {
        let actual_value = claims_obj
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AuthError::ClaimMismatch(format!("Claim '{}' not found or not a string", key))
            })?;

        // Check if expected value contains wildcard
        if expected_value.contains('*') {
            // Use glob-style pattern matching
            if !matches_wildcard_pattern(expected_value, actual_value) {
                return Err(AuthError::ClaimMismatch(format!(
                    "Claim mismatch: '{}' pattern '{}' does not match '{}'",
                    key, expected_value, actual_value
                )));
            }
        } else {
            // Exact matching (backward compatible)
            if actual_value != expected_value {
                return Err(AuthError::ClaimMismatch(format!(
                    "Claim mismatch: '{}' expected '{}', got '{}'",
                    key, expected_value, actual_value
                )));
            }
        }
    }

    Ok(())
}

/// Match a string against a glob-style pattern with `*` wildcards
///
/// This implements simple glob-style pattern matching where `*` matches any
/// sequence of characters (including empty string). Unlike filesystem globs,
/// this matches across any characters, including path separators.
///
/// Examples:
/// - `matches_wildcard_pattern("app*", "app-mr/6")` → true
/// - `matches_wildcard_pattern("app*", "webapp")` → false (doesn't start with "app")
/// - `matches_wildcard_pattern("*-prod", "api-prod")` → true
/// - `matches_wildcard_pattern("app-*-prod", "app-staging-prod")` → true
///
/// Note: Consecutive wildcards (e.g., `app**prod`) are treated as a single wildcard
/// due to split() creating empty parts, which always match.
pub fn matches_wildcard_pattern(pattern: &str, text: &str) -> bool {
    // Split pattern by '*' to get literal parts
    let parts: Vec<&str> = pattern.split('*').collect();

    // If no wildcards (shouldn't happen, but handle it)
    if parts.len() == 1 {
        return pattern == text;
    }

    let mut pos = 0;

    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            // First part must match the beginning
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            // Last part must match the end (empty part matches any suffix when pattern ends with *)
            if !text[pos..].ends_with(part) {
                return false;
            }
        } else {
            // Middle parts must appear in order
            if let Some(found_pos) = text[pos..].find(part) {
                pos += found_pos + part.len();
            } else {
                return false;
            }
        }
    }

    true
}

/// Validate that an OIDC discovery document's issuer matches the expected
/// issuer URL (RFC 8414 §3.1), modulo a trailing slash.
///
/// Returns `Ok(())` when they match, or an [`AuthError::InvalidConfig`] with a
/// mismatch description otherwise.
pub fn validate_oidc_issuer(
    expected_issuer: &str,
    discovered_issuer: &str,
) -> Result<(), AuthError> {
    let expected = expected_issuer.trim_end_matches('/');
    let actual = discovered_issuer.trim_end_matches('/');
    if expected != actual {
        return Err(AuthError::InvalidConfig(format!(
            "OIDC issuer mismatch: expected '{}', discovery returned '{}'",
            expected_issuer, discovered_issuer
        )));
    }
    Ok(())
}

/// Maximum length of the `/name` portion of a controller id, per Kubernetes
/// annotation key name rules.
const CONTROLLER_ID_NAME_MAX_LEN: usize = 63;

/// Whether `label` is a valid lowercase DNS-1123 label: non-empty, only
/// `[a-z0-9-]`, with no leading or trailing hyphen.
fn is_valid_dns_label(label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    let bytes = label.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    label
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether the `/name` segment is valid: non-empty, alphanumeric start/end,
/// with `[A-Za-z0-9_.-]` in between.
fn is_valid_name_segment(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = name.as_bytes();
    let alnum = |b: u8| b.is_ascii_alphanumeric();
    if !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// Validate a controller `id` value.
///
/// Format: DNS-1123 subdomain (at least one `.`) optionally followed by a
/// single `/name` segment matching the Kubernetes annotation key name rules
/// (alphanumeric start/end, `-`/`_`/`.` allowed in between, max 63 chars).
/// Whole id is capped at 253 chars.
///
/// Hand-rolled (no `regex`) to keep the crate pure; reproduces the structural
/// rules of the original `CONTROLLER_ID_RE`: lowercase host labels, mandatory
/// dot, no leading/trailing label hyphen, and the case-sensitivity split
/// between host (`[a-z0-9]`) and the optional `/name` (`[A-Za-z0-9]`).
///
/// NOTE: `crates/rise-resource-api` carries a parallel controller-id validator.
/// The two implementations must be kept in agreement; de-duplicating them into a
/// single shared validator is a tracked follow-up so they don't drift silently.
pub fn validate_controller_id(id: &str) -> Result<(), AuthError> {
    if id.is_empty() {
        return Err(AuthError::InvalidConfig(
            "controller id is empty".to_string(),
        ));
    }
    if id.len() > 253 {
        return Err(AuthError::InvalidConfig(format!(
            "controller id too long (>{} chars)",
            253
        )));
    }

    // Split into host and optional name. More than one '/' is invalid.
    let (host, name) = match id.split_once('/') {
        Some((h, n)) => (h, Some(n)),
        None => (id, None),
    };

    // DNS label length check (mirrors the original explicit cap).
    if let Some(label) = host.split('.').find(|label| label.len() > 63) {
        return Err(AuthError::InvalidConfig(format!(
            "controller id DNS label too long ({} > 63 chars): {:?}",
            label.len(),
            label
        )));
    }

    // Name length check.
    if let Some(name) = name {
        if name.len() > CONTROLLER_ID_NAME_MAX_LEN {
            return Err(AuthError::InvalidConfig(format!(
                "controller id name portion too long ({} > {} chars)",
                name.len(),
                CONTROLLER_ID_NAME_MAX_LEN
            )));
        }
    }

    // Structural validation: host must be >= 2 dot-separated valid labels.
    let labels: Vec<&str> = host.split('.').collect();
    let host_ok = labels.len() >= 2 && labels.iter().all(|l| is_valid_dns_label(l));
    let name_ok = match name {
        Some(n) => is_valid_name_segment(n),
        None => true,
    };
    if !host_ok || !name_ok {
        return Err(AuthError::InvalidConfig(format!(
            "invalid controller id {:?}: expected DNS subdomain with optional /name suffix",
            id
        )));
    }
    Ok(())
}

/// Check whether a JWT `aud` claim matches the expected audience.
///
/// `aud` may be a single string or an array of strings per RFC 7519 §4.1.3.
/// When the expected audience contains a `*` it is matched as a glob pattern
/// using the same rules as [`validate_custom_claims`].
pub fn audience_matches(claim: &serde_json::Value, expected: &str) -> bool {
    let check = |actual: &str| -> bool {
        if expected.contains('*') {
            matches_wildcard_pattern(expected, actual)
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
/// Pure helper so it can be unit-tested without spinning up JWKS. Caller is
/// responsible for verifying that the JWT signature and `iss` are correct
/// before invoking.
///
/// Each identity's `claims` constraints are applied via [`validate_custom_claims`]
/// (string-valued claims with glob `*`), except `aud`, which accepts either a
/// string or an array of strings per RFC 7519 §4.1.3.
///
/// Returns `Single` when exactly one identity satisfies all constraints,
/// `Multiple` when two or more do (ambiguous configuration), and `Unmatched`
/// with all per-identity rejection reasons when none match.
pub fn match_controller_identity<'a>(
    token_claims: &serde_json::Value,
    candidates: &'a [ControllerIdentity],
) -> ControllerMatch<'a> {
    let mut matched: Vec<&'a ControllerIdentity> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ident in candidates {
        let Some(expected_aud) = ident.claims.get("aud") else {
            errors.push(format!(
                "identity {:?}: missing required claims.aud constraint",
                ident.id
            ));
            continue;
        };
        let aud_claim = token_claims.get("aud").unwrap_or(&serde_json::Value::Null);
        if !audience_matches(aud_claim, expected_aud) {
            errors.push(format!(
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
        match validate_custom_claims(token_claims, &expected_string_claims) {
            Ok(()) => matched.push(ident),
            Err(e) => {
                errors.push(format!("identity {:?}: {}", ident.id, e));
            }
        }
    }

    match matched.len() {
        1 => ControllerMatch::Single(matched.into_iter().next().unwrap()),
        0 => ControllerMatch::Unmatched(if errors.is_empty() {
            "no candidates".to_string()
        } else {
            errors.join("; ")
        }),
        _ => ControllerMatch::Multiple(matched),
    }
}

/// Index a list of `ControllerIdentity` values by id and by issuer.
///
/// Validates each id, rejects duplicate ids, and returns a `ControllerIndexes`.
/// Multiple identities may share an issuer (they get disambiguated by claim
/// constraints at request time).
pub fn build_controller_indexes(
    controllers: &[ControllerIdentity],
) -> Result<ControllerIndexes, AuthError> {
    let mut by_id: HashMap<String, ControllerIdentity> = HashMap::new();
    let mut by_issuer: HashMap<String, Vec<ControllerIdentity>> = HashMap::new();

    for c in controllers {
        validate_controller_id(&c.id).map_err(|e| {
            AuthError::InvalidConfig(format!("invalid controller id for entry {:?}: {}", c.id, e))
        })?;
        if c.issuer.is_empty() {
            return Err(AuthError::InvalidConfig(format!(
                "controller {:?} has empty issuer",
                c.id
            )));
        }
        let Some(aud) = c.claims.get("aud") else {
            return Err(AuthError::InvalidConfig(format!(
                "controller {:?} must configure claims.aud",
                c.id
            )));
        };
        if aud.is_empty() {
            return Err(AuthError::InvalidConfig(format!(
                "controller {:?} has empty claims.aud",
                c.id
            )));
        }
        if by_id.insert(c.id.clone(), c.clone()).is_some() {
            return Err(AuthError::InvalidConfig(format!(
                "duplicate controller id: {:?}",
                c.id
            )));
        }
        by_issuer
            .entry(c.issuer.clone())
            .or_default()
            .push(c.clone());
    }

    Ok(ControllerIndexes { by_id, by_issuer })
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
    fn test_validate_controller_id_rejects_too_long_dns_label() {
        let id = format!("{}.example.com", "a".repeat(64));
        let err = validate_controller_id(&id).unwrap_err().to_string();
        assert!(err.contains("DNS label too long"), "got: {err}");
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

        let indexes = build_controller_indexes(&[c1, c2, c3]).unwrap();

        assert_eq!(indexes.by_id.len(), 3);
        assert!(indexes.by_id.contains_key("a.example.com"));
        assert!(indexes.by_id.contains_key("b.example.com/x"));
        assert!(indexes.by_id.contains_key("c.example.com"));

        assert_eq!(indexes.by_issuer.len(), 2);
        assert_eq!(indexes.by_issuer["https://issuer.example.com"].len(), 2);
        assert_eq!(indexes.by_issuer["https://other.example.com"].len(), 1);
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

    // --- validate_custom_claims / wildcard ---

    #[test]
    fn test_validate_custom_claims_success() {
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "project_path": "myorg/myrepo",
            "extra": "value"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("project_path".to_string(), "myorg/myrepo".to_string());

        assert!(validate_custom_claims(&jwt_claims, &expected).is_ok());
    }

    #[test]
    fn test_validate_custom_claims_missing() {
        let jwt_claims = serde_json::json!({"aud": "my-audience"});

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("project_path".to_string(), "myorg/myrepo".to_string());

        let result = validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_path"));
    }

    #[test]
    fn test_validate_custom_claims_mismatch() {
        let jwt_claims = serde_json::json!({
            "aud": "wrong-audience",
            "project_path": "myorg/myrepo"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());

        let result = validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mismatch"));
    }

    #[test]
    fn test_wildcard_pattern_prefix() {
        assert!(matches_wildcard_pattern("app*", "app"));
        assert!(matches_wildcard_pattern("app*", "app-mr/6"));
        assert!(matches_wildcard_pattern("app*", "app-staging"));
        assert!(matches_wildcard_pattern("app*", "application"));
        assert!(!matches_wildcard_pattern("app*", "myapp"));
        assert!(!matches_wildcard_pattern("app*", "webapp"));
    }

    #[test]
    fn test_wildcard_pattern_suffix() {
        assert!(matches_wildcard_pattern("*-prod", "api-prod"));
        assert!(matches_wildcard_pattern("*-prod", "web-prod"));
        assert!(matches_wildcard_pattern("*-prod", "my-service-prod"));
        assert!(!matches_wildcard_pattern("*-prod", "production"));
        assert!(!matches_wildcard_pattern("*-prod", "prod"));
        assert!(!matches_wildcard_pattern("*-prod", "api-prod-backup"));
    }

    #[test]
    fn test_wildcard_pattern_middle() {
        assert!(matches_wildcard_pattern("app-*-prod", "app-staging-prod"));
        assert!(matches_wildcard_pattern("app-*-prod", "app-test-prod"));
        assert!(matches_wildcard_pattern("app-*-prod", "app-mr/6-prod"));
        assert!(!matches_wildcard_pattern("app-*-prod", "app-prod"));
        assert!(!matches_wildcard_pattern("app-*-prod", "app-staging"));
        assert!(!matches_wildcard_pattern("app-*-prod", "web-staging-prod"));
    }

    #[test]
    fn test_wildcard_pattern_multiple() {
        assert!(matches_wildcard_pattern("*-app-*", "my-app-staging"));
        assert!(matches_wildcard_pattern("*-app-*", "test-app-mr/6"));
        assert!(matches_wildcard_pattern("*-app-*", "web-app-prod"));
        assert!(!matches_wildcard_pattern("*-app-*", "my-application"));
        assert!(!matches_wildcard_pattern("*-app-*", "app"));
    }

    #[test]
    fn test_wildcard_pattern_edge_cases() {
        assert!(matches_wildcard_pattern("*", "anything"));
        assert!(matches_wildcard_pattern("*", ""));
        assert!(matches_wildcard_pattern("app*", "application"));
        assert!(matches_wildcard_pattern("app*", "app"));
        assert!(matches_wildcard_pattern("*app", "myapp"));
        assert!(matches_wildcard_pattern("*app", "app"));
        assert!(matches_wildcard_pattern("app-*", "app-staging"));
        assert!(matches_wildcard_pattern("app-*", "app-"));
        assert!(!matches_wildcard_pattern("app-*", "app"));
        assert!(matches_wildcard_pattern("app**prod", "appprod"));
        assert!(matches_wildcard_pattern("app**prod", "app-staging-prod"));
        assert!(matches_wildcard_pattern("app***prod", "app-test-prod"));
        assert!(!matches_wildcard_pattern("app*", ""));
        assert!(!matches_wildcard_pattern("*app", "ap"));
    }

    #[test]
    fn test_validate_custom_claims_with_wildcard() {
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "environment": "app-mr/6"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("environment".to_string(), "app*".to_string());

        assert!(validate_custom_claims(&jwt_claims, &expected).is_ok());
    }

    #[test]
    fn test_validate_custom_claims_with_wildcard_no_match() {
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "environment": "webapp-staging"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("environment".to_string(), "app*".to_string());

        let result = validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("pattern"));
    }

    #[test]
    fn test_validate_custom_claims_wildcard_backward_compat() {
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "environment": "production"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("environment".to_string(), "production".to_string());
        assert!(validate_custom_claims(&jwt_claims, &expected).is_ok());

        let mut expected_wrong = HashMap::new();
        expected_wrong.insert("aud".to_string(), "my-audience".to_string());
        expected_wrong.insert("environment".to_string(), "staging".to_string());
        assert!(validate_custom_claims(&jwt_claims, &expected_wrong).is_err());
    }

    #[test]
    fn test_validate_custom_claims_mixed_exact_and_wildcard() {
        // Three claims: aud exact, project_path exact, environment wildcard.
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "project_path": "myorg/myrepo",
            "environment": "app-mr/6",
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("project_path".to_string(), "myorg/myrepo".to_string());
        expected.insert("environment".to_string(), "app*".to_string());
        assert!(validate_custom_claims(&jwt_claims, &expected).is_ok());

        // Same constraint set, but the wildcard claim no longer matches.
        let jwt_claims_fail = serde_json::json!({
            "aud": "my-audience",
            "project_path": "myorg/myrepo",
            "environment": "webapp-staging",
        });
        assert!(validate_custom_claims(&jwt_claims_fail, &expected).is_err());
    }

    // --- validate_oidc_issuer ---

    #[test]
    fn test_validate_oidc_issuer_match() {
        assert!(validate_oidc_issuer(
            "https://accounts.example.com",
            "https://accounts.example.com"
        )
        .is_ok());
    }

    #[test]
    fn test_validate_oidc_issuer_mismatch() {
        let err = validate_oidc_issuer("https://accounts.example.com", "https://evil.example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("OIDC issuer mismatch"), "got: {err}");
    }

    #[test]
    fn test_validate_oidc_issuer_trailing_slash_normalization() {
        assert!(validate_oidc_issuer(
            "https://accounts.example.com",
            "https://accounts.example.com/"
        )
        .is_ok());
        assert!(validate_oidc_issuer(
            "https://accounts.example.com/",
            "https://accounts.example.com"
        )
        .is_ok());
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
            ControllerMatch::Unmatched(detail) => {
                assert!(detail.contains("claims.aud"), "{detail}")
            }
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
            ControllerMatch::Unmatched(detail) => {
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
            ControllerMatch::Unmatched(detail) => {
                assert!(detail.contains("a.example.com"), "{detail}")
            }
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
        assert!(matches!(m, ControllerMatch::Unmatched(_)), "got {m:?}");
    }

    #[test]
    fn test_match_multiple_when_constraints_ambiguous() {
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
        assert!(matches!(m, ControllerMatch::Unmatched(_)));
    }
    fn claims_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_trust_match_none_without_audience_constraint() {
        let claims = claims_map(&[]);
        let candidates = [TrustCandidate {
            label: "a",
            claims: &claims,
        }];
        let token = serde_json::json!({"sub": "anyone", "aud": "anything"});
        let m = match_trust_candidates(&token, &candidates);
        match m {
            TrustMatch::Unmatched(detail) => assert!(detail.contains("claims.aud"), "{detail}"),
            other => panic!("expected Unmatched, got {other:?}"),
        }
    }

    #[test]
    fn test_trust_match_single_with_constraints() {
        let claims = claims_map(&[("aud", "rise"), ("sub", "ctrl-*"), ("scope", "controller")]);
        let candidates = [TrustCandidate {
            label: "a",
            claims: &claims,
        }];
        let token = serde_json::json!({
            "sub": "ctrl-abc",
            "aud": "rise",
            "scope": "controller",
        });
        let m = match_trust_candidates(&token, &candidates);
        assert!(matches!(m, TrustMatch::Single(0)), "got {m:?}");
    }

    #[test]
    fn test_trust_match_audience_array() {
        let claims = claims_map(&[("aud", "rise")]);
        let candidates = [TrustCandidate {
            label: "a",
            claims: &claims,
        }];
        let token = serde_json::json!({"sub": "x", "aud": ["other", "rise"]});
        let m = match_trust_candidates(&token, &candidates);
        assert!(matches!(m, TrustMatch::Single(0)), "got {m:?}");
    }

    #[test]
    fn test_trust_match_audience_array_no_match() {
        let claims = claims_map(&[("aud", "rise")]);
        let candidates = [TrustCandidate {
            label: "a",
            claims: &claims,
        }];
        let token = serde_json::json!({"sub": "x", "aud": ["other"]});
        let m = match_trust_candidates(&token, &candidates);
        match m {
            TrustMatch::Unmatched(detail) => {
                assert!(detail.contains("aud claim does not match"), "got: {detail}")
            }
            other => panic!("expected Unmatched, got {other:?}"),
        }
    }

    #[test]
    fn test_trust_match_none_when_subject_mismatches() {
        let claims = claims_map(&[("aud", "rise"), ("sub", "ctrl-*")]);
        let candidates = [TrustCandidate {
            label: "a",
            claims: &claims,
        }];
        let token = serde_json::json!({"aud": "rise", "sub": "other-bot"});
        let m = match_trust_candidates(&token, &candidates);
        match m {
            TrustMatch::Unmatched(detail) => assert!(detail.contains("\"a\""), "{detail}"),
            other => panic!("expected Unmatched, got {other:?}"),
        }
    }

    #[test]
    fn test_trust_match_none_when_extra_claim_missing() {
        let claims = claims_map(&[("aud", "rise"), ("scope", "controller")]);
        let candidates = [TrustCandidate {
            label: "a",
            claims: &claims,
        }];
        let token = serde_json::json!({"aud": "rise", "sub": "x"});
        let m = match_trust_candidates(&token, &candidates);
        assert!(matches!(m, TrustMatch::Unmatched(_)), "got {m:?}");
    }

    #[test]
    fn test_trust_match_multiple_when_constraints_ambiguous() {
        let claims_a = claims_map(&[("aud", "rise")]);
        let claims_b = claims_map(&[("aud", "rise")]);
        let candidates = [
            TrustCandidate {
                label: "a",
                claims: &claims_a,
            },
            TrustCandidate {
                label: "b",
                claims: &claims_b,
            },
        ];
        let token = serde_json::json!({"aud": "rise", "sub": "x"});
        let m = match_trust_candidates(&token, &candidates);
        match m {
            TrustMatch::Multiple(indices) => assert_eq!(indices, vec![0, 1]),
            other => panic!("expected Multiple, got {other:?}"),
        }
    }

    #[test]
    fn test_trust_match_disambiguates_by_audience() {
        let claims_a = claims_map(&[("aud", "rise-a")]);
        let claims_b = claims_map(&[("aud", "rise-b")]);
        let candidates = [
            TrustCandidate {
                label: "a",
                claims: &claims_a,
            },
            TrustCandidate {
                label: "b",
                claims: &claims_b,
            },
        ];
        let token = serde_json::json!({"sub": "x", "aud": "rise-b"});
        let m = match_trust_candidates(&token, &candidates);
        assert!(matches!(m, TrustMatch::Single(1)), "got {m:?}");
    }

    #[test]
    fn test_trust_match_disambiguates_by_subject() {
        let claims_a = claims_map(&[("aud", "rise"), ("sub", "ctrl-a")]);
        let claims_b = claims_map(&[("aud", "rise"), ("sub", "ctrl-b")]);
        let candidates = [
            TrustCandidate {
                label: "a",
                claims: &claims_a,
            },
            TrustCandidate {
                label: "b",
                claims: &claims_b,
            },
        ];
        let token = serde_json::json!({"aud": "rise", "sub": "ctrl-b"});
        let m = match_trust_candidates(&token, &candidates);
        assert!(matches!(m, TrustMatch::Single(1)), "got {m:?}");
    }

    #[test]
    fn test_trust_match_empty_candidates() {
        let m = match_trust_candidates(&serde_json::json!({}), &[]);
        assert!(matches!(m, TrustMatch::Unmatched(_)));
    }
}
