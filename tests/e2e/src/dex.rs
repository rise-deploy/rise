//! Obtaining an external OIDC token from the test Dex non-interactively, via the
//! OAuth2 resource-owner password grant. Used by the SA token-exchange scenario:
//! the token is the *proof* that the caller may assume the named service account.

use anyhow::{Context, Result};

/// How to reach the test Dex and what issuer it stamps.
#[derive(Clone, Debug)]
pub struct DexEndpoint {
    /// Token endpoint reachable from the harness host (e.g. `http://127.0.0.1:5556/dex/token`).
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    /// The `iss` Dex bakes into tokens (e.g. `http://rise-dex:5556/dex`) — also
    /// what the SA must be registered to trust and where the backend fetches JWKS.
    pub issuer: String,
}

/// Mint an OIDC `id_token` for `username`/`password` via the password grant.
pub fn mint_password_token(dex: &DexEndpoint, username: &str, password: &str) -> Result<String> {
    let resp = crate::http::client()
        .post(&dex.token_url)
        .form(&[
            ("grant_type", "password"),
            ("client_id", dex.client_id.as_str()),
            ("client_secret", dex.client_secret.as_str()),
            ("scope", "openid email profile"),
            ("username", username),
            ("password", password),
        ])
        .send()
        .with_context(|| format!("POST {} (password grant)", dex.token_url))?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Dex token endpoint returned {status}: {body}");
    }
    let json: serde_json::Value =
        serde_json::from_str(&body).context("parse Dex token response as JSON")?;
    json["id_token"]
        .as_str()
        .map(str::to_string)
        .context("Dex token response had no id_token")
}
