//! `rise login --device`, driven over HTTP: the CLI's side (start, poll) and
//! the browser's side (look up, approve or deny on Rise's `/device` page, with
//! a real session from [`crate::login::login`]).

use anyhow::{Context, Result};
use base64::Engine as _;

/// A device login the "CLI" started.
pub struct Started {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri_complete: String,
}

/// `POST /auth/authorize {flow: "device"}`.
pub fn start(api_base: &str) -> Result<Started> {
    let resp = crate::http::post_json(
        &format!("{api_base}/api/v1/auth/authorize"),
        None,
        &serde_json::json!({"flow": "device", "client_name": "rise-e2e"}),
    )?;
    anyhow::ensure!(
        resp.status == 200,
        "device authorize returned {}:\n{}",
        resp.status,
        resp.body
    );
    let body: serde_json::Value =
        serde_json::from_str(&resp.body).context("parse device authorize response")?;
    let field = |name: &str| {
        body[name]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("device authorize response has no {name}:\n{body}"))
    };
    Ok(Started {
        device_code: field("device_code")?,
        user_code: field("user_code")?,
        verification_uri_complete: field("verification_uri_complete")?,
    })
}

/// One poll of `POST /auth/device/exchange`: `Ok(token)` or `Err(error code)`.
pub fn poll(api_base: &str, device_code: &str) -> Result<std::result::Result<String, String>> {
    let resp = crate::http::post_json(
        &format!("{api_base}/api/v1/auth/device/exchange"),
        None,
        &serde_json::json!({"device_code": device_code}),
    )?;
    anyhow::ensure!(
        resp.status == 200,
        "device exchange returned {}:\n{}",
        resp.status,
        resp.body
    );
    let body: serde_json::Value =
        serde_json::from_str(&resp.body).context("parse device exchange response")?;
    if let Some(token) = body["token"].as_str() {
        return Ok(Ok(token.to_string()));
    }
    Ok(Err(body["error"]
        .as_str()
        .with_context(|| format!("device exchange answered neither token nor error:\n{body}"))?
        .to_string()))
}

/// `GET /auth/device?user_code=` as the signed-in browser.
pub fn lookup(api_base: &str, session: &str, user_code: &str) -> Result<serde_json::Value> {
    let resp = crate::http::get_auth(
        &format!("{api_base}/api/v1/auth/device?user_code={user_code}"),
        session,
    )?;
    anyhow::ensure!(
        resp.status == 200,
        "device lookup returned {}:\n{}",
        resp.status,
        resp.body
    );
    serde_json::from_str(&resp.body).context("parse device lookup response")
}

/// `POST /auth/device/{approve,deny}` as the signed-in browser.
pub fn decide(api_base: &str, session: &str, user_code: &str, approve: bool) -> Result<()> {
    let action = if approve { "approve" } else { "deny" };
    let resp = crate::http::post_json(
        &format!("{api_base}/api/v1/auth/device/{action}"),
        Some(session),
        &serde_json::json!({"user_code": user_code}),
    )?;
    anyhow::ensure!(
        resp.status == 204,
        "device {action} returned {}:\n{}",
        resp.status,
        resp.body
    );
    Ok(())
}

/// The unverified claims of a JWT, for comparing what two sessions name.
pub fn claims(token: &str) -> Result<serde_json::Value> {
    let payload = token.split('.').nth(1).context("token is not a JWT")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .context("decode JWT payload")?;
    serde_json::from_slice(&bytes).context("parse JWT claims")
}
