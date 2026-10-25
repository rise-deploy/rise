//! `rise promote` and the preview shown before creating a deployment from
//! another one (`rise deploy --from`): the server's plan for the create and the
//! variables it will change.

use std::fmt::Write as _;
use std::io::{BufRead as _, IsTerminal as _, Write as _};

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize};
use tracing::{debug, info, warn};

use super::core::{
    call_create_deployment_api, describe_api_error, follow_created_deployment,
    report_create_response, say, CreateDeploymentPayload, DeploymentOutputOptions, EnvOverride,
};
use crate::cli::auth_hint::AuthHintRequest;
use crate::config::Config;
use crate::token_source::token_with_retry;

/// What `GET /projects/{p}/deployments/{id}/plan` says creating a deployment
/// from `{id}` into a target would do. Every field defaults so a server that
/// adds fields, or leaves some out, still parses.
#[derive(Debug, Default, Deserialize)]
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
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PlanSource {
    pub deployment_id: String,
    pub environment: String,
    pub status: String,
    pub image: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PlanTarget {
    pub environment: String,
    pub group: String,
    pub current: Option<PlanCurrent>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PlanCurrent {
    pub deployment_id: String,
    pub status: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PlanGate {
    /// `allowed`, `admin_bypass` or `denied`.
    pub decision: String,
    pub promotion_source_environment: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PlanMigration {
    /// `ok`, `unknown` or `crossed`.
    pub status: String,
    pub candidate_version: Option<i64>,
    pub environment_version: Option<i64>,
    pub barriers: Vec<PlanBarrier>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PlanBarrier {
    pub version: i64,
    pub reason: String,
    pub recorded_by: Option<String>,
    pub recorded_at: String,
}

/// An inherited resource value changed to fit the target environment.
#[derive(Debug, Default, Deserialize)]
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
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EnvVarDiff {
    pub environment: Option<String>,
    pub changes: Vec<EnvVarDiffEntry>,
}

#[derive(Debug, Default, Deserialize)]
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
    let date = barrier
        .recorded_at
        .get(..10)
        .unwrap_or(&barrier.recorded_at);
    let recorded = match (date, barrier.recorded_by.as_deref()) {
        ("", None) => String::new(),
        ("", Some(by)) => format!(" (recorded by {by})"),
        (date, None) => format!(" (recorded {date})"),
        (date, Some(by)) => format!(" (recorded {date} by {by})"),
    };
    format!("v{}: {}{recorded}", barrier.version, barrier.reason)
}

/// One adjustment as a line: `app replicas 1 → 2 (production requires ≥ 2)`.
pub(super) fn format_adjustment(a: &ResourceAdjustment) -> String {
    let reason = if a.reason.is_empty() {
        String::new()
    } else {
        format!(" ({})", a.reason)
    };
    format!("{} {} {} → {}{reason}", a.container, a.field, a.from, a.to)
}

fn version_label(version: Option<i64>) -> String {
    version.map_or_else(|| "undeclared".to_string(), |v| format!("v{v}"))
}

fn format_migration(m: &PlanMigration) -> Vec<String> {
    let candidate = version_label(m.candidate_version);
    let env = version_label(m.environment_version);
    let note = match (
        m.status.as_str(),
        m.candidate_version,
        m.environment_version,
    ) {
        ("unknown", ..) => {
            return vec![format!(
                "{candidate} (environment at {env}; cannot check migration barriers)"
            )]
        }
        ("crossed", ..) => {
            let n = m.barriers.len();
            let mut lines = vec![format!(
                "{candidate} → environment at {env} (crosses {n} migration barrier{})",
                if n == 1 { "" } else { "s" }
            )];
            lines.extend(
                m.barriers
                    .iter()
                    .map(|b| format!("  {}", format_barrier(b))),
            );
            return lines;
        }
        (_, Some(a), Some(b)) if a > b => "forward",
        (_, Some(a), Some(b)) if a == b => "same version",
        (_, Some(_), Some(_)) => "backward, no barrier crossed",
        (_, _, None) => "environment has no recorded version",
        (_, None, Some(_)) => "not declared",
    };
    vec![format!("{candidate} → environment at {env} ({note})")]
}

fn format_diff_entry(e: &EnvVarDiffEntry) -> Option<String> {
    let secret_kind = if e.is_protected {
        "protected value"
    } else {
        "secret value"
    };
    let quoted = |v: Option<&str>| v.map_or_else(|| "(hidden)".to_string(), |v| format!("{v:?}"));
    let value = |v: Option<&String>| {
        if e.is_secret {
            format!("({secret_kind})")
        } else {
            format!("= {}", quoted(v.map(String::as_str)))
        }
    };
    let source = |s: &Option<String>| s.as_deref().unwrap_or("-").to_string();
    Some(match e.change.as_str() {
        "added" => format!(
            "+ {:<16} {}  ({})",
            e.key,
            value(e.to.as_ref()),
            source(&e.to_source)
        ),
        "removed" => format!(
            "- {:<16} {}  ({})",
            e.key,
            value(e.from.as_ref()),
            source(&e.from_source)
        ),
        "changed" => {
            let value = if e.is_secret {
                format!("{secret_kind} changed")
            } else {
                format!(
                    "{} → {}",
                    quoted(e.from.as_deref()),
                    quoted(e.to.as_deref())
                )
            };
            let sources = match (e.from_source.as_deref(), e.to_source.as_deref()) {
                (Some(a), Some(b)) if a != b => format!("  ({a} → {b})"),
                (_, Some(s)) | (Some(s), None) => format!("  ({s})"),
                (None, None) => String::new(),
            };
            format!("~ {:<16} {value}{sources}", e.key)
        }
        "unknown" => format!("? {:<16} {secret_kind} not comparable", e.key),
        _ => return None,
    })
}

/// Render a plan and the variable changes it brings as the terminal shows them.
pub fn format_plan(plan: &CreateFromPlan, diff: Option<&EnvVarDiff>) -> String {
    let verb = if plan.kind == "promote" {
        "Promoting"
    } else {
        "Redeploying"
    };
    let mut out = format!("{verb} {}", plan.source.deployment_id);
    let about: Vec<&str> = [&plan.source.environment, &plan.source.status]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(String::as_str)
        .collect();
    if !about.is_empty() {
        let _ = write!(out, " ({})", about.join(", "));
    }
    if !plan.target.environment.is_empty() {
        let _ = write!(out, " → {}", plan.target.environment);
    }
    if !plan.target.group.is_empty() {
        let _ = write!(out, " [group {}]", plan.target.group);
    }
    out.push('\n');

    let mut field = |label: &str, value: &str| {
        let label = if label.is_empty() {
            String::new()
        } else {
            format!("{label}:")
        };
        let _ = writeln!(out, "  {label:<11} {value}");
    };
    match (&plan.pinned_image, &plan.source.image) {
        (Some(pinned), _) => field("Image", &format!("{pinned}  (pinned)")),
        (None, Some(image)) => field("Image", image),
        (None, None) => {}
    }
    match &plan.target.current {
        Some(c) => field("Replacing", &format!("{} ({})", c.deployment_id, c.status)),
        None => field("Replacing", "nothing (no active deployment)"),
    }
    let required = plan.gate.promotion_source_environment.as_deref();
    let decision = match plan.gate.decision.as_str() {
        "denied" => Some("DENIED"),
        "admin_bypass" => Some("administrator bypass"),
        _ => None,
    };
    match (decision, required) {
        (Some(decision), _) => field(
            "Gate",
            &format!(
                "{decision}: {} only accepts deployments promoted from '{}'",
                plan.target.environment,
                required.unwrap_or("?")
            ),
        ),
        (None, Some(from)) => field("Gate", &format!("allowed (promotes from {from})")),
        (None, None) => {}
    }
    for (i, line) in format_migration(&plan.migration).iter().enumerate() {
        field(if i == 0 { "Migration" } else { "" }, line);
    }
    for (i, adjustment) in plan.adjustments.iter().enumerate() {
        let label = if i == 0 { "Resources" } else { "" };
        field(label, &format_adjustment(adjustment));
    }
    for warning in &plan.warnings {
        field("Warning", warning);
    }

    if let Some(diff) = diff {
        let target = diff
            .environment
            .as_deref()
            .unwrap_or(&plan.target.environment);
        let into = if target.is_empty() {
            String::new()
        } else {
            format!(" in {target}")
        };
        let _ = writeln!(
            out,
            "  Variables ({} now vs. the new deployment{into}):",
            plan.source.deployment_id
        );
        let lines: Vec<String> = diff.changes.iter().filter_map(format_diff_entry).collect();
        if lines.is_empty() {
            out.push_str("    (no changes)\n");
        }
        for line in lines {
            let _ = writeln!(out, "    {line}");
        }
        let unchanged = diff.changes.iter().filter(|c| c.change == "unchanged");
        let unchanged = unchanged.count();
        if unchanged > 0 {
            let _ = writeln!(out, "    ({unchanged} unchanged)");
        }
    }
    out
}

/// GET a preview endpoint. `Ok(None)` when the server has none (404).
async fn fetch_optional<T: DeserializeOwned>(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    url: &str,
    what: &str,
) -> Result<Option<T>> {
    let response = http_client
        .get(url)
        .bearer_auth(token)
        .send_with_auth_hint(backend_url)
        .await
        .with_context(|| format!("Failed to fetch {what}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            debug!("The server does not provide {what} (404): {body}");
            return Ok(None);
        }
        bail!(
            "Failed to fetch {what} ({status}): {}",
            describe_api_error(&body)
        );
    }
    let parsed = response
        .json()
        .await
        .with_context(|| format!("Failed to parse {what}"))?;
    Ok(Some(parsed))
}

/// A deployment to create from another one, as far as previewing it goes.
pub(super) struct PreviewRequest<'a> {
    pub project: &'a str,
    pub deployment_id: &'a str,
    pub environment: Option<&'a str>,
    pub group: Option<&'a str>,
    pub use_source_env_vars: bool,
}

impl PreviewRequest<'_> {
    fn deployment_url(&self, backend_url: &str) -> String {
        format!(
            "{}/api/v1/projects/{}/deployments/{}",
            backend_url,
            urlencoding::encode(self.project),
            urlencoding::encode(self.deployment_id)
        )
    }

    async fn plan(
        &self,
        http_client: &Client,
        backend_url: &str,
        token: &str,
    ) -> Result<Option<CreateFromPlan>> {
        let mut url = format!(
            "{}/plan?use_source_env_vars={}",
            self.deployment_url(backend_url),
            self.use_source_env_vars
        );
        if let Some(env) = self.environment {
            let _ = write!(url, "&environment={}", urlencoding::encode(env));
        }
        if let Some(group) = self.group {
            let _ = write!(url, "&group={}", urlencoding::encode(group));
        }
        fetch_optional(http_client, backend_url, token, &url, "the deployment plan").await
    }

    /// How the variables of a deployment created into `environment` differ
    /// from the source's.
    async fn env_diff(
        &self,
        http_client: &Client,
        backend_url: &str,
        token: &str,
        environment: Option<&str>,
    ) -> Result<Option<EnvVarDiff>> {
        let mut url = format!("{}/env/diff", self.deployment_url(backend_url));
        if let Some(env) = environment {
            let _ = write!(url, "?environment={}", urlencoding::encode(env));
        }
        fetch_optional(
            http_client,
            backend_url,
            token,
            &url,
            "the variable changes",
        )
        .await
    }
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
    let plan = match request.plan(http_client, backend_url, token).await {
        Ok(Some(plan)) => plan,
        Ok(None) => return,
        Err(e) => return warn!("Could not preview the deployment: {:?}", e),
    };
    let diff = if request.use_source_env_vars {
        None
    } else {
        let target = Some(plan.target.environment.as_str())
            .filter(|t| !t.is_empty())
            .or(request.environment);
        request
            .env_diff(http_client, backend_url, token, target)
            .await
            .unwrap_or_else(|e| {
                warn!("Could not compare environment variables: {:?}", e);
                None
            })
    };
    say(to_stderr, format_plan(&plan, diff.as_ref()).trim_end());
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
    let deploy = env_config.deploy.as_ref();
    TomlTargetOverrides {
        env_overrides: env_config
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
            .collect(),
        replicas: deploy.and_then(|d| d.replicas),
        cpu: deploy.and_then(|d| d.cpu.clone()),
        memory: deploy.and_then(|d| d.memory.clone()),
    }
}

/// The message for a promotion the target's promotion source does not allow.
fn gate_denied_message(plan: &CreateFromPlan) -> String {
    let target = &plan.target.environment;
    let required = plan.gate.promotion_source_environment.as_deref();
    let required = required.unwrap_or("?");
    format!(
        "Environment '{target}' only accepts deployments promoted from '{required}', and {} is in '{}'. \
         Promote a {required} deployment: rise promote <deployment-id> -E {target}",
        plan.source.deployment_id, plan.source.environment
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
        let _ = write!(message, "\n  {}", format_barrier(barrier));
    }
    message + "\nRe-run with --force to promote anyway."
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
    let (project, deployment_id) = (opts.project_name, opts.deployment_id);

    let target = match opts.target_environment {
        Some(target) => target.to_string(),
        None => crate::cli::environment::fetch_environments(http_client, backend_url, &token, project)
            .await?
            .into_iter()
            .find(|e| e.is_production)
            .map(|e| e.name)
            .with_context(|| {
                format!("Project '{project}' has no production environment; pass the target with -E <environment>")
            })?,
    };

    let request = PreviewRequest {
        project,
        deployment_id,
        environment: Some(&target),
        group: None,
        use_source_env_vars: false,
    };
    let plan = request
        .plan(http_client, backend_url, &token)
        .await?
        .with_context(|| {
            format!("Deployment '{deployment_id}' not found in project '{project}', or the server does not support promotion")
        })?;
    if plan.source.environment == target {
        bail!(
            "Deployment {deployment_id} is already in '{target}'. To redeploy it there, run: rise deploy --from {deployment_id} -E {target}"
        );
    }
    let diff = request
        .env_diff(http_client, backend_url, &token, Some(&target))
        .await?;
    say(to_stderr, format_plan(&plan, diff.as_ref()).trim_end());

    match plan.gate.decision.as_str() {
        "denied" => bail!("{}", gate_denied_message(&plan)),
        "admin_bypass" => warn!(
            "Bypassing the promotion gate as an administrator: '{}' only accepts deployments promoted from '{}'",
            target,
            plan.gate.promotion_source_environment.as_deref().unwrap_or("?")
        ),
        _ => {}
    }
    match plan.migration.status.as_str() {
        "crossed" if !opts.force => bail!("{}", barrier_crossed_message(&plan)),
        "crossed" => warn!("Promoting across recorded migration barriers (--force)"),
        "unknown" => warn!(
            "{deployment_id} declares no migration version, so migration barriers cannot be checked"
        ),
        _ => {}
    }

    let toml = toml_target_overrides(opts.toml_config.as_ref(), &target);
    if toml != TomlTargetOverrides::default() {
        let applied: Vec<String> = toml
            .env_overrides
            .iter()
            .map(|o| o.key.clone())
            .chain(toml.replicas.map(|r| format!("replicas={r}")))
            .chain(toml.cpu.as_ref().map(|c| format!("cpu={c}")))
            .chain(toml.memory.as_ref().map(|m| format!("memory={m}")))
            .collect();
        say(
            to_stderr,
            &format!(
                "  rise.toml [environments.{target}]: {}",
                applied.join(", ")
            ),
        );
    }

    if !opts.yes {
        let prompt = format!(
            "Promote {deployment_id} ({}) to {target}?",
            plan.source.environment
        );
        if !confirm(&prompt)? {
            bail!("Promotion cancelled");
        }
    }

    let cpu = opts.cpu.or(toml.cpu);
    let memory = opts.memory.or(toml.memory);
    let payload = CreateDeploymentPayload {
        project,
        environment: Some(&target),
        from_deployment: Some(deployment_id),
        env_overrides: &toml.env_overrides,
        replicas: opts.replicas.or(toml.replicas),
        cpu: cpu.as_deref(),
        memory: memory.as_deref(),
        force_migration_barrier: opts.force,
        ..Default::default()
    };
    info!("Promoting deployment '{deployment_id}' of project '{project}' to '{target}'");
    // The confirmation may have outlived the first token.
    let token = token_with_retry(&provider).await?;
    let created = call_create_deployment_api(
        http_client,
        backend_url,
        &token,
        &payload,
        "Failed to promote",
    )
    .await?;
    report_create_response(&created, to_stderr);
    say(
        to_stderr,
        &format!("✓ Created deployment {} in {target}", created.deployment_id),
    );

    follow_created_deployment(
        http_client,
        backend_url,
        config,
        project,
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

    fn plan(value: serde_json::Value) -> CreateFromPlan {
        serde_json::from_value(value).unwrap()
    }

    fn promote_plan() -> CreateFromPlan {
        plan(serde_json::json!({
            "kind": "promote",
            "source": {"deployment_id": "20261020-101500", "environment": "staging", "status": "Healthy",
                       "image": "registry.rise.dev/app:20261020-101500"},
            "target": {"environment": "production", "group": "default",
                       "current": {"deployment_id": "20261001-090000", "status": "Healthy"}},
            "pinned_image": "registry.rise.dev/app@sha256:3f2a",
            "gate": {"decision": "allowed", "promotion_source_environment": "staging"},
            "migration": {"status": "ok", "candidate_version": 5, "environment_version": 4},
            "adjustments": [{"container": "app", "field": "replicas", "from": "1", "to": "2", "reason": "production requires ≥ 2"}]
        }))
    }

    #[test]
    fn plan_renders_image_replacement_migration_and_resources() {
        let diff: EnvVarDiff = serde_json::from_value(serde_json::json!({
            "environment": "production",
            "changes": [
                {"key": "FEATURE_X", "change": "added", "to": "on", "to_source": "env:production"},
                {"key": "LOG_LEVEL", "change": "changed", "from": "debug", "to": "info", "from_source": "toml", "to_source": "env:production"},
                {"key": "DB_PASSWORD", "change": "changed", "is_secret": true, "is_protected": true, "from_source": "env:staging", "to_source": "env:production"},
                {"key": "SAME", "change": "unchanged", "from": "1", "to": "1"}
            ]
        }))
        .unwrap();
        assert_eq!(
            format_plan(&promote_plan(), Some(&diff)),
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
        for expected in [
            "  Gate:       DENIED: production only accepts deployments promoted from 'staging'\n",
            "  Migration:  v3 → environment at v5 (crosses 1 migration barrier)\n",
            "               v4: drops users.name (recorded 2026-10-01 by a@example.com)\n",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains("Variables"), "{text}");
        assert!(barrier_crossed_message(&plan).contains("--force"));
        assert!(gate_denied_message(&plan)
            .contains("Promote a staging deployment: rise promote <deployment-id> -E production"));
    }

    #[test]
    fn redeploy_plan_without_target_deployment_or_changes() {
        let plan = plan(serde_json::json!({
            "kind": "redeploy",
            "source": {"deployment_id": "d1", "environment": "production", "status": "Superseded", "image": "img:1"},
            "target": {"environment": "production", "group": "default", "current": null},
            "gate": {"decision": "allowed", "promotion_source_environment": null},
            "migration": {"status": "unknown", "candidate_version": null, "environment_version": 2},
            "warnings": ["no migration version declared"]
        }));
        let diff = EnvVarDiff {
            environment: Some("production".to_string()),
            changes: vec![],
        };
        assert_eq!(
            format_plan(&plan, Some(&diff)),
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
        assert_eq!(
            toml_target_overrides(Some(&config), "production"),
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
    }
}
