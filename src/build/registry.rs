// Container registry operations (push and login)

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;
use tracing::{debug, info};

/// Push image to container registry
pub(crate) fn docker_push(container_cli: &str, image_tag: &str) -> Result<()> {
    info!("Pushing image to registry: {}", image_tag);

    let mut cmd = Command::new(container_cli);
    cmd.arg("push").arg(image_tag);

    debug!("Executing command: {:?}", cmd);

    let status = cmd
        .status()
        .with_context(|| format!("Failed to execute {} push", container_cli))?;

    if !status.success() {
        bail!("{} push failed with status: {}", container_cli, status);
    }

    Ok(())
}

/// Pull image from a registry
pub(crate) fn docker_pull(container_cli: &str, image: &str, platform: &str) -> Result<()> {
    info!("Pulling image: {} (platform: {})", image, platform);

    let mut cmd = Command::new(container_cli);
    cmd.arg("pull").arg("--platform").arg(platform).arg(image);

    debug!("Executing command: {:?}", cmd);

    let status = cmd
        .status()
        .with_context(|| format!("Failed to execute {} pull", container_cli))?;

    if !status.success() {
        bail!("{} pull failed with status: {}", container_cli, status);
    }

    Ok(())
}

/// Inspect a locally-tagged image and return its sha256 digest as it lives in
/// the registry after a `docker push`. Reads `.RepoDigests[]` and returns the
/// `sha256:...` portion for the entry whose ref matches `image_tag` (ignoring
/// the tag suffix). When no entry matches (local image carries digests only
/// from prior pushes to unrelated repos), falls back to `buildx imagetools`
/// rather than picking an arbitrary digest — picking the wrong one would
/// deploy unrelated content under a trusted digest.
pub(crate) fn inspect_push_digest(container_cli: &str, image_tag: &str) -> Result<Option<String>> {
    let output = Command::new(container_cli)
        .arg("inspect")
        .arg("--format")
        .arg("{{json .RepoDigests}}")
        .arg(image_tag)
        .output()
        .with_context(|| format!("Failed to execute {container_cli} inspect"))?;

    // Fall back to buildx imagetools when the image isn't in the local daemon's
    // image store — happens when buildx pushed directly from a remote/container
    // driver (railpack, pack) without loading locally.
    if !output.status.success() {
        return inspect_push_digest_via_buildx(container_cli, image_tag).map(Some);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let repo_digests: Vec<String> = serde_json::from_str(stdout.trim())
        .with_context(|| format!("Failed to parse RepoDigests JSON: {stdout}"))?;

    // Strip tag part of image_tag for matching: `host/repo/app:tag` → `host/repo/app`.
    let image_repo = image_tag
        .rsplit_once(':')
        .map(|(repo, _)| repo)
        .unwrap_or(image_tag);

    let matching = repo_digests
        .iter()
        .find(|d| d.starts_with(&format!("{image_repo}@")));

    if let Some(d) = matching {
        return Ok(d.rsplit_once('@').map(|(_, digest)| digest.to_string()));
    }

    // No RepoDigests entry matches the pushed repo — fall back to buildx
    // imagetools so we never pin an unrelated repo's digest from the local
    // image store.
    inspect_push_digest_via_buildx(container_cli, image_tag).map(Some)
}

fn inspect_push_digest_via_buildx(container_cli: &str, image_tag: &str) -> Result<String> {
    // `--raw` returns the raw manifest bytes; the digest is `sha256:` of
    // those bytes. This is what the registry itself stores under the tag,
    // and it's stable across docker/buildx CLI versions unlike the
    // human-readable `Digest:` line in the default output.
    let output = Command::new(container_cli)
        .arg("buildx")
        .arg("imagetools")
        .arg("inspect")
        .arg("--raw")
        .arg(image_tag)
        .output()
        .with_context(|| format!("Failed to execute {container_cli} buildx imagetools inspect"))?;

    if !output.status.success() {
        bail!(
            "{container_cli} buildx imagetools inspect failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&output.stdout);
    let bytes = hasher.finalize();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("sha256:{hex}"))
}

/// Tag a container image
pub(crate) fn docker_tag(container_cli: &str, source: &str, target: &str) -> Result<()> {
    info!("Tagging image: {} -> {}", source, target);

    let mut cmd = Command::new(container_cli);
    cmd.arg("tag").arg(source).arg(target);

    debug!("Executing command: {:?}", cmd);

    let status = cmd
        .status()
        .with_context(|| format!("Failed to execute {} tag", container_cli))?;

    if !status.success() {
        bail!("{} tag failed with status: {}", container_cli, status);
    }

    Ok(())
}

/// Inject a bearer token directly into the container CLI's auth config file.
///
/// Writes `{"auths":{"<registry>":{"registrytoken":"<token>"}}}` into the appropriate
/// config file for Docker (`~/.docker/config.json`) or Podman
/// (`$REGISTRY_AUTH_FILE` / `$XDG_RUNTIME_DIR/containers/auth.json` /
/// `~/.config/containers/auth.json`).
///
/// This is used for GitLab scoped JWTs, which cannot be applied via `docker login`
/// because the login command makes its own auth handshake that rejects pre-obtained tokens.
pub(crate) fn inject_registry_auth(container_cli: &str, registry: &str, token: &str) -> Result<()> {
    let auth_file = resolve_auth_file_path(container_cli)?;
    debug!(
        "Injecting registry auth for {} into {}",
        registry,
        auth_file.display()
    );

    // Create parent directory if it doesn't exist
    if let Some(parent) = auth_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }

    // Read existing config or start fresh
    let mut config: serde_json::Value = if auth_file.exists() {
        let content = std::fs::read_to_string(&auth_file)
            .with_context(|| format!("Failed to read {}", auth_file.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {} as JSON", auth_file.display()))?
    } else {
        serde_json::json!({})
    };

    // Set the registrytoken entry
    config["auths"][registry]["registrytoken"] = serde_json::Value::String(token.to_string());

    // Write atomically via a temp file in the same directory
    let tmp_path = auth_file.with_extension("tmp");
    std::fs::write(&tmp_path, serde_json::to_string_pretty(&config)?)
        .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &auth_file)
        .with_context(|| format!("Failed to update {}", auth_file.display()))?;

    Ok(())
}

/// Resolve the auth config file path for the given container CLI.
fn resolve_auth_file_path(container_cli: &str) -> Result<PathBuf> {
    if container_cli.contains("podman") {
        // Podman credential lookup order
        if let Ok(path) = std::env::var("REGISTRY_AUTH_FILE") {
            return Ok(PathBuf::from(path));
        }
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return Ok(PathBuf::from(runtime_dir).join("containers/auth.json"));
        }
        let home = home_dir().context("Could not determine home directory for Podman auth file")?;
        Ok(home.join(".config/containers/auth.json"))
    } else {
        // Docker credential lookup order
        if let Ok(docker_config) = std::env::var("DOCKER_CONFIG") {
            return Ok(PathBuf::from(docker_config).join("config.json"));
        }
        let home = home_dir().context("Could not determine home directory for Docker config")?;
        Ok(home.join(".docker/config.json"))
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Login to container registry
pub(crate) fn docker_login(
    container_cli: &str,
    registry: &str,
    username: &str,
    password: &str,
) -> Result<()> {
    debug!(
        "Executing: {} login {} --username {} --password-stdin",
        container_cli, registry, username
    );

    let status = Command::new(container_cli)
        .arg("login")
        .arg(registry)
        .arg("--username")
        .arg(username)
        .arg("--password-stdin")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(password.as_bytes())?;
            }
            child.wait()
        })
        .with_context(|| format!("Failed to execute {} login", container_cli))?;

    if !status.success() {
        bail!("{} login failed with status: {}", container_cli, status);
    }

    Ok(())
}
