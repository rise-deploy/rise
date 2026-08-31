use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

pub fn normalize_backend_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// The container runtime engine behind the CLI command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRuntime {
    Docker,
    Podman,
}

/// Container CLI identity, carrying the command to invoke and the detected runtime.
///
/// Handles the case where `docker` is a Podman alias (e.g. podman-docker package)
/// by inspecting version command output during construction.
#[derive(Debug, Clone)]
pub struct ContainerCli {
    command: String,
    runtime: ContainerRuntime,
    buildx_supports_push: bool,
}

impl ContainerCli {
    /// Build a `ContainerCli` from an explicitly provided command name.
    ///
    /// Detects the runtime by inspecting the binary name first, then falling
    /// back to checking version command output (handles `docker` → Podman aliases).
    pub fn from_command(command: impl Into<String>) -> Self {
        let command = command.into();
        let runtime = detect_runtime(&command);
        let buildx_supports_push = detect_buildx_push_support(&command);
        Self {
            command,
            runtime,
            buildx_supports_push,
        }
    }

    /// The CLI command to invoke (e.g. `"docker"` or `"podman"`).
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The detected container runtime engine.
    pub fn runtime(&self) -> ContainerRuntime {
        self.runtime
    }

    /// Whether this CLI frontend likely supports `buildx build --push`.
    pub fn buildx_supports_push(&self) -> bool {
        self.buildx_supports_push
    }
}

/// Detect which container runtime a CLI command is backed by.
fn detect_runtime(command: &str) -> ContainerRuntime {
    // Fast path: binary name is literally "podman"
    if command_file_name(command) == Some("podman") {
        return ContainerRuntime::Podman;
    }

    // Slow path: e.g. `docker` might be a Podman alias (podman-docker package)
    probe_runtime(command).unwrap_or(ContainerRuntime::Docker)
}

/// Return the file name component of a command path.
fn command_file_name(command: &str) -> Option<&str> {
    use std::path::Path;
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
}

/// Heuristic for buildx `--push` support:
/// treat Podman frontends as unsupported, everything else as supported.
fn detect_buildx_push_support(command: &str) -> bool {
    !command.to_lowercase().contains("podman")
}

/// Parse runtime from version command output.
///
/// Combines stdout and stderr because wrappers may emit identifying text to either stream.
fn runtime_from_version_output(stdout: &[u8], stderr: &[u8]) -> ContainerRuntime {
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    if combined.to_lowercase().contains("podman") {
        ContainerRuntime::Podman
    } else {
        ContainerRuntime::Docker
    }
}

/// Probe runtime by executing `<command> version` and falling back to `<command> --version`.
///
/// `version` can include both client and server info, which detects the case
/// where the Docker CLI talks to a Podman server (e.g. Docker CLI connected to
/// a Podman backend in a VM). If that probe fails (for example because Docker
/// daemon is down), we fall back to `--version` so CLI presence is still
/// detected.
///
/// Returns `None` if command execution fails or exits non-zero.
fn probe_runtime(command: &str) -> Option<ContainerRuntime> {
    use std::process::Command;

    for args in &[&["version"][..], &["--version"][..]] {
        let output = Command::new(command).args(*args).output().ok()?;
        if output.status.success() {
            return Some(runtime_from_version_output(&output.stdout, &output.stderr));
        }
    }

    None
}

// TODO: Use keyring crate for secure token storage instead of plain JSON
// This would store tokens in the system's secure credential storage:
// - macOS: Keychain
// - Linux: Secret Service API / libsecret
// - Windows: Credential Manager

/// Only ASCII letters, digits, `-` and `_` are allowed in a profile name — it
/// is used verbatim as a file name under `~/.config/rise/profiles/`.
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("Profile name cannot be empty");
    }
    if name.len() > 63 {
        anyhow::bail!("Profile name '{}' is too long (max 63 characters)", name);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "Invalid profile name '{}': only ASCII letters, digits, '-' and '_' are allowed",
            name
        );
    }
    Ok(())
}

/// Create `dir` with `0700` permissions on Unix if it doesn't already exist.
fn ensure_config_dir(dir: &std::path::Path) -> Result<()> {
    if !dir.exists() {
        #[cfg(unix)]
        {
            // Create parent directories with default permissions
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent).context("Failed to create config parent directory")?;
            }
            // Create the target directory with 0700 atomically
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .context("Failed to create config directory")?;
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(dir).context("Failed to create config directory")?;
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    pub token: Option<String>,
    pub backend_url: Option<String>,
    pub container_cli: Option<String>,
    pub managed_buildkit: Option<bool>,
}

/// Process-wide override for the active profile, set at most once by `main()`
/// from an explicit `--profile` flag. The outer `Option` tracks whether an
/// override was set at all (unset = no `--profile` flag was given, so
/// `RISE_PROFILE` should be consulted instead); the inner `Option` is the
/// resolved profile itself (`None` = the default profile, i.e. `--profile
/// default`).
///
/// Using a `OnceLock` here — rather than round-tripping through
/// `std::env::set_var`/`remove_var` — avoids mutating the process
/// environment after the async runtime's worker threads are running, which
/// is unsound if anything else reads the environment concurrently.
static PROFILE_OVERRIDE: OnceLock<Option<String>> = OnceLock::new();

/// Set the process-wide profile override. Must be called at most once, before
/// any other profile resolution — `main()` does this immediately after
/// parsing CLI args, when `--profile` was passed.
pub fn set_profile_override(profile: Option<String>) {
    let _ = PROFILE_OVERRIDE.set(profile);
}

impl Config {
    /// The active login profile, i.e. the one selected via `--profile` /
    /// `RISE_PROFILE` for the lifetime of this process, or `None` for the
    /// default profile.
    ///
    /// `--profile` is resolved once in `main()` into [`set_profile_override`],
    /// so every independent config load in the process — not just the one in
    /// `main()` — agrees on the same active profile. Absent that override,
    /// falls back to the `RISE_PROFILE` environment variable.
    pub fn active_profile() -> Result<Option<String>> {
        if let Some(overridden) = PROFILE_OVERRIDE.get() {
            return Ok(overridden.clone());
        }
        #[cfg(not(test))]
        if let Ok(val) = std::env::var("RISE_PROFILE") {
            let trimmed = val.trim();
            if trimmed.is_empty() || trimmed == "default" {
                return Ok(None);
            }
            validate_profile_name(trimmed)?;
            return Ok(Some(trimmed.to_string()));
        }
        Ok(None)
    }

    /// The active profile's name for display purposes (`"default"` when unset).
    pub fn active_profile_label() -> Result<String> {
        Ok(Self::active_profile()?.unwrap_or_else(|| "default".to_string()))
    }

    /// List the names of all registered non-default profiles, i.e. every
    /// profile a `rise login --profile <name>` has ever saved.
    pub fn list_profiles() -> Result<Vec<String>> {
        let home = dirs::home_dir().context("Failed to get home directory")?;
        let profiles_dir = home.join(".config").join("rise").join("profiles");

        let mut names = Vec::new();
        if profiles_dir.exists() {
            for entry in fs::read_dir(&profiles_dir).context("Failed to read profiles directory")? {
                let entry = entry.context("Failed to read profile directory entry")?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Remove a profile's saved configuration file. `"default"` removes the
    /// base `config.json`.
    pub fn remove_profile(name: &str) -> Result<()> {
        let path = if name == "default" {
            Self::path_for(None)?
        } else {
            Self::path_for(Some(name))?
        };

        if !path.exists() {
            anyhow::bail!("Profile '{}' does not exist", name);
        }

        fs::remove_file(&path).context("Failed to remove profile config file")?;
        Ok(())
    }

    /// The config file path for a given profile (`None` = default profile).
    pub fn path_for(profile: Option<&str>) -> Result<PathBuf> {
        let home = dirs::home_dir().context("Failed to get home directory")?;
        let config_dir = home.join(".config").join("rise");
        ensure_config_dir(&config_dir)?;

        match profile {
            None => Ok(config_dir.join("config.json")),
            Some(name) => {
                validate_profile_name(name)?;
                let profiles_dir = config_dir.join("profiles");
                ensure_config_dir(&profiles_dir)?;
                Ok(profiles_dir.join(format!("{name}.json")))
            }
        }
    }

    /// Get the path to the active profile's config file
    pub fn config_path() -> Result<PathBuf> {
        Self::path_for(Self::active_profile()?.as_deref())
    }

    /// Load the active profile's configuration from disk
    pub fn load() -> Result<Self> {
        Self::load_named(Self::active_profile()?.as_deref())
    }

    /// Load a specific profile's configuration from disk, independent of the
    /// active profile. Used to inspect other profiles (e.g. `rise profile list`)
    /// without switching the active one.
    pub fn load_named(profile: Option<&str>) -> Result<Self> {
        let config_path = Self::path_for(profile)?;

        if !config_path.exists() {
            return Ok(Config::default());
        }

        let contents = fs::read_to_string(&config_path).context("Failed to read config file")?;

        let config: Config =
            serde_json::from_str(&contents).context("Failed to parse config file")?;

        Ok(config)
    }

    /// Save configuration to disk
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path()?;
        Self::write_config_file(&config_path, self)
    }

    /// Write configuration to a specific path with restrictive permissions on Unix
    fn write_config_file(config_path: &std::path::Path, config: &Config) -> Result<()> {
        let json = serde_json::to_string_pretty(config).context("Failed to serialize config")?;

        // On Unix, create/write the file with 0600 permissions atomically
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(config_path)
                .context("Failed to create config file")?;
            file.write_all(json.as_bytes())
                .context("Failed to write config file")?;
        }

        #[cfg(not(unix))]
        {
            fs::write(config_path, json).context("Failed to write config file")?;
        }

        Ok(())
    }

    /// Set the authentication token
    pub fn set_token(&mut self, token: String) -> Result<()> {
        self.token = Some(token);
        self.save()
    }

    /// The token persisted in the config file (ignores RISE_TOKEN env).
    ///
    /// Token *source* selection (RISE_TOKEN, RISE_TOKEN_COMMAND, GitHub Actions
    /// OIDC, then this stored token) lives in [`crate::cli::token_source`].
    pub fn stored_token(&self) -> Option<String> {
        self.token.clone()
    }

    /// Set the backend URL
    pub fn set_backend_url(&mut self, url: String) -> Result<()> {
        self.backend_url = Some(normalize_backend_url(&url));
        self.save()
    }

    /// Get the backend URL (with default fallback)
    /// Checks RISE_URL environment variable first, then falls back to config file, then to default
    pub fn get_backend_url(&self) -> String {
        #[cfg(not(test))]
        if let Ok(url) = std::env::var("RISE_URL") {
            return normalize_backend_url(&url);
        }
        self.backend_url
            .as_deref()
            .map(normalize_backend_url)
            .unwrap_or_else(|| "http://localhost:3000".to_string())
    }

    /// Set the container CLI
    #[allow(dead_code)]
    pub fn set_container_cli(&mut self, cli: String) -> Result<()> {
        self.container_cli = Some(cli);
        self.save()
    }

    /// Get the container CLI to use (docker or podman)
    /// Checks RISE_CONTAINER_CLI environment variable first, then falls back to config file,
    /// then to auto-detection (podman if available, docker otherwise)
    pub fn get_container_cli(&self) -> ContainerCli {
        #[cfg(not(test))]
        if let Ok(cli) = std::env::var("RISE_CONTAINER_CLI") {
            return ContainerCli::from_command(cli);
        }
        if let Some(ref cli) = self.container_cli {
            return ContainerCli::from_command(cli.clone());
        }
        detect_container_cli()
    }

    /// Get whether to use managed BuildKit daemon
    /// Checks RISE_MANAGED_BUILDKIT environment variable first, then falls back to config file
    /// Returns false by default (opt-in feature)
    #[allow(dead_code)]
    pub fn get_managed_buildkit(&self) -> bool {
        #[cfg(not(test))]
        if let Some(val) = crate::build::parse_bool_env_var("RISE_MANAGED_BUILDKIT") {
            return val;
        }
        self.managed_buildkit.unwrap_or(false)
    }

    /// Set whether to use managed BuildKit daemon
    #[allow(dead_code)]
    pub fn set_managed_buildkit(&mut self, enabled: bool) -> Result<()> {
        self.managed_buildkit = Some(enabled);
        self.save()
    }
}

/// Auto-detect which container CLI is available.
///
/// Checks `docker` first, then `podman`. Also detects the case where
/// `docker` is a Podman alias (e.g. podman-docker package) by inspecting
/// version command output — the same probe that checks availability.
fn detect_container_cli() -> ContainerCli {
    // Check if docker is available (and whether it's secretly Podman)
    if let Some(runtime) = probe_runtime("docker") {
        return ContainerCli {
            command: "docker".to_string(),
            runtime,
            buildx_supports_push: detect_buildx_push_support("docker"),
        };
    }

    // Check if podman is available
    if probe_runtime("podman").is_some() {
        return ContainerCli {
            command: "podman".to_string(),
            runtime: ContainerRuntime::Podman,
            buildx_supports_push: detect_buildx_push_support("podman"),
        };
    }

    // Default to docker if neither is detected
    ContainerCli {
        command: "docker".to_string(),
        runtime: ContainerRuntime::Docker,
        buildx_supports_push: detect_buildx_push_support("docker"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(overrides: impl FnOnce(&mut Config)) -> Config {
        let mut c = Config::default();
        overrides(&mut c);
        c
    }

    #[test]
    fn test_backend_url_default() {
        assert_eq!(Config::default().get_backend_url(), "http://localhost:3000");
    }

    #[test]
    fn test_backend_url_from_config() {
        let c = config(|c| c.backend_url = Some("https://api.example.com".to_string()));
        assert_eq!(c.get_backend_url(), "https://api.example.com");
    }

    #[test]
    fn test_backend_url_trailing_slash_is_trimmed() {
        let c = config(|c| c.backend_url = Some("https://api.example.com/".to_string()));
        assert_eq!(c.get_backend_url(), "https://api.example.com");
    }

    #[test]
    fn test_normalize_backend_url_trims_multiple_trailing_slashes() {
        assert_eq!(
            normalize_backend_url("https://api.example.com///"),
            "https://api.example.com"
        );
    }

    #[test]
    fn test_token_none_by_default() {
        assert_eq!(Config::default().stored_token(), None);
    }

    #[test]
    fn test_token_from_config() {
        let c = config(|c| c.token = Some("config-token".to_string()));
        assert_eq!(c.stored_token(), Some("config-token".to_string()));
    }

    #[test]
    fn test_active_profile_is_default_in_tests() {
        // RISE_PROFILE reads are disabled under #[cfg(test)] (like the other
        // env-checking getters), so this only exercises the "unset" path.
        assert_eq!(Config::active_profile().unwrap(), None);
        assert_eq!(Config::active_profile_label().unwrap(), "default");
    }

    #[test]
    fn test_validate_profile_name_accepts_alnum_dash_underscore() {
        assert!(validate_profile_name("work").is_ok());
        assert!(validate_profile_name("work-2").is_ok());
        assert!(validate_profile_name("work_2").is_ok());
        assert!(validate_profile_name("default").is_ok());
    }

    #[test]
    fn test_validate_profile_name_rejects_empty() {
        assert!(validate_profile_name("").is_err());
    }

    #[test]
    fn test_validate_profile_name_rejects_path_separators() {
        assert!(validate_profile_name("../escape").is_err());
        assert!(validate_profile_name("a/b").is_err());
    }

    #[test]
    fn test_validate_profile_name_rejects_too_long() {
        let long_name = "a".repeat(64);
        assert!(validate_profile_name(&long_name).is_err());
        let ok_name = "a".repeat(63);
        assert!(validate_profile_name(&ok_name).is_ok());
    }

    #[test]
    fn test_managed_buildkit_default_false() {
        assert!(!Config::default().get_managed_buildkit());
    }

    #[test]
    fn test_managed_buildkit_from_config() {
        let c = config(|c| c.managed_buildkit = Some(true));
        assert!(c.get_managed_buildkit());

        let c = config(|c| c.managed_buildkit = Some(false));
        assert!(!c.get_managed_buildkit());
    }

    #[test]
    fn test_runtime_from_version_output_docker_sample() {
        // Sample Docker output:
        // Docker version 27.3.1, build ce12230
        let runtime = runtime_from_version_output(b"Docker version 27.3.1, build ce12230\n", b"");
        assert_eq!(runtime, ContainerRuntime::Docker);
    }

    #[test]
    fn test_runtime_from_version_output_podman_sample_stdout() {
        // Sample Podman output:
        // podman version 5.0.2
        let runtime = runtime_from_version_output(b"podman version 5.0.2\n", b"");
        assert_eq!(runtime, ContainerRuntime::Podman);
    }

    #[test]
    fn test_runtime_from_version_output_podman_sample_stderr() {
        // Sample podman-docker wrapper behavior (identity text on stderr):
        // Emulate Docker CLI using podman. Create /etc/containers/nodocker to quiet msg.
        let runtime = runtime_from_version_output(
            b"Docker version 5.0.2\n",
            b"Emulate Docker CLI using podman. Create /etc/containers/nodocker to quiet msg.\n",
        );
        assert_eq!(runtime, ContainerRuntime::Podman);
    }

    #[test]
    fn test_runtime_from_version_output_docker_cli_podman_server() {
        // Docker CLI connected to a Podman server (e.g. via VM).
        // `docker version` output contains "Podman Engine:" in server section.
        let stdout = b"Client:\n Version: 29.2.1\n\nServer: linux/arm64/fedora-43\n Podman Engine:\n  Version: 5.7.1\n";
        let runtime = runtime_from_version_output(stdout, b"");
        assert_eq!(runtime, ContainerRuntime::Podman);
    }

    #[test]
    fn test_command_file_name_extracts_binary_name() {
        assert_eq!(command_file_name("podman"), Some("podman"));
        assert_eq!(command_file_name("/usr/bin/podman"), Some("podman"));
        assert_eq!(command_file_name("/usr/local/bin/docker"), Some("docker"));
    }

    #[cfg(unix)]
    #[test]
    fn test_write_config_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp_dir = tempfile::tempdir().unwrap();
        let config_path = tmp_dir.path().join("config.json");

        let c = Config {
            token: Some("secret-token".to_string()),
            ..Config::default()
        };

        // Exercise the actual write_config_file() implementation
        Config::write_config_file(&config_path, &c).unwrap();

        let metadata = fs::metadata(&config_path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "Config file should have 0600 permissions, got {:o}",
            mode
        );

        // Verify the content is valid JSON and round-trips correctly
        let contents = fs::read_to_string(&config_path).unwrap();
        let loaded: Config = serde_json::from_str(&contents).unwrap();
        assert_eq!(loaded.token, Some("secret-token".to_string()));
    }
}
