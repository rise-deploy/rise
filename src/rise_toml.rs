//! Shared type definitions for the `rise.toml` / `.rise.toml` project configuration file.
//!
//! These types are used by both the CLI (for reading/writing config) and the backend
//! (for generating a JSON Schema endpoint).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Root structure for rise.toml / .rise.toml configuration file
#[derive(Debug, Deserialize, Serialize, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct ProjectBuildConfig {
    /// Optional version (must be 1 if present)
    pub version: Option<u32>,

    /// Project metadata (optional)
    #[serde(default)]
    pub project: Option<ProjectConfig>,

    /// Build configuration (optional)
    #[serde(default)]
    pub build: Option<BuildConfig>,

    /// Deployment resource configuration (optional)
    #[serde(default)]
    pub deploy: Option<DeployConfig>,

    /// Workload identity configuration (optional)
    #[serde(default)]
    pub identity: Option<IdentityConfig>,

    /// Per-environment configuration (optional)
    #[serde(default)]
    pub environments: BTreeMap<String, EnvironmentConfig>,
}

/// Workload identity configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct IdentityConfig {
    /// Audiences to auto-mint workload identity tokens for and mount as files.
    /// Map key = in-pod token filename, value = the token audience.
    #[serde(default)]
    pub audiences: BTreeMap<String, String>,
}

/// Per-environment configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct EnvironmentConfig {
    /// Whether this is the default environment for local deployments
    #[serde(default)]
    pub default: bool,

    /// Plain-text environment variables scoped to this environment
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Environment-specific deployment resource overrides
    #[serde(default)]
    pub deploy: Option<DeployConfig>,
}

/// Deployment resource configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct DeployConfig {
    /// Number of replicas
    pub replicas: Option<u32>,

    /// CPU allocation (e.g., "500m", "1") — sets both K8s request and limit
    pub cpu: Option<String>,

    /// Memory allocation (e.g., "256Mi", "1Gi") — sets both K8s request and limit
    pub memory: Option<String>,
}

/// Project metadata configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct ProjectConfig {
    /// Project name
    pub name: String,

    /// Plain-text environment variables (non-secret)
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Build configuration options for a project
#[derive(Debug, Deserialize, Serialize, Default)]
#[cfg_attr(feature = "backend", derive(schemars::JsonSchema))]
pub struct BuildConfig {
    /// Build backend (docker, docker:build, docker:buildx, buildctl, pack, railpack[:buildx], railpack:buildctl)
    pub backend: Option<String>,

    /// Buildpack builder to use (only for pack backend)
    pub builder: Option<String>,

    /// Buildpack(s) to use (only for pack backend)
    pub buildpacks: Option<Vec<String>>,

    /// Build arguments to pass to the build
    /// Format: KEY=VALUE or KEY (to pass from environment)
    #[serde(alias = "env")]
    pub args: Option<Vec<String>>,

    /// Container CLI to use (docker or podman)
    pub container_cli: Option<String>,

    /// Enable managed BuildKit daemon with SSL certificate support
    pub managed_buildkit: Option<bool>,

    /// Path to Dockerfile (relative to rise.toml location). Defaults to "Dockerfile" or "Containerfile"
    pub dockerfile: Option<String>,

    /// Default build context (docker/podman only) - the context directory for the build
    /// This is the path argument to `docker build <path>`. Defaults to rise.toml location.
    /// Path is relative to the rise.toml file location.
    pub build_context: Option<String>,

    /// Build contexts (docker/podman only) - additional named contexts for multi-stage builds
    /// Format: { "name" = "path" } where path is relative to the rise.toml file location
    #[serde(default)]
    pub build_contexts: Option<HashMap<String, String>>,

    /// Disable build cache
    pub no_cache: Option<bool>,

    /// Target platform for the container image build (e.g., "linux/amd64", "linux/arm64").
    /// Defaults to linux/amd64.
    pub platform: Option<String>,
}
