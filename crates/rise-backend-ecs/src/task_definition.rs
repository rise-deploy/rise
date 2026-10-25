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
//!    nothing else. This is the whole point of ADR-0005 D7 and the one place
//!    the Docker backend's documented plaintext gap is closed rather than
//!    inherited.
//! 2. **Traefik configuration rides in `dockerLabels`.** Traefik's ECS provider
//!    reads container `dockerLabels` and nothing else; putting the labels
//!    anywhere else means the router is never created and the app is simply
//!    unreachable, with no error anywhere.
//!
//! Every task also carries the workload-identity sidecar (ADR-0005 D8), which
//! writes the identity files onto a volume shared with the app; see
//! [`IdentityAgentSpec`].

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use rise_backend_core::desired::DesiredContainer;
use rise_backend_core::identity::IDENTITY_MOUNT_PATH;
use rise_backend_core::labels::{ns_key, SUFFIX_CONTROLLER_CLASS};
use rise_backend_core::naming::sanitize_ecs_name;
use rise_backend_traefik::render::{render_traefik_labels_for, TraefikRenderConfig};

use crate::capacity::Compatibility;
use crate::sizing::{self, TaskSize};

/// ECS caps a task definition at 64 KiB. Plain env values and one ~120-character
/// SSM ARN per secret both count toward it. We check before registering so an
/// over-large deployment fails with a clear message instead of an opaque
/// `ClientException` from `RegisterTaskDefinition`.
pub const MAX_TASK_DEFINITION_BYTES: usize = 64 * 1024;

/// ECS allows at most 10 container definitions per task.
pub const MAX_CONTAINERS_PER_TASK: usize = 10;

/// Container name of the workload-identity sidecar. Reserved: an app container
/// by this name is rejected rather than silently shadowed.
pub const IDENTITY_AGENT_CONTAINER: &str = "rise-identity-agent";

/// Task volume shared by the sidecar (read-write) and the app (read-only).
pub const IDENTITY_VOLUME: &str = "rise-identity";

/// Where the Rise image keeps its binary; the sidecar's health check runs it.
const RISE_BINARY: &str = "/usr/local/bin/rise";

/// Environment variable the sidecar reads its bootstrap credential from.
pub const IDENTITY_CREDENTIAL_ENV: &str = "RISE_IDENTITY_CREDENTIAL";

/// Environment variable naming the token-exchange endpoint the sidecar mints
/// the token files from. Set only when the deployment declares audiences.
pub const IDENTITY_TOKEN_URL_ENV: &str = "RISE_IDENTITY_TOKEN_URL";

/// Environment variable carrying `[identity].audiences` (filename → audience)
/// as a JSON object. Not secret: it is the deployment's own `rise.toml`.
pub const IDENTITY_AUDIENCES_ENV: &str = "RISE_IDENTITY_AUDIENCES";

/// Environment variable carrying the token lifetime the sidecar asks for.
pub const IDENTITY_TOKEN_TTL_ENV: &str = "RISE_IDENTITY_TOKEN_TTL_SECONDS";

/// Memory the sidecar reserves out of the task's size. A soft reservation, not
/// a limit: it keeps placement honest without capping an agent that briefly
/// needs more.
pub const IDENTITY_AGENT_MEMORY_RESERVATION_MIB: i32 = 32;

/// How long the sidecar may take to write its files before failed health checks
/// count against it — ECS's maximum.
pub const IDENTITY_AGENT_START_PERIOD_SECS: i32 = 300;

/// What the workload-identity sidecar needs to be rendered for one deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityAgentSpec {
    /// The Rise image the sidecar runs (`rise identity agent`).
    pub image: String,
    /// SSM parameter holding this deployment's bootstrap credential.
    pub credential_parameter: String,
    /// Full URL of the token-exchange endpoint.
    pub token_url: String,
    /// `[identity].audiences`, filename → audience. Empty when the deployment
    /// declares none: the credential is then all it gets.
    pub audiences: BTreeMap<String, String>,
    /// Lifetime to request for each token: `identity_token_ttl_seconds`, the
    /// lifetime the other backends give the token files.
    pub token_ttl_seconds: u64,
}

/// A container's view of a task volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPointSpec {
    pub source_volume: String,
    pub container_path: String,
    pub read_only: bool,
}

/// A container health check, in ECS's terms (seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheckSpec {
    pub command: Vec<String>,
    pub interval: i32,
    pub timeout: i32,
    pub retries: i32,
    pub start_period: i32,
}

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
    /// Secrets Manager secret ARN ECS reads to authenticate the image pull.
    ///
    /// Only for a private registry that is not ECR: ECR pulls are authenticated
    /// by the execution role itself. ECS re-reads this at every task start, so
    /// rotating the secret's contents needs no redeploy — but pointing at a
    /// different secret does, which is why the ARN is part of `content_hash`.
    pub repository_credentials_secret_arn: Option<String>,
    /// Whether the task stops when this container does. `false` only for the
    /// identity sidecar, which ECS restarts in place instead.
    pub essential: bool,
    /// Arguments to the image's entrypoint. Empty keeps the image's own `CMD`.
    pub command: Vec<String>,
    pub mount_points: Vec<MountPointSpec>,
    /// Containers that must report `HEALTHY` before this one starts.
    pub depends_on_healthy: Vec<String>,
    pub health_check: Option<HealthCheckSpec>,
    /// Restart the container in place when it exits (ECS container restart
    /// policy). Only meaningful for a non-essential container.
    pub restart_on_exit: bool,
    pub memory_reservation_mib: Option<i32>,
    /// Whether the image is part of [`TaskDefinitionSpec::content_hash`]. `false`
    /// only for the identity sidecar: its image follows the Rise version, and
    /// hashing it would roll every service in the install on every upgrade.
    /// New deployments still get the new image; running ones keep theirs.
    pub image_in_hash: bool,
}

/// A complete ECS task definition, ready to convert to SDK input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDefinitionSpec {
    pub family: String,
    pub cpu: String,
    pub memory: String,
    pub cpu_architecture: String,
    pub execution_role_arn: Option<String>,
    pub task_role_arn: Option<String>,
    /// The app container first, then any sidecars. Code that means "the app"
    /// goes through [`TaskDefinitionSpec::app`].
    pub containers: Vec<ContainerDefinitionSpec>,
    /// Task-scoped volumes (no host path), by name.
    pub volumes: Vec<String>,
    /// Resolved size, retained so the reconciler can log a round-up.
    pub size: TaskSize,
}

impl TaskDefinitionSpec {
    /// The app container. [`build`] always renders it first.
    pub fn app(&self) -> &ContainerDefinitionSpec {
        &self.containers[0]
    }

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
            n += c
                .repository_credentials_secret_arn
                .as_deref()
                .map_or(0, |a| a.len() + 32);
            for (k, v) in &c.docker_labels {
                n += k.len() + v.len() + 12;
            }
            if let Some(log) = &c.log_config {
                n += log.log_group.len() + log.region.len() + log.stream_prefix.len() + 64;
            }
            n += c.command.iter().map(|a| a.len() + 4).sum::<usize>();
            for m in &c.mount_points {
                n += m.source_volume.len() + m.container_path.len() + 64;
            }
            n += c
                .depends_on_healthy
                .iter()
                .map(|d| d.len() + 48)
                .sum::<usize>();
            if let Some(hc) = &c.health_check {
                n += hc.command.iter().map(|a| a.len() + 4).sum::<usize>() + 96;
            }
            n += 96; // essential, restart policy, memory reservation
        }
        n + self.volumes.iter().map(|v| v.len() + 24).sum::<usize>()
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
            field(if c.image_in_hash {
                c.image.as_bytes()
            } else {
                b""
            });
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
            if let Some(log) = &c.log_config {
                field(log.log_group.as_bytes());
                field(log.region.as_bytes());
                field(log.stream_prefix.as_bytes());
            } else {
                field(b"");
                field(b"");
                field(b"");
            }
            field(
                c.repository_credentials_secret_arn
                    .as_deref()
                    .unwrap_or("")
                    .as_bytes(),
            );
            field(&[u8::from(c.essential), u8::from(c.restart_on_exit)]);
            field(&(c.command.len() as u64).to_le_bytes());
            for arg in &c.command {
                field(arg.as_bytes());
            }
            field(&(c.mount_points.len() as u64).to_le_bytes());
            for m in &c.mount_points {
                field(m.source_volume.as_bytes());
                field(m.container_path.as_bytes());
                field(&[u8::from(m.read_only)]);
            }
            field(&(c.depends_on_healthy.len() as u64).to_le_bytes());
            for d in &c.depends_on_healthy {
                field(d.as_bytes());
            }
            match &c.health_check {
                Some(hc) => {
                    field(&(hc.command.len() as u64).to_le_bytes());
                    for arg in &hc.command {
                        field(arg.as_bytes());
                    }
                    for v in [hc.interval, hc.timeout, hc.retries, hc.start_period] {
                        field(&v.to_le_bytes());
                    }
                }
                None => field(b""),
            }
            field(&c.memory_reservation_mib.unwrap_or(0).to_le_bytes());
        }
        field(&(self.volumes.len() as u64).to_le_bytes());
        for v in &self.volumes {
            field(v.as_bytes());
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
    /// What the task definition must declare in `requiresCompatibilities`, and
    /// therefore whether `cpu`/`memory` are restricted to Fargate's size table.
    pub compatibility: Compatibility,
    pub execution_role_arn: Option<&'a str>,
    pub task_role_arn: Option<&'a str>,
    /// Secrets Manager secret ARN for a private non-ECR registry; see
    /// [`ContainerDefinitionSpec::repository_credentials_secret_arn`].
    pub repository_credentials_secret_arn: Option<&'a str>,
    pub log_group: Option<&'a str>,
    pub region: &'a str,
    pub traefik: TraefikRenderConfig<'a>,
    /// Namespace for Rise's own label keys, e.g. `rise.dev`.
    pub label_namespace: &'a str,
    /// Stamped into `dockerLabels` so a Traefik constrained to one controller
    /// class routes only that install's containers. See [`build`].
    pub controller_class: &'a str,
}

/// The two CPU architectures Fargate accepts, in the exact spelling ECS's
/// `runtimePlatform` expects.
pub const FARGATE_CPU_ARCHITECTURES: [&str; 2] = ["X86_64", "ARM64"];

/// Canonicalise a configured CPU architecture to the token ECS expects.
///
/// Operators reach for whichever spelling their toolchain uses — `amd64` from
/// Docker and OCI, `aarch64` from `uname`, lower case from a shell variable —
/// and all of them mean one of exactly two things to Fargate. Accept them, and
/// **fail on anything else rather than passing it through**: the AWS SDK's
/// `CpuArchitecture::from` maps an unrecognised string to an `Unknown` variant
/// that serialises verbatim, so a typo would survive all the way to
/// `RegisterTaskDefinition` and come back as an AWS-shaped error, per
/// deployment, long after the operator who made it stopped looking.
pub fn canonical_cpu_architecture(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "x86-64" | "amd64" | "x64" => Ok("X86_64"),
        "arm64" | "aarch64" => Ok("ARM64"),
        other => bail!(
            "cpu_architecture {other:?} is not a Fargate CPU architecture. Use {} \
             (or a common spelling of one: amd64, x86_64, arm64, aarch64).",
            FARGATE_CPU_ARCHITECTURES.join(" or ")
        ),
    }
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
///
/// `identity` adds the workload-identity sidecar and the volume it shares with
/// the app. `None` renders the app alone, for a deployment that was already
/// running before the sidecar existed (see the reconciler).
pub fn build(
    desired: &DesiredContainer,
    secrets: &[SecretRef],
    identity: Option<&IdentityAgentSpec>,
    cfg: &TaskDefinitionConfig<'_>,
) -> Result<TaskDefinitionSpec> {
    if desired.container == IDENTITY_AGENT_CONTAINER {
        bail!(
            "container name {IDENTITY_AGENT_CONTAINER:?} is reserved for Rise's workload-identity \
             sidecar on the ECS backend; rename the container"
        );
    }
    let size = sizing::resolve(&desired.cpu, &desired.memory, cfg.compatibility)?;

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

    let mut docker_labels: BTreeMap<String, String> =
        render_traefik_labels_for(desired, &cfg.traefik)
            .into_iter()
            .collect();
    // Rise's bookkeeping otherwise lives in ECS resource tags, which Traefik's
    // ECS provider never reads -- it matches `constraints` against container
    // labels only. Emitting the controller class here is what lets several Rise
    // installs share one cluster: each Traefik is constrained to its own class
    // and ignores the others' containers. The key cannot sit under `traefik.*`,
    // which Traefik reserves for its own configuration and rejects as a
    // constraint key.
    docker_labels.insert(
        ns_key(cfg.label_namespace, SUFFIX_CONTROLLER_CLASS),
        cfg.controller_class.to_string(),
    );

    let log_config = cfg.log_group.map(|group| LogConfig {
        log_group: group.to_string(),
        region: cfg.region.to_string(),
        stream_prefix: format!(
            "{}/{}/{}",
            sanitize_ecs_name(cfg.resource_prefix),
            desired.project_uuid,
            desired.deployment_uuid
        ),
    });

    let mut app = ContainerDefinitionSpec {
        name: desired.container.clone(),
        image: desired.image.clone(),
        port: desired.port,
        environment,
        secrets: secrets.to_vec(),
        docker_labels,
        log_config,
        repository_credentials_secret_arn: cfg
            .repository_credentials_secret_arn
            .map(str::to_string),
        essential: true,
        command: Vec::new(),
        mount_points: Vec::new(),
        depends_on_healthy: Vec::new(),
        health_check: None,
        restart_on_exit: false,
        memory_reservation_mib: None,
        image_in_hash: true,
    };
    let mut containers = Vec::new();
    let mut volumes = Vec::new();
    if let Some(identity) = identity {
        app.mount_points.push(MountPointSpec {
            source_volume: IDENTITY_VOLUME.to_string(),
            container_path: IDENTITY_MOUNT_PATH.to_string(),
            read_only: true,
        });
        // The files must exist before the app's process first looks for them.
        app.depends_on_healthy
            .push(IDENTITY_AGENT_CONTAINER.to_string());
        containers.push(app);
        containers.push(identity_agent_container(desired, identity, cfg));
        volumes.push(IDENTITY_VOLUME.to_string());
    } else {
        containers.push(app);
    }

    let spec = TaskDefinitionSpec {
        family: family_name(
            cfg.resource_prefix,
            &desired.project,
            &desired.deployment_group,
            &desired.container,
        ),
        cpu: size.cpu_string(),
        memory: size.memory_string(),
        cpu_architecture: canonical_cpu_architecture(cfg.cpu_architecture)?.to_string(),
        execution_role_arn: cfg.execution_role_arn.map(str::to_string),
        task_role_arn: cfg.task_role_arn.map(str::to_string),
        containers,
        volumes,
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

/// The workload-identity sidecar's container definition.
///
/// Non-essential with a restart policy: a crashed agent comes back in place
/// rather than taking the app's task down. It carries no Traefik labels --
/// Traefik runs with `exposedByDefault=false` and must never route to it -- and
/// logs under its own stream prefix, outside the `{prefix}/{project}/{deployment}/`
/// subtree the deployment log reader lists, so `rise deployment logs` shows the
/// app alone, as on the other backends.
fn identity_agent_container(
    desired: &DesiredContainer,
    identity: &IdentityAgentSpec,
    cfg: &TaskDefinitionConfig<'_>,
) -> ContainerDefinitionSpec {
    let mut environment = BTreeMap::new();
    if !identity.audiences.is_empty() {
        environment.insert(
            IDENTITY_TOKEN_URL_ENV.to_string(),
            identity.token_url.clone(),
        );
        environment.insert(
            IDENTITY_AUDIENCES_ENV.to_string(),
            serde_json::to_string(&identity.audiences).unwrap_or_default(),
        );
        environment.insert(
            IDENTITY_TOKEN_TTL_ENV.to_string(),
            identity.token_ttl_seconds.to_string(),
        );
    }
    let log_config = cfg.log_group.map(|group| LogConfig {
        log_group: group.to_string(),
        region: cfg.region.to_string(),
        stream_prefix: format!(
            "{}-identity/{}/{}",
            sanitize_ecs_name(cfg.resource_prefix),
            desired.project_uuid,
            desired.deployment_uuid
        ),
    });
    ContainerDefinitionSpec {
        name: IDENTITY_AGENT_CONTAINER.to_string(),
        image: identity.image.clone(),
        port: None,
        environment,
        secrets: vec![SecretRef {
            name: IDENTITY_CREDENTIAL_ENV.to_string(),
            value_from: identity.credential_parameter.clone(),
        }],
        docker_labels: BTreeMap::new(),
        log_config,
        // The Rise image is not in the app's private registry.
        repository_credentials_secret_arn: None,
        essential: false,
        command: vec!["identity".to_string(), "agent".to_string()],
        mount_points: vec![MountPointSpec {
            source_volume: IDENTITY_VOLUME.to_string(),
            container_path: IDENTITY_MOUNT_PATH.to_string(),
            read_only: false,
        }],
        depends_on_healthy: Vec::new(),
        health_check: Some(HealthCheckSpec {
            command: vec![
                "CMD".to_string(),
                RISE_BINARY.to_string(),
                "identity".to_string(),
                "agent".to_string(),
                "--check".to_string(),
            ],
            // A short interval so the app starts as soon as the files exist, and
            // ECS's longest start period, during which failed checks do not
            // count: a sidecar briefly throttled or waiting out a control-plane
            // restart keeps retrying instead of being marked unhealthy, which
            // would stop the task before the app ever ran.
            interval: 5,
            timeout: 2,
            retries: 3,
            start_period: IDENTITY_AGENT_START_PERIOD_SECS,
        }),
        restart_on_exit: true,
        memory_reservation_mib: Some(IDENTITY_AGENT_MEMORY_RESERVATION_MIB),
        image_in_hash: false,
    }
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
            compatibility: Compatibility::Fargate,
            execution_role_arn: Some("arn:aws:iam::1:role/exec"),
            task_role_arn: None,
            repository_credentials_secret_arn: None,
            log_group: Some("/rise/myapp"),
            region: "eu-central-1",
            label_namespace: "rise.dev",
            controller_class: "default",
            traefik: TraefikRenderConfig {
                label_namespace: "rise.dev",
                controller_class: "default",
                traefik_entrypoint: "web",
                catalog_entrypoint: "rise-catalog",
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
            project_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
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
        let spec = build(&desired(), &secrets, None, &cfg(&classes)).expect("builds");
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

    /// Traefik's ECS provider matches `constraints` against container labels
    /// and nothing else, and it rejects `traefik.*` as a constraint key. Without
    /// a Rise-namespaced label here, two installs sharing a cluster cannot be
    /// told apart by their Traefiks and each would route the other's containers.
    #[test]
    fn the_controller_class_is_emitted_as_a_constrainable_docker_label() {
        let classes = access_classes();
        let mut cfg = cfg(&classes);
        cfg.controller_class = "pr-457";
        cfg.traefik.controller_class = "pr-457";
        let spec = build(&desired(), &[], None, &cfg).expect("build");
        let labels = &spec.containers[0].docker_labels;

        assert_eq!(
            labels.get("rise.dev/controller-class").map(String::as_str),
            Some("pr-457"),
            "no constrainable scope label: {labels:?}"
        );
        assert!(
            !"rise.dev/controller-class".starts_with("traefik."),
            "the key must stay outside the namespace Traefik reserves"
        );
    }

    #[test]
    fn traefik_config_lands_in_docker_labels() {
        // Traefik's ECS provider reads container dockerLabels and nothing else.
        // Emit them anywhere else and the router is never created — the app is
        // unreachable and no component logs an error.
        let classes = access_classes();
        let spec = build(&desired(), &[], None, &cfg(&classes)).expect("builds");
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
        let spec = build(&desired(), &[], None, &cfg(&classes)).expect("builds");
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
        let spec = build(&worker, &[], None, &cfg(&classes)).expect("builds");
        // The scope label is about ownership, not routing, so it is still here;
        // what must be absent is anything Traefik would act on. Without
        // `traefik.enable=true` the container stays undiscovered either way.
        assert!(
            !spec.containers[0]
                .docker_labels
                .keys()
                .any(|k| k.starts_with("traefik.")),
            "a port-less container must get no Traefik labels: {:?}",
            spec.containers[0].docker_labels
        );
    }

    #[test]
    fn content_hash_changes_with_image_env_and_labels_but_not_with_reordering() {
        // The reconciler registers a new revision only when this hash moves.
        // Too sensitive and it burns the 1/s RegisterTaskDefinition budget every
        // tick; too insensitive and a changed image never rolls out.
        let classes = access_classes();
        let c = cfg(&classes);
        let base = build(&desired(), &[], None, &c).expect("builds");

        let mut reordered = desired();
        reordered.env.reverse();
        assert_eq!(
            base.content_hash(),
            build(&reordered, &[], None, &c)
                .expect("builds")
                .content_hash(),
            "env ordering must not move the hash — it would re-register every tick"
        );

        let mut new_image = desired();
        new_image.image = "registry/myapp:20260101-130000".to_string();
        assert_ne!(
            base.content_hash(),
            build(&new_image, &[], None, &c)
                .expect("builds")
                .content_hash(),
            "a new image must move the hash or the deploy never rolls out"
        );

        let mut new_env = desired();
        new_env.env.push(("EXTRA".to_string(), "1".to_string()));
        assert_ne!(
            base.content_hash(),
            build(&new_env, &[], None, &c)
                .expect("builds")
                .content_hash()
        );
    }

    #[test]
    fn log_stream_prefix_is_immutable_and_part_of_the_hash() {
        let classes = access_classes();
        let c = cfg(&classes);
        let base = build(&desired(), &[], None, &c).expect("builds");
        assert_eq!(
            base.containers[0]
                .log_config
                .as_ref()
                .map(|log| log.stream_prefix.as_str()),
            Some("rise/22222222-2222-2222-2222-222222222222/11111111-1111-1111-1111-111111111111")
        );

        let mut other = desired();
        other.deployment_uuid = "33333333-3333-3333-3333-333333333333".to_string();
        let changed = build(&other, &[], None, &c).expect("builds");
        assert_ne!(base.content_hash(), changed.content_hash());

        let mut renamed = desired();
        renamed.project = "renamed-project".to_string();
        let renamed = build(&renamed, &[], None, &c).expect("builds");
        assert_eq!(
            base.containers[0].log_config, renamed.containers[0].log_config,
            "mutable project names must not determine historical log identity"
        );
    }

    #[test]
    fn cpu_architecture_normalises_or_fails_but_never_guesses() {
        // Operators type whichever spelling their toolchain uses; all of these
        // mean one of exactly two things to Fargate.
        for raw in ["x86_64", "X86_64", "amd64", "x86-64", "x64", " AMD64 "] {
            assert_eq!(canonical_cpu_architecture(raw).expect("accepts"), "X86_64");
        }
        for raw in ["arm64", "ARM64", "aarch64"] {
            assert_eq!(canonical_cpu_architecture(raw).expect("accepts"), "ARM64");
        }

        // Anything else must fail here. Defaulting to X86_64 would hand an ARM64
        // cluster images its tasks cannot execute, and passing the string
        // through becomes an `Unknown` SDK variant AWS rejects per deploy.
        for raw in ["", "x86", "riscv64", "arm/v7", "X86_65"] {
            let err = canonical_cpu_architecture(raw).expect_err("must reject");
            assert!(
                err.to_string().contains("X86_64 or ARM64"),
                "should name the valid values: {err}"
            );
        }
    }

    #[test]
    fn the_builder_rejects_an_architecture_it_cannot_canonicalise() {
        // The builder is the last pure checkpoint before the value becomes an
        // SDK enum, where an unrecognised string stops being an error and starts
        // being an `Unknown` variant shipped to AWS.
        let classes = access_classes();
        let mut c = cfg(&classes);
        c.cpu_architecture = "amd64";
        assert_eq!(
            build(&desired(), &[], None, &c)
                .expect("normalises")
                .cpu_architecture,
            "X86_64"
        );

        c.cpu_architecture = "riscv64";
        let err = build(&desired(), &[], None, &c).expect_err("must reject");
        assert!(err.to_string().contains("riscv64"), "unhelpful: {err}");
    }

    #[test]
    fn repository_credentials_reach_the_container_and_the_hash() {
        // The ARN is how a private non-ECR registry works at all on ECS: without
        // it on the container definition the pull is anonymous and fails. And
        // pointing at a *different* secret has to roll the service, since ECS
        // only re-reads the secret when a task starts.
        let classes = access_classes();
        let mut c = cfg(&classes);
        let anonymous = build(&desired(), &[], None, &c).expect("builds");
        assert!(anonymous.containers[0]
            .repository_credentials_secret_arn
            .is_none());

        c.repository_credentials_secret_arn =
            Some("arn:aws:secretsmanager:eu-central-1:1:secret:reg-a");
        let with_creds = build(&desired(), &[], None, &c).expect("builds");
        assert_eq!(
            with_creds.containers[0]
                .repository_credentials_secret_arn
                .as_deref(),
            Some("arn:aws:secretsmanager:eu-central-1:1:secret:reg-a")
        );
        assert_ne!(anonymous.content_hash(), with_creds.content_hash());

        c.repository_credentials_secret_arn =
            Some("arn:aws:secretsmanager:eu-central-1:1:secret:reg-b");
        let rotated = build(&desired(), &[], None, &c).expect("builds");
        assert_ne!(with_creds.content_hash(), rotated.content_hash());
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
            None,
            &c,
        )
        .expect("builds");
        let b = build(
            &desired(),
            &[SecretRef {
                name: "API_KEY".to_string(),
                value_from: "/rise/myapp/default/dep-b/API_KEY".to_string(),
            }],
            None,
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
        let err = build(&huge, &[], None, &cfg(&classes)).expect_err("must be rejected");
        assert!(
            err.to_string().contains("over the ECS limit"),
            "unhelpful message: {err}"
        );
    }

    /// `audience`, when set, declares one `[identity]` audience (file `e2e`).
    fn identity(audience: Option<&str>) -> IdentityAgentSpec {
        IdentityAgentSpec {
            image: "ghcr.io/rise-deploy/rise:1.0.0".to_string(),
            credential_parameter: "/rise/myapp/default/20260101-120000/rise-identity/credential"
                .to_string(),
            token_url: "https://rise.dev/api/v1/identity/token".to_string(),
            audiences: audience
                .map(|a| BTreeMap::from([("e2e".to_string(), a.to_string())]))
                .unwrap_or_default(),
            token_ttl_seconds: 3600,
        }
    }

    /// The contract the app reads is a directory the sidecar fills. The app
    /// must see it read-only and must not start before it is filled; the
    /// sidecar must be the only holder of the credential.
    #[test]
    fn the_identity_sidecar_shares_a_volume_the_app_mounts_read_only() {
        let classes = access_classes();
        let spec = build(
            &desired(),
            &[],
            Some(&identity(Some("rise-e2e-audience"))),
            &cfg(&classes),
        )
        .expect("builds");

        assert_eq!(spec.volumes, vec![IDENTITY_VOLUME.to_string()]);
        let app = spec.app();
        assert_eq!(app.name, "app", "the app must stay first");
        assert!(app.essential);
        assert_eq!(
            app.mount_points,
            vec![MountPointSpec {
                source_volume: IDENTITY_VOLUME.to_string(),
                container_path: "/var/run/secrets/rise/identity".to_string(),
                read_only: true,
            }]
        );
        assert_eq!(app.depends_on_healthy, vec![IDENTITY_AGENT_CONTAINER]);
        assert!(
            !app.secrets
                .iter()
                .any(|s| s.name == IDENTITY_CREDENTIAL_ENV),
            "the credential must never reach the app's environment"
        );

        let agent = &spec.containers[1];
        assert_eq!(agent.name, IDENTITY_AGENT_CONTAINER);
        assert!(!agent.essential, "a crashed agent must not stop the app");
        assert!(agent.restart_on_exit);
        assert_eq!(agent.command, vec!["identity", "agent"]);
        assert!(agent.mount_points.iter().all(|m| !m.read_only));
        assert_eq!(
            agent.secrets,
            vec![SecretRef {
                name: IDENTITY_CREDENTIAL_ENV.to_string(),
                value_from: "/rise/myapp/default/20260101-120000/rise-identity/credential"
                    .to_string(),
            }]
        );
        let env = |k: &str| agent.environment.get(k).map(String::as_str);
        assert_eq!(
            env(IDENTITY_TOKEN_URL_ENV),
            Some("https://rise.dev/api/v1/identity/token")
        );
        assert_eq!(
            env(IDENTITY_AUDIENCES_ENV),
            Some(r#"{"e2e":"rise-e2e-audience"}"#)
        );
        assert_eq!(env(IDENTITY_TOKEN_TTL_ENV), Some("3600"));
        let check = agent
            .health_check
            .as_ref()
            .expect("HEALTHY dependency needs a check");
        assert_eq!(
            check.start_period, 300,
            "a throttled or restarting control plane must not fail the task at startup"
        );
        assert!(
            agent.docker_labels.is_empty(),
            "Traefik must never route to the sidecar: {:?}",
            agent.docker_labels
        );
        assert!(agent.repository_credentials_secret_arn.is_none());
    }

    #[test]
    fn a_deployment_without_audiences_gets_the_credential_but_no_token_endpoint() {
        let classes = access_classes();
        let spec = build(&desired(), &[], Some(&identity(None)), &cfg(&classes)).expect("builds");
        let agent = &spec.containers[1];
        assert!(agent.environment.is_empty(), "{:?}", agent.environment);
        assert_eq!(agent.secrets.len(), 1, "the credential is universal");
    }

    /// Only the app's logs belong to the deployment's log view. The reader
    /// lists `{prefix}/{project}/{deployment}/`; the sidecar must sit outside it.
    #[test]
    fn the_sidecar_logs_outside_the_deployment_log_prefix() {
        let classes = access_classes();
        let spec = build(&desired(), &[], Some(&identity(None)), &cfg(&classes)).expect("builds");
        let app_prefix = format!("{}/", spec.app().log_config.as_ref().unwrap().stream_prefix);
        let agent_prefix = &spec.containers[1]
            .log_config
            .as_ref()
            .unwrap()
            .stream_prefix;
        assert!(
            !format!("{agent_prefix}/").starts_with(&app_prefix)
                && !agent_prefix.starts_with("rise/"),
            "sidecar stream {agent_prefix:?} would appear in the app's logs"
        );
    }

    /// The agent image follows the Rise version. Hashing it would roll every
    /// service on every upgrade; everything else about the sidecar still rolls.
    #[test]
    fn the_agent_image_alone_does_not_move_the_hash() {
        let classes = access_classes();
        let c = cfg(&classes);
        let base = build(&desired(), &[], Some(&identity(None)), &c).expect("builds");

        let mut upgraded = identity(None);
        upgraded.image = "ghcr.io/rise-deploy/rise:2.0.0".to_string();
        let upgraded = build(&desired(), &[], Some(&upgraded), &c).expect("builds");
        assert_eq!(
            upgraded.containers[1].image,
            "ghcr.io/rise-deploy/rise:2.0.0"
        );
        assert_eq!(base.content_hash(), upgraded.content_hash());

        let with_tokens = build(&desired(), &[], Some(&identity(Some("aud"))), &c).expect("builds");
        assert_ne!(base.content_hash(), with_tokens.content_hash());

        let without_sidecar = build(&desired(), &[], None, &c).expect("builds");
        assert_ne!(base.content_hash(), without_sidecar.content_hash());
    }

    #[test]
    fn an_app_container_cannot_take_the_sidecar_s_name() {
        let classes = access_classes();
        let mut clash = desired();
        clash.container = IDENTITY_AGENT_CONTAINER.to_string();
        let err = build(&clash, &[], Some(&identity(None)), &cfg(&classes))
            .expect_err("must be rejected");
        assert!(err.to_string().contains("reserved"), "unhelpful: {err}");
    }
}
