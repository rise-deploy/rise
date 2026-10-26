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

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub backend_url: Option<String>,
    pub container_cli: Option<String>,
    pub managed_buildkit: Option<bool>,
}

/// The selected alias and effective backend URL for every config load in this process.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub profile: Option<String>,
    pub url: String,
}

static PROFILE_OVERRIDE: OnceLock<Option<String>> = OnceLock::new();
static TARGET_OVERRIDE: OnceLock<ResolvedTarget> = OnceLock::new();

const DEFAULT_PROFILE_FILE: &str = "default-profile";

pub fn set_profile_override(profile: Option<String>) {
    let _ = PROFILE_OVERRIDE.set(profile);
}

pub fn set_target_override(target: ResolvedTarget) {
    let _ = TARGET_OVERRIDE.set(target);
}

impl Config {
    /// Explicit profile selection, including the literal `default`.
    fn explicit_profile() -> Result<Option<Option<String>>> {
        if let Some(profile) = PROFILE_OVERRIDE.get() {
            return Ok(Some(profile.clone()));
        }
        #[cfg(not(test))]
        if let Ok(value) = std::env::var("RISE_PROFILE") {
            let name = value.trim();
            if name.is_empty() || name == "default" {
                return Ok(Some(None));
            }
            validate_profile_name(name)?;
            return Ok(Some(Some(name.to_string())));
        }
        Ok(None)
    }

    pub fn has_target_override(url: Option<&str>) -> Result<bool> {
        Ok(url.is_some()
            || Self::explicit_profile()?.is_some()
            || (!cfg!(test) && std::env::var("RISE_URL").is_ok()))
    }

    /// Resolve the effective URL before looking up credentials. Project defaults
    /// apply only in the absence of explicit URL and profile overrides.
    pub fn resolve_target(
        url: Option<&str>,
        project_target: Option<&crate::rise_toml::TargetConfig>,
        allow_new_profile: bool,
    ) -> Result<ResolvedTarget> {
        if let Some(target) = TARGET_OVERRIDE.get() {
            return Ok(target.clone());
        }
        let explicit_profile = Self::explicit_profile()?;
        let allow_new_profile = allow_new_profile && explicit_profile.is_some();
        let env_url = if cfg!(test) {
            None
        } else {
            std::env::var("RISE_URL").ok()
        };
        let mut url = url.or(env_url.as_deref()).map(str::to_string);
        let profile = if let Some(profile) = explicit_profile {
            profile
        } else if url.is_some() {
            None
        } else if let Some(target) = project_target {
            match target {
                crate::rise_toml::TargetConfig::Url(value) => {
                    url = Some(value.clone());
                    None
                }
                crate::rise_toml::TargetConfig::Profile(name) => {
                    validate_profile_name(name)?;
                    if name == "default" {
                        None
                    } else {
                        Some(name.clone())
                    }
                }
            }
        } else {
            Self::default_profile()?
        };
        if let Some(name) = &profile {
            if !Self::path_for(Some(name))?.exists() && !allow_new_profile {
                anyhow::bail!("Profile '{}' does not exist; register it with 'rise login --profile {} --url <URL>' first", name, name);
            }
        }
        let saved = Self::load_named(profile.as_deref())?;
        let url = normalize_backend_url(
            url.as_deref()
                .or(saved.backend_url.as_deref())
                .unwrap_or("http://localhost:3000"),
        );
        let parsed = url::Url::parse(&url).context("Invalid Rise backend URL")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            anyhow::bail!("Rise backend URL must be an absolute HTTP or HTTPS URL");
        }
        Ok(ResolvedTarget { profile, url })
    }

    pub fn active_profile() -> Result<Option<String>> {
        if let Some(target) = TARGET_OVERRIDE.get() {
            return Ok(target.profile.clone());
        }
        if let Some(profile) = Self::explicit_profile()? {
            return Ok(profile);
        }
        #[cfg(not(test))]
        return Ok(Self::resolve_target(None, None, false)?.profile);
        #[cfg(test)]
        Ok(None)
    }

    pub fn active_profile_label() -> Result<String> {
        Ok(Self::active_profile()?.unwrap_or_else(|| "default".to_string()))
    }

    /// The profile selected by `rise profile use`, independent of overrides.
    pub fn default_profile() -> Result<Option<String>> {
        let path = Self::config_dir()?.join(DEFAULT_PROFILE_FILE);
        Self::read_default_profile_file(&path)
    }

    /// Select the fallback profile when no explicit override or project target applies.
    pub fn set_default_profile(name: &str) -> Result<()> {
        validate_profile_name(name)?;

        if name != "default" && !Self::path_for(Some(name))?.exists() {
            anyhow::bail!(
                "Profile '{}' does not exist; register it with 'rise login --profile {}' first",
                name,
                name
            );
        }

        let path = Self::config_dir()?.join(DEFAULT_PROFILE_FILE);
        Self::write_private_file(&path, format!("{name}\n").as_bytes())
    }

    fn read_default_profile_file(path: &std::path::Path) -> Result<Option<String>> {
        if !path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(path).context("Failed to read default profile")?;
        let name = contents.trim();
        if name.is_empty() || name == "default" {
            return Ok(None);
        }
        validate_profile_name(name).context("Invalid default profile")?;
        Ok(Some(name.to_string()))
    }

    fn config_dir() -> Result<PathBuf> {
        let home = dirs::home_dir().context("Failed to get home directory")?;
        let config_dir = home.join(".config").join("rise");
        ensure_config_dir(&config_dir)?;
        Ok(config_dir)
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

        let config = Self::load_named((name != "default").then_some(name))?;
        if let Some(url) = config.backend_url.as_deref() {
            if !Self::credential_path(url)?.exists() {
                if let Some(token) = Self::token_for_url(url)? {
                    Self::write_credential(url, token)?;
                }
            }
        }

        fs::remove_file(&path).context("Failed to remove profile config file")?;
        if Self::default_profile()?.as_deref() == Some(name) {
            Self::set_default_profile("default")?;
        }
        Ok(())
    }

    /// The config file path for a given profile (`None` = default profile).
    pub fn path_for(profile: Option<&str>) -> Result<PathBuf> {
        let config_dir = Self::config_dir()?;

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
        Self::load_target(false)
    }

    /// Login can establish a credential even when existing saved tokens disagree.
    pub fn load_for_login() -> Result<Self> {
        Self::load_target(true)
    }

    fn load_target(for_login: bool) -> Result<Self> {
        let target = Self::resolve_target(None, None, for_login)?;
        let mut config = Self::load_named(target.profile.as_deref())?;
        config.token = if for_login {
            None
        } else {
            Self::token_for_url(&target.url)?
        };
        config.backend_url = Some(target.url);
        Ok(config)
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
        // Remove once profile-local credentials are no longer supported.
        let previous = Self::load_named(Self::active_profile()?.as_deref())?;
        if previous.token.is_some() {
            if let Some(url) = previous.backend_url.as_deref() {
                if !Self::credential_path(url)?.exists() {
                    if let Some(token) = Self::token_for_url(url)? {
                        Self::write_credential(url, token)?;
                    }
                }
            }
        }
        let mut settings = self.clone();
        settings.token = None;
        Self::write_config_file(&config_path, &settings)
    }

    /// Write configuration to a specific path with restrictive permissions on Unix
    fn write_config_file(config_path: &std::path::Path, config: &Config) -> Result<()> {
        let json = serde_json::to_string_pretty(config).context("Failed to serialize config")?;
        Self::write_private_file(config_path, json.as_bytes())
    }

    fn write_private_file(path: &std::path::Path, contents: &[u8]) -> Result<()> {
        use std::io::Write;
        // NamedTempFile uses 0600 on Unix; persist atomically replaces the destination.
        let mut file =
            tempfile::NamedTempFile::new_in(path.parent().context("Missing config directory")?)
                .context("Failed to create config file")?;
        file.write_all(contents)
            .context("Failed to write config file")?;
        file.persist(path)
            .context("Failed to persist config file")?;
        Ok(())
    }

    /// Credentials are shared by every alias pointing at the same normalized URL.
    pub fn credential_path(url: &str) -> Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let dir = Self::config_dir()?.join("credentials");
        ensure_config_dir(&dir)?;
        let key: String = Sha256::digest(normalize_backend_url(url).as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(dir.join(format!("{key}.json")))
    }

    pub fn token_for_url(url: &str) -> Result<Option<String>> {
        let url = normalize_backend_url(url);
        let path = Self::credential_path(&url)?;
        if path.exists() {
            let contents = fs::read_to_string(&path).context("Failed to read saved credential")?;
            let saved: Config =
                serde_json::from_str(&contents).context("Failed to parse saved credential")?;
            anyhow::ensure!(
                saved.backend_url.as_deref() == Some(url.as_str()),
                "Saved credential URL does not match its storage key"
            );
            return Ok(saved.token);
        }

        // Remove once profile-local credentials are no longer supported.
        let mut names = vec!["default".to_string()];
        names.extend(Self::list_profiles()?);
        let mut token = None;
        let mut matches = Vec::new();
        let mut conflict = false;
        for name in names {
            let config = Self::load_named((name != "default").then_some(name.as_str()))?;
            if config
                .backend_url
                .as_deref()
                .map(normalize_backend_url)
                .as_deref()
                != Some(url.as_str())
            {
                continue;
            }
            if let Some(value) = config.token.filter(|value| !value.is_empty()) {
                conflict |= token.as_ref().is_some_and(|existing| existing != &value);
                token = Some(value);
                matches.push(name);
            }
        }
        if conflict {
            anyhow::bail!("Profiles {} contain different saved tokens for '{}'. Run 'rise login --url {}' to establish the shared login", matches.join(", "), url, url);
        }
        Ok(token)
    }

    /// Save a successful login for its effective URL and persist the selected alias.
    pub fn save_login(&mut self, url: &str, token: String) -> Result<()> {
        let url = normalize_backend_url(url);
        Self::write_credential(&url, token.clone())?;
        self.backend_url = Some(url);
        self.token = Some(token);
        self.save()
    }

    fn write_credential(url: &str, token: String) -> Result<()> {
        let url = normalize_backend_url(url);
        let credential = Config {
            token: Some(token),
            backend_url: Some(url.clone()),
            ..Config::default()
        };
        Self::write_config_file(&Self::credential_path(&url)?, &credential)
    }

    /// The saved login token for the resolved URL (ignores RISE_TOKEN env).
    pub fn stored_token(&self) -> Option<String> {
        self.token.clone()
    }

    /// The effective URL is resolved before credentials are loaded.
    pub fn get_backend_url(&self) -> String {
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

    #[test]
    fn login_persists_url_credentials_without_losing_previous_login() {
        const CHILD: &str = "RISE_TEST_LOGIN_STORAGE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "cli::config::tests::login_persists_url_credentials_without_losing_previous_login", "--nocapture"])
                .env(CHILD, "1")
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let previous_url = "https://previous.example.com";
        let url = "https://rise.example.com";
        Config::write_config_file(
            &Config::path_for(Some("work")).unwrap(),
            &Config {
                backend_url: Some(previous_url.into()),
                token: Some("previous-token".into()),
                container_cli: Some("podman".into()),
                ..Config::default()
            },
        )
        .unwrap();
        Config::write_config_file(
            &Config::path_for(Some("other")).unwrap(),
            &Config {
                backend_url: Some(url.into()),
                token: Some("other-token".into()),
                ..Config::default()
            },
        )
        .unwrap();
        set_target_override(ResolvedTarget {
            profile: Some("work".into()),
            url: url.into(),
        });

        let mut config = Config::load_for_login().unwrap();
        config
            .save_login(&format!("{url}/"), "fresh-token".into())
            .unwrap();
        assert_eq!(
            Config::token_for_url(previous_url).unwrap().as_deref(),
            Some("previous-token")
        );
        assert_eq!(
            Config::load().unwrap().stored_token().as_deref(),
            Some("fresh-token")
        );
        let alias = Config::load_named(Some("work")).unwrap();
        assert_eq!(alias.backend_url.as_deref(), Some(url));
        assert_eq!(alias.container_cli.as_deref(), Some("podman"));
        assert!(alias.token.is_none());

        config.save_login(url, "refreshed-token".into()).unwrap();
        assert_eq!(
            Config::token_for_url(url).unwrap().as_deref(),
            Some("refreshed-token")
        );
        assert_eq!(
            Config::token_for_url("https://unknown.example.com").unwrap(),
            None
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(Config::credential_path(url).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

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
        assert_eq!(Config::active_profile().unwrap(), None);
        assert_eq!(Config::active_profile_label().unwrap(), "default");
    }

    #[test]
    fn test_read_default_profile_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().join(DEFAULT_PROFILE_FILE);

        assert_eq!(Config::read_default_profile_file(&path).unwrap(), None);
        fs::write(&path, "default\n").unwrap();
        assert_eq!(Config::read_default_profile_file(&path).unwrap(), None);
        fs::write(&path, "work\n").unwrap();
        assert_eq!(
            Config::read_default_profile_file(&path).unwrap(),
            Some("work".to_string())
        );
    }

    #[test]
    fn test_read_default_profile_file_rejects_invalid_name() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().join(DEFAULT_PROFILE_FILE);
        fs::write(&path, "../work\n").unwrap();

        assert!(Config::read_default_profile_file(&path).is_err());
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
