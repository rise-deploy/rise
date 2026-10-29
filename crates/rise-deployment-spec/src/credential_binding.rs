//! Stable bindings between an OAuth credential and its upstream configuration.

use serde_json::{json, Value};

/// Includes the effective endpoint, including endpoints obtained by discovery.
/// Exact string equality is deliberate: configuration edits require resealing.
pub fn oauth_credential_binding(spec: &Value, token_endpoint: &str) -> String {
    json!([
        "rise-oauth-client-v2",
        spec.get("client_id"),
        spec.get("issuer_url"),
        spec.get("authorization_endpoint"),
        spec.get("token_endpoint"),
        spec.get("token_endpoint_auth_method")
            .and_then(Value::as_str)
            .unwrap_or("client_secret_post"),
        token_endpoint,
    ])
    .to_string()
}
