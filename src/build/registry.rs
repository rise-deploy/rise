// Container registry operations (push and login)

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;
use tracing::{debug, info};

/// Push image to container registry
pub(crate) fn docker_push(container_cli: &str, image_tag: &str) -> Result<()> {
    docker_push_with_output(container_cli, image_tag, super::CommandOutput::Inherit)
}

pub(crate) fn docker_push_with_output(
    container_cli: &str,
    image_tag: &str,
    output: super::CommandOutput,
) -> Result<()> {
    info!("Pushing image to registry: {}", image_tag);

    let mut cmd = Command::new(container_cli);
    cmd.arg("push").arg(image_tag);

    debug!("Executing command: {}", super::command_for_log(&cmd));

    let status = super::run_command(
        cmd,
        &format!("Failed to execute {} push", container_cli),
        output,
    )?;

    if !status.success() {
        bail!("{} push failed with status: {}", container_cli, status);
    }

    Ok(())
}

/// Pull image from a registry
pub(crate) fn docker_pull_with_output(
    container_cli: &str,
    image: &str,
    platform: &str,
    output: super::CommandOutput,
) -> Result<()> {
    info!("Pulling image: {} (platform: {})", image, platform);

    let mut cmd = Command::new(container_cli);
    cmd.arg("pull").arg("--platform").arg(platform).arg(image);

    debug!("Executing command: {}", super::command_for_log(&cmd));

    let status = super::run_command(
        cmd,
        &format!("Failed to execute {} pull", container_cli),
        output,
    )?;

    if !status.success() {
        bail!("{} pull failed with status: {}", container_cli, status);
    }

    Ok(())
}

/// Tag a container image
pub(crate) fn docker_tag_with_output(
    container_cli: &str,
    source: &str,
    target: &str,
    output: super::CommandOutput,
) -> Result<()> {
    info!("Tagging image: {} -> {}", source, target);

    let mut cmd = Command::new(container_cli);
    cmd.arg("tag").arg(source).arg(target);

    debug!("Executing command: {}", super::command_for_log(&cmd));

    let status = super::run_command(
        cmd,
        &format!("Failed to execute {} tag", container_cli),
        output,
    )?;

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

    write_private_file(
        &auth_file,
        serde_json::to_string_pretty(&config)?.as_bytes(),
    )
}

/// Atomically replace `path` with `contents`, readable by the owner only.
///
/// The auth file holds every registry credential the container CLI stores, so it is
/// written `0600` regardless of the mode the previous file had. A symlinked auth file
/// is updated at its target, leaving the link in place.
fn write_private_file(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;

    let path = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path)
            .with_context(|| format!("Failed to resolve symlink {}", path.display()))?,
        _ => path.to_path_buf(),
    };
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    // NamedTempFile is created 0600 on Unix; persist renames it over the destination.
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("Failed to create temporary file in {}", dir.display()))?;
    file.write_all(contents)
        .with_context(|| format!("Failed to write {}", file.path().display()))?;
    file.as_file()
        .sync_all()
        .with_context(|| format!("Failed to flush {}", file.path().display()))?;
    file.persist(&path)
        .with_context(|| format!("Failed to update {}", path.display()))?;
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
pub(crate) fn docker_login_with_output(
    container_cli: &str,
    registry: &str,
    username: &str,
    password: &str,
    output: super::CommandOutput,
) -> Result<()> {
    debug!(
        "Executing: {} login {} --username {} --password-stdin",
        container_cli, registry, username
    );

    let mut command = Command::new(container_cli);
    command
        .arg("login")
        .arg(registry)
        .arg("--username")
        .arg(username)
        .arg("--password-stdin");
    let status = super::run_command_with_stdin(
        command,
        password.as_bytes(),
        &format!("Failed to execute {} login", container_cli),
        output,
    )?;

    if !status.success() {
        bail!("{} login failed with status: {}", container_cli, status);
    }

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn private_file_replaces_world_readable_file_with_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private_file(&path, br#"{"auths":{}}"#).unwrap();

        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"auths":{}}"#);
    }

    #[test]
    fn private_file_creates_new_file_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        write_private_file(&path, b"{}").unwrap();

        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn private_file_updates_symlink_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.json");
        let link = dir.path().join("config.json");
        std::fs::write(&target, "{}").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_private_file(&link, b"{\"a\":1}").unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"a\":1}");
        assert_eq!(mode(&target), 0o600);
    }
}
