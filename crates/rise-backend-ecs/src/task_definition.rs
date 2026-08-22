//! Pure `DesiredContainer` → ECS task-definition mapping.
//!
//! Produces a [`TaskDefinitionSpec`] value object rather than SDK types, so the
//! whole mapping is unit-testable with no AWS in sight — the same discipline
//! that lets the Docker backend test its bollard create-spec offline.
//! [`crate::reconciler`] converts the spec into `RegisterTaskDefinition` input.
//!
//! Two properties here carry real consequence and are covered by tests:
//!
//! 1. **Secrets never appear in `environment`.** They are written to SSM and
//!    referenced by ARN in `secrets`, so `DescribeTaskDefinition` — readable by
//!    anyone with `ecs:DescribeTaskDefinition` — reveals a parameter name and
//!    nothing else. This is the whole point of ADR-0004 D7 and the one place
//!    the Docker backend's documented plaintext gap is closed rather than
//!    inherited.
//! 2. **Traefik configuration rides in `dockerLabels`.** Traefik's ECS provider
//!    reads container `dockerLabels` and nothing else; putting the labels
//!    anywhere else means the router is never created and the app is simply
//!    unreachable, with no error anywhere.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use rise_backend_core::desired::DesiredContainer;
use rise_backend_core::naming::sanitize_ecs_name;
use rise_backend_core::traefik_render::{render_traefik_labels_for, TraefikRenderConfig};

use crate::sizing::{self, FargateSize};

/// ECS caps a task definition at 64 KiB. Plain env values and one ~120-character
/// SSM ARN per secret both count toward it. We check before registering so an
/// over-large deployment fails with a clear message instead of an opaque
/// `ClientException` from `RegisterTaskDefinition`.
pub const MAX_TASK_DEFINITION_BYTES: usize = 64 * 1024;

/// ECS allows at most 10 container definitions per task.
pub const MAX_CONTAINERS_PER_TASK: usize = 10;

/// A secret env var, resolved to the SSM parameter holding its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    /// Environment variable name inside the container.
    pub name: String,
    /// SSM parameter name or ARN ECS resolves at task start.
    pub value_from: String,
}

/// Where a container's stdout/stderr go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    pub log_group: String,
    pub region: String,
    pub stream_prefix: String,
}

/// One container definition within a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerDefinitionSpec {
    pub name: String,
    pub image: String,
    pub port: Option<u16>,
    /// Plain environment, sorted so the rendered task definition is byte-stable
    /// across reconciles (an unstable ordering would register a fresh revision
    /// every tick and burn the 1/s `RegisterTaskDefinition` budget).
    pub environment: BTreeMap<String, String>,
    /// Secret environment, injected by ECS from SSM at task start.
    pub secrets: Vec<SecretRef>,
    /// Traefik dynamic configuration. Empty for a non-routable container.
    pub docker_labels: BTreeMap<String, String>,
    pub log_config: Option<LogConfig>,
}

/// A complete Fargate task definition, ready to convert to SDK input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDefinitionSpec {
    pub family: String,
    pub cpu: String,
    pub memory: String,
    pub cpu_architecture: String,
    pub execution_role_arn: Option<String>,
    pub task_role_arn: Option<String>,
    pub containers: Vec<ContainerDefinitionSpec>,
    /// Resolved size, retained so the reconciler can log a round-up.
    pub size: FargateSize,
}

impl TaskDefinitionSpec {
    /// Rough serialized size, used to fail early against the 64 KiB ceiling.
    /// Deliberately an over-estimate of the JSON payload rather than an exact
    /// one: being conservative here costs nothing, while under-estimating means
    /// the error resurfaces from AWS at reconcile time.
    pub fn approx_bytes(&self) -> usize {
        let mut n = self.family.len() + self.cpu.len() + self.memory.len() + 256;
        for c in &self.containers {
            n += c.name.len() + c.image.len() + 128;
            for (k, v) in &c.environment {
                n += k.len() + v.len() + 24;
            }
            for s in &c.secrets {
                n += s.name.len() + s.value_from.len() + 28;
            }
            for (k, v) in &c.docker_labels {
                n += k.len() + v.len() + 12;
            }
        }
        n
    }

    /// Content hash over everything that determines the registered revision.
    ///
    /// The reconciler registers a new revision only when this changes, so a
    /// steady deployment costs zero `RegisterTaskDefinition` calls — which
    /// matters because that API sustains only 1 request/second.
    pub fn content_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(self.family.as_bytes());
        field(self.cpu.as_bytes());
        field(self.memory.as_bytes());
        field(self.cpu_architecture.as_bytes());
        field(self.execution_role_arn.as_deref().unwrap_or("").as_bytes());
        field(self.task_role_arn.as_deref().unwrap_or("").as_bytes());
        for c in &self.containers {
            field(c.name.as_bytes());
            field(c.image.as_bytes());
            field(c.port.map(|p| p.to_string()).unwrap_or_default().as_bytes());
            for (k, v) in &c.environment {
                field(k.as_bytes());
                field(v.as_bytes());
            }
            for s in &c.secrets {
                field(s.name.as_bytes());
                field(s.value_from.as_bytes());
            }
            for (k, v) in &c.docker_labels {
                field(k.as_bytes());
                field(v.as_bytes());
            }
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Static configuration the task-definition builder needs.
pub struct TaskDefinitionConfig<'a> {
    pub resource_prefix: &'a str,
    pub cpu_architecture: &'a str,
    pub execution_role_arn: Option<&'a str>,
    pub task_role_arn: Option<&'a str>,
    pub log_group: Option<&'a str>,
    pub region: &'a str,
    pub traefik: TraefikRenderConfig<'a>,
}

/// Task-definition family for a container spec.
///
/// Deliberately **deployment-id free**: a family accumulates one revision per
/// deploy, which is how ECS models the history of a workload, and is what makes
/// a rollback a revision reference rather than an archaeology exercise.
pub fn family_name(
    resource_prefix: &str,
    project: &str,
    deployment_group: &str,
    container: &str,
) -> String {
    sanitize_ecs_name(&format!(
        "{resource_prefix}-{project}-{deployment_group}-{container}"
    ))
}

/// Build the task definition for one desired container.
///
/// `secrets` maps env var name → SSM parameter name/ARN; those names are removed
/// from the plain environment. The caller (the reconciler) has already written
/// the parameters, so a name appearing here is guaranteed resolvable.
pub fn build(
    desired: &DesiredContainer,
    secrets: &[SecretRef],
    cfg: &TaskDefinitionConfig<'_>,
) -> Result<TaskDefinitionSpec> {
    let size = sizing::resolve(&desired.cpu, &desired.memory)?;

    // Secrets are injected by ECS; their names must not also appear as plain
    // environment or the plaintext would be back in the task definition.
    let secret_names: std::collections::HashSet<&str> =
        secrets.iter().map(|s| s.name.as_str()).collect();
    let environment: BTreeMap<String, String> = desired
        .env
        .iter()
        .filter(|(k, _)| !secret_names.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let docker_labels: BTreeMap<String, String> = render_traefik_labels_for(desired, &cfg.traefik)
        .into_iter()
        .collect();

    let log_config = cfg.log_group.map(|group| LogConfig {
        log_group: group.to_string(),
        region: cfg.region.to_string(),
        stream_prefix: format!("{}-{}", desired.project, desired.deployment_group),
    });

    let spec = TaskDefinitionSpec {
        family: family_name(
            cfg.resource_prefix,
            &desired.project,
            &desired.deployment_group,
            &desired.container,
        ),
        cpu: size.cpu_string(),
        memory: size.memory_string(),
        cpu_architecture: cfg.cpu_architecture.to_string(),
        execution_role_arn: cfg.execution_role_arn.map(str::to_string),
        task_role_arn: cfg.task_role_arn.map(str::to_string),
        containers: vec![ContainerDefinitionSpec {
            name: desired.container.clone(),
            image: desired.image.clone(),
            port: desired.port,
            environment,
            secrets: secrets.to_vec(),
            docker_labels,
            log_config,
        }],
        size,
    };

    if spec.containers.len() > MAX_CONTAINERS_PER_TASK {
        bail!(
            "task definition has {} containers, exceeding the ECS limit of {MAX_CONTAINERS_PER_TASK}",
            spec.containers.len()
        );
    }
    let bytes = spec.approx_bytes();
    if bytes > MAX_TASK_DEFINITION_BYTES {
        bail!(
            "task definition is ~{bytes} bytes, over the ECS limit of \
             {MAX_TASK_DEFINITION_BYTES}. Reduce the number or size of environment \
             variables — large values belong in a secret or an object store."
        );
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rise_backend_core::desired::DesiredRoute;
    use rise_backend_core::AccessRequirement;
    use std::collections::HashMap;

    fn access_classes() -> HashMap<String, AccessRequirement> {
        let mut m = HashMap::new();
        m.insert("public".to_string(), AccessRequirement::None);
        m.insert("private".to_string(), AccessRequirement::Member);
        m
    }

    fn cfg<'a>(classes: &'a HashMap<String, AccessRequirement>) -> TaskDefinitionConfig<'a> {
        TaskDefinitionConfig {
            resource_prefix: "rise",
            cpu_architecture: "X86_64",
            execution_role_arn: Some("arn:aws:iam::1:role/exec"),
            task_role_arn: None,
            log_group: Some("/rise/myapp"),
            region: "eu-central-1",
            traefik: TraefikRenderConfig {
                label_namespace: "rise.dev",
                controller_class: "default",
                traefik_entrypoint: "web",
                traefik_certresolver: None,
                // ECS must never receive traefik.docker.network.
                network: None,
                auth_backend_url: "http://rise.rise-e2e.local:3000",
                access_classes: classes,
            },
        }
    }

    fn desired() -> DesiredContainer {
        DesiredContainer {
            project: "myapp".to_string(),
            access_class: "public".to_string(),
            deployment_group: "default".to_string(),
            deployment_id: "20260101-120000".to_string(),
            deployment_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            container: "app".to_string(),
            environment: None,
            image: "registry/myapp:20260101-120000".to_string(),
            port: Some(8080),
            cpu: "500m".to_string(),
            memory: "256Mi".to_string(),
            env: vec![
                ("PORT".to_string(), "8080".to_string()),
                ("API_KEY".to_string(), "super-secret-value".to_string()),
            ],
            env_hash: "envhash".to_string(),
            routes: vec![DesiredRoute {
                hosts: vec!["myapp.rise.dev".to_string()],
                path_prefix: None,
                access: None,
            }],
            routable: true,
            route_hash: "rh".to_string(),
            generation: 1,
            replica: 0,
            health_path: Some("/".to_string()),
            health_check_interval_secs: None,
            health_check_timeout_secs: None,
        }
    }

    #[test]
    fn secret_values_never_appear_in_the_task_definition() {
        // The reason D7 exists. Anyone with ecs:DescribeTaskDefinition can read
        // a task definition; if the plaintext were in `environment`, the SSM
        // indirection would be decoration and the Docker backend's documented
        // plaintext gap would simply have been inherited.
        let classes = access_classes();
        let secrets = vec![SecretRef {
            name: "API_KEY".to_string(),
            value_from:
                "arn:aws:ssm:eu-central-1:1:parameter/rise/myapp/default/20260101-120000/API_KEY"
                    .to_string(),
        }];
        let spec = build(&desired(), &secrets, &cfg(&classes)).expect("builds");
        let container = &spec.containers[0];

        assert!(
            !container.environment.contains_key("API_KEY"),
            "the secret's name must not remain in plain environment"
        );
        let rendered = format!("{container:?}");
        assert!(
            !rendered.contains("super-secret-value"),
            "the secret VALUE leaked into the task definition: {rendered}"
        );
        assert_eq!(container.secrets, secrets);
        // Non-secret env is untouched.
        assert_eq!(
            container.environment.get("PORT").map(String::as_str),
            Some("8080")
        );
    }

    #[test]
    fn traefik_config_lands_in_docker_labels() {
        // Traefik's ECS provider reads container dockerLabels and nothing else.
        // Emit them anywhere else and the router is never created — the app is
        // unreachable and no component logs an error.
        let classes = access_classes();
        let spec = build(&desired(), &[], &cfg(&classes)).expect("builds");
        let labels = &spec.containers[0].docker_labels;

        assert_eq!(
            labels.get("traefik.enable").map(String::as_str),
            Some("true")
        );
        assert!(
            labels.keys().any(|k| k.ends_with(".rule")),
            "no router rule label emitted: {labels:?}"
        );
        assert!(
            labels
                .keys()
                .any(|k| k.contains("loadbalancer.server.port")),
            "no service port label emitted: {labels:?}"
        );
    }

    #[test]
    fn the_docker_network_label_is_never_emitted_for_ecs() {
        // ECS tasks are on awsvpc ENIs. traefik.docker.network would make the
        // ECS provider try to resolve a Docker network that does not exist.
        let classes = access_classes();
        let spec = build(&desired(), &[], &cfg(&classes)).expect("builds");
        assert!(
            !spec.containers[0]
                .docker_labels
                .contains_key("traefik.docker.network"),
            "the Docker-provider network label must not reach ECS"
        );
    }

    #[test]
    fn a_worker_container_gets_no_traefik_labels() {
        // A port-less container is not routable; emitting a router for it would
        // register a Traefik service with no reachable server, which then reports
        // DOWN and blocks the whole deployment from ever becoming Healthy.
        let classes = access_classes();
        let mut worker = desired();
        worker.port = None;
        worker.routes = vec![];
        let spec = build(&worker, &[], &cfg(&classes)).expect("builds");
        assert!(spec.containers[0].docker_labels.is_empty());
    }

    #[test]
    fn content_hash_changes_with_image_env_and_labels_but_not_with_reordering() {
        // The reconciler registers a new revision only when this hash moves.
        // Too sensitive and it burns the 1/s RegisterTaskDefinition budget every
        // tick; too insensitive and a changed image never rolls out.
        let classes = access_classes();
        let c = cfg(&classes);
        let base = build(&desired(), &[], &c).expect("builds");

        let mut reordered = desired();
        reordered.env.reverse();
        assert_eq!(
            base.content_hash(),
            build(&reordered, &[], &c).expect("builds").content_hash(),
            "env ordering must not move the hash — it would re-register every tick"
        );

        let mut new_image = desired();
        new_image.image = "registry/myapp:20260101-130000".to_string();
        assert_ne!(
            base.content_hash(),
            build(&new_image, &[], &c).expect("builds").content_hash(),
            "a new image must move the hash or the deploy never rolls out"
        );

        let mut new_env = desired();
        new_env.env.push(("EXTRA".to_string(), "1".to_string()));
        assert_ne!(
            base.content_hash(),
            build(&new_env, &[], &c).expect("builds").content_hash()
        );
    }

    #[test]
    fn changing_only_the_secret_arn_moves_the_hash() {
        // Rotating a secret rewrites its SSM parameter under a new deployment
        // path. If the hash ignored `secrets`, the service would keep pointing
        // at the old parameter — which the GC then deletes, and every task
        // start fails to resolve it.
        let classes = access_classes();
        let c = cfg(&classes);
        let a = build(
            &desired(),
            &[SecretRef {
                name: "API_KEY".to_string(),
                value_from: "/rise/myapp/default/dep-a/API_KEY".to_string(),
            }],
            &c,
        )
        .expect("builds");
        let b = build(
            &desired(),
            &[SecretRef {
                name: "API_KEY".to_string(),
                value_from: "/rise/myapp/default/dep-b/API_KEY".to_string(),
            }],
            &c,
        )
        .expect("builds");
        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn family_name_is_deployment_id_free_and_stable() {
        // One family per workload, one revision per deploy. Folding the
        // deployment id into the family would create a fresh family per deploy
        // and lose the revision history a rollback relies on.
        let a = family_name("rise", "myapp", "default", "app");
        let b = family_name("rise", "myapp", "default", "app");
        assert_eq!(a, b);
        assert!(!a.contains("20260101"));
        assert_eq!(a, "rise-myapp-default-app");
    }

    #[test]
    fn oversized_environment_is_rejected_before_registration() {
        // Better a clear deploy-time error than ECS's opaque ClientException.
        let classes = access_classes();
        let mut huge = desired();
        huge.env = (0..500)
            .map(|i| (format!("VAR_{i}"), "x".repeat(200)))
            .collect();
        let err = build(&huge, &[], &cfg(&classes)).expect_err("must be rejected");
        assert!(
            err.to_string().contains("over the ECS limit"),
            "unhelpful message: {err}"
        );
    }
}
