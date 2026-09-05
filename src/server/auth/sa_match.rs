//! Shared service-account matching.
//!
//! The token-exchange endpoint and the legacy per-request `resolve_for_project`
//! must agree byte-for-byte on how a verified external token maps to a service
//! account. This module is the single home for that pure matching logic,
//! operating over already-fetched service account rows; the DB fetch and the
//! error-shape mapping stay with each caller. Controller-token rejection is a
//! separate, earlier check the callers run against live trust-policy
//! resources (`crate::server::auth::controller::resolve_external`), since it
//! now requires a store lookup rather than pure matching against config.

use std::collections::HashMap;

use rise_backend_auth::{validate_custom_claims, AuthError};
use uuid::Uuid;

use crate::db::models::ServiceAccount;

/// Why a verified external token did not resolve to exactly one service account.
#[derive(Debug)]
pub enum SaMatchError {
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
/// Matches the token's claims against each candidate SA's expected claims
/// (glob `*` supported), failing closed on malformed SA config, and requires
/// exactly one match.
///
/// `service_accounts` are the rows already fetched for `(project_id, issuer)`;
/// passing an empty slice yields `NoMatch { had_candidates: false, .. }`.
pub fn match_service_account<'a>(
    token_claims: &serde_json::Value,
    service_accounts: &'a [ServiceAccount],
) -> Result<&'a ServiceAccount, SaMatchError> {
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

    #[test]
    fn single_match_returns_sa() {
        let token = json!({ "sub": "deploy-bot" });
        let sas = vec![sa(json!({ "sub": "deploy-bot" }))];
        let matched = match_service_account(&token, &sas).expect("should match");
        assert_eq!(matched.id, sas[0].id);
    }

    #[test]
    fn no_match_distinguishes_empty_from_mismatch() {
        let token = json!({ "sub": "deploy-bot" });

        // Empty candidate set.
        let err = match_service_account(&token, &[]).unwrap_err();
        assert!(matches!(
            err,
            SaMatchError::NoMatch {
                had_candidates: false,
                ..
            }
        ));

        // Candidates exist but none match.
        let sas = vec![sa(json!({ "sub": "other" }))];
        let err = match_service_account(&token, &sas).unwrap_err();
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
        let err = match_service_account(&token, &sas).unwrap_err();
        assert!(matches!(err, SaMatchError::Ambiguous(ids) if ids.len() == 2));
    }

    #[test]
    fn malformed_claims_fail_closed() {
        let token = json!({ "sub": "12345" });
        // Non-string claim value is invalid for HashMap<String, String>.
        let sas = vec![sa(json!({ "sub": 12345 }))];
        let err = match_service_account(&token, &sas).unwrap_err();
        assert!(matches!(err, SaMatchError::MalformedClaims(_)));
    }
}
