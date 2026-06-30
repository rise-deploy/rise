//! Small wrappers around the `vault` CLI used by `rise vault` and the
//! deploy-time JFrog auto-mint flow.
//!
//! Shell out rather than HTTP-direct: the CLI handles `~/.vault-token`, the
//! OIDC browser flow, and method-specific arg formats for free. The backend
//! (`server::registry::providers::jfrog`) talks HTTP because it runs unattended;
//! here we have a human session, so reusing the CLI is the simpler choice.

use anyhow::{anyhow, bail, Context, Result};
use std::process::{Command, Stdio};

const VAULT_BIN: &str = "vault";

/// Ensure the local Vault token is valid; trigger `vault login -method=oidc`
/// if not. `addr` is exported as `VAULT_ADDR` so callers don't have to.
pub fn ensure_session(addr: &str, auth_path: &str, auth_role: &str) -> Result<()> {
    if session_valid(addr)? {
        tracing::debug!("Vault session is valid, skipping login");
        return Ok(());
    }

    tracing::info!(
        "Vault session missing or expired, running `vault login -method=oidc -path={}` (this will open a browser)",
        auth_path,
    );

    let status = Command::new(VAULT_BIN)
        .env("VAULT_ADDR", addr)
        .arg("login")
        .arg("-method=oidc")
        .arg(format!("-path={auth_path}"))
        .arg(format!("role={auth_role}"))
        .status()
        .with_context(|| {
            format!(
                "Failed to spawn `{VAULT_BIN} login` — is the vault CLI on $PATH? (mise install)"
            )
        })?;

    if !status.success() {
        bail!(
            "`{VAULT_BIN} login` exited with status {} — check the browser flow completed",
            status
        );
    }

    if !session_valid(addr)? {
        bail!("`{VAULT_BIN} login` reported success but the session is still invalid");
    }
    Ok(())
}

/// Cheap idempotent check — `vault token lookup` succeeds iff there's a valid
/// `~/.vault-token` and the server accepts it. We discard the body; the exit
/// code is the signal.
fn session_valid(addr: &str) -> Result<bool> {
    let output = Command::new(VAULT_BIN)
        .env("VAULT_ADDR", addr)
        .arg("token")
        .arg("lookup")
        .arg("-format=json")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("Failed to spawn `{VAULT_BIN} token lookup`"))?;
    Ok(output.status.success())
}

/// Return the email of the user backing the current Vault token. Looks at
/// `data.meta.email` first (OIDC providers usually set this), falling back to
/// `data.display_name` with the configured `oidc-<auth_path>-` prefix stripped.
pub fn identity(addr: &str, auth_path: &str) -> Result<String> {
    let output = Command::new(VAULT_BIN)
        .env("VAULT_ADDR", addr)
        .arg("token")
        .arg("lookup")
        .arg("-format=json")
        .output()
        .with_context(|| format!("Failed to spawn `{VAULT_BIN} token lookup`"))?;
    if !output.status.success() {
        bail!(
            "`{VAULT_BIN} token lookup` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_identity(&output.stdout, auth_path)
}

/// Mint a JFrog access token from Vault. Returns the secret token string.
pub fn read_artifactory_token(addr: &str, path: &str) -> Result<String> {
    let output = Command::new(VAULT_BIN)
        .env("VAULT_ADDR", addr)
        .arg("read")
        .arg("-field=access_token")
        .arg(path)
        .output()
        .with_context(|| format!("Failed to spawn `{VAULT_BIN} read {path}`"))?;
    if !output.status.success() {
        bail!(
            "`{VAULT_BIN} read {path}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let token = String::from_utf8(output.stdout)
        .context("vault token is not valid UTF-8")?
        .trim()
        .to_string();
    if token.is_empty() {
        bail!("`{VAULT_BIN} read {path}` returned an empty token");
    }
    Ok(token)
}

/// Replace `{email}` in `path` with `email`. Returns the path unchanged when
/// no placeholder is present, so callers can use literal paths too.
pub fn substitute_path(path: &str, email: &str) -> String {
    path.replace("{email}", email)
}

/// Extract the user's email from a `vault token lookup -format=json` payload.
/// Public for unit testing.
///
/// `auth_path` is the configured Vault auth path (e.g. `global/entra`). When
/// the payload falls back to `display_name`, Vault formats it as
/// `oidc-{auth_path with `/` → `-`}-{email}`; we strip that exact prefix
/// rather than scanning for the last hyphen, which would mangle emails whose
/// local-part contains a hyphen (`jean-paul@…`).
pub(crate) fn parse_identity(stdout: &[u8], auth_path: &str) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_slice(stdout).context("vault token lookup output is not JSON")?;
    let data = parsed
        .get("data")
        .ok_or_else(|| anyhow!("vault token lookup payload missing `data` object"))?;

    if let Some(email) = data
        .get("meta")
        .and_then(|m| m.get("email"))
        .and_then(|v| v.as_str())
    {
        return Ok(email.to_string());
    }

    if let Some(display) = data.get("display_name").and_then(|v| v.as_str()) {
        let prefix = format!("oidc-{}-", auth_path.trim_matches('/').replace('/', "-"));
        if let Some(rest) = display.strip_prefix(&prefix) {
            if rest.contains('@') {
                return Ok(rest.to_string());
            }
        }
        return Ok(display.to_string());
    }

    bail!("vault token lookup payload has neither `data.meta.email` nor `data.display_name`")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_identity_prefers_meta_email() {
        let payload = br#"{"data": {"meta": {"email": "a@b.com"}, "display_name": "oidc-global-entra-c@b.com"}}"#;
        assert_eq!(parse_identity(payload, "global/entra").unwrap(), "a@b.com");
    }

    #[test]
    fn parse_identity_falls_back_to_display_name_with_prefix_stripped() {
        let payload = br#"{"data": {"display_name": "oidc-global-entra-someone@example.com"}}"#;
        assert_eq!(
            parse_identity(payload, "global/entra").unwrap(),
            "someone@example.com"
        );
    }

    #[test]
    fn parse_identity_preserves_hyphenated_email_local_part() {
        // Previous rfind('-') logic mangled this to `paul@example.com`.
        let payload = br#"{"data": {"display_name": "oidc-global-entra-jean-paul@example.com"}}"#;
        assert_eq!(
            parse_identity(payload, "global/entra").unwrap(),
            "jean-paul@example.com",
        );
    }

    #[test]
    fn parse_identity_returns_display_name_verbatim_when_prefix_does_not_match() {
        // Auth path doesn't match what's in the display_name — leave it alone.
        let payload = br#"{"data": {"display_name": "raw-token"}}"#;
        assert_eq!(
            parse_identity(payload, "global/entra").unwrap(),
            "raw-token"
        );
    }

    #[test]
    fn parse_identity_errors_when_data_missing() {
        let payload = br#"{"errors": ["nope"]}"#;
        assert!(parse_identity(payload, "global/entra").is_err());
    }

    #[test]
    fn substitute_path_replaces_email_placeholder() {
        assert_eq!(
            substitute_path("global/artifactory/user_token/{email}", "x@y.z"),
            "global/artifactory/user_token/x@y.z",
        );
    }

    #[test]
    fn substitute_path_is_a_noop_without_placeholder() {
        assert_eq!(
            substitute_path("global/artifactory/static", "ignored@example.com"),
            "global/artifactory/static",
        );
    }
}
