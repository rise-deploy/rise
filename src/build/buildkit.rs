// BuildKit daemon lifecycle management

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::Command;
use tracing::{debug, info, warn};

use crate::config::{ContainerCli, ContainerRuntime};

use super::method::{requires_buildkit, BuildMethod};

/// Encode a byte slice as a lowercase hex string without per-byte allocations.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{:02x}", b).unwrap();
    }
    s
}

/// Daemon version label value. Bump this whenever the managed BuildKit daemon
/// creation parameters change (e.g. new flags, image updates) to force
/// recreation of stale daemons.
const DAEMON_VERSION: &str = "2";

/// Compute SHA256 hash of a file
pub(crate) fn compute_file_hash(path: &Path) -> Result<String> {
    let contents = fs::read(path).context("Failed to read certificate file")?;
    let mut hasher = Sha256::new();
    hasher.update(&contents);
    let result = hasher.finalize();
    Ok(hex_encode(&result))
}

/// Compute SHA256 hash of a string
fn compute_string_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let result = hasher.finalize();
    hex_encode(&result)
}

/// Generate buildkitd.toml configuration for insecure registries
/// Returns (config_content, config_hash) or None if no insecure registries configured
fn generate_buildkit_config() -> Option<(String, String)> {
    let insecure_registries =
        super::env_var_non_empty("RISE_MANAGED_BUILDKIT_INSECURE_REGISTRIES")?;

    let registries: Vec<&str> = insecure_registries
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if registries.is_empty() {
        return None;
    }

    let mut config = String::new();
    for registry in registries {
        // Only set http=true, NOT insecure=true. BuildKit treats insecure=true
        // as "use HTTPS with self-signed certs" which overrides http=true and
        // breaks plain HTTP registries. See https://github.com/moby/buildkit/issues/5872
        config.push_str(&format!("[registry.\"{}\"]\n  http = true\n\n", registry));
    }

    let hash = compute_string_hash(&config);
    Some((config, hash))
}

/// Write buildkitd.toml config to a file and return the path
/// Returns None if no config needed
fn write_buildkit_config() -> Result<Option<std::path::PathBuf>> {
    let Some((config_content, _hash)) = generate_buildkit_config() else {
        return Ok(None);
    };

    // Use ~/.rise/buildkitd.toml for the config file
    let home_dir = dirs::home_dir().context("Failed to determine home directory")?;
    let rise_dir = home_dir.join(".rise");

    // Create .rise directory if it doesn't exist
    fs::create_dir_all(&rise_dir).context("Failed to create .rise directory")?;

    let config_path = rise_dir.join("buildkitd.toml");
    fs::write(&config_path, config_content).context("Failed to write buildkitd.toml")?;

    Ok(Some(config_path))
}

/// Check if a Docker network exists
fn network_exists(container_cli: &str, network_name: &str) -> bool {
    let output = Command::new(container_cli)
        .args(["network", "inspect", network_name])
        .output();

    matches!(output, Ok(output) if output.status.success())
}

/// Create a Docker network if it doesn't exist
fn create_network(
    container_cli: &str,
    network_name: &str,
    output: super::CommandOutput,
) -> Result<()> {
    if network_exists(container_cli, network_name) {
        debug!("Network '{}' already exists", network_name);
        return Ok(());
    }

    info!("Creating Docker network '{}'", network_name);
    let mut command = Command::new(container_cli);
    command.args(["network", "create", network_name]);
    let status = super::run_command(command, "Failed to create network", output)?;

    if !status.success() {
        bail!("Failed to create network '{}'", network_name);
    }

    info!("Network '{}' created successfully", network_name);
    Ok(())
}

/// Connect a container to a Docker network
fn connect_to_network(
    container_cli: &str,
    network_name: &str,
    container_name: &str,
    output: super::CommandOutput,
) -> Result<()> {
    info!(
        "Connecting container '{}' to network '{}'",
        container_name, network_name
    );

    let mut command = Command::new(container_cli);
    command.args(["network", "connect", network_name, container_name]);
    let status = super::run_command(command, "Failed to connect to network", output)?;

    if !status.success() {
        bail!(
            "Failed to connect container '{}' to network '{}'",
            container_name,
            network_name
        );
    }

    info!(
        "Container '{}' connected to network '{}' successfully",
        container_name, network_name
    );
    Ok(())
}

/// Get network label from container
fn get_network_label_from_container(container_cli: &str, daemon_name: &str) -> Option<String> {
    let output = Command::new(container_cli)
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"rise.network_name\"}}",
            daemon_name,
        ])
        .output()
        .ok()?;

    if output.status.success() {
        let label_value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !label_value.is_empty() && label_value != "<no value>" {
            return Some(label_value);
        }
    }

    None
}

/// Get config hash label from container
fn get_config_hash_label_from_container(container_cli: &str, daemon_name: &str) -> Option<String> {
    let output = Command::new(container_cli)
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"rise.config_hash\"}}",
            daemon_name,
        ])
        .output()
        .ok()?;

    if output.status.success() {
        let label_value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !label_value.is_empty() && label_value != "<no value>" {
            return Some(label_value);
        }
    }

    None
}

/// Get proxy hash label from container
fn get_proxy_hash_label(container_cli: &str, daemon_name: &str) -> Option<String> {
    let output = Command::new(container_cli)
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"rise.proxy_hash\"}}",
            daemon_name,
        ])
        .output()
        .ok()?;

    if output.status.success() {
        let label_value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !label_value.is_empty() && label_value != "<no value>" {
            return Some(label_value);
        }
    }

    None
}

/// Get host_network label from container
fn get_host_network_label(container_cli: &str, daemon_name: &str) -> bool {
    let output = Command::new(container_cli)
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"rise.host_network\"}}",
            daemon_name,
        ])
        .output()
        .ok();

    if let Some(output) = output {
        if output.status.success() {
            let label_value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return label_value == "true";
        }
    }

    false
}

/// Compute expected proxy hash from proxy vars (None if no proxy vars)
fn compute_proxy_hash(proxy_vars: &std::collections::HashMap<String, String>) -> Option<String> {
    if proxy_vars.is_empty() {
        return None;
    }

    let mut sorted_keys: Vec<&String> = proxy_vars.keys().collect();
    sorted_keys.sort();

    let mut input = String::new();
    for key in &sorted_keys {
        input.push_str(key);
        input.push('=');
        input.push_str(&proxy_vars[*key]);
        input.push('\n');
    }

    Some(compute_string_hash(&input))
}

/// Get daemon version label from container
fn get_daemon_version_label(container_cli: &str, daemon_name: &str) -> Option<String> {
    let output = Command::new(container_cli)
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"rise.daemon_version\"}}",
            daemon_name,
        ])
        .output()
        .ok()?;

    if output.status.success() {
        let label_value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !label_value.is_empty() && label_value != "<no value>" {
            return Some(label_value);
        }
    }

    None
}

/// Represents the state of a BuildKit daemon
#[derive(Debug, PartialEq)]
enum DaemonState {
    /// Daemon has an SSL certificate with the given hash, optional network, and optional config hash
    HasCert(String, Option<String>, Option<String>),
    /// Daemon was created without an SSL certificate, with optional network and optional config hash
    NoCert(Option<String>, Option<String>),
    /// Daemon does not exist
    NotFound,
}

/// Get the current state of the BuildKit daemon
fn get_daemon_state(container_cli: &str, daemon_name: &str) -> DaemonState {
    // Check if daemon exists
    let inspect_status = Command::new(container_cli)
        .args(["inspect", daemon_name])
        .output();

    let Ok(output) = inspect_status else {
        return DaemonState::NotFound;
    };

    if !output.status.success() {
        return DaemonState::NotFound;
    }

    // Get network label if present
    let network_name = get_network_label_from_container(container_cli, daemon_name);

    // Get config hash label if present
    let config_hash = get_config_hash_label_from_container(container_cli, daemon_name);

    // Daemon exists, check for no_ssl_cert label
    let no_cert_output = Command::new(container_cli)
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"rise.no_ssl_cert\"}}",
            daemon_name,
        ])
        .output();

    if let Ok(output) = no_cert_output {
        if output.status.success() {
            let label_value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if label_value == "true" {
                return DaemonState::NoCert(network_name, config_hash);
            }
        }
    }

    // Check for SSL cert hash label
    let cert_hash_output = Command::new(container_cli)
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"rise.ssl_cert_hash\"}}",
            daemon_name,
        ])
        .output();

    if let Ok(output) = cert_hash_output {
        if output.status.success() {
            let cert_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !cert_hash.is_empty() {
                return DaemonState::HasCert(cert_hash, network_name, config_hash);
            }
        }
    }

    // Daemon exists but has no labels (old daemon or created externally)
    // Treat as NoCert to avoid assuming anything
    DaemonState::NoCert(network_name, config_hash)
}

/// Stop and remove BuildKit daemon.
///
/// Uses `rm -f` instead of `stop` because Podman does not always clean up
/// `--rm` containers reliably on `stop`, leaving the container name in use.
/// `rm -f` works correctly on both Docker and Podman.
fn stop_buildkit_daemon(
    container_cli: &ContainerCli,
    daemon_name: &str,
    output: super::CommandOutput,
) -> Result<()> {
    info!("Stopping existing BuildKit daemon '{}'", daemon_name);

    let mut command = Command::new(container_cli.command());
    command.args(["rm", "-f", daemon_name]);
    let status = super::run_command(command, "Failed to remove BuildKit daemon", output)?;

    if !status.success() {
        bail!("Failed to remove BuildKit daemon");
    }

    Ok(())
}

/// Create BuildKit daemon with optional SSL certificate mounted and network connection
/// Returns the BUILDKIT_HOST value to be used with this daemon
fn create_buildkit_daemon(
    container_cli: &ContainerCli,
    daemon_name: &str,
    ssl_cert_file: Option<&Path>,
    network_name: Option<&str>,
    proxy_vars: &std::collections::HashMap<String, String>,
    output: super::CommandOutput,
) -> Result<String> {
    if let Some(cert_path) = ssl_cert_file {
        info!(
            "Creating managed BuildKit daemon '{}' with SSL certificate: {}",
            daemon_name,
            cert_path.display()
        );
    } else {
        info!(
            "Creating managed BuildKit daemon '{}' without SSL certificate",
            daemon_name
        );
    }

    if let Some(network) = network_name {
        info!("BuildKit daemon will be connected to network '{}'", network);
    }

    let mut cmd = Command::new(container_cli.command());
    cmd.args([
        "run",
        "--privileged",
        "--name",
        daemon_name,
        "--rm",
        "-d",
        "--add-host",
        "host.docker.internal:host-gateway",
        "--label",
    ])
    .arg(format!("rise.daemon_version={}", DAEMON_VERSION));

    // Podman with cgroup v2 does not delegate the memory controller to containers
    // by default, which causes runc (used internally by BuildKit) to fail with:
    //   "can't get final child's PID from pipe: EOF"
    // Using --cgroupns=host lets BuildKit access the host cgroup hierarchy directly.
    if container_cli.runtime() == ContainerRuntime::Podman {
        debug!("Podman detected, adding --cgroupns=host for cgroup v2 compatibility");
        cmd.arg("--cgroupns=host");
    }

    // Add labels and volume mount based on SSL cert presence
    if let Some(cert_path) = ssl_cert_file {
        // Resolve certificate path to absolute path
        let cert_path_abs = if cert_path.is_absolute() {
            cert_path.to_path_buf()
        } else {
            std::env::current_dir()?.join(cert_path)
        };

        let cert_path_abs = cert_path_abs
            .canonicalize()
            .context("Failed to resolve SSL certificate path")?;

        let cert_str = cert_path_abs
            .to_str()
            .context("SSL certificate path contains invalid UTF-8")?;

        // Compute hash of certificate file
        let cert_hash = compute_file_hash(&cert_path_abs)?;

        cmd.arg("--label")
            .arg(format!("rise.ssl_cert_file={}", cert_str))
            .arg("--label")
            .arg(format!("rise.ssl_cert_hash={}", cert_hash))
            .arg("--volume")
            .arg(format!(
                "{}:/etc/ssl/certs/ca-certificates.crt:ro",
                cert_str
            ));
    } else {
        // No SSL cert, add label to track this state
        cmd.arg("--label").arg("rise.no_ssl_cert=true");
    }

    // Host networking: if enabled, use --network=host instead of a named Docker network.
    // Useful in development when BuildKit needs to reach host-bound ports (e.g. localhost:3082).
    let use_host_network = super::env_var_non_empty("RISE_MANAGED_BUILDKIT_HOST_NETWORK").is_some();

    if use_host_network {
        if network_name.is_some() {
            warn!(
                "RISE_MANAGED_BUILDKIT_HOST_NETWORK is set — \
                 ignoring RISE_MANAGED_BUILDKIT_NETWORK_NAME (host networking takes precedence)"
            );
        }
        cmd.arg("--network=host")
            .arg("--label")
            .arg("rise.host_network=true");
    }

    // Add network label if network is specified
    if let Some(network) = network_name {
        cmd.arg("--label")
            .arg(format!("rise.network_name={}", network));
    }

    // Write and mount buildkitd.toml config if insecure registries are configured
    if let Some((_config_content, config_hash)) = generate_buildkit_config() {
        let config_path = write_buildkit_config()?;

        if let Some(config_file) = config_path {
            let config_str = config_file
                .to_str()
                .context("Config file path contains invalid UTF-8")?;

            info!(
                "Mounting BuildKit config with insecure registries: {}",
                config_str
            );

            cmd.arg("--label")
                .arg(format!("rise.config_hash={}", config_hash))
                .arg("--volume")
                .arg(format!("{}:/etc/buildkit/buildkitd.toml:ro", config_str));
        }
    }

    // Pass proxy environment variables so the daemon can fetch images through the proxy
    if !proxy_vars.is_empty() {
        info!("Passing proxy environment variables to BuildKit daemon");
        let mut sorted_keys: Vec<&String> = proxy_vars.keys().collect();
        sorted_keys.sort();

        let mut proxy_hash_input = String::new();
        for key in &sorted_keys {
            let value = &proxy_vars[*key];
            cmd.arg("-e").arg(format!("{}={}", key, value));
            proxy_hash_input.push_str(key);
            proxy_hash_input.push('=');
            proxy_hash_input.push_str(value);
            proxy_hash_input.push('\n');
        }

        let proxy_hash = compute_string_hash(&proxy_hash_input);
        cmd.arg("--label")
            .arg(format!("rise.proxy_hash={}", proxy_hash));
    }

    cmd.arg("moby/buildkit");

    let status = super::run_command(cmd, "Failed to start BuildKit daemon", output)?;

    if !status.success() {
        bail!("Failed to create BuildKit daemon");
    }

    info!("BuildKit daemon '{}' created successfully", daemon_name);

    // Connect to network if specified (skip when using host networking)
    if !use_host_network {
        if let Some(network) = network_name {
            create_network(container_cli.command(), network, output)?;
            connect_to_network(container_cli.command(), network, daemon_name, output)?;
        }
    }

    // Return BUILDKIT_HOST value for this daemon
    Ok(format!("docker-container://{}", daemon_name))
}

/// Ensure managed BuildKit daemon is running with correct SSL certificate, network, and config
/// Returns the BUILDKIT_HOST value to be used with this daemon
pub(crate) fn ensure_managed_buildkit_daemon(
    ssl_cert_file: Option<&Path>,
    container_cli: &ContainerCli,
    output: super::CommandOutput,
) -> Result<String> {
    let daemon_name = "rise-buildkit";

    // Read proxy vars (transformed for container use) so the daemon can fetch images through proxy
    let proxy_vars = super::proxy::read_and_transform_proxy_vars();

    // Read network configuration from environment
    let network_name = super::env_var_non_empty("RISE_MANAGED_BUILDKIT_NETWORK_NAME");

    // Get expected config hash (None if no insecure registries configured)
    let expected_config_hash = generate_buildkit_config().map(|(_, hash)| hash);

    // Get expected proxy hash
    let expected_proxy_hash = compute_proxy_hash(&proxy_vars);

    // Get current daemon state
    let current_state = get_daemon_state(container_cli.command(), daemon_name);

    // Check daemon version — recreate if outdated (e.g. missing --add-host flag)
    if current_state != DaemonState::NotFound {
        let current_version = get_daemon_version_label(container_cli.command(), daemon_name);
        if current_version.as_deref() != Some(DAEMON_VERSION) {
            info!(
                "BuildKit daemon version changed ({:?} -> {}), recreating",
                current_version, DAEMON_VERSION
            );
            stop_buildkit_daemon(container_cli, daemon_name, output)?;
            return create_buildkit_daemon(
                container_cli,
                daemon_name,
                ssl_cert_file,
                network_name.as_deref(),
                &proxy_vars,
                output,
            );
        }

        // Check if proxy configuration has changed
        let current_proxy_hash = get_proxy_hash_label(container_cli.command(), daemon_name);
        if current_proxy_hash != expected_proxy_hash {
            info!("Proxy configuration has changed, recreating daemon");
            stop_buildkit_daemon(container_cli, daemon_name, output)?;
            return create_buildkit_daemon(
                container_cli,
                daemon_name,
                ssl_cert_file,
                network_name.as_deref(),
                &proxy_vars,
                output,
            );
        }

        // Check if host networking mode has changed
        let want_host_network =
            super::env_var_non_empty("RISE_MANAGED_BUILDKIT_HOST_NETWORK").is_some();
        let has_host_network = get_host_network_label(container_cli.command(), daemon_name);
        if want_host_network != has_host_network {
            info!(
                "Host networking changed (was={}, want={}), recreating daemon",
                has_host_network, want_host_network
            );
            stop_buildkit_daemon(container_cli, daemon_name, output)?;
            return create_buildkit_daemon(
                container_cli,
                daemon_name,
                ssl_cert_file,
                network_name.as_deref(),
                &proxy_vars,
                output,
            );
        }
    }

    match (ssl_cert_file, &current_state) {
        // Certificate provided, daemon has matching cert
        (Some(cert_path), DaemonState::HasCert(current_hash, current_network, current_config)) => {
            // Verify hash matches
            let cert_path_abs = cert_path
                .canonicalize()
                .context("Failed to resolve SSL certificate path")?;
            let expected_hash = compute_file_hash(&cert_path_abs)?;

            // Check if network has changed
            let network_changed = &network_name != current_network;

            // Check if config has changed
            let config_changed = &expected_config_hash != current_config;

            if current_hash == &expected_hash && !network_changed && !config_changed {
                debug!(
                    "BuildKit daemon is up-to-date with current SSL_CERT_FILE, network, and config"
                );
                return Ok(format!("docker-container://{}", daemon_name));
            }

            if current_hash != &expected_hash {
                info!("SSL certificate has changed (hash mismatch), recreating daemon");
            } else if network_changed {
                info!("Network configuration has changed, recreating daemon");
            } else if config_changed {
                info!("BuildKit config has changed (insecure registries), recreating daemon");
            }
            stop_buildkit_daemon(container_cli, daemon_name, output)?;
        }

        // Certificate provided, but daemon has no cert label
        (Some(_), DaemonState::NoCert(current_network, current_config)) => {
            // Check if network has changed
            let network_changed = &network_name != current_network;

            // Check if config has changed
            let config_changed = &expected_config_hash != current_config;

            if network_changed {
                info!("Network configuration has changed, recreating daemon");
            } else if config_changed {
                info!("BuildKit config has changed, recreating daemon");
            } else {
                info!("SSL certificate now available, recreating daemon with certificate");
            }
            stop_buildkit_daemon(container_cli, daemon_name, output)?;
        }

        // No certificate, daemon has no cert label (matches)
        (None, DaemonState::NoCert(current_network, current_config)) => {
            // Check if network has changed
            let network_changed = &network_name != current_network;

            // Check if config has changed
            let config_changed = &expected_config_hash != current_config;

            if !network_changed && !config_changed {
                debug!("BuildKit daemon is up-to-date (no SSL certificate)");
                return Ok(format!("docker-container://{}", daemon_name));
            }

            if network_changed {
                info!("Network configuration has changed, recreating daemon");
            } else if config_changed {
                info!("BuildKit config has changed (insecure registries), recreating daemon");
            }
            stop_buildkit_daemon(container_cli, daemon_name, output)?;
        }

        // No certificate, but daemon has cert (mismatch)
        (None, DaemonState::HasCert(_, current_network, current_config)) => {
            // Check if network has changed
            let network_changed = &network_name != current_network;

            // Check if config has changed
            let config_changed = &expected_config_hash != current_config;

            if network_changed && config_changed {
                info!("SSL certificate removed, network and config changed, recreating daemon");
            } else if network_changed {
                info!("SSL certificate removed and network changed, recreating daemon");
            } else if config_changed {
                info!("SSL certificate removed and config changed, recreating daemon");
            } else {
                info!("SSL certificate removed, recreating daemon without certificate");
            }
            stop_buildkit_daemon(container_cli, daemon_name, output)?;
        }

        // Daemon doesn't exist
        (_, DaemonState::NotFound) => {
            // Will create new daemon below
        }
    }

    // Create new daemon with or without certificate and return its BUILDKIT_HOST
    create_buildkit_daemon(
        container_cli,
        daemon_name,
        ssl_cert_file,
        network_name.as_deref(),
        &proxy_vars,
        output,
    )
}

/// Ensure buildx builder exists for the given BuildKit daemon
/// Returns the builder name to use
pub(crate) fn ensure_buildx_builder(
    container_cli: &str,
    buildkit_host: &str,
    output: super::CommandOutput,
) -> Result<String> {
    let builder_name = "rise-buildkit";

    // Check if builder already exists
    let inspect_status = Command::new(container_cli)
        .args(["buildx", "inspect", builder_name])
        .output();

    match inspect_status {
        Ok(builder_inspect) if builder_inspect.status.success() => {
            // Builder exists, check if it's pointing to the correct endpoint
            let inspect_output = String::from_utf8_lossy(&builder_inspect.stdout);

            // Check if the buildkit_host appears in the inspect output
            // The output contains lines like "Endpoint: docker-container://rise-buildkit"
            if inspect_output.contains(buildkit_host) {
                debug!(
                    "Buildx builder '{}' already exists with correct endpoint",
                    builder_name
                );
                return Ok(builder_name.to_string());
            }

            // Builder exists but points to wrong endpoint, remove and recreate
            info!(
                "Buildx builder '{}' exists but points to different endpoint, recreating",
                builder_name
            );
            let mut command = Command::new(container_cli);
            command.args(["buildx", "rm", builder_name]);
            let _ = super::run_command(command, "Failed to remove buildx builder", output);
        }
        _ => {
            info!(
                "Creating buildx builder '{}' for BuildKit daemon: {}",
                builder_name, buildkit_host
            );
        }
    }

    // Create new builder pointing to the BuildKit daemon
    let mut command = Command::new(container_cli);
    command.args(["buildx", "create", "--name", builder_name, buildkit_host]);
    let status = super::run_command(command, "Failed to create buildx builder", output)?;

    if !status.success() {
        bail!("Failed to create buildx builder '{}'", builder_name);
    }

    info!("Buildx builder '{}' created successfully", builder_name);
    Ok(builder_name.to_string())
}

/// Resolve the gateway IP of a container (i.e. the host IP reachable from inside
/// that container). Used to pass a concrete IP for `--add-host host.docker.internal:<ip>`
/// to buildx build, since the remote driver cannot resolve the `host-gateway` magic value
/// but build containers need to reach the host (e.g. for proxy).
pub(crate) fn resolve_host_gateway_ip(container_cli: &str, container_name: &str) -> Option<String> {
    // Prefer reading the resolved host.docker.internal IP from the container's /etc/hosts.
    // The daemon is created with `--add-host host.docker.internal:host-gateway`, and the
    // container runtime resolves `host-gateway` to a concrete IP at creation time. Reading
    // it back gives us the correct host IP even through VM layers (e.g. Podman Machine),
    // where NetworkSettings.Gateway returns the bridge IP inside the VM rather than the
    // actual host IP.
    if let Some(ip) = resolve_from_etc_hosts(container_cli, container_name) {
        debug!(
            "Resolved host gateway IP for '{}' from /etc/hosts: {}",
            container_name, ip
        );
        return Some(ip);
    }

    // Fallback: use the default bridge network gateway from container inspection.
    let output = Command::new(container_cli)
        .args([
            "inspect",
            "--format",
            "{{.NetworkSettings.Gateway}}",
            container_name,
        ])
        .output()
        .ok()?;

    if output.status.success() {
        let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !ip.is_empty() {
            debug!(
                "Resolved host gateway IP for '{}' from NetworkSettings.Gateway: {}",
                container_name, ip
            );
            return Some(ip);
        }
    }

    warn!(
        "Failed to resolve host gateway IP for container '{}'",
        container_name
    );
    None
}

/// Read the resolved `host.docker.internal` IP from a container's /etc/hosts file.
fn resolve_from_etc_hosts(container_cli: &str, container_name: &str) -> Option<String> {
    let output = Command::new(container_cli)
        .args(["exec", container_name, "cat", "/etc/hosts"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let hosts = String::from_utf8_lossy(&output.stdout);
    for line in hosts.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.contains("host.docker.internal") {
            let ip = line.split_whitespace().next()?;
            return Some(ip.to_string());
        }
    }

    None
}

/// Resolve the host gateway IP for a BuildKit container and apply it to proxy
/// variables on the given command.
///
/// `container_name` is the BuildKit container (or buildx builder) to resolve
/// the gateway IP from.  When `had_remote_host` is true but resolution fails,
/// a warning is emitted so the user knows proxy routing may break.
///
/// Returns the (potentially rewritten) proxy/secret variables.
pub(crate) fn resolve_and_apply_host_gateway(
    cmd: &mut std::process::Command,
    container_cli: &str,
    vars: &std::collections::HashMap<String, String>,
    container_name: Option<&str>,
    had_remote_host: bool,
) -> std::collections::HashMap<String, String> {
    let gateway_ip = container_name.and_then(|name| resolve_host_gateway_ip(container_cli, name));

    if had_remote_host && gateway_ip.is_none() && super::proxy::needs_host_gateway(vars) {
        warn!(
            "Proxy configuration references host.docker.internal but the host gateway IP \
             could not be resolved for the remote BuildKit driver. Proxy routing may fail \
             inside the build container."
        );
    }

    super::proxy::apply_host_gateway(cmd, vars, gateway_ip.as_deref())
}

/// Warn user about SSL certificate issues when managed BuildKit is disabled
pub(crate) fn check_ssl_cert_and_warn(method: &BuildMethod, managed_buildkit: bool) {
    if super::env_var_non_empty("SSL_CERT_FILE").is_some()
        && requires_buildkit(method)
        && !managed_buildkit
    {
        warn!(
            "SSL_CERT_FILE is set but managed BuildKit daemon is disabled. \
             Builds may fail with SSL certificate errors in corporate environments."
        );
        warn!("To enable automatic BuildKit daemon management:");
        warn!("  rise build --managed-buildkit ...");
        warn!("Or set environment variable:");
        warn!("  export RISE_MANAGED_BUILDKIT=true");
    }
}
