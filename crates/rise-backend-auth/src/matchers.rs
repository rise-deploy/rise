//! Pure claim-matching helpers.
//!
//! These functions operate over already-verified claims and perform no I/O.
//! They back controller and service-account trust-policy matching in
//! rise-deploy.

use std::collections::{BTreeMap, HashMap};

use crate::error::AuthError;

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
/// Pure helper: mandatory `aud` via [`audience_matches`], remaining claims via
/// [`validate_custom_claims`]. Operates over borrowed candidates rather than a
/// concrete config type, so it backs any trust-policy-shaped resource
/// (Controller, ServiceAccount) without this crate depending on either.
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

#[cfg(test)]
mod tests {
    use super::*;

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
