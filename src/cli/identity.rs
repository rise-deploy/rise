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

    let token = exchange(http_client, &url, &credential, audience, ttl_seconds).await?;

    eprintln!("expires_in: {}s", token.expires_in);
    println!("{}", token.token);
    Ok(())
}

/// Exchange the bootstrap credential at the token endpoint `url` for one token.
async fn exchange(
    http_client: &Client,
    url: &str,
    credential: &str,
    audience: &str,
    ttl_seconds: Option<u64>,
) -> Result<ExchangeTokenResponse> {
    let mut body = serde_json::json!({ "audience": audience });
    if let Some(ttl) = ttl_seconds {
        body["ttl_seconds"] = serde_json::json!(ttl);
    }

    let response = http_client
        .post(url)
        .header("Authorization", format!("Bearer {}", credential))
        .timeout(std::time::Duration::from_secs(30))
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

    response
        .json()
        .await
        .context("Failed to parse token exchange response")
}

// ── identity agent ───────────────────────────────────────────────────────────
//
// The workload-identity sidecar the ECS backend runs in every task (ADR-0005
// D8). ECS has no Secret volume and no way to write a file into a running
// container, so the files the other backends deliver from the controller are
// written from inside the task instead, onto a volume shared with the app. The
// tokens come from the same exchange endpoint `rise identity token` uses, at
// the lifetime the other backends give the token files.

/// Root of the shared identity volume, as the app sees it.
const IDENTITY_DIR: &str = "/var/run/secrets/rise/identity";
/// Container-local marker (never on the shared volume, so it stays out of the
/// contract the app reads) that the health check keys on.
const AGENT_READY_FILE: &str = "/tmp/rise-identity-agent-ready";

/// Retry delays after a failed token fetch. Capped so a recovering server is
/// noticed within a minute, well inside any sensible token TTL.
const AGENT_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_secs(2);
const AGENT_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// What the agent fetches, read from the environment the controller sets.
struct AgentConfig {
    credential: String,
    /// The exchange endpoint. `None` when the deployment declares no audiences:
    /// the credential is then all the app gets.
    token_url: Option<String>,
    /// `[identity].audiences`: token filename → audience.
    audiences: std::collections::BTreeMap<String, String>,
    /// The lifetime to ask for; the server caps it either way.
    ttl_seconds: Option<u64>,
}

impl AgentConfig {
    fn from_env() -> Result<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let credential = var("RISE_IDENTITY_CREDENTIAL")
            .map(|c| c.trim().to_string())
            .context(
                "RISE_IDENTITY_CREDENTIAL is not set; the identity agent runs as a Rise sidecar",
            )?;
        let audiences = match var("RISE_IDENTITY_AUDIENCES") {
            Some(raw) => serde_json::from_str(&raw)
                .context("RISE_IDENTITY_AUDIENCES is not a JSON object of filename → audience")?,
            None => Default::default(),
        };
        let ttl_seconds = var("RISE_IDENTITY_TOKEN_TTL_SECONDS")
            .map(|t| t.parse())
            .transpose()
            .context("RISE_IDENTITY_TOKEN_TTL_SECONDS is not a number of seconds")?;
        Ok(Self {
            credential,
            token_url: var("RISE_IDENTITY_TOKEN_URL"),
            audiences,
            ttl_seconds,
        })
    }
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
/// Ready means every file the app will look for exists: the credential, plus a
/// token per declared audience. Until then the app container does not start.
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

    let config = AgentConfig::from_env()?;
    // A restarted agent must not report ready off a previous run's marker
    // before it has written anything itself.
    let _ = std::fs::remove_file(&paths.ready_file);
    write_credential(&paths.dir, &config.credential)?;
    tracing::info!(dir = %paths.dir.display(), "Wrote the workload identity credential");

    let run = async {
        match (&config.token_url, config.audiences.is_empty()) {
            (Some(url), false) => refresh_tokens_forever(http_client, url, &config, &paths).await,
            _ => {
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

/// Mint a token per declared audience, write them, and repeat at half their
/// lifetime — forever, retrying failures with backoff. A failed refresh leaves
/// the previous tokens in place: they stay valid for the second half of their
/// lifetime, which is the margin the refresh schedule exists for.
async fn refresh_tokens_forever(
    http_client: &Client,
    url: &str,
    config: &AgentConfig,
    paths: &AgentPaths,
) -> Result<()> {
    let mut retry = AGENT_RETRY_INITIAL;
    let mut ready = false;
    loop {
        let wait = match fetch_tokens(http_client, url, config).await {
            Ok((tokens, expires_in)) => {
                write_tokens(&paths.dir, &tokens)?;
                if !ready {
                    mark_ready(&paths.ready_file)?;
                    ready = true;
                }
                retry = AGENT_RETRY_INITIAL;
                let wait = jittered(refresh_after(expires_in));
                tracing::info!(
                    count = tokens.len(),
                    expires_in_secs = expires_in,
                    refresh_in_secs = wait.as_secs(),
                    "Wrote workload identity tokens"
                );
                wait
            }
            Err(e) => {
                let wait = jittered(retry);
                tracing::warn!(
                    retry_in_secs = wait.as_secs(),
                    "Failed to fetch workload identity tokens: {:?}",
                    e
                );
                retry = (retry * 2).min(AGENT_RETRY_MAX);
                wait
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// One token per declared audience, keyed by filename, and the shortest
/// lifetime among them. All or nothing: a partial set is retried whole, so the
/// app never starts with some token files missing.
async fn fetch_tokens(
    http_client: &Client,
    url: &str,
    config: &AgentConfig,
) -> Result<(std::collections::BTreeMap<String, String>, u64)> {
    let mut tokens = std::collections::BTreeMap::new();
    let mut expires_in = u64::MAX;
    for (filename, audience) in &config.audiences {
        let token = exchange(
            http_client,
            url,
            &config.credential,
            audience,
            config.ttl_seconds,
        )
        .await
        .with_context(|| format!("audience {audience:?} (token file {filename:?})"))?;
        expires_in = expires_in.min(token.expires_in);
        tokens.insert(filename.clone(), token.token);
    }
    Ok((tokens, expires_in))
}

/// Re-mint at half the lifetime, the policy every backend follows
/// (`rise_backend_core::token_ttl::remint_after_secs`, which a CLI-only build
/// cannot link).
fn refresh_after(expires_in_secs: u64) -> std::time::Duration {
    std::time::Duration::from_secs((expires_in_secs / 2).max(1))
}

/// Up to 20% earlier than `d`, at random. Every task of a deployment starts at
/// once; without this their refreshes, and their retries against a rate
/// limit, would stay in lockstep for the life of the deployment.
fn jittered(d: std::time::Duration) -> std::time::Duration {
    d.mul_f64(1.0 - rand::random::<f64>() * 0.2)
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
        // Validated at deploy time; checked again because these become paths.
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
    fn tokens_refresh_at_half_their_lifetime_jittered_earlier_never_later() {
        assert_eq!(refresh_after(3600).as_secs(), 1800);
        assert_eq!(refresh_after(1).as_secs(), 1, "never a busy loop");
        for _ in 0..100 {
            let d = jittered(std::time::Duration::from_secs(1800));
            assert!(
                d.as_secs_f64() <= 1800.0 && d.as_secs_f64() >= 1440.0,
                "{d:?}"
            );
        }
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
