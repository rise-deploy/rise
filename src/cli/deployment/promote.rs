//! `rise promote` and the preview shown before creating a deployment from
//! another one (`rise deploy --from`): the server's plan for the create and the
//! variables it will change.

use std::fmt::Write as _;
use std::io::{BufRead as _, IsTerminal as _, Write as _};

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing::{debug, info, warn};

use super::core::{
    describe_api_error, follow_created_deployment, report_create_response, CreateDeploymentPayload,
    DeploymentOutputOptions, EnvOverride,
};
use crate::cli::auth_hint::AuthHintRequest;
use crate::config::Config;
use crate::token_source::token_with_retry;

/// What `GET /projects/{p}/deployments/{id}/plan` says creating a deployment
/// from `{id}` into a target would do. Every field defaults so a server that
/// adds fields, or leaves some out, still parses.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct CreateFromPlan {
    /// `redeploy` (same environment) or `promote` (another environment).
    pub kind: String,
    pub source: PlanSource,
    pub target: PlanTarget,
    pub pinned_image: Option<String>,
    pub gate: PlanGate,
    pub migration: PlanMigration,
    pub adjustments: Vec<ResourceAdjustment>,
    pub use_source_env_vars_allowed: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct PlanSource {
    pub deployment_id: String,
    pub environment: String,
    pub status: String,
    pub image: Option<String>,
    pub image_digest: Option<String>,
    pub migration_version: Option<i64>,
    pub git_revision: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct PlanTarget {
    pub environment: String,
    pub group: String,
    pub current: Option<PlanCurrent>,
    pub migration_high_water: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct PlanCurrent {
    pub deployment_id: String,
    pub status: String,
    pub image: Option<String>,
    pub image_digest: Option<String>,
    pub migration_version: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct PlanGate {
    /// `allowed`, `admin_bypass` or `denied`.
    pub decision: String,
    pub promotion_source_environment: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct PlanMigration {
    /// `ok`, `unknown` or `crossed`.
    pub status: String,
    pub candidate_version: Option<i64>,
    pub environment_version: Option<i64>,
    pub barriers: Vec<PlanBarrier>,
}

/// A recorded migration barrier.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct PlanBarrier {
    pub version: i64,
    pub reason: String,
    pub recorded_by: Option<String>,
    pub recorded_at: String,
}

/// An inherited resource value changed to fit the target environment.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct ResourceAdjustment {
    pub container: String,
    pub field: String,
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// `GET /projects/{p}/deployments/{id}/env/diff`: the source's variables
/// against what a deployment created from it gets.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct EnvVarDiff {
    pub environment: Option<String>,
    pub changes: Vec<EnvVarDiffEntry>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct EnvVarDiffEntry {
    pub key: String,
    /// `added`, `removed`, `changed`, `unchanged` or `unknown`.
    pub change: String,
    pub is_secret: bool,
    pub is_protected: bool,
    pub from: Option<String>,
    pub to: Option<String>,
    pub from_source: Option<String>,
    pub to_source: Option<String>,
}

/// One barrier as a line: `v5: drops column foo (recorded 2026-10-01 by a@b)`.
pub(super) fn format_barrier(barrier: &PlanBarrier) -> String {
    let mut line = format!("v{}: {}", barrier.version, barrier.reason);
    let date = barrier
        .recorded_at
        .get(..10)
        .unwrap_or(barrier.recorded_at.as_str());
    match (date.is_empty(), barrier.recorded_by.as_deref()) {
        (false, Some(by)) => write!(line, " (recorded {} by {})", date, by),
        (false, None) => write!(line, " (recorded {})", date),
        (true, Some(by)) => write!(line, " (recorded by {})", by),
        (true, None) => Ok(()),
    }
    .expect("writing to a String cannot fail");
    line
}

/// One adjustment as a line: `app replicas 1 → 2 (production requires ≥ 2)`.
pub(super) fn format_adjustment(adjustment: &ResourceAdjustment) -> String {
    let mut line = format!(
        "{} {} {} → {}",
        adjustment.container, adjustment.field, adjustment.from, adjustment.to
    );
    if !adjustment.reason.is_empty() {
        write!(line, " ({})", adjustment.reason).expect("writing to a String cannot fail");
    }
    line
}

fn version_label(version: Option<i64>) -> String {
    version
        .map(|v| format!("v{}", v))
        .unwrap_or_else(|| "undeclared".to_string())
}

fn format_migration(migration: &PlanMigration) -> Vec<String> {
    let candidate = version_label(migration.candidate_version);
    let mut lines = Vec::new();
    match migration.status.as_str() {
        "crossed" => {
            lines.push(format!(
                "{} → environment at {} (crosses {} migration barrier{})",
                candidate,
                version_label(migration.environment_version),
                migration.barriers.len(),
                if migration.barriers.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
            for barrier in &migration.barriers {
                lines.push(format!("  {}", format_barrier(barrier)));
            }
        }
        "unknown" => lines.push(format!(
            "{} (environment at {}; cannot check migration barriers)",
            candidate,
            version_label(migration.environment_version)
        )),
        _ => {
            let direction = match (migration.candidate_version, migration.environment_version) {
                (Some(a), Some(b)) if a > b => "forward",
                (Some(a), Some(b)) if a == b => "same version",
                (Some(_), Some(_)) => "backward, no barrier crossed",
                (_, None) => "environment has no recorded version",
                (None, Some(_)) => "not declared",
            };
            lines.push(format!(
                "{} → environment at {} ({})",
                candidate,
                version_label(migration.environment_version),
                direction
            ));
        }
    }
    lines
}

fn quoted(value: Option<&str>) -> String {
    match value {
        Some(v) => format!("{:?}", v),
        None => "(hidden)".to_string(),
    }
}

fn format_diff_entry(entry: &EnvVarDiffEntry) -> Option<String> {
    let secret_kind = if entry.is_protected {
        "protected value"
    } else {
        "secret value"
    };
    let line = match entry.change.as_str() {
        "added" => {
            let value = if entry.is_secret {
                format!("({})", secret_kind)
            } else {
                format!("= {}", quoted(entry.to.as_deref()))
            };
            format!(
                "+ {:<16} {}  ({})",
                entry.key,
                value,
                entry.to_source.as_deref().unwrap_or("-")
            )
        }
        "removed" => {
            let value = if entry.is_secret {
                format!("({})", secret_kind)
            } else {
                format!("= {}", quoted(entry.from.as_deref()))
            };
            format!(
                "- {:<16} {}  ({})",
                entry.key,
                value,
                entry.from_source.as_deref().unwrap_or("-")
            )
        }
        "changed" => {
            let value = if entry.is_secret {
                format!("{} changed", secret_kind)
            } else {
                format!(
                    "{} → {}",
                    quoted(entry.from.as_deref()),
                    quoted(entry.to.as_deref())
                )
            };
            let sources = match (entry.from_source.as_deref(), entry.to_source.as_deref()) {
                (Some(a), Some(b)) if a != b => format!("  ({} → {})", a, b),
                (_, Some(b)) => format!("  ({})", b),
                (Some(a), None) => format!("  ({})", a),
                (None, None) => String::new(),
            };
            format!("~ {:<16} {}{}", entry.key, value, sources)
        }
        "unknown" => format!("? {:<16} {} not comparable", entry.key, secret_kind),
        _ => return None,
    };
    Some(line)
}

/// Render a plan and the variable changes it brings as the terminal shows them.
pub fn format_plan(plan: &CreateFromPlan, diff: Option<&EnvVarDiff>) -> String {
    let mut out = String::new();
    let verb = if plan.kind == "promote" {
        "Promoting"
    } else {
        "Redeploying"
    };
    let mut head = format!("{} {}", verb, plan.source.deployment_id);
    let mut about: Vec<&str> = Vec::new();
    if !plan.source.environment.is_empty() {
        about.push(&plan.source.environment);
    }
    if !plan.source.status.is_empty() {
        about.push(&plan.source.status);
    }
    if !about.is_empty() {
        write!(head, " ({})", about.join(", ")).expect("writing to a String cannot fail");
    }
    if !plan.target.environment.is_empty() {
        write!(head, " → {}", plan.target.environment).expect("writing to a String cannot fail");
    }
    if !plan.target.group.is_empty() {
        write!(head, " [group {}]", plan.target.group).expect("writing to a String cannot fail");
    }
    out.push_str(&head);
    out.push('\n');

    let mut field = |label: &str, value: &str| {
        let label = if label.is_empty() {
            String::new()
        } else {
            format!("{}:", label)
        };
        out.push_str(&format!("  {:<11} {}\n", label, value));
    };

    match (&plan.pinned_image, &plan.source.image) {
        (Some(pinned), _) => field("Image", &format!("{}  (pinned)", pinned)),
        (None, Some(image)) => field("Image", image),
        (None, None) => {}
    }
    match &plan.target.current {
        Some(current) => field(
            "Replacing",
            &format!("{} ({})", current.deployment_id, current.status),
        ),
        None => field("Replacing", "nothing (no active deployment)"),
    }
    match plan.gate.decision.as_str() {
        "denied" => field(
            "Gate",
            &format!(
                "DENIED: {} only accepts deployments promoted from '{}'",
                plan.target.environment,
                plan.gate
                    .promotion_source_environment
                    .as_deref()
                    .unwrap_or("?")
            ),
        ),
        "admin_bypass" => field(
            "Gate",
            &format!(
                "administrator bypass: {} only accepts deployments promoted from '{}'",
                plan.target.environment,
                plan.gate
                    .promotion_source_environment
                    .as_deref()
                    .unwrap_or("?")
            ),
        ),
        _ => {
            if let Some(ref from) = plan.gate.promotion_source_environment {
                field("Gate", &format!("allowed (promotes from {})", from));
            }
        }
    }
    let migration = format_migration(&plan.migration);
    for (i, line) in migration.iter().enumerate() {
        field(if i == 0 { "Migration" } else { "" }, line);
    }
    for (i, adjustment) in plan.adjustments.iter().enumerate() {
        field(
            if i == 0 { "Resources" } else { "" },
            &format_adjustment(adjustment),
        );
    }
    for warning in &plan.warnings {
        field("Warning", warning);
    }

    if let Some(diff) = diff {
        let target = diff
            .environment
            .as_deref()
            .unwrap_or(plan.target.environment.as_str());
        out.push_str(&format!(
            "  Variables ({} now vs. {}):\n",
            plan.source.deployment_id,
            if target.is_empty() {
                "the new deployment".to_string()
            } else {
                format!("the new deployment in {}", target)
            }
        ));
        let lines: Vec<String> = diff.changes.iter().filter_map(format_diff_entry).collect();
        let unchanged = diff
            .changes
            .iter()
            .filter(|c| c.change == "unchanged")
            .count();
        if lines.is_empty() {
            out.push_str("    (no changes)\n");
        }
        for line in lines {
            out.push_str(&format!("    {}\n", line));
        }
        if unchanged > 0 {
            out.push_str(&format!("    ({} unchanged)\n", unchanged));
        }
    }
    out
}

/// Print a plan to stdout, or to stderr when stdout carries JSON.
pub fn print_plan(plan: &CreateFromPlan, diff: Option<&EnvVarDiff>, to_stderr: bool) {
    let text = format_plan(plan, diff);
    if to_stderr {
        eprint!("{}", text);
    } else {
        print!("{}", text);
    }
}

/// Fetch the plan for creating a deployment from `deployment_id`. `Ok(None)`
/// when the server has no plan endpoint (404).
pub(super) async fn fetch_plan(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    request: &PreviewRequest<'_>,
) -> Result<Option<CreateFromPlan>> {
    let mut url = format!(
        "{}/api/v1/projects/{}/deployments/{}/plan?use_source_env_vars={}",
        backend_url,
        urlencoding::encode(request.project),
        urlencoding::encode(request.deployment_id),
        request.use_source_env_vars
    );
    if let Some(env) = request.environment {
        url.push_str(&format!("&environment={}", urlencoding::encode(env)));
    }
    if let Some(group) = request.group {
        url.push_str(&format!("&group={}", urlencoding::encode(group)));
    }

    let response = http_client
        .get(&url)
        .bearer_auth(token)
        .send_with_auth_hint(backend_url)
        .await
        .context("Failed to fetch the deployment plan")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        let body = response.text().await.unwrap_or_default();
        debug!("No deployment plan available (404): {}", body);
        return Ok(None);
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Failed to fetch the deployment plan ({}): {}",
            status,
            describe_api_error(&body)
        );
    }
    let plan = response
        .json()
        .await
        .context("Failed to parse the deployment plan")?;
    Ok(Some(plan))
}

/// Fetch how the variables of a deployment created from `deployment_id` into
/// `environment` differ from the source's. `Ok(None)` when the server has no
/// such endpoint.
pub(super) async fn fetch_env_diff(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    deployment_id: &str,
    environment: Option<&str>,
) -> Result<Option<EnvVarDiff>> {
    let mut url = format!(
        "{}/api/v1/projects/{}/deployments/{}/env/diff",
        backend_url,
        urlencoding::encode(project),
        urlencoding::encode(deployment_id)
    );
    if let Some(env) = environment {
        url.push_str(&format!("?environment={}", urlencoding::encode(env)));
    }
    let response = http_client
        .get(&url)
        .bearer_auth(token)
        .send_with_auth_hint(backend_url)
        .await
        .context("Failed to fetch the variable changes")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        debug!("No variable diff available (404)");
        return Ok(None);
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Failed to fetch the variable changes ({}): {}",
            status,
            describe_api_error(&body)
        );
    }
    let diff = response
        .json()
        .await
        .context("Failed to parse the variable changes")?;
    Ok(Some(diff))
}

/// A deployment to create from another one, as far as previewing it goes.
pub(super) struct PreviewRequest<'a> {
    pub project: &'a str,
    pub deployment_id: &'a str,
    pub environment: Option<&'a str>,
    pub group: Option<&'a str>,
    pub use_source_env_vars: bool,
}

/// Best-effort preview for `rise deploy --from`: print the plan and, unless
/// the source's variables are reused as they are, the variable changes. The
/// create itself enforces every rule, so a preview that cannot be fetched is
/// only logged.
pub(super) async fn preview_create_from(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    request: PreviewRequest<'_>,
    to_stderr: bool,
) {
    let plan = match fetch_plan(http_client, backend_url, token, &request).await {
        Ok(Some(plan)) => plan,
        Ok(None) => return,
        Err(e) => {
            warn!("Could not preview the deployment: {:?}", e);
            return;
        }
    };
    let diff = if request.use_source_env_vars {
        None
    } else {
        let target = (!plan.target.environment.is_empty())
            .then_some(plan.target.environment.as_str())
            .or(request.environment);
        match fetch_env_diff(
            http_client,
            backend_url,
            token,
            request.project,
            request.deployment_id,
            target,
        )
        .await
        {
            Ok(diff) => diff,
            Err(e) => {
                warn!("Could not compare environment variables: {:?}", e);
                None
            }
        }
    };
    print_plan(&plan, diff.as_ref(), to_stderr);
}

/// Options for `rise promote`.
pub struct PromoteOptions<'a> {
    pub project_name: &'a str,
    pub deployment_id: &'a str,
    /// Target environment; defaults to the project's production environment.
    pub target_environment: Option<&'a str>,
    /// Skip the confirmation prompt.
    pub yes: bool,
    /// Promote across a recorded migration barrier.
    pub force: bool,
    /// rise.toml, unless `--no-config`: `[environments.<target>]` supplies
    /// variable overrides and explicit resources.
    pub toml_config: Option<crate::build::config::ProjectBuildConfig>,
    pub replicas: Option<u32>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub json_output: bool,
    pub status_file: Option<String>,
}

/// What rise.toml's `[environments.<target>]` adds to a promotion.
#[derive(Debug, Default, PartialEq)]
pub(super) struct TomlTargetOverrides {
    pub env_overrides: Vec<EnvOverride>,
    pub replicas: Option<u32>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

/// Collect `[environments.<target>].env` as overrides scoped to the target and
/// `[environments.<target>.deploy]` resources. Only the target's section is
/// read: the source's own rise.toml variables travel with it.
pub(super) fn toml_target_overrides(
    config: Option<&crate::build::config::ProjectBuildConfig>,
    target: &str,
) -> TomlTargetOverrides {
    let Some(env_config) = config.and_then(|c| c.environments.get(target)) else {
        return TomlTargetOverrides::default();
    };
    let env_overrides = env_config
        .env
        .iter()
        .map(|(key, value)| EnvOverride {
            key: key.clone(),
            value: value.clone(),
            is_secret: false,
            is_protected: false,
            source: Some("toml".to_string()),
            for_environment: Some(target.to_string()),
        })
        .collect();
    let deploy = env_config.deploy.as_ref();
    TomlTargetOverrides {
        env_overrides,
        replicas: deploy.and_then(|d| d.replicas),
        cpu: deploy.and_then(|d| d.cpu.clone()),
        memory: deploy.and_then(|d| d.memory.clone()),
    }
}

/// The message for a promotion the target's promotion source does not allow.
fn gate_denied_message(plan: &CreateFromPlan) -> String {
    let target = &plan.target.environment;
    let required = plan
        .gate
        .promotion_source_environment
        .as_deref()
        .unwrap_or("?");
    format!(
        "Environment '{}' only accepts deployments promoted from '{}', and {} is in '{}'. \
         Promote a {} deployment: rise promote <deployment-id> -E {}",
        target, required, plan.source.deployment_id, plan.source.environment, required, target
    )
}

fn barrier_crossed_message(plan: &CreateFromPlan) -> String {
    let mut message = format!(
        "Promoting {} ({}) into {} (at {}) crosses recorded migration barriers:",
        plan.source.deployment_id,
        version_label(plan.migration.candidate_version),
        plan.target.environment,
        version_label(plan.migration.environment_version)
    );
    for barrier in &plan.migration.barriers {
        message.push_str(&format!("\n  {}", format_barrier(barrier)));
    }
    message.push_str("\nRe-run with --force to promote anyway.");
    message
}

async fn resolve_target_environment(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(target) = explicit {
        return Ok(target.to_string());
    }
    let environments =
        crate::cli::environment::fetch_environments(http_client, backend_url, token, project)
            .await?;
    environments
        .into_iter()
        .find(|e| e.is_production)
        .map(|e| e.name)
        .with_context(|| {
            format!(
                "Project '{}' has no production environment; pass the target with -E <environment>",
                project
            )
        })
}

fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("refusing to promote non-interactively; pass --yes");
    }
    eprint!("{} [y/N] ", prompt);
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .context("Failed to read the confirmation")?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

/// `rise promote`: show the plan and variable changes for creating a
/// deployment from `deployment_id` in the target environment, check the
/// promotion gate and migration barriers, confirm, create it and follow it.
pub async fn promote(
    http_client: &Client,
    backend_url: &str,
    config: &Config,
    opts: PromoteOptions<'_>,
) -> Result<()> {
    let provider = crate::token_source::resolve_token_provider(http_client, config)?;
    let token = token_with_retry(&provider).await?;
    let to_stderr = opts.json_output;
    let say = |line: &str| {
        if to_stderr {
            eprintln!("{}", line);
        } else {
            println!("{}", line);
        }
    };

    let target = resolve_target_environment(
        http_client,
        backend_url,
        &token,
        opts.project_name,
        opts.target_environment,
    )
    .await?;

    let request = PreviewRequest {
        project: opts.project_name,
        deployment_id: opts.deployment_id,
        environment: Some(&target),
        group: None,
        use_source_env_vars: false,
    };
    let plan = fetch_plan(http_client, backend_url, &token, &request)
        .await?
        .with_context(|| {
            format!(
                "Deployment '{}' not found in project '{}', or the server does not support promotion",
                opts.deployment_id, opts.project_name
            )
        })?;
    if plan.source.environment == target {
        bail!(
            "Deployment {} is already in '{}'. To redeploy it there, run: rise deploy --from {} -E {}",
            opts.deployment_id,
            target,
            opts.deployment_id,
            target
        );
    }
    let diff = fetch_env_diff(
        http_client,
        backend_url,
        &token,
        opts.project_name,
        opts.deployment_id,
        Some(&target),
    )
    .await?;
    print_plan(&plan, diff.as_ref(), to_stderr);

    match plan.gate.decision.as_str() {
        "denied" => bail!("{}", gate_denied_message(&plan)),
        "admin_bypass" => warn!(
            "Bypassing the promotion gate as an administrator: '{}' only accepts deployments promoted from '{}'",
            target,
            plan.gate
                .promotion_source_environment
                .as_deref()
                .unwrap_or("?")
        ),
        _ => {}
    }
    match plan.migration.status.as_str() {
        "crossed" if !opts.force => bail!("{}", barrier_crossed_message(&plan)),
        "crossed" => warn!("Promoting across recorded migration barriers (--force)"),
        "unknown" => warn!(
            "{} declares no migration version, so migration barriers cannot be checked",
            opts.deployment_id
        ),
        _ => {}
    }

    let toml = toml_target_overrides(opts.toml_config.as_ref(), &target);
    let replicas = opts.replicas.or(toml.replicas);
    let cpu = opts.cpu.clone().or(toml.cpu.clone());
    let memory = opts.memory.clone().or(toml.memory.clone());
    if toml != TomlTargetOverrides::default() {
        let mut applied: Vec<String> = toml.env_overrides.iter().map(|o| o.key.clone()).collect();
        if let Some(r) = toml.replicas {
            applied.push(format!("replicas={}", r));
        }
        if let Some(ref c) = toml.cpu {
            applied.push(format!("cpu={}", c));
        }
        if let Some(ref m) = toml.memory {
            applied.push(format!("memory={}", m));
        }
        say(&format!(
            "  rise.toml [environments.{}]: {}",
            target,
            applied.join(", ")
        ));
    }

    if !opts.yes {
        let prompt = format!(
            "Promote {} ({}) to {}?",
            opts.deployment_id, plan.source.environment, target
        );
        if !confirm(&prompt)? {
            bail!("Promotion cancelled");
        }
    }

    let payload = CreateDeploymentPayload {
        project: opts.project_name,
        environment: Some(&target),
        from_deployment: Some(opts.deployment_id),
        env_overrides: &toml.env_overrides,
        replicas,
        cpu: cpu.as_deref(),
        memory: memory.as_deref(),
        force_migration_barrier: opts.force,
        ..Default::default()
    };
    info!(
        "Promoting deployment '{}' of project '{}' to '{}'",
        opts.deployment_id, opts.project_name, target
    );
    let token = token_with_retry(&provider).await?;
    let url = format!("{}/api/v1/deployments", backend_url);
    let response = http_client
        .post(&url)
        .bearer_auth(&token)
        .json(&payload.to_json()?)
        .send_with_auth_hint(backend_url)
        .await
        .context("Failed to create deployment")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Failed to promote ({}): {}",
            status,
            describe_api_error(&body)
        );
    }
    let created: super::core::CreateDeploymentResponse = response
        .json()
        .await
        .context("Failed to parse deployment response")?;
    report_create_response(&created, to_stderr);
    say(&format!(
        "✓ Created deployment {} in {}",
        created.deployment_id, target
    ));

    follow_created_deployment(
        http_client,
        backend_url,
        config,
        opts.project_name,
        &created.deployment_id,
        DeploymentOutputOptions {
            json: opts.json_output,
            status_file: opts.status_file.as_deref(),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn promote_plan() -> CreateFromPlan {
        serde_json::from_value(serde_json::json!({
            "kind": "promote",
            "source": {
                "deployment_id": "20261020-101500",
                "environment": "staging",
                "status": "Healthy",
                "image": "registry.rise.dev/app:20261020-101500",
                "migration_version": 5
            },
            "target": {
                "environment": "production",
                "group": "default",
                "current": {"deployment_id": "20261001-090000", "status": "Healthy"},
                "migration_high_water": 4
            },
            "pinned_image": "registry.rise.dev/app@sha256:3f2a",
            "gate": {"decision": "allowed", "promotion_source_environment": "staging"},
            "migration": {"status": "ok", "candidate_version": 5, "environment_version": 4, "barriers": []},
            "adjustments": [{"container": "app", "field": "replicas", "from": "1", "to": "2", "reason": "production requires ≥ 2"}],
            "use_source_env_vars_allowed": false,
            "warnings": []
        }))
        .unwrap()
    }

    #[test]
    fn plan_renders_image_replacement_migration_and_resources() {
        let diff: EnvVarDiff = serde_json::from_value(serde_json::json!({
            "environment": "production",
            "changes": [
                {"key": "FEATURE_X", "change": "added", "is_secret": false, "is_protected": false, "to": "on", "to_source": "env:production"},
                {"key": "LOG_LEVEL", "change": "changed", "is_secret": false, "is_protected": false, "from": "debug", "to": "info", "from_source": "toml", "to_source": "env:production"},
                {"key": "DB_PASSWORD", "change": "changed", "is_secret": true, "is_protected": true, "from_source": "env:staging", "to_source": "env:production"},
                {"key": "SAME", "change": "unchanged", "is_secret": false, "is_protected": false, "from": "1", "to": "1"}
            ]
        }))
        .unwrap();
        let text = format_plan(&promote_plan(), Some(&diff));
        assert_eq!(
            text,
            "Promoting 20261020-101500 (staging, Healthy) → production [group default]\n\
             \x20 Image:      registry.rise.dev/app@sha256:3f2a  (pinned)\n\
             \x20 Replacing:  20261001-090000 (Healthy)\n\
             \x20 Gate:       allowed (promotes from staging)\n\
             \x20 Migration:  v5 → environment at v4 (forward)\n\
             \x20 Resources:  app replicas 1 → 2 (production requires ≥ 2)\n\
             \x20 Variables (20261020-101500 now vs. the new deployment in production):\n\
             \x20   + FEATURE_X        = \"on\"  (env:production)\n\
             \x20   ~ LOG_LEVEL        \"debug\" → \"info\"  (toml → env:production)\n\
             \x20   ~ DB_PASSWORD      protected value changed  (env:staging → env:production)\n\
             \x20   (1 unchanged)\n"
        );
    }

    #[test]
    fn plan_lists_crossed_barriers_and_denied_gate() {
        let mut plan = promote_plan();
        plan.gate.decision = "denied".to_string();
        plan.migration = serde_json::from_value(serde_json::json!({
            "status": "crossed",
            "candidate_version": 3,
            "environment_version": 5,
            "barriers": [{"version": 4, "reason": "drops users.name", "recorded_by": "a@example.com", "recorded_at": "2026-10-01T09:00:00Z"}]
        }))
        .unwrap();
        plan.adjustments.clear();
        let text = format_plan(&plan, None);
        assert!(
            text.contains(
                "  Gate:       DENIED: production only accepts deployments promoted from 'staging'\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  Migration:  v3 → environment at v5 (crosses 1 migration barrier)\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "               v4: drops users.name (recorded 2026-10-01 by a@example.com)\n"
            ),
            "{text}"
        );
        assert!(!text.contains("Variables"), "{text}");
        assert!(barrier_crossed_message(&plan).contains("--force"));
        assert!(gate_denied_message(&plan)
            .contains("Promote a staging deployment: rise promote <deployment-id> -E production"));
    }

    #[test]
    fn redeploy_plan_without_target_deployment_or_changes() {
        let plan: CreateFromPlan = serde_json::from_value(serde_json::json!({
            "kind": "redeploy",
            "source": {"deployment_id": "d1", "environment": "production", "status": "Superseded", "image": "img:1"},
            "target": {"environment": "production", "group": "default", "current": null},
            "gate": {"decision": "allowed", "promotion_source_environment": null},
            "migration": {"status": "unknown", "candidate_version": null, "environment_version": 2},
            "warnings": ["no migration version declared"]
        }))
        .unwrap();
        let diff = EnvVarDiff {
            environment: Some("production".to_string()),
            changes: vec![],
        };
        let text = format_plan(&plan, Some(&diff));
        assert_eq!(
            text,
            "Redeploying d1 (production, Superseded) → production [group default]\n\
             \x20 Image:      img:1\n\
             \x20 Replacing:  nothing (no active deployment)\n\
             \x20 Migration:  undeclared (environment at v2; cannot check migration barriers)\n\
             \x20 Warning:    no migration version declared\n\
             \x20 Variables (d1 now vs. the new deployment in production):\n\
             \x20   (no changes)\n"
        );
    }

    #[test]
    fn a_minimal_plan_parses() {
        let plan: CreateFromPlan = serde_json::from_str(r#"{"kind":"promote"}"#).unwrap();
        assert_eq!(plan.kind, "promote");
        assert!(plan.adjustments.is_empty());
    }

    #[test]
    fn toml_overrides_come_only_from_the_target_section() {
        let config: crate::build::config::ProjectBuildConfig = toml::from_str(
            r#"
            [project]
            name = "app"
            env = { GLOBAL = "g" }

            [environments.staging.env]
            STAGING_ONLY = "s"

            [environments.production]
            env = { LOG_LEVEL = "info" }
            deploy = { replicas = 2, cpu = "1" }
        "#,
        )
        .unwrap();
        let overrides = toml_target_overrides(Some(&config), "production");
        assert_eq!(
            overrides,
            TomlTargetOverrides {
                env_overrides: vec![EnvOverride {
                    key: "LOG_LEVEL".to_string(),
                    value: "info".to_string(),
                    is_secret: false,
                    is_protected: false,
                    source: Some("toml".to_string()),
                    for_environment: Some("production".to_string()),
                }],
                replicas: Some(2),
                cpu: Some("1".to_string()),
                memory: None,
            }
        );
        assert_eq!(
            toml_target_overrides(Some(&config), "qa"),
            TomlTargetOverrides::default()
        );
        assert_eq!(
            toml_target_overrides(None, "production"),
            TomlTargetOverrides::default()
        );
    }
}
