//! Bollard client construction from the configured `docker_host`.

use anyhow::{Context, Result};
use bollard::Docker;

/// Default timeout (seconds) for socket/TCP connections, matching bollard's
/// own `DEFAULT_TIMEOUT`.
const CONNECT_TIMEOUT_SECS: u64 = 120;

/// Build a bollard [`Docker`] client from an optional `docker_host`.
///
/// - `None` → bollard's local defaults (unix socket / npipe, honoring
///   `DOCKER_HOST` where bollard's `*_defaults` helpers do).
/// - `unix://...` / `npipe://...` / a bare path → socket connection.
/// - `tcp://...` / `http://...` / `https://...` → HTTP connection.
pub fn connect(docker_host: Option<&str>) -> Result<Docker> {
    let docker = match docker_host {
        None => Docker::connect_with_local_defaults()
            .context("Failed to connect to local Docker daemon")?,
        Some(host) => {
            let is_socket = host.starts_with("unix://")
                || host.starts_with("npipe://")
                || host.starts_with('/');
            if is_socket {
                Docker::connect_with_socket(
                    host,
                    CONNECT_TIMEOUT_SECS,
                    bollard::API_DEFAULT_VERSION,
                )
                .with_context(|| format!("Failed to connect to Docker socket at {host}"))?
            } else {
                Docker::connect_with_http(host, CONNECT_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
                    .with_context(|| format!("Failed to connect to Docker daemon at {host}"))?
            }
        }
    };
    Ok(docker)
}
