use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

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

/// Default Vault OIDC mount path for `vault login -path=...`. Override per-laptop
/// via `rise vault configure --auth-path ...` or `$RISE_VAULT_AUTH_PATH`.
pub const DEFAULT_VAULT_AUTH_PATH: &str = "global/entra";
/// Default Vault OIDC role passed as `role=<...>` to `vault login`.
pub const DEFAULT_VAULT_AUTH_ROLE: &str = "developer";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    pub token: Option<String>,
    pub backend_url: Option<String>,
    pub container_cli: Option<String>,
    pub managed_buildkit: Option<bool>,
    /// Vault address (e.g. `https://vault.example.com:8200`). Persisted so
    /// `rise deploy` can auto-mint JFrog tokens without prompting per-invocation.
    pub vault_address: Option<String>,
    /// Vault OIDC auth mount path used by `rise vault login`. Defaults to
    /// `DEFAULT_VAULT_AUTH_PATH` when unset.
    pub vault_auth_path: Option<String>,
    /// Vault OIDC role passed as `role=...` to `vault login`. Defaults to
    /// `DEFAULT_VAULT_AUTH_ROLE` when unset.
    pub vault_auth_role: Option<String>,
    /// Vault KV/secret path that yields a JFrog access token. Supports the
    /// `{email}` placeholder, substituted with the authenticated user's email
    /// at runtime. When set together with `vault_address`, `rise deploy`
    /// auto-mints a fresh JFrog token via this path before each client-
    /// controlled push.
    pub vault_artifactory_token_path: Option<String>,
}

impl Config {
    /// Get the path to the config file
    pub fn config_path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("Failed to get home directory")?;

        let config_dir = home.join(".config").join("rise");

        // Create directory if it doesn't exist, with restrictive permissions on Unix
        if !config_dir.exists() {
            #[cfg(unix)]
            {
                // Create parent directories with default permissions
                if let Some(parent) = config_dir.parent() {
                    fs::create_dir_all(parent)
                        .context("Failed to create config parent directory")?;
                }
                // Create the rise config directory with 0700 atomically
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&config_dir)
                    .context("Failed to create config directory")?;
            }
            #[cfg(not(unix))]
            {
                fs::create_dir_all(&config_dir).context("Failed to create config directory")?;
            }
        }

        Ok(config_dir.join("config.json"))
    }

    /// Load configuration from disk
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;

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

    /// Get the authentication token
    /// Checks RISE_TOKEN environment variable first, then falls back to config file
    pub fn get_token(&self) -> Option<String> {
        #[cfg(not(test))]
        if let Ok(token) = std::env::var("RISE_TOKEN") {
            crate::login::token_utils::log_token_debug(&token, "RISE_TOKEN environment variable");
            return Some(token);
        }

        if let Some(token) = self.token.as_deref() {
            crate::login::token_utils::log_token_debug(token, "~/.config/rise/config.json");
        }

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

    /// Persist the Vault address (idempotent).
    pub fn set_vault_address(&mut self, addr: String) -> Result<()> {
        self.vault_address = Some(addr.trim_end_matches('/').to_string());
        self.save()
    }

    /// Read the Vault address. Env var `RISE_VAULT_ADDR` wins over the config
    /// file. Returns `None` when neither is set — caller should error and tell
    /// the user to run `rise vault configure --address <url>`.
    pub fn get_vault_address(&self) -> Option<String> {
        #[cfg(not(test))]
        if let Ok(addr) = std::env::var("RISE_VAULT_ADDR") {
            return Some(addr.trim_end_matches('/').to_string());
        }
        self.vault_address.clone()
    }

    /// Persist the Vault OIDC auth path.
    pub fn set_vault_auth_path(&mut self, path: String) -> Result<()> {
        self.vault_auth_path = Some(path);
        self.save()
    }

    /// Read the Vault OIDC auth path with env override + default fallback.
    pub fn get_vault_auth_path(&self) -> String {
        #[cfg(not(test))]
        if let Ok(path) = std::env::var("RISE_VAULT_AUTH_PATH") {
            return path;
        }
        self.vault_auth_path
            .clone()
            .unwrap_or_else(|| DEFAULT_VAULT_AUTH_PATH.to_string())
    }

    /// Persist the Vault OIDC role.
    pub fn set_vault_auth_role(&mut self, role: String) -> Result<()> {
        self.vault_auth_role = Some(role);
        self.save()
    }

    /// Read the Vault OIDC role with env override + default fallback.
    pub fn get_vault_auth_role(&self) -> String {
        #[cfg(not(test))]
        if let Ok(role) = std::env::var("RISE_VAULT_AUTH_ROLE") {
            return role;
        }
        self.vault_auth_role
            .clone()
            .unwrap_or_else(|| DEFAULT_VAULT_AUTH_ROLE.to_string())
    }

    /// Persist the Vault path used to mint JFrog tokens.
    pub fn set_vault_artifactory_token_path(&mut self, path: String) -> Result<()> {
        self.vault_artifactory_token_path = Some(path);
        self.save()
    }

    /// Read the Vault artifactory-token path. Env `RISE_VAULT_ARTIFACTORY_TOKEN_PATH`
    /// wins over the config file. Returns `None` when unset — caller treats
    /// that as "auto-mint disabled".
    pub fn get_vault_artifactory_token_path(&self) -> Option<String> {
        #[cfg(not(test))]
        if let Ok(p) = std::env::var("RISE_VAULT_ARTIFACTORY_TOKEN_PATH") {
            return Some(p);
        }
        self.vault_artifactory_token_path.clone()
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
    fn test_vault_address_none_by_default() {
        assert_eq!(Config::default().get_vault_address(), None);
    }

    #[test]
    fn test_vault_address_from_config() {
        let c = config(|c| c.vault_address = Some("https://vault.example.com:8200".to_string()));
        assert_eq!(
            c.get_vault_address().as_deref(),
            Some("https://vault.example.com:8200"),
        );
    }

    #[test]
    fn test_set_vault_address_trims_trailing_slash() {
        // Avoid `set_vault_address` here — it persists to disk. Verify the
        // normalisation by mimicking what the setter does in-memory.
        let c = config(|c| {
            c.vault_address = Some(
                "https://vault.example.com:8200/"
                    .trim_end_matches('/')
                    .to_string(),
            )
        });
        assert_eq!(
            c.get_vault_address().as_deref(),
            Some("https://vault.example.com:8200"),
        );
    }

    #[test]
    fn test_vault_auth_path_defaults_when_unset() {
        assert_eq!(
            Config::default().get_vault_auth_path(),
            DEFAULT_VAULT_AUTH_PATH,
        );
    }

    #[test]
    fn test_vault_auth_path_from_config_overrides_default() {
        let c = config(|c| c.vault_auth_path = Some("custom/oidc".to_string()));
        assert_eq!(c.get_vault_auth_path(), "custom/oidc");
    }

    #[test]
    fn test_vault_auth_role_defaults_when_unset() {
        assert_eq!(
            Config::default().get_vault_auth_role(),
            DEFAULT_VAULT_AUTH_ROLE,
        );
    }

    #[test]
    fn test_vault_auth_role_from_config_overrides_default() {
        let c = config(|c| c.vault_auth_role = Some("ops".to_string()));
        assert_eq!(c.get_vault_auth_role(), "ops");
    }

    #[test]
    fn test_vault_artifactory_token_path_none_by_default() {
        assert!(Config::default()
            .get_vault_artifactory_token_path()
            .is_none());
    }

    #[test]
    fn test_vault_artifactory_token_path_from_config() {
        let c = config(|c| {
            c.vault_artifactory_token_path =
                Some("global/artifactory/user_token/{email}".to_string())
        });
        assert_eq!(
            c.get_vault_artifactory_token_path().as_deref(),
            Some("global/artifactory/user_token/{email}"),
        );
    }

    #[test]
    fn test_token_none_by_default() {
        assert_eq!(Config::default().get_token(), None);
    }

    #[test]
    fn test_token_from_config() {
        let c = config(|c| c.token = Some("config-token".to_string()));
        assert_eq!(c.get_token(), Some("config-token".to_string()));
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
