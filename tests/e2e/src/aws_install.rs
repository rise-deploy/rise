//! Standalone AWS install suite.
//!
//! Applies the documented production install -- `modules/rise-aws` for IAM,
//! `modules/rise-ecs` for everything else, wired as the `rise-ecs` README shows
//! -- against Floci, a local AWS emulator, then plans it again, checks the two
//! modules agree with each other, and destroys it.
//!
//! What it proves is the infrastructure: that the install applies from nothing,
//! converges, and tears down cleanly. It does not prove Rise *runs* on ECS --
//! Floci is started with ECS tasks, RDS, ELBv2 and EC2 mocked, so no container
//! behind any of it starts. That is the ECS backend suite's job.
//!
//! No credentials: Floci accepts any, which is why this runs on every pull
//! request, forks included.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::{cli, http, report};

/// Must match `name` in `aws-install/variables.tf`; the task definitions the
/// permission probes read are named after it.
const NAME: &str = "rise-floci";

/// Attributes Floci reports differently from what it was sent, so a second plan
/// always shows them. Each one is Floci's gap, not the module's: anything not
/// on this list that changes on a second plan fails the suite.
const EMULATOR_DRIFT: &[(&str, &str, &str)] = &[(
    "aws_db_instance",
    "storage_type",
    "Floci reports every instance as gp2",
)];

pub fn run() -> Result<()> {
    report::section("AWS install suite (Floci)");
    let install = Install::from_env()?;

    report::step("Floci is reachable", || install.wait_floci())?;

    report::step("terraform init", || {
        // Local state that outlived its emulator describes nothing that exists.
        for stale in ["terraform.tfstate", "terraform.tfstate.backup"] {
            let _ = std::fs::remove_file(install.dir.join(stale));
        }
        cli::run_checked(install.terraform(&["init", "-input=false"])).map(|_| ())
    })?;

    let vars = [
        format!("-var=endpoint={}", install.endpoint),
        format!("-var=region={}", install.region),
    ];
    let with_vars = |args: &[&str]| -> Command {
        let mut all: Vec<&str> = args.to_vec();
        all.extend(vars.iter().map(String::as_str));
        install.terraform(&all)
    };

    let applied = report::step("apply", || {
        cli::run_checked(with_vars(&["apply", "-auto-approve", "-input=false"])).map(|_| ())
    });

    let checks = applied.and_then(|()| {
        report::step("second plan is empty", || {
            install.assert_converged(&with_vars)
        })?;
        for (kind, attr, why) in EMULATOR_DRIFT {
            report::note(&format!("  ignored {kind}.{attr}: {why}"));
        }
        report::step_value("permission probes agree", || install.assert_wiring()).map(|_| ())
    });

    // Destroy whatever got created, whether or not the checks passed: a local
    // run against a long-lived Floci should not leave the next one a mess.
    let destroyed = report::step("destroy", || {
        cli::run_checked(with_vars(&["destroy", "-auto-approve", "-input=false"])).map(|_| ())
    });

    match (checks, destroyed) {
        (Ok(()), Ok(())) => {
            report::note("PASS aws-install");
            Ok(())
        }
        (Err(e), Ok(())) => Err(e),
        (Ok(()), Err(e)) => Err(e),
        (Err(e), Err(teardown)) => {
            eprintln!("[e2e] aws-install teardown also failed: {teardown:#}");
            Err(e)
        }
    }
}

struct Install {
    dir: PathBuf,
    endpoint: String,
    region: String,
}

impl Install {
    fn from_env() -> Result<Self> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("aws-install")
            .canonicalize()
            .context("resolve tests/e2e/aws-install")?;
        let endpoint = std::env::var("RISE_E2E_FLOCI_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://localhost:4566".into());
        let region = std::env::var("AWS_REGION")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "us-east-1".into());
        report::note(&format!("Floci at {endpoint}, region {region}"));
        Ok(Self {
            dir,
            endpoint,
            region,
        })
    }

    fn aws(&self, args: &[&str]) -> Result<String> {
        let mut c = Command::new("aws");
        self.floci_env(&mut c);
        c.args(args).args(["--output", "json"]);
        let out = cli::run(c)?;
        if !out.success() {
            anyhow::bail!("aws {} failed: {}", args.join(" "), out.stderr.trim());
        }
        Ok(out.stdout)
    }

    fn aws_json(&self, args: &[&str]) -> Result<Value> {
        let out = self.aws(args)?;
        serde_json::from_str(&out).with_context(|| format!("parse `aws {}`", args.join(" ")))
    }

    fn terraform(&self, args: &[&str]) -> Command {
        let mut c = Command::new("terraform");
        self.floci_env(&mut c);
        c.current_dir(&self.dir)
            .env("TF_IN_AUTOMATION", "1")
            .args(args);
        c
    }

    /// Point a command at Floci and nowhere else. The dummy credentials also
    /// keep a developer's own AWS profile out of it: nothing here should ever
    /// reach a real account.
    fn floci_env(&self, c: &mut Command) {
        c.env("AWS_ENDPOINT_URL", &self.endpoint)
            .env("AWS_REGION", &self.region)
            .env("AWS_DEFAULT_REGION", &self.region)
            .env("AWS_ACCESS_KEY_ID", "test")
            .env("AWS_SECRET_ACCESS_KEY", "test")
            .env_remove("AWS_PROFILE")
            .env_remove("AWS_SESSION_TOKEN")
            .env("AWS_PAGER", "");
    }

    fn wait_floci(&self) -> Result<()> {
        let url = format!("{}/_floci/health", self.endpoint.trim_end_matches('/'));
        http::poll(
            Duration::from_secs(60),
            Duration::from_secs(2),
            "Floci health",
            || Ok(http::get(&url, None)?.status == 200),
        )
    }

    /// A second plan against what the first apply built must change nothing.
    /// A perpetual diff means every operator `apply` rewrites something --
    /// replaces a secret version, rolls a task definition -- for no reason.
    fn assert_converged(&self, with_vars: &dyn Fn(&[&str]) -> Command) -> Result<()> {
        let plan_file = self.dir.join("converge.tfplan");
        let plan_arg = format!("-out={}", plan_file.display());
        cli::run_checked(with_vars(&["plan", "-input=false", &plan_arg]))?;
        let shown = cli::run_checked(self.terraform(&["show", "-json", "converge.tfplan"]));
        let _ = std::fs::remove_file(&plan_file);
        let plan: Value = serde_json::from_str(&shown?.stdout).context("parse plan JSON")?;

        let drift = unexpected_drift(&plan);
        if !drift.is_empty() {
            anyhow::bail!(
                "a second plan wants to change what the first apply built:\n  {}",
                drift.join("\n  ")
            );
        }
        Ok(())
    }

    /// The contract between the two modules. rise-aws scopes every policy by
    /// names -- cluster, log group, SSM and ECR prefixes, secret ARNs -- that it
    /// derives independently of rise-ecs, so a disagreement applies cleanly and
    /// fails only when Rise first makes the call. Floci evaluates IAM policies,
    /// so ask it directly: for each call the control plane, its tasks and
    /// Traefik make at runtime, against the resources this install actually
    /// configured Rise with, is the answer the one the design intends? The
    /// denials matter as much as the grants -- they are the scoping.
    fn assert_wiring(&self) -> Result<usize> {
        let task = |family: &str| -> Result<Value> {
            let td = self.aws_json(&[
                "ecs",
                "describe-task-definition",
                "--task-definition",
                family,
            ])?;
            Ok(td["taskDefinition"].clone())
        };
        let control_plane = task(&format!("{NAME}-control-plane"))?;
        let traefik = task(&format!("{NAME}-traefik"))?;
        let container = &control_plane["containerDefinitions"][0];

        // Rise's view of its own environment: what it will call AWS with.
        let env: std::collections::HashMap<&str, &str> = container["environment"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e["name"].as_str()?, e["value"].as_str()?)))
            .collect();
        let var = |k: &str| -> Result<&str> {
            env.get(k)
                .copied()
                .with_context(|| format!("the control plane has no {k}"))
        };
        let str_of = |v: &Value, what: &str| -> Result<String> {
            v.as_str()
                .map(str::to_string)
                .with_context(|| format!("no {what}"))
        };

        let region = &self.region;
        let account = str_of(
            &self.aws_json(&["sts", "get-caller-identity"])?["Account"],
            "account",
        )?;
        let cluster = str_of(
            &self.aws_json(&[
                "ecs",
                "describe-clusters",
                "--clusters",
                var("RISE_ECS_CLUSTER")?,
            ])?["clusters"][0]["clusterArn"],
            "cluster ARN",
        )?;
        let other_cluster = format!("arn:aws:ecs:{region}:{account}:cluster/elsewhere");
        let controller = str_of(&control_plane["taskRoleArn"], "control-plane task role")?;
        let execution = var("RISE_ECS_EXECUTION_ROLE_ARN")?.to_string();
        let workload = var("RISE_ECS_TASK_ROLE_ARN")?.to_string();
        let traefik_role = str_of(&traefik["taskRoleArn"], "traefik task role")?;
        let log_group = var("RISE_ECS_LOG_GROUP")?;
        let ssm_prefix = var("RISE_ECS_SSM_PREFIX")?.trim_matches('/');
        let repo = format!(
            "arn:aws:ecr:{region}:{account}:repository/{}e2e",
            var("RISE_ECR_REPO_PREFIX")?
        );
        // One deployment's names, built the way the ECS reconciler builds them.
        let service = format!(
            "arn:aws:ecs:{region}:{account}:service/{}/{}-e2e-default-1-web",
            var("RISE_ECS_CLUSTER")?,
            var("RISE_ECS_RESOURCE_PREFIX")?
        );
        let parameter =
            format!("arn:aws:ssm:{region}:{account}:parameter/{ssm_prefix}/e2e/default/1/KEY");
        let foreign_parameter =
            format!("arn:aws:ssm:{region}:{account}:parameter/elsewhere/e2e/default/1/KEY");
        let log_stream =
            format!("arn:aws:logs:{region}:{account}:log-group:{log_group}:log-stream:rise/e2e");
        let log_group_arn = format!("arn:aws:logs:{region}:{account}:log-group:{log_group}");
        anyhow::ensure!(
            control_plane["executionRoleArn"] == execution.as_str(),
            "the control plane pulls as {} but hands its workloads {execution}",
            control_plane["executionRoleArn"]
        );

        let on_cluster = |c: &str| vec![("ecs:cluster", c.to_string())];
        let to_ecs = || vec![("iam:PassedToService", "ecs-tasks.amazonaws.com".to_string())];
        let mut probes: Vec<Probe> = vec![
            // The control plane, reconciling a deployment.
            Probe::allow(&controller, "ecs:DescribeClusters", &cluster),
            Probe::allow(&controller, "ecs:CreateService", &service).with(on_cluster(&cluster)),
            Probe::deny(&controller, "ecs:CreateService", &service)
                .with(on_cluster(&other_cluster)),
            Probe::allow(&controller, "iam:PassRole", &execution).with(to_ecs()),
            Probe::allow(&controller, "iam:PassRole", &workload).with(to_ecs()),
            Probe::deny(&controller, "iam:PassRole", &controller).with(to_ecs()),
            Probe::allow(&controller, "ssm:PutParameter", &parameter),
            Probe::deny(&controller, "ssm:PutParameter", &foreign_parameter),
            Probe::allow(&controller, "ecr:CreateRepository", &repo),
            Probe::allow(
                &controller,
                "sts:AssumeRole",
                var("RISE_ECR_PUSH_ROLE_ARN")?,
            ),
            Probe::allow(&controller, "logs:FilterLogEvents", &log_group_arn),
            // ECS starting a deployed task under the execution role.
            Probe::allow(&execution, "ecr:BatchGetImage", &repo),
            Probe::allow(&execution, "ssm:GetParameters", &parameter),
            Probe::allow(&execution, "logs:PutLogEvents", &log_stream),
            // Traefik discovering what to route.
            Probe::allow(&traefik_role, "ecs:ListTasks", "*"),
            Probe::allow(&traefik_role, "ecs:DescribeTasks", "*"),
        ];
        // ECS resolving the control plane's own secrets at task start.
        for secret in container["secrets"].as_array().into_iter().flatten() {
            let arn = str_of(&secret["valueFrom"], "secret valueFrom")?;
            probes.push(Probe::allow(
                &execution,
                "secretsmanager:GetSecretValue",
                &arn,
            ));
        }

        let mut wrong = Vec::new();
        for p in &probes {
            let mut args = vec![
                "iam".to_string(),
                "simulate-principal-policy".into(),
                "--policy-source-arn".into(),
                p.role.clone(),
                "--action-names".into(),
                p.action.into(),
                "--resource-arns".into(),
                p.resource.clone(),
            ];
            for (key, value) in &p.context {
                args.push("--context-entries".into());
                args.push(format!(
                    "ContextKeyName={key},ContextKeyValues={value},ContextKeyType=string"
                ));
            }
            args.extend(["--query".into(), "EvaluationResults[0].EvalDecision".into()]);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let decision = self.aws_json(&args)?;
            let decision = decision.as_str().unwrap_or("no decision");
            let allowed = decision == "allowed";
            if allowed != p.allowed {
                wrong.push(format!(
                    "{} {} on {}: {decision}, expected {}",
                    p.role.rsplit('/').next().unwrap_or(&p.role),
                    p.action,
                    p.resource,
                    if p.allowed { "allowed" } else { "denied" }
                ));
            }
        }
        anyhow::ensure!(
            wrong.is_empty(),
            "rise-aws's policies disagree with what rise-ecs configured:\n  {}",
            wrong.join("\n  ")
        );
        Ok(probes.len())
    }
}

/// One IAM question: may `role` perform `action` on `resource`?
struct Probe {
    role: String,
    action: &'static str,
    resource: String,
    context: Vec<(&'static str, String)>,
    allowed: bool,
}

impl Probe {
    fn allow(role: &str, action: &'static str, resource: &str) -> Self {
        Self {
            role: role.to_string(),
            action,
            resource: resource.to_string(),
            context: Vec::new(),
            allowed: true,
        }
    }

    fn deny(role: &str, action: &'static str, resource: &str) -> Self {
        Self {
            allowed: false,
            ..Self::allow(role, action, resource)
        }
    }

    fn with(mut self, context: Vec<(&'static str, String)>) -> Self {
        self.context = context;
        self
    }
}

/// Every attribute a plan would change, minus [`EMULATOR_DRIFT`], as
/// `address: attribute` lines.
fn unexpected_drift(plan: &Value) -> Vec<String> {
    let mut drift = Vec::new();
    for rc in plan["resource_changes"].as_array().into_iter().flatten() {
        let change = &rc["change"];
        let actions: Vec<&str> = change["actions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if matches!(actions.as_slice(), ["no-op"] | ["read"]) {
            continue;
        }
        let address = rc["address"].as_str().unwrap_or("?");
        let kind = rc["type"].as_str().unwrap_or("?");
        let empty = serde_json::Map::new();
        let before = change["before"].as_object().unwrap_or(&empty);
        let after = change["after"].as_object().unwrap_or(&empty);
        let unknown = change["after_unknown"].as_object().unwrap_or(&empty);

        let changed: BTreeSet<&str> = before
            .keys()
            .chain(after.keys())
            .map(String::as_str)
            .filter(|k| before.get(*k) != after.get(*k) || unknown.contains_key(*k))
            .filter(|k| {
                !EMULATOR_DRIFT
                    .iter()
                    .any(|(t, attr, _)| *t == kind && attr == k)
            })
            .collect();
        // A create or delete with no attribute to blame is still drift.
        if changed.is_empty() && actions != ["update"] {
            drift.push(format!("{address}: {}", actions.join("+")));
        }
        for attr in changed {
            drift.push(format!("{address}: {attr} ({})", actions.join("+")));
        }
    }
    drift
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(changes: Value) -> Value {
        json!({ "resource_changes": changes })
    }

    #[test]
    fn an_empty_plan_has_no_drift() {
        let p = plan(json!([{
            "address": "aws_iam_role.x", "type": "aws_iam_role",
            "change": { "actions": ["no-op"], "before": {"a": 1}, "after": {"a": 1} }
        }]));
        assert!(unexpected_drift(&p).is_empty());
    }

    #[test]
    fn a_known_emulator_gap_is_not_drift() {
        let p = plan(json!([{
            "address": "module.db.aws_db_instance.this[0]", "type": "aws_db_instance",
            "change": {
                "actions": ["update"],
                "before": {"storage_type": "gp2", "engine": "postgres"},
                "after": {"storage_type": "gp3", "engine": "postgres"},
                "after_unknown": {}
            }
        }]));
        assert!(unexpected_drift(&p).is_empty());
    }

    #[test]
    fn a_replacement_names_the_attribute_that_forced_it() {
        let p = plan(json!([{
            "address": "aws_secretsmanager_secret_version.s", "type": "aws_secretsmanager_secret_version",
            "change": {
                "actions": ["delete", "create"],
                "before": {"secret_string_wo_version": 1, "arn": "a"},
                "after": {"secret_string_wo_version": 2},
                "after_unknown": {"arn": true}
            }
        }]));
        assert_eq!(
            unexpected_drift(&p),
            vec![
                "aws_secretsmanager_secret_version.s: arn (delete+create)",
                "aws_secretsmanager_secret_version.s: secret_string_wo_version (delete+create)",
            ]
        );
    }

    #[test]
    fn the_same_attribute_on_another_type_is_still_drift() {
        let p = plan(json!([{
            "address": "aws_ebs_volume.v", "type": "aws_ebs_volume",
            "change": {
                "actions": ["update"],
                "before": {"storage_type": "gp2"},
                "after": {"storage_type": "gp3"},
                "after_unknown": {}
            }
        }]));
        assert_eq!(
            unexpected_drift(&p),
            vec!["aws_ebs_volume.v: storage_type (update)"]
        );
    }
}
