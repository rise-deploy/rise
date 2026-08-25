use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Module declarations with feature gates
#[cfg(feature = "cli")]
mod api;
#[cfg(feature = "cli")]
mod build;
#[cfg(feature = "cli")]
mod cli;

#[cfg(any(feature = "cli", feature = "backend"))]
mod rise_toml;

#[cfg(feature = "backend")]
mod db;
#[cfg(feature = "backend")]
mod server;

// Re-export for convenience (CLI modules)
#[cfg(feature = "cli")]
use cli::*;

/// Resolve environment from explicit flag or the `default = true` environment in rise.toml.
#[cfg(feature = "cli")]
fn resolve_environment(
    explicit: Option<String>,
    config: Option<&build::config::ProjectBuildConfig>,
) -> Option<String> {
    explicit.or_else(|| {
        config.and_then(|cfg| {
            cfg.environments
                .iter()
                .find(|(_, env)| env.default)
                .map(|(name, _)| name.clone())
        })
    })
}

/// Resolve project name from explicit argument or rise.toml fallback.
#[cfg(feature = "cli")]
fn resolve_project_name(explicit_project: Option<String>, path: &str) -> Result<String> {
    resolve_project_name_with_config(explicit_project, path, None)
}

/// Like [`resolve_project_name`] but accepts a preloaded config to avoid
/// duplicate disk loads (and duplicate log/warning output) when the caller
/// has already loaded the config for other purposes.
#[cfg(feature = "cli")]
fn resolve_project_name_with_config(
    explicit_project: Option<String>,
    path: &str,
    preloaded_config: Option<&build::config::ProjectBuildConfig>,
) -> Result<String> {
    if let Some(project) = explicit_project {
        return Ok(project);
    }

    // Use preloaded config if available, otherwise load from disk
    let owned_config;
    let config = if let Some(cfg) = preloaded_config {
        Some(cfg)
    } else {
        owned_config = build::config::load_full_project_config(path)?;
        owned_config.as_ref()
    };

    if let Some(cfg) = config {
        if let Some(ref project_config) = cfg.project {
            Ok(project_config.name.clone())
        } else {
            anyhow::bail!("No project name specified and rise.toml has no [project] section")
        }
    } else {
        anyhow::bail!("No project name specified and no rise.toml found")
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Shared arguments for deployment creation
#[derive(Debug, Clone, clap::Args)]
struct DeployArgs {
    /// Project name (optional if rise.toml contains [project] section)
    #[arg(long, short = 'p')]
    project: Option<String>,
    /// Path to the directory containing the application (defaults to current directory)
    #[arg(default_value = ".")]
    path: String,
    /// Pre-built image to deploy (e.g., nginx:latest). Skips build if provided.
    #[arg(long, short)]
    image: Option<String>,
    /// Create deployment from an existing deployment (e.g., '20240101-120000'). Skips build and reuses the image.
    #[arg(long)]
    from: Option<String>,
    /// When used with --from, copy environment variables from source deployment instead of using current project vars
    #[arg(long)]
    use_source_env_vars: bool,
    /// Deployment group (e.g., 'default', 'mr/27'). Defaults to 'default' if not specified.
    #[arg(long, short)]
    group: Option<String>,
    /// Target environment (e.g., 'production', 'staging'). Resolved from group if not specified.
    #[arg(long, short = 'E')]
    environment: Option<String>,
    /// Expiration duration (e.g., '7d', '2h', '30m'). Deployment will be automatically cleaned up after this period.
    #[arg(long)]
    expire: Option<String>,
    /// HTTP port the application listens on (e.g., 3000, 8080, 5000).
    /// Required when using --image. Defaults to 8080 for buildpack builds.
    #[arg(long)]
    http_port: Option<u16>,
    /// Push the --image to the Rise registry instead of deploying it directly.
    /// Pulls the image locally and pushes to the internal registry.
    #[arg(long)]
    push_image: bool,
    /// Runtime environment variables (format: KEY=VALUE, can be specified multiple times).
    /// These are set on the deployment and available to the running container.
    #[arg(long = "env", short = 'e', value_name = "KEY=VALUE", value_parser = parse_key_val::<String, String>)]
    env: Vec<(String, String)>,
    /// Secret runtime environment variables (encrypted at rest, retrievable via `rise env get`).
    /// Format: KEY=VALUE, can be specified multiple times.
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_key_val::<String, String>)]
    secret_env: Vec<(String, String)>,
    /// Protected secret runtime environment variables (encrypted at rest, NOT retrievable).
    /// Format: KEY=VALUE, can be specified multiple times.
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_key_val::<String, String>)]
    protected_env: Vec<(String, String)>,
    /// File with runtime environment variables (same format as `rise env import`).
    /// Lines: KEY=value (plain) or KEY=secret:value (protected secret).
    #[arg(long)]
    env_file: Option<String>,
    /// URL to the CI pipeline/job that created this deployment.
    /// Auto-detected from: CI_JOB_URL (GitLab), GITHUB_SERVER_URL + GITHUB_REPOSITORY + GITHUB_RUN_ID (GitHub Actions).
    #[arg(long)]
    job_url: Option<String>,
    /// URL to the pull request/merge request associated with this deployment.
    /// Auto-detected from: CI_MERGE_REQUEST_URL (GitLab), GITHUB_SERVER_URL + GITHUB_REPOSITORY + GITHUB_REF + GITHUB_EVENT_NAME (GitHub Actions).
    #[arg(long)]
    pull_request_url: Option<String>,
    /// URL of the Git repository this deployment is created from.
    /// Auto-detected from common CI environment variables, or the local `origin`
    /// git remote. ssh://, git:// and git@ URLs are converted to HTTPS.
    #[arg(long)]
    git_repository: Option<String>,
    /// Number of replicas for this deployment (overrides rise.toml)
    #[arg(long)]
    replicas: Option<u32>,
    /// CPU allocation (e.g., "500m", "1") — sets both K8s request and limit (overrides rise.toml)
    #[arg(long)]
    cpu: Option<String>,
    /// Memory allocation (e.g., "256Mi", "1Gi") — sets both K8s request and limit (overrides rise.toml)
    #[arg(long)]
    memory: Option<String>,
    #[command(flatten)]
    build_args: build::BuildArgs,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Backend server and controller commands
    #[cfg(feature = "backend")]
    #[command(subcommand)]
    Backend(backend::BackendCommands),
    /// Build a container image locally without deploying
    Build {
        /// Tag for the built image (e.g., myapp:latest, registry.io/org/app:v1.0)
        tag: String,
        /// Path to the directory containing the application
        #[arg(default_value = ".")]
        path: String,
        /// Push image to registry after building
        #[arg(long)]
        push: bool,
        #[command(flatten)]
        build_args: build::BuildArgs,
    },
    /// Deploy an application (shortcut for 'deployment create')
    Deploy {
        #[command(flatten)]
        args: DeployArgs,
    },
    /// Deployment management commands
    #[command(subcommand)]
    #[command(visible_alias = "d")]
    Deployment(DeploymentCommands),
    /// Custom domain management commands
    #[command(subcommand)]
    #[command(visible_alias = "dom")]
    Domain(DomainCommands),
    /// Encrypt a secret for use in extension specs
    Encrypt {
        /// Secret to encrypt (or read from stdin if not provided)
        plaintext: Option<String>,
    },
    /// Environment management commands (production, staging, etc.)
    #[command(subcommand)]
    #[command(visible_alias = "envs")]
    Environment(EnvironmentCommands),
    /// Environment variable management commands
    #[command(subcommand)]
    #[command(visible_alias = "e")]
    Env(EnvCommands),
    /// Extension management commands
    #[command(subcommand)]
    #[command(visible_alias = "ext")]
    Extension(ExtensionCommands),
    /// Authenticate with the Rise backend
    Login {
        /// Backend URL to authenticate with
        #[arg(long)]
        url: Option<String>,
        /// Use browser-based OAuth2 authorization code flow (default)
        #[arg(long, conflicts_with = "device")]
        browser: bool,
        /// Use device authorization flow
        #[arg(long, conflicts_with = "browser")]
        device: bool,
    },
    /// Project management commands
    #[command(subcommand)]
    #[command(visible_alias = "p")]
    Project(ProjectCommands),
    /// Build and run a container locally for development
    Run {
        /// Project name (optional, used to load environment variables)
        #[arg(long, short)]
        project: Option<String>,
        /// Container to run (required for multi-container projects). Selects
        /// which `[containers.X]` to build and run; use `rise compose up` to run
        /// all containers together.
        #[arg(long)]
        container: Option<String>,
        /// Load environment variables from the associated Rise project (non-secret only). Defaults to true.
        #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
        use_project_env: bool,
        /// Path to the directory containing the application
        #[arg(default_value = ".")]
        path: String,
        /// Target environment (e.g., 'staging'). Resolved from rise.toml if not specified.
        #[arg(long, short = 'E')]
        environment: Option<String>,
        /// HTTP port the application listens on (also sets PORT env var)
        #[arg(long, default_value = "8080")]
        http_port: u16,
        /// Port to publish on the host. Defaults to the selected container's
        /// port for --container, otherwise to --http-port.
        #[arg(long)]
        expose: Option<u16>,
        /// Runtime environment variables (format: KEY=VALUE, can be specified multiple times)
        #[arg(long = "env", short = 'e', value_parser = parse_key_val::<String, String>)]
        run_env: Vec<(String, String)>,
        #[command(flatten)]
        build_args: build::BuildArgs,
    },
    /// Run a project locally via Docker Compose
    #[command(subcommand)]
    Compose(ComposeCommands),
    /// Service account (workload identity) management commands
    #[command(subcommand)]
    #[command(visible_alias = "sa")]
    ServiceAccount(ServiceAccountCommands),
    /// Workload identity token commands (run inside a Rise deployment)
    #[command(subcommand)]
    Identity(IdentityCommands),
    /// Install and manage Rise skills for AI assistants (Crush, Claude Code)
    #[command(subcommand)]
    Skill(SkillCommands),
    /// Team management commands
    #[command(subcommand)]
    #[command(visible_alias = "t")]
    Team(TeamCommands),
}

#[derive(Subcommand, Debug)]
enum ComposeCommands {
    /// Build a project locally and run it via Docker Compose (ephemeral — no
    /// file is written to disk).
    Up {
        /// Path to the directory containing the application
        #[arg(default_value = ".")]
        path: String,
        /// Project name (optional, used to load environment variables)
        #[arg(long, short)]
        project: Option<String>,
        /// Target environment (e.g., 'staging'). Resolved from rise.toml if not specified.
        #[arg(long, short = 'E')]
        environment: Option<String>,
        /// HTTP port a single-container application listens on (also sets PORT env var).
        #[arg(long, default_value = "8080")]
        http_port: u16,
        /// Host port the Traefik router publishes (replicates `[routes]`).
        #[arg(long, default_value = "8080")]
        router_port: u16,
        /// Run in the background instead of streaming logs in the foreground.
        #[arg(long, short = 'd')]
        detach: bool,
        #[command(flatten)]
        build_args: build::BuildArgs,
    },
    /// Tear down a stack started by `rise compose up`.
    Down {
        /// Path to the directory containing the application
        #[arg(default_value = ".")]
        path: String,
        /// Project name (optional, used to derive the compose project name)
        #[arg(long, short)]
        project: Option<String>,
        #[command(flatten)]
        build_args: build::BuildArgs,
    },
    /// List the containers of a running stack.
    Ps {
        /// Path to the directory containing the application
        #[arg(default_value = ".")]
        path: String,
        /// Project name (optional, used to derive the compose project name)
        #[arg(long, short)]
        project: Option<String>,
        #[command(flatten)]
        build_args: build::BuildArgs,
    },
    /// Show logs from a running stack.
    Logs {
        /// Path to the directory containing the application
        #[arg(default_value = ".")]
        path: String,
        /// Project name (optional, used to derive the compose project name)
        #[arg(long, short)]
        project: Option<String>,
        /// Stream new log output until interrupted.
        #[arg(long, short)]
        follow: bool,
        /// Number of lines to show from the end of each container's log.
        #[arg(long)]
        tail: Option<String>,
        /// Restrict output to these containers (defaults to all).
        #[arg(long = "container", short = 'c')]
        containers: Vec<String>,
        #[command(flatten)]
        build_args: build::BuildArgs,
    },
    /// Write a persistent `compose.yaml` you can customize and run yourself.
    Generate {
        /// Path to the directory containing the application
        #[arg(default_value = ".")]
        path: String,
        /// Project name (optional, used to load environment variables)
        #[arg(long, short)]
        project: Option<String>,
        /// Target environment (e.g., 'staging'). Resolved from rise.toml if not specified.
        #[arg(long, short = 'E')]
        environment: Option<String>,
        /// HTTP port a single-container application listens on (also sets PORT env var).
        #[arg(long, default_value = "8080")]
        http_port: u16,
        /// Host port the Traefik router publishes (replicates `[routes]`).
        #[arg(long, default_value = "8080")]
        router_port: u16,
        /// Write to stdout instead of a file.
        #[arg(long)]
        stdout: bool,
        /// Output file path (defaults to `<path>/compose.yaml`).
        #[arg(long, short = 'o')]
        output: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum IdentityCommands {
    /// Request a workload identity token for an audience
    Token {
        /// Audience (`aud` claim) to request the token for
        #[arg(long)]
        audience: String,
        /// Bootstrap credential. Defaults to the credential file mounted into
        /// every Rise deployment (/var/run/secrets/rise/identity/credential).
        #[arg(long)]
        credential: Option<String>,
        /// Requested token lifetime in seconds. Capped by the server's configured maximum
        /// (default 900). Omitting this field uses the server maximum.
        #[arg(long)]
        ttl_seconds: Option<u64>,
    },
}

#[derive(Subcommand, Debug)]
enum SkillCommands {
    /// Install or update a Rise skill from GitHub into your AI assistant's skills directory
    Install {
        /// Skill name (defaults to the bundled 'rise-app-builder' skill)
        name: Option<String>,
        /// Git ref to fetch from (branch, tag, or sha). Defaults to 'develop'
        #[arg(long)]
        git_ref: Option<String>,
        /// Directory to install into. Defaults to ~/.claude/skills
        #[arg(long)]
        target: Option<std::path::PathBuf>,
    },
    /// List installed skills
    List {
        /// Directory to inspect. Defaults to ~/.claude/skills
        #[arg(long)]
        target: Option<std::path::PathBuf>,
    },
    /// Remove an installed skill
    #[command(visible_alias = "rm")]
    Uninstall {
        /// Skill name to remove
        name: String,
        /// Directory to remove from. Defaults to ~/.claude/skills
        #[arg(long)]
        target: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum ProjectCommands {
    /// Create a new project
    #[command(visible_alias = "c")]
    #[command(visible_alias = "new")]
    Create {
        /// Project name (read from rise.toml if not provided)
        name: Option<String>,
        /// Access class (e.g., public, private)
        #[arg(long, default_value = "public")]
        access_class: String,
        /// Owner (format: "user:email" or "team:name", defaults to current user)
        #[arg(long)]
        owner: Option<String>,
        /// URL to where the project code lives (e.g. a GitHub/GitLab repository)
        #[arg(long)]
        source_url: Option<String>,
        /// Path where to create rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Don't create a rise.toml file
        #[arg(long)]
        no_rise_toml: bool,
    },
    /// List all projects
    #[command(visible_alias = "ls")]
    #[command(visible_alias = "l")]
    List {},
    /// Show project details
    #[command(visible_alias = "s")]
    Show {
        /// Project name (optional if rise.toml contains [project] section)
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Update project
    #[command(visible_alias = "u")]
    #[command(visible_alias = "edit")]
    Update {
        /// Project name (optional if rise.toml contains [project] section)
        project: Option<String>,
        /// New project name
        #[arg(long)]
        name: Option<String>,
        /// New access class (e.g., public, private)
        #[arg(long)]
        access_class: Option<String>,
        /// Transfer ownership (format: "user:email" or "team:name")
        #[arg(long)]
        owner: Option<String>,
        /// URL to where the project code lives (e.g. a GitHub/GitLab repository). Use empty string to clear.
        #[arg(long)]
        source_url: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Delete a project
    #[command(visible_alias = "del")]
    #[command(visible_alias = "rm")]
    Delete {
        /// Project name (optional if rise.toml contains [project] section)
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Manage app users/teams (view-only access to deployed apps)
    #[command(subcommand)]
    AppUser(AppUserCommands),
}

#[derive(Subcommand, Debug)]
enum AppUserCommands {
    /// Add a user or team as an app user (view-only access to deployed app)
    #[command(visible_alias = "a")]
    Add {
        /// Project name (optional if rise.toml contains [project] section)
        project: Option<String>,
        /// User or team identifier (format: "user:email" or "team:name")
        identifier: String,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Remove a user or team from app users
    #[command(visible_alias = "rm")]
    #[command(visible_alias = "del")]
    Remove {
        /// Project name (optional if rise.toml contains [project] section)
        project: Option<String>,
        /// User or team identifier (format: "user:email" or "team:name")
        identifier: String,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// List all app users and teams
    #[command(visible_alias = "ls")]
    #[command(visible_alias = "l")]
    List {
        /// Project name (optional if rise.toml contains [project] section)
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
}

#[derive(Subcommand, Debug)]
enum TeamCommands {
    /// Create a new team
    #[command(visible_alias = "c")]
    #[command(visible_alias = "new")]
    Create {
        /// Team name
        name: String,
        /// Owner emails (comma-separated, defaults to current user)
        #[arg(long)]
        owners: Option<String>,
        /// Member emails (comma-separated, optional)
        #[arg(long, default_value = "")]
        members: String,
    },
    /// List all teams
    #[command(visible_alias = "ls")]
    #[command(visible_alias = "l")]
    List {},
    /// Show team details
    #[command(visible_alias = "s")]
    Show {
        /// Team name
        team: String,
    },
    /// Update team
    #[command(visible_alias = "u")]
    #[command(visible_alias = "edit")]
    Update {
        /// Team name
        team: String,
        /// New team name
        #[arg(long)]
        name: Option<String>,
        /// Add owners (comma-separated email addresses)
        #[arg(long)]
        add_owners: Option<String>,
        /// Remove owners (comma-separated email addresses)
        #[arg(long)]
        remove_owners: Option<String>,
        /// Add members (comma-separated email addresses)
        #[arg(long)]
        add_members: Option<String>,
        /// Remove members (comma-separated email addresses)
        #[arg(long)]
        remove_members: Option<String>,
    },
    /// Delete a team
    #[command(visible_alias = "del")]
    #[command(visible_alias = "rm")]
    Delete {
        /// Team name
        team: String,
    },
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
enum DeploymentCommands {
    /// Create a new deployment
    #[command(visible_alias = "c")]
    #[command(visible_alias = "new")]
    Create {
        #[command(flatten)]
        args: DeployArgs,
    },
    /// List deployments for a project
    #[command(visible_alias = "ls")]
    #[command(visible_alias = "l")]
    List {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Filter by deployment group
        #[arg(long, short)]
        group: Option<String>,
        /// Limit number of deployments to show
        #[arg(long, short, default_value = "10")]
        limit: usize,
    },
    /// Show deployment details
    #[command(visible_alias = "s")]
    Show {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Deployment ID
        deployment_id: String,
        /// Follow deployment until completion
        #[arg(long, short)]
        follow: bool,
        /// Timeout for following deployment
        #[arg(long, default_value = "5m")]
        timeout: String,
    },
    /// Stop all deployments in a group
    Stop {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Deployment group to stop
        #[arg(long, short)]
        group: String,
    },
    /// Show logs from a deployment
    Logs {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Deployment ID (YYYYMMDD-HHMMSS format)
        deployment_id: String,
        /// Follow log output (stream continuously)
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show from end of logs
        #[arg(long)]
        tail: Option<usize>,
        /// Show timestamps in log output
        #[arg(long)]
        timestamps: bool,
        /// Show logs since duration (e.g., "5m", "1h")
        #[arg(long)]
        since: Option<String>,
        /// Filter to lines with the given level. Repeat to allow multiple
        /// (e.g. `--level warn --level error`). Omit for all levels. The
        /// set of accepted values is server-defined; see
        /// `GET /api/v1/logs/capabilities` for the configured backend's
        /// vocabulary.
        #[arg(long = "level")]
        level: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ServiceAccountCommands {
    /// Create a new service account for a project
    #[command(visible_alias = "c")]
    #[command(visible_alias = "new")]
    Create {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// OIDC issuer URL (e.g., https://gitlab.com)
        #[arg(long)]
        issuer: String,
        /// Claims to match (format: key=value, can be specified multiple times)
        #[arg(long = "claim", value_parser = parse_key_val::<String, String>)]
        claims: Vec<(String, String)>,
    },
    /// List all service accounts for a project
    #[command(visible_alias = "ls")]
    #[command(visible_alias = "l")]
    List {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Show service account details
    #[command(visible_alias = "s")]
    #[command(visible_alias = "get")]
    Show {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Service account ID
        id: String,
    },
    /// Delete a service account
    #[command(visible_alias = "del")]
    #[command(visible_alias = "rm")]
    Delete {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Service account ID
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum EnvCommands {
    /// Set an environment variable for a project
    #[command(visible_alias = "s")]
    Set {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Variable name (e.g., DATABASE_URL)
        key: String,
        /// Variable value
        value: String,
        /// Mark as secret (encrypted at rest)
        #[arg(long)]
        secret: bool,
        /// Mark secret as protected (cannot be decrypted via API). Only applies to secrets. Defaults to true for secrets, must be false for non-secrets.
        #[arg(long)]
        protected: Option<bool>,
        /// Scope variable to a specific environment (e.g., 'staging'). Without this, the variable is global.
        #[arg(long, short = 'E')]
        environment: Option<String>,
    },
    /// List environment variables for a project
    #[command(visible_alias = "ls")]
    #[command(visible_alias = "l")]
    List {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Filter by environment (shows global + environment-scoped vars merged)
        #[arg(long, short = 'E')]
        environment: Option<String>,
    },
    /// Get the value of a specific environment variable
    #[command(visible_alias = "g")]
    Get {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Variable name
        key: String,
        /// Get environment-scoped variable
        #[arg(long, short = 'E')]
        environment: Option<String>,
    },
    /// Delete an environment variable from a project
    #[command(visible_alias = "unset")]
    #[command(visible_alias = "rm")]
    #[command(visible_alias = "del")]
    Delete {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Variable name
        key: String,
        /// Delete environment-scoped variable
        #[arg(long, short = 'E')]
        environment: Option<String>,
    },
    /// Import environment variables from a file
    #[command(visible_alias = "i")]
    Import {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Path to file containing environment variables
        /// Format: KEY=value or KEY=secret:value (for secrets)
        /// Lines starting with # are comments
        file: std::path::PathBuf,
        /// Scope imported variables to a specific environment
        #[arg(long, short = 'E')]
        environment: Option<String>,
    },
    /// Export resolved environment variables in dotfile format (KEY=value)
    #[command(visible_alias = "x")]
    Export {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Target environment (e.g., 'staging'). Resolved from rise.toml if not specified.
        #[arg(long, short = 'E')]
        environment: Option<String>,
    },
    /// Show environment variables for a deployment (read-only)
    ShowDeployment {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Deployment ID
        deployment_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum EnvironmentCommands {
    /// Create a new environment for a project
    #[command(visible_alias = "c")]
    #[command(visible_alias = "new")]
    Create {
        /// Environment name (e.g., 'staging', 'dev')
        name: String,
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Primary deployment group for this environment
        #[arg(long, short)]
        group: Option<String>,
        /// Set as the production environment (gets the production URL)
        #[arg(long)]
        production: bool,
        /// Badge color (green, blue, yellow, red, purple, orange, gray)
        #[arg(long, default_value = "green")]
        color: String,
    },
    /// List all environments for a project
    #[command(visible_alias = "ls")]
    #[command(visible_alias = "l")]
    List {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Show environment details
    #[command(visible_alias = "s")]
    Show {
        /// Environment name
        name: String,
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Update an environment
    #[command(visible_alias = "u")]
    #[command(visible_alias = "edit")]
    Update {
        /// Environment name
        name: String,
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// New environment name
        #[arg(long)]
        rename: Option<String>,
        /// Set primary deployment group (use empty string to clear)
        #[arg(long, short)]
        group: Option<String>,
        /// Set as the production environment
        #[arg(long)]
        production: Option<bool>,
        /// Badge color (green, blue, yellow, red, purple, orange, gray)
        #[arg(long)]
        color: Option<String>,
    },
    /// Delete an environment
    #[command(visible_alias = "del")]
    #[command(visible_alias = "rm")]
    Delete {
        /// Environment name
        name: String,
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
}

#[derive(Subcommand, Debug)]
enum DomainCommands {
    /// Add a custom domain to a project
    #[command(visible_alias = "a")]
    Add {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Domain name (e.g., example.com)
        domain: String,
        /// Environment to attach the domain to. Defaults to the project's production environment.
        #[arg(long, short = 'e')]
        environment: Option<String>,
    },
    /// List custom domains for a project
    #[command(visible_alias = "ls")]
    #[command(visible_alias = "l")]
    List {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Remove a custom domain from a project
    #[command(visible_alias = "rm")]
    #[command(visible_alias = "del")]
    Remove {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Domain name
        domain: String,
    },
}

#[derive(Subcommand, Debug)]
enum ExtensionCommands {
    /// Create or update an extension for a project
    #[command(visible_alias = "c")]
    #[command(visible_alias = "new")]
    Create {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Extension name
        extension: String,
        /// Extension type (handler identifier, e.g., "aws-rds-provisioner", "oauth")
        #[arg(long)]
        r#type: String,
        /// Extension spec as JSON string
        #[arg(long)]
        spec: String,
    },
    /// Update an extension (full replace)
    #[command(visible_alias = "u")]
    Update {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Extension name
        extension: String,
        /// Extension spec as JSON string
        #[arg(long)]
        spec: String,
    },
    /// Patch an extension (partial update, null values unset fields)
    #[command(visible_alias = "p")]
    Patch {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Extension name
        extension: String,
        /// Extension spec patch as JSON string
        #[arg(long)]
        spec: String,
    },
    /// List all extensions for a project
    #[command(visible_alias = "ls")]
    #[command(visible_alias = "l")]
    List {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Show extension details
    #[command(visible_alias = "s")]
    Show {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Extension name
        extension: String,
    },
    /// Delete an extension from a project
    #[command(visible_alias = "rm")]
    #[command(visible_alias = "del")]
    Delete {
        /// Project name (optional if rise.toml contains [project] section)
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Directory containing rise.toml (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: String,
        /// Extension name
        extension: String,
    },
}

/// Parse a single key-value pair
fn parse_key_val<T, U>(
    s: &str,
) -> Result<(T, U), Box<dyn std::error::Error + Send + Sync + 'static>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
    U: std::str::FromStr,
    U::Err: std::error::Error + Send + Sync + 'static,
{
    let pos = s
        .find('=')
        .ok_or_else(|| format!("invalid KEY=value: no `=` found in `{s}`"))?;
    Ok((s[..pos].parse()?, s[pos + 1..].parse()?))
}

/// Whether to colour log output.
///
/// Defaults to colouring only a terminal. Anything collecting the stream
/// verbatim -- a CloudWatch log group behind an ECS task, `docker logs`, a CI
/// job, a pipe -- shows escape codes as literal text otherwise, which is how
/// this reads in the ECS console. That covers every such collector rather than
/// detecting one of them.
///
/// `RISE_LOG_COLOR` overrides: `always`/`never` (`1`/`0`, `true`/`false`), or
/// `auto` for the default. `NO_COLOR` disables when set to anything non-empty,
/// per <https://no-color.org>.
fn ansi_enabled(rise_log_color: Option<&str>, no_color: Option<&str>, is_terminal: bool) -> bool {
    match rise_log_color.map(str::trim) {
        Some("always" | "1" | "true" | "yes") => return true,
        Some("never" | "0" | "false" | "no") => return false,
        // Anything else, "auto" included, falls through: an unrecognised value
        // must not be louder than NO_COLOR or quieter than the default.
        _ => {}
    }
    if no_color.is_some_and(|v| !v.is_empty()) {
        return false;
    }
    is_terminal
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing for all commands
    let ansi = ansi_enabled(
        std::env::var("RISE_LOG_COLOR").ok().as_deref(),
        std::env::var("NO_COLOR").ok().as_deref(),
        std::io::IsTerminal::is_terminal(&std::io::stderr()),
    );
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(ansi),
        )
        .init();

    let cli = Cli::parse();

    // Transform Deploy command to Deployment::Create for zero-duplication aliasing
    let cli_command = match cli.command {
        Commands::Deploy { args } => Commands::Deployment(DeploymentCommands::Create { args }),
        other => other,
    };

    // Backend commands don't need CLI config (they use Settings from TOML/env vars)
    // Only client commands (login, project, team, deployment, service-account) need it
    #[cfg(feature = "backend")]
    if let Commands::Backend(backend_cmd) = &cli_command {
        return backend::handle_backend_command(backend_cmd.clone()).await;
    }

    // Load CLI config for client commands
    let http_client = Client::new();
    let mut config = config::Config::load()?;
    let backend_url = config.get_backend_url();

    // Check version compatibility for all commands except Login
    // (Backend commands are handled above and don't use the HTTP API; Login might use a custom URL)
    if !matches!(&cli_command, Commands::Login { .. }) {
        // Non-fatal version check - just warns user
        let _ = version::check_version_compatibility(&http_client, &backend_url).await;
    }

    match &cli_command {
        Commands::Login {
            url,
            browser: _,
            device,
        } => {
            // Use provided URL or fall back to config default
            let login_url = url
                .as_deref()
                .map(config::normalize_backend_url)
                .unwrap_or_else(|| backend_url.clone());

            if *device {
                // Device flow (explicit)
                login::handle_device_flow(&http_client, &login_url, &mut config, url.as_deref())
                    .await?;
            } else {
                // Authorization code flow with PKCE (default)
                login::handle_authorization_code_flow(
                    &http_client,
                    &login_url,
                    &mut config,
                    url.as_deref(),
                )
                .await?;
            }
        }
        #[cfg(feature = "backend")]
        Commands::Backend(_) => {
            // Already handled above before config loading
            unreachable!("Backend commands should have been handled earlier")
        }
        Commands::Encrypt { plaintext } => {
            cli::encrypt::encrypt_command(&config, plaintext.clone()).await?;
        }
        Commands::Project(project_cmd) => match project_cmd {
            ProjectCommands::Create {
                name,
                access_class,
                owner,
                source_url,
                path,
                no_rise_toml,
            } => {
                project::create_project(
                    &http_client,
                    &backend_url,
                    &config,
                    name,
                    access_class,
                    owner.clone(),
                    source_url.clone(),
                    path,
                    *no_rise_toml,
                )
                .await?;
            }
            ProjectCommands::List {} => {
                project::list_projects(&http_client, &backend_url, &config).await?;
            }
            ProjectCommands::Show { project, path } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                project::show_project(&http_client, &backend_url, &config, &project_name).await?;
            }
            ProjectCommands::Update {
                project,
                name,
                access_class,
                owner,
                source_url,
                path,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                // Convert "--source-url ''" (empty string) to Some(None) to clear
                let source_url_opt: Option<Option<String>> =
                    source_url
                        .as_ref()
                        .map(|u| if u.is_empty() { None } else { Some(u.clone()) });
                project::update_project(
                    &http_client,
                    &backend_url,
                    &config,
                    &project_name,
                    name.clone(),
                    access_class.clone(),
                    owner.clone(),
                    source_url_opt,
                )
                .await?;
            }
            ProjectCommands::Delete { project, path } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                project::delete_project(&http_client, &backend_url, &config, &project_name).await?;
            }
            ProjectCommands::AppUser(app_user_cmd) => {
                // App-user management is project-scoped; resolve the project once
                // up front, since every subcommand below targets it.
                let (project, path) = match app_user_cmd {
                    AppUserCommands::Add { project, path, .. }
                    | AppUserCommands::Remove { project, path, .. }
                    | AppUserCommands::List { project, path } => (project, path),
                };
                let project_name = resolve_project_name(project.clone(), path)?;
                let token =
                    cli::token_source::resolve_token_with_retry(&http_client, &config).await?;
                match app_user_cmd {
                    AppUserCommands::Add { identifier, .. } => {
                        cli::project::add_app_user(
                            &http_client,
                            &backend_url,
                            &token,
                            &project_name,
                            identifier,
                        )
                        .await?;
                    }
                    AppUserCommands::Remove { identifier, .. } => {
                        cli::project::remove_app_user(
                            &http_client,
                            &backend_url,
                            &token,
                            &project_name,
                            identifier,
                        )
                        .await?;
                    }
                    AppUserCommands::List { .. } => {
                        cli::project::list_app_users(
                            &http_client,
                            &backend_url,
                            &token,
                            &project_name,
                        )
                        .await?;
                    }
                }
            }
        },
        Commands::Team(team_cmd) => match team_cmd {
            TeamCommands::Create {
                name,
                owners,
                members,
            } => {
                let owners_vec: Option<Vec<String>> = owners.as_ref().map(|o| {
                    o.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                });
                let members_vec: Vec<String> = members
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                team::create_team(
                    &http_client,
                    &backend_url,
                    &config,
                    name,
                    owners_vec,
                    members_vec,
                )
                .await?;
            }
            TeamCommands::List {} => {
                team::list_teams(&http_client, &backend_url, &config).await?;
            }
            TeamCommands::Show { team } => {
                team::show_team(&http_client, &backend_url, &config, team).await?;
            }
            TeamCommands::Update {
                team,
                name,
                add_owners,
                remove_owners,
                add_members,
                remove_members,
            } => {
                let add_owners_vec: Vec<String> = add_owners
                    .as_ref()
                    .map(|s| {
                        s.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                let remove_owners_vec: Vec<String> = remove_owners
                    .as_ref()
                    .map(|s| {
                        s.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                let add_members_vec: Vec<String> = add_members
                    .as_ref()
                    .map(|s| {
                        s.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                let remove_members_vec: Vec<String> = remove_members
                    .as_ref()
                    .map(|s| {
                        s.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();

                team::update_team(
                    &http_client,
                    &backend_url,
                    &config,
                    team,
                    name.clone(),
                    add_owners_vec,
                    remove_owners_vec,
                    add_members_vec,
                    remove_members_vec,
                )
                .await?;
            }
            TeamCommands::Delete { team } => {
                team::delete_team(&http_client, &backend_url, &config, team).await?;
            }
        },
        Commands::Deployment(deployment_cmd) => match deployment_cmd {
            DeploymentCommands::Create { args } => {
                // Load rise.toml once — reused for project name resolution,
                // environment resolution, env var collection, and build config.
                let toml_config = build::config::load_full_project_config(&args.path)
                    .context("Failed to load rise.toml")?;

                let project_name = resolve_project_name_with_config(
                    args.project.clone(),
                    &args.path,
                    toml_config.as_ref(),
                )?;

                // Both --image and --from cannot be specified together
                if args.image.is_some() && args.from.is_some() {
                    eprintln!("Error: Cannot specify both --image and --from");
                    std::process::exit(1);
                }

                // --push-image requires --image
                if args.push_image && args.image.is_none() {
                    eprintln!("Error: --push-image requires --image");
                    std::process::exit(1);
                }

                // --push-image is incompatible with --from
                if args.push_image && args.from.is_some() {
                    eprintln!("Error: --push-image cannot be used with --from");
                    std::process::exit(1);
                }

                // For pre-built images, --http-port is required since we can't infer it
                if let Some(image) = &args.image {
                    if args.http_port.is_none() {
                        eprintln!("Error: --http-port is required when using --image");
                        eprintln!(
                            "Example: rise deployment create {} --image {} --http-port 80",
                            project_name, image
                        );
                        std::process::exit(1);
                    }
                }

                // Resolve environment: explicit --environment flag takes precedence,
                // otherwise fall back to the `default = true` environment from rise.toml.
                let resolved_environment =
                    resolve_environment(args.environment.clone(), toml_config.as_ref());

                // Collect runtime env overrides with source tracking.
                // All toml vars are sent tagged with for_environment; the server
                // filters them after resolving the deployment's target environment.
                // Priority: toml < cli (later overrides earlier for same key within
                // the same for_environment scope).
                let mut env_overrides: Vec<deployment::EnvOverride> = Vec::new();

                // 1. Collect [project.env] vars from rise.toml (global, no for_environment)
                if let Some(ref cfg) = toml_config {
                    if let Some(ref project_config) = cfg.project {
                        for (key, value) in &project_config.env {
                            env_overrides.push(deployment::EnvOverride {
                                key: key.clone(),
                                value: value.clone(),
                                is_secret: false,
                                is_protected: false,
                                source: Some("toml".to_string()),
                                for_environment: None,
                            });
                        }
                    }

                    // 2. Collect [environments.*.env] vars from rise.toml, tagged with
                    //    for_environment so the server can filter after resolution.
                    for (env_name, env_config) in &cfg.environments {
                        for (key, value) in &env_config.env {
                            env_overrides.push(deployment::EnvOverride {
                                key: key.clone(),
                                value: value.clone(),
                                is_secret: false,
                                is_protected: false,
                                source: Some("toml".to_string()),
                                for_environment: Some(env_name.clone()),
                            });
                        }
                    }
                }

                // 3. CLI overrides (global, override toml for same key)

                // --env KEY=VALUE → plain text
                for (key, value) in &args.env {
                    env_overrides.retain(|o| o.key != *key);
                    env_overrides.push(deployment::EnvOverride {
                        key: key.clone(),
                        value: value.clone(),
                        is_secret: false,
                        is_protected: false,
                        source: Some("cli".to_string()),
                        for_environment: None,
                    });
                }

                // --secret-env KEY=VALUE → secret, retrievable
                for (key, value) in &args.secret_env {
                    env_overrides.retain(|o| o.key != *key);
                    env_overrides.push(deployment::EnvOverride {
                        key: key.clone(),
                        value: value.clone(),
                        is_secret: true,
                        is_protected: false,
                        source: Some("cli".to_string()),
                        for_environment: None,
                    });
                }

                // --protected-env KEY=VALUE → secret, NOT retrievable
                for (key, value) in &args.protected_env {
                    env_overrides.retain(|o| o.key != *key);
                    env_overrides.push(deployment::EnvOverride {
                        key: key.clone(),
                        value: value.clone(),
                        is_secret: true,
                        is_protected: true,
                        source: Some("cli".to_string()),
                        for_environment: None,
                    });
                }

                // --env-file → parse file
                if let Some(ref env_file_path) = args.env_file {
                    let contents = tokio::fs::read_to_string(env_file_path)
                        .await
                        .with_context(|| format!("Failed to read env file: {}", env_file_path))?;
                    let parsed = cli::env::parse_env_file(&contents)
                        .with_context(|| format!("Failed to parse env file: {}", env_file_path))?;
                    for var in parsed {
                        env_overrides.retain(|o| o.key != var.key);
                        env_overrides.push(deployment::EnvOverride {
                            key: var.key,
                            value: var.value,
                            is_secret: var.is_secret,
                            is_protected: var.is_secret, // secrets from file are protected by default
                            source: Some("cli".to_string()),
                            for_environment: None,
                        });
                    }
                }

                // Pass through the http_port option - server will resolve from:
                // 1. Explicit http_port (if provided)
                // 2. Source deployment's http_port (if --from is used)
                // 3. Project's PORT env var (if set)
                // 4. Default 8080
                // Resolve deployment resources from CLI flags > rise.toml environment > rise.toml global
                let toml_env_deploy = resolved_environment.as_deref().and_then(|env_name| {
                    toml_config
                        .as_ref()
                        .and_then(|c| c.environments.get(env_name))
                        .and_then(|e| e.deploy.as_ref())
                });
                let toml_global_deploy = toml_config.as_ref().and_then(|c| c.deploy.as_ref());

                let replicas = args
                    .replicas
                    .or_else(|| toml_env_deploy.and_then(|d| d.replicas))
                    .or_else(|| toml_global_deploy.and_then(|d| d.replicas));
                let cpu = args
                    .cpu
                    .clone()
                    .or_else(|| toml_env_deploy.and_then(|d| d.cpu.clone()))
                    .or_else(|| toml_global_deploy.and_then(|d| d.cpu.clone()));
                let memory = args
                    .memory
                    .clone()
                    .or_else(|| toml_env_deploy.and_then(|d| d.memory.clone()))
                    .or_else(|| toml_global_deploy.and_then(|d| d.memory.clone()));

                deployment::create_deployment(
                    &http_client,
                    &backend_url,
                    &config,
                    deployment::DeploymentOptions {
                        project_name: &project_name,
                        path: &args.path,
                        image: args.image.as_deref(),
                        group: args.group.as_deref(),
                        environment: resolved_environment.as_deref(),
                        expires_in: args.expire.as_deref(),
                        http_port: args.http_port,
                        build_args: &args.build_args,
                        from_deployment: args.from.as_deref(),
                        use_source_env_vars: args.use_source_env_vars,
                        push_image: args.push_image,
                        env_overrides,
                        job_url: args.job_url.clone(),
                        pull_request_url: args.pull_request_url.clone(),
                        git_repository_url: args.git_repository.clone(),
                        toml_config,
                        replicas,
                        cpu,
                        memory,
                    },
                )
                .await?;
            }
            DeploymentCommands::List {
                project,
                path,
                group,
                limit,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                deployment::list_deployments(
                    &http_client,
                    &backend_url,
                    &config,
                    &project_name,
                    group.as_deref(),
                    *limit,
                )
                .await?;
            }
            DeploymentCommands::Show {
                project,
                path,
                deployment_id,
                follow,
                timeout,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                deployment::show_deployment(
                    &http_client,
                    &backend_url,
                    &config,
                    &project_name,
                    deployment_id,
                    *follow,
                    timeout,
                )
                .await?;
            }
            DeploymentCommands::Stop {
                project,
                path,
                group,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                deployment::stop_deployments_by_group(
                    &http_client,
                    &backend_url,
                    &config,
                    &project_name,
                    group,
                )
                .await?;
            }
            DeploymentCommands::Logs {
                project,
                path,
                deployment_id,
                follow,
                tail,
                timestamps,
                since,
                level,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                let token =
                    cli::token_source::resolve_token_with_retry(&http_client, &config).await?;
                deployment::get_logs(
                    &http_client,
                    &backend_url,
                    &token,
                    deployment::GetLogsParams {
                        project: &project_name,
                        deployment_id,
                        follow: *follow,
                        tail: *tail,
                        timestamps: *timestamps,
                        since: since.as_deref(),
                        levels: level,
                    },
                )
                .await?;
            }
        },
        Commands::ServiceAccount(sa_cmd) => match sa_cmd {
            ServiceAccountCommands::Create {
                project,
                path,
                issuer,
                claims,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                let claims_map: std::collections::HashMap<String, String> =
                    claims.iter().cloned().collect();

                // Validate aud claim requirement
                if !claims_map.contains_key("aud") {
                    eprintln!(
                        "Error: The 'aud' (audience) claim is required for service accounts."
                    );
                    eprintln!("       Recommended value: https://rise.example.net");
                    eprintln!("       Example: --claim aud=https://rise.example.net");
                    std::process::exit(1);
                }

                // Validate at least one additional claim
                if claims_map.len() < 2 {
                    eprintln!("Error: At least one claim in addition to 'aud' is required.");
                    eprintln!(
                        "       Example: --claim aud=https://rise.example.net --claim project_path=myorg/myrepo"
                    );
                    std::process::exit(1);
                }

                service_account::create_service_account(
                    &http_client,
                    &backend_url,
                    &config,
                    &project_name,
                    issuer,
                    claims_map,
                )
                .await?;
            }
            ServiceAccountCommands::List { project, path } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                service_account::list_service_accounts(
                    &http_client,
                    &backend_url,
                    &config,
                    &project_name,
                )
                .await?;
            }
            ServiceAccountCommands::Show { project, path, id } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                service_account::show_service_account(
                    &http_client,
                    &backend_url,
                    &config,
                    &project_name,
                    id,
                )
                .await?;
            }
            ServiceAccountCommands::Delete { project, path, id } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                service_account::delete_service_account(
                    &http_client,
                    &backend_url,
                    &config,
                    &project_name,
                    id,
                )
                .await?;
            }
        },
        Commands::Identity(identity_cmd) => match identity_cmd {
            IdentityCommands::Token {
                audience,
                credential,
                ttl_seconds,
            } => {
                cli::identity::token_command(
                    &http_client,
                    audience,
                    credential.as_deref(),
                    *ttl_seconds,
                )
                .await?;
            }
        },
        Commands::Skill(skill_cmd) => match skill_cmd {
            SkillCommands::Install {
                name,
                git_ref,
                target,
            } => {
                cli::skill::install_command(
                    name.as_ref().cloned(),
                    git_ref.as_ref().cloned(),
                    target.as_ref().cloned(),
                )
                .await?;
            }
            SkillCommands::List { target } => {
                cli::skill::list_command(target.clone())?;
            }
            SkillCommands::Uninstall { name, target } => {
                cli::skill::uninstall_command(name.clone(), target.clone())?;
            }
        },
        Commands::Environment(env_cmd) => {
            environment::handle_environment_command(&http_client, &backend_url, &config, env_cmd)
                .await?;
        }
        Commands::Env(env_cmd) => {
            // Env-var management is project-scoped; resolve the project once up
            // front, since every subcommand below targets it.
            let (project, path) = match env_cmd {
                EnvCommands::Set { project, path, .. }
                | EnvCommands::List { project, path, .. }
                | EnvCommands::Get { project, path, .. }
                | EnvCommands::Delete { project, path, .. }
                | EnvCommands::Import { project, path, .. }
                | EnvCommands::Export { project, path, .. }
                | EnvCommands::ShowDeployment { project, path, .. } => (project, path),
            };
            let project_name = resolve_project_name(project.clone(), path)?;
            let token = cli::token_source::resolve_token_with_retry(&http_client, &config).await?;
            match env_cmd {
                EnvCommands::Set {
                    key,
                    value,
                    secret,
                    protected,
                    environment,
                    ..
                } => {
                    // Protected defaults to true for secrets, false for non-secrets
                    // Can be explicitly overridden with --protected flag
                    let is_protected = protected.unwrap_or(*secret);
                    env::set_env(
                        &http_client,
                        &backend_url,
                        &token,
                        &project_name,
                        key,
                        value,
                        *secret,
                        is_protected,
                        environment.as_deref(),
                    )
                    .await?;
                }
                EnvCommands::List { environment, .. } => {
                    env::list_env(
                        &http_client,
                        &backend_url,
                        &token,
                        &project_name,
                        environment.as_deref(),
                    )
                    .await?;
                }
                EnvCommands::Get {
                    key, environment, ..
                } => {
                    env::get_env(
                        &http_client,
                        &backend_url,
                        &token,
                        &project_name,
                        key,
                        environment.as_deref(),
                    )
                    .await?;
                }
                EnvCommands::Delete {
                    key, environment, ..
                } => {
                    env::unset_env(
                        &http_client,
                        &backend_url,
                        &token,
                        &project_name,
                        key,
                        environment.as_deref(),
                    )
                    .await?;
                }
                EnvCommands::Import {
                    file, environment, ..
                } => {
                    env::import_env(
                        &http_client,
                        &backend_url,
                        &token,
                        &project_name,
                        file,
                        environment.as_deref(),
                    )
                    .await?;
                }
                EnvCommands::Export {
                    path, environment, ..
                } => {
                    // Reuses the project resolved above; loads config only for
                    // the environment lookup the export needs.
                    let toml_config = build::config::load_full_project_config(path)?;
                    let resolved_env =
                        resolve_environment(environment.clone(), toml_config.as_ref());
                    env::export_env(
                        &http_client,
                        &backend_url,
                        &token,
                        &project_name,
                        resolved_env.as_deref(),
                    )
                    .await?;
                }
                EnvCommands::ShowDeployment { deployment_id, .. } => {
                    env::list_deployment_env(
                        &http_client,
                        &backend_url,
                        &token,
                        &project_name,
                        deployment_id,
                    )
                    .await?;
                }
            }
        }
        Commands::Domain(domain_cmd) => {
            // Custom-domain management is project-scoped; resolve the project once
            // up front, since every subcommand below targets it.
            let (project, path) = match domain_cmd {
                DomainCommands::Add { project, path, .. }
                | DomainCommands::List { project, path }
                | DomainCommands::Remove { project, path, .. } => (project, path),
            };
            let project_name = resolve_project_name(project.clone(), path)?;
            let token = cli::token_source::resolve_token_with_retry(&http_client, &config).await?;
            match domain_cmd {
                DomainCommands::Add {
                    domain,
                    environment,
                    ..
                } => {
                    domain::add_domain(
                        &http_client,
                        &backend_url,
                        &token,
                        &project_name,
                        domain,
                        environment.as_deref(),
                    )
                    .await?;
                }
                DomainCommands::List { .. } => {
                    domain::list_domains(&http_client, &backend_url, &token, &project_name).await?;
                }
                DomainCommands::Remove { domain, .. } => {
                    domain::remove_domain(
                        &http_client,
                        &backend_url,
                        &token,
                        &project_name,
                        domain,
                    )
                    .await?;
                }
            }
        }
        Commands::Extension(extension_cmd) => match extension_cmd {
            ExtensionCommands::Create {
                project,
                path,
                extension,
                r#type,
                spec,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                let spec: serde_json::Value =
                    serde_json::from_str(spec).context("Failed to parse spec as JSON")?;
                extension::create_extension(&project_name, extension, r#type, spec).await?;
            }
            ExtensionCommands::Update {
                project,
                path,
                extension,
                spec,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                let spec: serde_json::Value =
                    serde_json::from_str(spec).context("Failed to parse spec as JSON")?;
                extension::update_extension(&project_name, extension, spec).await?;
            }
            ExtensionCommands::Patch {
                project,
                path,
                extension,
                spec,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                let spec: serde_json::Value =
                    serde_json::from_str(spec).context("Failed to parse spec as JSON")?;
                extension::patch_extension(&project_name, extension, spec).await?;
            }
            ExtensionCommands::List { project, path } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                extension::list_extensions(&project_name).await?;
            }
            ExtensionCommands::Show {
                project,
                path,
                extension,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                extension::show_extension(&project_name, extension).await?;
            }
            ExtensionCommands::Delete {
                project,
                path,
                extension,
            } => {
                let project_name = resolve_project_name(project.clone(), path)?;
                extension::delete_extension(&project_name, extension).await?;
            }
        },
        Commands::Build {
            tag,
            path,
            push,
            build_args,
        } => {
            let push_mode = if *push && build_args.separate_push {
                build::BuildPushMode::Deferred
            } else if *push {
                build::BuildPushMode::Inline
            } else {
                build::BuildPushMode::Disabled
            };
            let options = build::BuildOptions::from_build_args(
                &config,
                tag.clone(),
                path.clone(),
                build_args,
                None,
                None,
            )
            .with_push_mode(push_mode);
            let container_cli = options.container_cli.command().to_string();

            build::build_image(options)?;
            if *push && build_args.separate_push {
                build::docker_push(&container_cli, tag)?;
            }
        }
        Commands::Run {
            project,
            container,
            use_project_env,
            path,
            environment,
            http_port,
            expose,
            run_env,
            build_args,
        } => {
            // Resolve environment from --environment flag or rise.toml default
            let toml_config = build::config::load_full_project_config(path)?;
            let resolved_env = resolve_environment(environment.clone(), toml_config.as_ref());

            cli::run::run_locally(
                &http_client,
                &config,
                cli::run::RunOptions {
                    project_name: project.as_deref(),
                    container: container.as_deref(),
                    use_project_env: *use_project_env,
                    path,
                    environment: resolved_env.as_deref(),
                    http_port: *http_port,
                    expose: *expose,
                    run_env,
                    build_args,
                },
            )
            .await?;
        }
        Commands::Compose(compose_cmd) => match compose_cmd {
            ComposeCommands::Up {
                path,
                project,
                environment,
                http_port,
                router_port,
                detach,
                build_args,
            } => {
                let toml_config = build::config::load_full_project_config(path)?;
                let resolved_env = resolve_environment(environment.clone(), toml_config.as_ref());
                cli::compose::up(
                    &http_client,
                    &config,
                    cli::compose::UpOptions {
                        path,
                        project: project.as_deref(),
                        environment: resolved_env.as_deref(),
                        http_port: *http_port,
                        router_port: *router_port,
                        detach: *detach,
                        build_args,
                    },
                )
                .await?;
            }
            ComposeCommands::Down {
                path,
                project,
                build_args,
            } => {
                cli::compose::down(
                    &config,
                    cli::compose::DownOptions {
                        path,
                        project: project.as_deref(),
                        build_args,
                    },
                )
                .await?;
            }
            ComposeCommands::Ps {
                path,
                project,
                build_args,
            } => {
                cli::compose::ps(
                    &config,
                    cli::compose::PsOptions {
                        path,
                        project: project.as_deref(),
                        build_args,
                    },
                )
                .await?;
            }
            ComposeCommands::Logs {
                path,
                project,
                follow,
                tail,
                containers,
                build_args,
            } => {
                cli::compose::logs(
                    &config,
                    cli::compose::LogsOptions {
                        path,
                        project: project.as_deref(),
                        follow: *follow,
                        tail: tail.clone(),
                        containers,
                        build_args,
                    },
                )
                .await?;
            }
            ComposeCommands::Generate {
                path,
                project,
                environment,
                http_port,
                router_port,
                stdout,
                output,
            } => {
                let toml_config = build::config::load_full_project_config(path)?;
                let resolved_env = resolve_environment(environment.clone(), toml_config.as_ref());
                cli::compose::generate(
                    &http_client,
                    &config,
                    cli::compose::GenerateOptions {
                        path,
                        project: project.as_deref(),
                        environment: resolved_env.as_deref(),
                        http_port: *http_port,
                        router_port: *router_port,
                        stdout: *stdout,
                        output: output.as_deref(),
                    },
                )
                .await?;
            }
        },
        Commands::Deploy { .. } => {
            // Already transformed to Deployment(Create) above
            unreachable!("Deploy command should have been transformed to Deployment(Create)")
        }
    }

    Ok(())
}

#[cfg(test)]
mod log_color_tests {
    use super::ansi_enabled;

    #[test]
    fn a_terminal_is_coloured_and_a_pipe_is_not() {
        assert!(ansi_enabled(None, None, true));
        assert!(!ansi_enabled(None, None, false));
    }

    #[test]
    fn rise_log_color_overrides_both_the_terminal_and_no_color() {
        assert!(ansi_enabled(Some("always"), Some("1"), false));
        assert!(!ansi_enabled(Some("never"), None, true));
    }

    #[test]
    fn no_color_disables_colour_on_a_terminal() {
        assert!(!ansi_enabled(None, Some("1"), true));
        // Set but empty is not "set", per the convention.
        assert!(ansi_enabled(None, Some(""), true));
    }

    #[test]
    fn an_unrecognised_override_defers_rather_than_forcing() {
        assert!(ansi_enabled(Some("auto"), None, true));
        assert!(!ansi_enabled(Some("auto"), None, false));
        assert!(!ansi_enabled(Some("banana"), Some("1"), true));
    }
}
