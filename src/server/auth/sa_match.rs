//! Shared service-account / controller matching.
//!
//! The token-exchange endpoint and the legacy per-request `resolve_for_project`
//! must agree byte-for-byte on how a verified external token maps to a service
//! account (or is rejected as a controller token). This module is the single
//! home for that pure matching logic, operating over already-fetched service
//! account rows; the DB fetch and the error-shape mapping stay with each caller.

use std::collections::HashMap;

use rise_backend_auth::{
    match_controller_identity, validate_custom_claims, AuthError, ControllerIdentity,
    ControllerMatch,
};
use uuid::Uuid;

use crate::db::models::ServiceAccount;

/// Why a verified external token did not resolve to exactly one service account.
#[derive(Debug)]
pub enum SaMatchError {
    /// The token matched a configured controller identity — controller tokens
    /// are not service accounts.
    ControllerToken,
    /// The token matched multiple controller identities (ambiguous config).
    AmbiguousController,
    /// One of the candidate service accounts has malformed (non string-valued)
    /// claims configuration. Carries the offending SA id.
    MalformedClaims(Uuid),
    /// No service account matched. `had_candidates` distinguishes "no SAs
    /// configured for this issuer" from "SAs exist but none matched the claims";
    /// `last_error` carries the most recent claim-mismatch detail.
    NoMatch {
        had_candidates: bool,
        last_error: Option<AuthError>,
    },
    /// More than one service account matched (ambiguous claim configuration).
    /// Carries the matched SA ids for diagnostics.
    Ambiguous(Vec<Uuid>),
}

/// Match a verified external token against a project's service accounts.
///
/// Mirrors the resolution order of the legacy two-phase path:
/// 1. Reject controller tokens up front (a controller for this issuer must not
///    act as a service account).
/// 2. Match the token's claims against each candidate SA's expected claims
///    (glob `*` supported), failing closed on malformed SA config.
/// 3. Require exactly one match.
///
/// `service_accounts` are the rows already fetched for `(project_id, issuer)`;
/// passing an empty slice yields `NoMatch { had_candidates: false, .. }`.
pub fn match_service_account<'a>(
    token_claims: &serde_json::Value,
    issuer: &str,
    service_accounts: &'a [ServiceAccount],
    controllers_by_issuer: &HashMap<String, Vec<ControllerIdentity>>,
) -> Result<&'a ServiceAccount, SaMatchError> {
    // 1. Controller tokens are not service accounts.
    if let Some(candidates) = controllers_by_issuer.get(issuer) {
        match match_controller_identity(token_claims, candidates) {
            ControllerMatch::Single(_) => return Err(SaMatchError::ControllerToken),
            ControllerMatch::Multiple(_) => return Err(SaMatchError::AmbiguousController),
            ControllerMatch::Unmatched(_) => {}
        }
    }

    // 2. Match claims against each candidate SA.
    let mut matching: Vec<&ServiceAccount> = Vec::new();
    let mut last_error = None;
    for sa in service_accounts {
        let expected: HashMap<String, String> = match serde_json::from_value(sa.claims.clone()) {
            Ok(claims) => claims,
            Err(_) => return Err(SaMatchError::MalformedClaims(sa.id)),
        };
        match validate_custom_claims(token_claims, &expected) {
            Ok(()) => matching.push(sa),
            Err(e) => last_error = Some(e),
        }
    }

    // 3. Exactly one match.
    match matching.len() {
        0 => Err(SaMatchError::NoMatch {
            had_candidates: !service_accounts.is_empty(),
            last_error,
        }),
        1 => Ok(matching[0]),
        _ => Err(SaMatchError::Ambiguous(
            matching.iter().map(|sa| sa.id).collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn sa(claims: serde_json::Value) -> ServiceAccount {
        ServiceAccount {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            issuer_url: "https://gitlab.com".to_string(),
            claims,
            sequence: 0,
            allowed_environment_ids: None,
            deleted_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn controllers(
        issuer: &str,
        claims: serde_json::Value,
    ) -> HashMap<String, Vec<ControllerIdentity>> {
        let expected: HashMap<String, String> = serde_json::from_value(claims).unwrap();
        HashMap::from([(
            issuer.to_string(),
            vec![ControllerIdentity {
                id: "controller.example.com".to_string(),
                issuer: issuer.to_string(),
                claims: expected,
            }],
        )])
    }

    #[test]
    fn single_match_returns_sa() {
        let token = json!({ "sub": "deploy-bot" });
        let sas = vec![sa(json!({ "sub": "deploy-bot" }))];
        let matched = match_service_account(&token, "https://gitlab.com", &sas, &HashMap::new())
            .expect("should match");
        assert_eq!(matched.id, sas[0].id);
    }

    #[test]
    fn no_match_distinguishes_empty_from_mismatch() {
        let token = json!({ "sub": "deploy-bot" });

        // Empty candidate set.
        let err =
            match_service_account(&token, "https://gitlab.com", &[], &HashMap::new()).unwrap_err();
        assert!(matches!(
            err,
            SaMatchError::NoMatch {
                had_candidates: false,
                ..
            }
        ));

        // Candidates exist but none match.
        let sas = vec![sa(json!({ "sub": "other" }))];
        let err =
            match_service_account(&token, "https://gitlab.com", &sas, &HashMap::new()).unwrap_err();
        assert!(matches!(
            err,
            SaMatchError::NoMatch {
                had_candidates: true,
                last_error: Some(_)
            }
        ));
    }

    #[test]
    fn ambiguous_when_multiple_match() {
        let token = json!({ "sub": "deploy-bot" });
        // Two SAs with empty claims both match everything.
        let sas = vec![sa(json!({})), sa(json!({}))];
        let err =
            match_service_account(&token, "https://gitlab.com", &sas, &HashMap::new()).unwrap_err();
        assert!(matches!(err, SaMatchError::Ambiguous(ids) if ids.len() == 2));
    }

    #[test]
    fn controller_token_rejected_before_sa_match() {
        let token = json!({ "aud": "rise-controller", "sub": "deploy-bot" });
        // An SA that would otherwise match.
        let sas = vec![sa(json!({ "sub": "deploy-bot" }))];
        let controllers = controllers(
            "https://gitlab.com",
            json!({ "aud": "rise-controller", "sub": "deploy-bot" }),
        );
        let err =
            match_service_account(&token, "https://gitlab.com", &sas, &controllers).unwrap_err();
        assert!(matches!(err, SaMatchError::ControllerToken));
    }

    #[test]
    fn same_issuer_different_audience_is_not_a_controller() {
        // The token does not satisfy the controller's `aud` constraint, so it is
        // free to match a service account on the same issuer.
        let token = json!({ "aud": "rise-service-account", "sub": "deploy-bot" });
        let sas = vec![sa(
            json!({ "aud": "rise-service-account", "sub": "deploy-bot" }),
        )];
        let controllers = controllers(
            "https://gitlab.com",
            json!({ "aud": "rise-controller", "sub": "deploy-bot" }),
        );
        let matched =
            match_service_account(&token, "https://gitlab.com", &sas, &controllers).unwrap();
        assert_eq!(matched.id, sas[0].id);
    }

    #[test]
    fn malformed_claims_fail_closed() {
        let token = json!({ "sub": "12345" });
        // Non-string claim value is invalid for HashMap<String, String>.
        let sas = vec![sa(json!({ "sub": 12345 }))];
        let err =
            match_service_account(&token, "https://gitlab.com", &sas, &HashMap::new()).unwrap_err();
        assert!(matches!(err, SaMatchError::MalformedClaims(_)));
    }
}
