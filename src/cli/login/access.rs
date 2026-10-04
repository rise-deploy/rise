//! What a login asks for (ADR-0006): `--scope` flags turned into the access
//! request the backend validates, and the access a session ended up with,
//! read back from `/users/me` (the approver of a device login may have
//! changed the request).

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};

/// The presets the backend knows. Anything else in a `--scope` value is
/// passed on as an individual permission for the backend to validate.
const PRESETS: [&str; 4] = ["read", "deploy", "develop", "admin"];

/// The access request for `--scope` flags, or `None` for full access.
///
/// Each flag is `<project>[/<environment>]=<preset|permission>[,...]`, e.g.
/// `my-app/staging=develop` or `my-app=view,logs`.
pub fn request_from_scopes(scopes: &[String], full_access: bool) -> Result<Option<Value>> {
    if full_access {
        return Ok(Some(json!({"kind": "full"})));
    }
    if scopes.is_empty() {
        return Ok(None);
    }
    let grants = scopes
        .iter()
        .map(|scope| parse_scope(scope))
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(json!({"kind": "restricted", "grants": grants})))
}

fn parse_scope(scope: &str) -> Result<Value> {
    let Some((target, values)) = scope.split_once('=') else {
        bail!("--scope '{scope}' needs a value, e.g. '{scope}=read'");
    };
    let (project, environment) = match target.split_once('/') {
        Some((project, environment)) => (project, Some(environment)),
        None => (target, None),
    };
    if project.is_empty() || environment.is_some_and(str::is_empty) {
        bail!("--scope '{scope}': name a project, or a project and environment as 'project/env'");
    }
    let mut preset = None;
    let mut permissions = Vec::new();
    for value in values.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if PRESETS.contains(&value) {
            if preset.replace(value).is_some() {
                bail!("--scope '{scope}' names more than one preset");
            }
        } else {
            permissions.push(value);
        }
    }
    if preset.is_none() && permissions.is_empty() {
        bail!("--scope '{scope}' names no preset or permission");
    }
    let mut grant = json!({"project": project});
    if let Some(environment) = environment {
        grant["environment"] = json!(environment);
    }
    if let Some(preset) = preset {
        grant["preset"] = json!(preset);
    }
    if !permissions.is_empty() {
        grant["permissions"] = json!(permissions);
    }
    Ok(grant)
}

/// Whether a stored session token carries no ceiling, i.e. has full access.
/// Unreadable tokens count as not full: no warning is better than a wrong one.
pub fn token_has_full_access(token: &str) -> bool {
    super::token_utils::decode_jwt_claims(token)
        .map(|claims| claims.get("authorization_details").is_none())
        .unwrap_or(false)
}

/// Say, before a login starts, that it will replace a full-access session with
/// a restricted one.
pub fn warn_if_narrowing(backend_url: &str, request: Option<&Value>) {
    let restricted = request.is_some_and(|r| r["kind"] == "restricted");
    let stored_full = crate::config::Config::token_for_url(backend_url)
        .ok()
        .flatten()
        .is_some_and(|token| token_has_full_access(&token));
    if restricted && stored_full {
        println!(
            "Note: this replaces your current full-access login for {backend_url} on this machine."
        );
    }
}

/// Print the access the new session holds, as `/users/me` reports it.
pub async fn print_granted_access(http_client: &Client, backend_url: &str, token: &str) {
    if let Some(access) = fetch_granted_access(http_client, backend_url, token).await {
        print_access(&access);
    }
}

/// The access the session holding `token` was granted, or `None` when the
/// backend cannot say (a failure here never fails the login).
pub async fn fetch_granted_access(
    http_client: &Client,
    backend_url: &str,
    token: &str,
) -> Option<Value> {
    match fetch_access(http_client, backend_url, token).await {
        Ok(access) => Some(access),
        Err(e) => {
            tracing::debug!("Failed to read the session's access: {e:#}");
            None
        }
    }
}

pub fn print_access(access: &Value) {
    for line in describe_access(access) {
        println!("{line}");
    }
}

/// `access` as the CLI success page's `access` parameter: `full`, or one
/// `<target>: <access>` line per grant.
pub fn access_page_param(access: &Value) -> String {
    match grant_rows(access) {
        None => "full".to_string(),
        Some(rows) => rows
            .iter()
            .map(|(target, what)| format!("{target}: {what}"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

async fn fetch_access(http_client: &Client, backend_url: &str, token: &str) -> Result<Value> {
    let response = http_client
        .get(format!("{backend_url}/api/v1/users/me"))
        .bearer_auth(token)
        .send()
        .await
        .context("Failed to reach /users/me")?
        .error_for_status()
        .context("/users/me refused the new session")?;
    let me: Value = response.json().await.context("Failed to parse /users/me")?;
    me.get("access")
        .cloned()
        .context("/users/me reports no access (older backend)")
}

/// One `(target, access)` row per grant, or `None` for full access.
fn grant_rows(access: &Value) -> Option<Vec<(String, String)>> {
    if access["kind"] != "restricted" {
        return None;
    }
    let grants = access["grants"].as_array().cloned().unwrap_or_default();
    let rows = grants
        .iter()
        .map(|grant| {
            let project = grant["project"].as_str().unwrap_or("?");
            let target = match grant["environment"].as_str() {
                Some(environment) => format!("{project}/{environment}"),
                None => project.to_string(),
            };
            let what = match grant["preset"].as_str() {
                Some(preset) => preset.to_string(),
                None => grant["permissions"]
                    .as_array()
                    .map(|p| {
                        p.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default(),
            };
            (target, what)
        })
        .collect();
    Some(rows)
}

fn describe_access(access: &Value) -> Vec<String> {
    let Some(rows) = grant_rows(access) else {
        return vec!["  Access: full".to_string()];
    };
    let mut lines = vec!["  Access: restricted".to_string()];
    if rows.is_empty() {
        lines.push("    (nothing)".to_string());
    }
    let width = rows
        .iter()
        .map(|(target, _)| target.len())
        .max()
        .unwrap_or(0);
    for (target, what) in &rows {
        lines.push(format!("    {target:width$}  {what}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_become_grants() {
        let request = request_from_scopes(
            &[
                "my-app/staging=develop".to_string(),
                "my-app=view, logs".to_string(),
            ],
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            request,
            json!({"kind": "restricted", "grants": [
                {"project": "my-app", "environment": "staging", "preset": "develop"},
                {"project": "my-app", "permissions": ["view", "logs"]},
            ]})
        );
    }

    #[test]
    fn no_scope_is_full_access_and_bad_scopes_fail() {
        assert_eq!(request_from_scopes(&[], false).unwrap(), None);
        assert_eq!(
            request_from_scopes(&[], true).unwrap(),
            Some(json!({"kind": "full"}))
        );
        for bad in [
            "my-app",
            "my-app=",
            "/staging=read",
            "my-app/=read",
            "a=read,admin",
        ] {
            assert!(
                request_from_scopes(&[bad.to_string()], false).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn access_is_described_per_target() {
        let lines = describe_access(&json!({"kind": "restricted", "grants": [
            {"project": "my-app", "permissions": ["view", "logs"]},
            {"project": "my-app", "environment": "staging", "preset": "develop"},
        ]}));
        assert_eq!(
            lines,
            vec![
                "  Access: restricted",
                "    my-app          view, logs",
                "    my-app/staging  develop",
            ]
        );
        assert_eq!(
            describe_access(&json!({"kind": "full"})),
            vec!["  Access: full"]
        );
    }

    #[test]
    fn access_page_param_lists_grants() {
        assert_eq!(
            access_page_param(&json!({"kind": "restricted", "grants": [
                {"project": "my-app", "permissions": ["view", "logs"]},
                {"project": "my-app", "environment": "staging", "preset": "deploy"},
            ]})),
            "my-app: view, logs\nmy-app/staging: deploy"
        );
        assert_eq!(access_page_param(&json!({"kind": "full"})), "full");
    }
}
