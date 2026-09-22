use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ExchangeTokenResponse {
    token: String,
    expires_in: u64,
}

/// Standard in-pod path of the workload-identity bootstrap credential.
const IDENTITY_CREDENTIAL_FILE: &str = "/var/run/secrets/rise/identity/credential";

/// Resolve the bootstrap credential from `--credential`, falling back to the
/// standard credential file mounted into every Rise deployment.
fn resolve_credential(explicit: Option<&str>) -> Result<String> {
    if let Some(c) = explicit {
        return Ok(c.to_string());
    }
    let credential = std::fs::read_to_string(IDENTITY_CREDENTIAL_FILE).with_context(|| {
        format!(
            "No workload identity credential found. Pass --credential, or run this \
             inside a Rise deployment where '{}' is mounted.",
            IDENTITY_CREDENTIAL_FILE
        )
    })?;
    Ok(credential.trim().to_string())
}

/// Request a workload identity token for `audience` and print it to stdout.
///
/// Uses the bootstrap credential (not a user session) and the `RISE_ISSUER`
/// env var injected into every Rise deployment.
pub async fn token_command(
    http_client: &Client,
    audience: &str,
    credential: Option<&str>,
    ttl_seconds: Option<u64>,
) -> Result<()> {
    let credential = resolve_credential(credential)?;

    let issuer = std::env::var("RISE_ISSUER")
        .context("RISE_ISSUER is not set; this command runs inside a Rise deployment")?;
    let url = format!("{}/api/v1/identity/token", issuer.trim_end_matches('/'));

    let mut body = serde_json::json!({ "audience": audience });
    if let Some(ttl) = ttl_seconds {
        body["ttl_seconds"] = serde_json::json!(ttl);
    }

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", credential))
        .json(&body)
        .send()
        .await
        .context("Failed to send token exchange request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!("Token exchange failed (status {}): {}", status, body);
    }

    let token: ExchangeTokenResponse = response
        .json()
        .await
        .context("Failed to parse token exchange response")?;

    eprintln!("expires_in: {}s", token.expires_in);
    println!("{}", token.token);
    Ok(())
}

// ── identity agent ───────────────────────────────────────────────────────────
//
// The workload-identity sidecar the ECS backend runs in every task (ADR-0005
// D8). ECS has no Secret volume and no way to write a file into a running
// container, so the files the other backends deliver from the controller are
// written from inside the task instead, onto a volume shared with the app.
// The agent is deliberately a file writer and nothing more: which audiences to
// mint, their TTL and when to refresh them are all decided by the server.

/// Root of the shared identity volume, as the app sees it.
const IDENTITY_DIR: &str = "/var/run/secrets/rise/identity";
/// Container-local marker (never on the shared volume, so it stays out of the
/// contract the app reads) that the health check keys on.
const AGENT_READY_FILE: &str = "/tmp/rise-identity-agent-ready";

/// Retry delays after a failed token fetch. Capped so a recovering server is
/// noticed within a minute, well inside any sensible token TTL.
const AGENT_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_secs(2);
const AGENT_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Deserialize)]
struct AudienceTokensResponse {
    tokens: std::collections::BTreeMap<String, String>,
    refresh_after_secs: u64,
}

/// Where the agent writes, and where it reports readiness. Overridable so the
/// agent can run outside a task (tests, local debugging).
struct AgentPaths {
    dir: std::path::PathBuf,
    ready_file: std::path::PathBuf,
}

impl AgentPaths {
    fn from_env() -> Self {
        let var = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
                .into()
        };
        Self {
            dir: var("RISE_IDENTITY_DIR", IDENTITY_DIR),
            ready_file: var("RISE_IDENTITY_AGENT_READY_FILE", AGENT_READY_FILE),
        }
    }
}

/// Run the identity agent, or with `check` report whether it is ready.
///
/// Reads the bootstrap credential from `RISE_IDENTITY_CREDENTIAL` and, when the
/// deployment declares `[identity].audiences`, the token endpoint from
/// `RISE_IDENTITY_TOKENS_URL`. Ready means every file the app will look for
/// exists: the credential, plus the tokens when there are any to fetch. Until
/// then the app container does not start.
pub async fn agent_command(http_client: &Client, check: bool) -> Result<()> {
    let paths = AgentPaths::from_env();
    if check {
        anyhow::ensure!(
            paths.ready_file.exists(),
            "identity agent is not ready yet ({} is absent)",
            paths.ready_file.display()
        );
        return Ok(());
    }

    let credential = std::env::var("RISE_IDENTITY_CREDENTIAL")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .context(
            "RISE_IDENTITY_CREDENTIAL is not set; the identity agent runs as a Rise sidecar",
        )?;
    let tokens_url = std::env::var("RISE_IDENTITY_TOKENS_URL")
        .ok()
        .filter(|u| !u.trim().is_empty());

    // A restarted agent must not report ready off a previous run's marker
    // before it has written anything itself.
    let _ = std::fs::remove_file(&paths.ready_file);
    write_credential(&paths.dir, &credential)?;
    tracing::info!(dir = %paths.dir.display(), "Wrote the workload identity credential");

    let run = async {
        match tokens_url {
            Some(url) => refresh_tokens_forever(http_client, &url, &credential, &paths).await,
            None => {
                // No audiences declared: the credential is everything the app
                // gets. Stay up so ECS does not treat the sidecar as failed.
                mark_ready(&paths.ready_file)?;
                std::future::pending::<Result<()>>().await
            }
        }
    };
    tokio::select! {
        result = run => result,
        // PID 1 in its container, where SIGTERM has no default handler: without
        // this the task would wait out ECS's stop timeout on every shutdown.
        _ = terminate_signal() => {
            tracing::info!("Identity agent shutting down");
            Ok(())
        }
    }
}

async fn terminate_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(e) => {
                tracing::warn!("Failed to install the SIGTERM handler: {:?}", e);
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Fetch the deployment's audience tokens, write them, and repeat when the
/// server says they are due — forever, retrying failures with backoff. A failed
/// refresh leaves the previous tokens in place: they stay valid for the second
/// half of their lifetime, which is the margin the refresh schedule exists for.
async fn refresh_tokens_forever(
    http_client: &Client,
    url: &str,
    credential: &str,
    paths: &AgentPaths,
) -> Result<()> {
    let mut retry = AGENT_RETRY_INITIAL;
    let mut ready = false;
    loop {
        let wait = match fetch_audience_tokens(http_client, url, credential).await {
            Ok(response) => {
                write_tokens(&paths.dir, &response.tokens)?;
                if !ready {
                    mark_ready(&paths.ready_file)?;
                    ready = true;
                }
                tracing::info!(
                    count = response.tokens.len(),
                    refresh_after_secs = response.refresh_after_secs,
                    "Wrote workload identity tokens"
                );
                retry = AGENT_RETRY_INITIAL;
                std::time::Duration::from_secs(response.refresh_after_secs.max(1))
            }
            Err(e) => {
                tracing::warn!(
                    retry_in_secs = retry.as_secs(),
                    "Failed to fetch workload identity tokens: {:?}",
                    e
                );
                let wait = retry;
                retry = (retry * 2).min(AGENT_RETRY_MAX);
                wait
            }
        };
        tokio::time::sleep(wait).await;
    }
}

async fn fetch_audience_tokens(
    http_client: &Client,
    url: &str,
    credential: &str,
) -> Result<AudienceTokensResponse> {
    let response = http_client
        .post(url)
        .header("Authorization", format!("Bearer {}", credential))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("Failed to reach {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("{url} answered {status}: {body}");
    }
    response
        .json()
        .await
        .context("Failed to parse the audience-tokens response")
}

fn write_credential(dir: &std::path::Path, credential: &str) -> Result<()> {
    ensure_dir(dir)?;
    write_file_atomically(dir, "credential", credential.as_bytes())
}

fn write_tokens(
    dir: &std::path::Path,
    tokens: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let tokens_dir = dir.join("tokens");
    ensure_dir(&tokens_dir)?;
    for (filename, jwt) in tokens {
        // The server filters these already; they become paths, so check again.
        if !is_safe_token_filename(filename) {
            tracing::warn!(filename = %filename, "Skipping a token with an unsafe filename");
            continue;
        }
        write_file_atomically(&tokens_dir, filename, jwt.as_bytes())?;
    }
    Ok(())
}

/// Mirrors `rise_backend_core::identity::is_safe_token_filename`, which a CLI-only
/// build cannot link.
fn is_safe_token_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Create `dir` world-traversable: the app may run as any UID.
fn ensure_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("Failed to set permissions on {}", dir.display()))?;
    }
    Ok(())
}

/// Replace `dir/name` in one rename, so the app never reads a half-written token.
/// Mode `0o444`, matching the other backends, so any UID can read it.
fn write_file_atomically(dir: &std::path::Path, name: &str, contents: &[u8]) -> Result<()> {
    let target = dir.join(name);
    let staging = dir.join(format!(".{name}.tmp"));
    // A read-only leftover from an interrupted write would fail the create.
    let _ = std::fs::remove_file(&staging);
    std::fs::write(&staging, contents)
        .with_context(|| format!("Failed to write {}", staging.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o444))
            .with_context(|| format!("Failed to set permissions on {}", staging.display()))?;
    }
    std::fs::rename(&staging, &target)
        .with_context(|| format!("Failed to move {} into place", target.display()))
}

fn mark_ready(ready_file: &std::path::Path) -> Result<()> {
    std::fs::write(ready_file, b"ready\n")
        .with_context(|| format!("Failed to write {}", ready_file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_written_read_only_and_replaced_atomically() {
        let dir = tempfile::tempdir().unwrap();
        write_credential(dir.path(), "cred-1").unwrap();
        // A second write must replace a read-only file rather than fail on it.
        write_credential(dir.path(), "cred-2").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("credential")).unwrap(),
            "cred-2"
        );

        let mut tokens = std::collections::BTreeMap::new();
        tokens.insert("e2e".to_string(), "jwt-1".to_string());
        write_tokens(dir.path(), &tokens).unwrap();
        tokens.insert("e2e".to_string(), "jwt-2".to_string());
        write_tokens(dir.path(), &tokens).unwrap();
        let token = dir.path().join("tokens").join("e2e");
        assert_eq!(std::fs::read_to_string(&token).unwrap(), "jwt-2");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode(&token) & 0o777, 0o444);
            assert_eq!(mode(&dir.path().join("credential")) & 0o777, 0o444);
            assert_eq!(mode(&dir.path().join("tokens")) & 0o777, 0o755);
        }
        // No staging file is left behind for the app to trip over.
        assert!(!dir.path().join("tokens").join(".e2e.tmp").exists());
    }

    #[test]
    fn a_token_filename_that_would_escape_the_directory_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut tokens = std::collections::BTreeMap::new();
        tokens.insert("../credential".to_string(), "evil".to_string());
        tokens.insert("ok".to_string(), "jwt".to_string());
        write_tokens(dir.path(), &tokens).unwrap();
        assert!(!dir.path().join("credential").exists());
        assert!(dir.path().join("tokens").join("ok").exists());
    }
}
