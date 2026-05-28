//! Resource builder for generating Kubernetes resource specs.
//!
//! This module extracts all pure resource-spec-generation logic from
//! `KubernetesController` into a standalone `ResourceBuilder` struct.
//! It holds configuration (ingress templates, registry settings, etc.)
//! but NOT a kube client or resource version cache.
//!
//! Used by the Metacontroller sync webhook to compute desired children.

use k8s_openapi::api::apps::v1::{Deployment as K8sDeployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EnvFromSource, EnvVar, HTTPGetAction, HostAlias,
    KeyToPath, LocalObjectReference, Namespace, PodSecurityContext, PodSpec, PodTemplateSpec,
    Probe, ProjectedVolumeSource, ResourceRequirements, SeccompProfile, Secret, SecretEnvSource,
    SecretVolumeSource, SecurityContext, Service, ServiceAccount, ServiceAccountTokenProjection,
    ServicePort, ServiceSpec, Volume, VolumeMount, VolumeProjection,
};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, NetworkPolicy, NetworkPolicySpec, ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::ByteString;
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::warn;

use crate::db::models::{CustomDomain, Deployment, Project};
use crate::server::registry::{
    models::{RegistryAuthMethod, RegistryCredentials},
    RegistryProvider,
};
use crate::server::settings::AccessRequirement;

// Re-export constants used by webhook and other consumers
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const LABEL_PROJECT: &str = "rise.dev/project";
pub const LABEL_DEPLOYMENT_GROUP: &str = "rise.dev/deployment-group";
pub const LABEL_DEPLOYMENT_ID: &str = "rise.dev/deployment-id";
pub const LABEL_DEPLOYMENT_UUID: &str = "rise.dev/deployment-uuid";
pub const LABEL_ENVIRONMENT: &str = "rise.dev/environment";
pub const ANNOTATION_LAST_REFRESH: &str = "rise.dev/last-refresh";
pub const ANNOTATION_ENV_SECRET_HASH: &str = "rise.dev/env-secret-hash";
pub const IMAGE_PULL_SECRET_NAME: &str = "rise-registry-creds";

const EXTRA_SERVICE_TOKENS_VOLUME_NAME: &str = "rise-extra-service-tokens";
const EXTRA_SERVICE_TOKENS_MOUNT_PATH: &str = "/var/run/secrets/rise/tokens";

// Workload identity: a per-deployment Secret carrying the bootstrap credential
// and any auto-minted workload tokens, mounted as files into the app's pod.
const IDENTITY_VOLUME_NAME: &str = "rise-identity";
const IDENTITY_MOUNT_PATH: &str = "/var/run/secrets/rise/identity";
const IDENTITY_TOKENS_SUBDIR: &str = "tokens";
/// Secret data key for the bootstrap credential.
pub const IDENTITY_CREDENTIAL_KEY: &str = "credential";
/// Prefix for Secret data keys holding auto-minted workload tokens.
pub const IDENTITY_TOKEN_KEY_PREFIX: &str = "token-";

/// Describes the workload-identity Secret mounted into a deployment's pod.
#[derive(Debug, Clone)]
pub struct IdentityMount {
    /// Name of the `rise-identity` Secret.
    pub secret_name: String,
    /// Token filenames (the `[identity]` audience map keys) carried by the Secret.
    pub token_filenames: Vec<String>,
}

/// Container waiting state reasons that indicate irrecoverable errors
pub const IRRECOVERABLE_CONTAINER_REASONS: &[&str] = &[
    "InvalidImageName",
    "ErrImagePull",
    "ImagePullBackOff",
    "ImageInspectError",
    "CrashLoopBackOff",
    "CreateContainerConfigError",
    "CreateContainerError",
    "RunContainerError",
];

/// Helper enum for probe type
#[derive(Debug, Clone, Copy)]
enum ProbeType {
    Liveness,
    Readiness,
}

/// Label set on a Pod/Service to identify which container within a
/// multi-container deployment it belongs to. Only emitted on the
/// multi-container path; legacy single-container Deployments don't carry it,
/// preserving K8s selector immutability for existing rollouts.
pub const LABEL_CONTAINER: &str = "rise.dev/container";

/// Default container name used when a deployment row has no `containers` JSON
/// (legacy single-container path). Matches the hardcoded value used to live
/// inside the K8s pod spec, so no on-cluster renaming happens for old rows.
pub const LEGACY_CONTAINER_NAME: &str = "app";

/// Per-container runtime parameters consumed by `create_k8s_deployment_for`
/// and `create_service_for`. Built by the reconciler from either:
///   - the deployment's `containers` JSON (one runtime per ContainerSpec), or
///   - the deployment row's legacy columns (a single runtime named "app").
#[derive(Debug, Clone)]
pub struct ContainerRuntime<'a> {
    pub name: &'a str,
    pub image: &'a str,
    /// `None` for workers (no port → no Service emitted, no HTTP probes).
    pub http_port: Option<u16>,
    pub replicas: i32,
    pub cpu: &'a str,
    pub memory: &'a str,
    /// Container-scoped + project-global env vars merged. The reconciler does
    /// the merge before constructing this.
    pub env_vars: Vec<EnvVar>,
    /// Optional Secret name carrying the global encrypted env vars. The same
    /// Secret is referenced by every container's pod (project-global scope).
    pub secret_env_name: Option<String>,
    /// Hash of the secret data, written as a pod-template annotation so
    /// secret rotations trigger pod restarts.
    pub secret_env_hash: Option<String>,
    /// Optional per-container probe configuration overriding the server's
    /// `HealthProbeConfig` defaults. `Some(spec)` with `disabled=true` turns
    /// probes off entirely. `None` falls back to the server defaults.
    pub health_check: Option<crate::server::deployment::models::HealthCheckSpec>,
    /// True when this runtime represents the legacy implicit `app` container
    /// (the deployment row has no `containers` JSON). Used to keep K8s
    /// resource names unsuffixed for back-compat.
    pub is_legacy: bool,
}

/// Parsed ingress URL components
#[derive(Debug, Clone)]
pub struct IngressUrl {
    pub host: String,
    pub path_prefix: Option<String>,
}

/// Holds configuration for building Kubernetes resource specs.
/// Does NOT hold a kube client — pure spec generation only.
pub struct ResourceBuilder {
    pub production_ingress_url_template: String,
    pub staging_ingress_url_template: Option<String>,
    pub environment_ingress_url_template: Option<String>,
    pub ingress_port: Option<u16>,
    pub ingress_schema: String,
    pub registry_provider: Arc<dyn RegistryProvider>,
    pub auth_backend_url: String,
    pub auth_signin_url: String,
    pub backend_address: Option<crate::server::settings::BackendAddress>,
    pub namespace_labels: std::collections::HashMap<String, String>,
    pub namespace_annotations: std::collections::HashMap<String, String>,
    pub ingress_annotations: std::collections::HashMap<String, String>,
    pub ingress_tls_secret_name: Option<String>,
    pub custom_domain_tls_mode: crate::server::settings::CustomDomainTlsMode,
    pub custom_domain_ingress_annotations: std::collections::HashMap<String, String>,
    pub node_selector: std::collections::HashMap<String, String>,
    pub image_pull_secret_name: Option<String>,
    pub access_classes: std::collections::HashMap<String, crate::server::settings::AccessClass>,
    pub host_aliases: std::collections::HashMap<String, String>,
    pub extra_service_token_audiences: std::collections::HashMap<String, String>,
    pub use_default_service_account_for_production: bool,
    pub network_policy: crate::server::settings::NetworkPolicyConfig,
    pub pod_security_enabled: bool,
    pub health_probes: Option<crate::server::settings::HealthProbeConfig>,
}

impl ResourceBuilder {
    // ── Naming helpers ─────────────────────────────────────────────────

    /// Compute the Kubernetes namespace for a project, given the per-Org
    /// `namespace_prefix` resolved by the caller. The prefix is resolved
    /// per-request (see `webhook::load_org_view`) rather than baked into
    /// the builder so a project in any Organization can be namespaced
    /// without rebuilding the controller's `ResourceBuilder`.
    pub fn namespace_name(project: &Project, namespace_prefix: &str) -> String {
        format!("{namespace_prefix}{}", project.name)
    }

    pub fn sanitize_label_value(value: &str) -> String {
        crate::server::deployment::models::normalize_deployment_group(value)
    }

    pub fn escaped_group_name(deployment_group: &str) -> String {
        if deployment_group == crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP {
            "default".to_string()
        } else {
            Self::sanitize_label_value(deployment_group)
        }
    }

    pub fn service_name(_project: &Project, deployment: &Deployment) -> String {
        Self::escaped_group_name(&deployment.deployment_group)
    }

    pub fn deployment_name(project: &Project, deployment: &Deployment) -> String {
        format!("{}-{}", project.name, deployment.deployment_id)
    }

    pub fn deployment_env_secret_name(project: &Project, deployment: &Deployment) -> String {
        format!("{}-env", Self::deployment_name(project, deployment))
    }

    pub fn deployment_identity_secret_name(project: &Project, deployment: &Deployment) -> String {
        format!("{}-identity", Self::deployment_name(project, deployment))
    }

    pub fn ingress_name(_project: &Project, deployment: &Deployment) -> String {
        Self::escaped_group_name(&deployment.deployment_group)
    }

    pub fn custom_domain_ingress_name(_project: &Project, deployment: &Deployment) -> String {
        format!(
            "{}-custom-domains",
            Self::escaped_group_name(&deployment.deployment_group)
        )
    }

    pub fn network_policy_name(_project: &Project, deployment: &Deployment) -> String {
        Self::escaped_group_name(&deployment.deployment_group)
    }

    pub fn environment_service_account_name(environment_name: &str) -> String {
        format!("env-{}", environment_name)
    }

    // ── URL resolution ─────────────────────────────────────────────────

    fn parse_ingress_url(url: &str) -> IngressUrl {
        match url.find('/') {
            Some(slash_pos) => IngressUrl {
                host: url[..slash_pos].to_string(),
                path_prefix: Some(url[slash_pos..].to_string()),
            },
            None => IngressUrl {
                host: url.to_string(),
                path_prefix: None,
            },
        }
    }

    pub fn resolved_ingress_url(&self, project: &Project, deployment: &Deployment) -> String {
        self.resolved_ingress_url_for_group(project, &deployment.deployment_group)
    }

    pub fn resolved_ingress_url_for_group(
        &self,
        project: &Project,
        deployment_group: &str,
    ) -> String {
        if deployment_group == crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP {
            self.production_ingress_url_template
                .replace("{project_name}", &project.name)
        } else if let Some(ref staging_template) = self.staging_ingress_url_template {
            staging_template
                .replace("{project_name}", &project.name)
                .replace(
                    "{deployment_group}",
                    &Self::escaped_group_name(deployment_group),
                )
        } else {
            let base_url = self
                .production_ingress_url_template
                .replace("{project_name}", &project.name);
            if let Some(dot_pos) = base_url.find('.') {
                format!(
                    "{}-{}{}",
                    &base_url[..dot_pos],
                    Self::escaped_group_name(deployment_group),
                    &base_url[dot_pos..]
                )
            } else {
                format!(
                    "{}-{}",
                    base_url,
                    Self::escaped_group_name(deployment_group)
                )
            }
        }
    }

    /// Resolve the deployment-group-specific host (the staging-template form).
    ///
    /// Always returns the templated DG host when `staging_ingress_url_template` is set,
    /// even for the "default" group (yielding e.g. `myapp-default.preview.example.com`).
    /// When no staging template is configured, returns the synthesized form for
    /// non-default groups only — for the default group it returns `None` to avoid
    /// colliding with the project's production hostname.
    pub fn resolved_deployment_group_url(
        &self,
        project: &Project,
        deployment_group: &str,
    ) -> Option<String> {
        if let Some(ref staging_template) = self.staging_ingress_url_template {
            Some(
                staging_template
                    .replace("{project_name}", &project.name)
                    .replace(
                        "{deployment_group}",
                        &Self::escaped_group_name(deployment_group),
                    ),
            )
        } else if deployment_group != crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP {
            let base_url = self
                .production_ingress_url_template
                .replace("{project_name}", &project.name);
            Some(if let Some(dot_pos) = base_url.find('.') {
                format!(
                    "{}-{}{}",
                    &base_url[..dot_pos],
                    Self::escaped_group_name(deployment_group),
                    &base_url[dot_pos..]
                )
            } else {
                format!(
                    "{}-{}",
                    base_url,
                    Self::escaped_group_name(deployment_group)
                )
            })
        } else {
            None
        }
    }

    pub fn resolved_environment_url(
        &self,
        project: &Project,
        environment: &crate::db::models::Environment,
    ) -> Option<String> {
        if environment.is_production {
            Some(
                self.production_ingress_url_template
                    .replace("{project_name}", &project.name),
            )
        } else {
            self.environment_ingress_url_template
                .as_ref()
                .map(|env_template| {
                    env_template
                        .replace("{project_name}", &project.name)
                        .replace("{environment}", &environment.name)
                })
        }
    }

    fn full_ingress_url_from_host(&self, url: &str) -> String {
        if let Some(port) = self.ingress_port {
            let parsed = Self::parse_ingress_url(url);
            let host_with_port = format!("{}:{}", parsed.host, port);
            match parsed.path_prefix {
                Some(path) => format!("{}{}", host_with_port, path),
                None => host_with_port,
            }
        } else {
            url.to_string()
        }
    }

    fn full_ingress_url(&self, project: &Project, deployment: &Deployment) -> String {
        let url = self.resolved_ingress_url(project, deployment);
        self.full_ingress_url_from_host(&url)
    }

    fn ingress_url_components(&self, project: &Project, deployment: &Deployment) -> IngressUrl {
        let url = self.resolved_ingress_url(project, deployment);
        Self::parse_ingress_url(&url)
    }

    /// Returns `true` if `host` would also be emitted as the env URL of some
    /// environment whose `primary_deployment_group` is *not* `deployment_group`.
    /// That's the cross-ingress collision case: a deployment-group's name
    /// happens to match an environment name, so its DG URL would steal a host
    /// that "belongs" to the env's primary group.
    fn host_conflicts_with_other_env(
        &self,
        project: &Project,
        deployment_group: &str,
        host: &str,
        all_environments: &[crate::db::models::Environment],
    ) -> bool {
        all_environments.iter().any(|env| {
            if env.primary_deployment_group.as_deref() == Some(deployment_group) {
                return false;
            }
            match self.resolved_environment_url(project, env) {
                Some(env_url) => Self::parse_ingress_url(&env_url).host == host,
                None => false,
            }
        })
    }

    /// Compute the ordered, deduplicated list of hosts that should appear on the
    /// primary ingress for `(project, deployment_group)`.
    ///
    /// The list is built from:
    /// 1. The deployment-group host (`resolved_deployment_group_url`), when one
    ///    is configured for this group AND its host doesn't collide with the
    ///    env URL of an environment whose primary group is a different group.
    /// 2. The host of the environment whose `primary_deployment_group` matches
    ///    this group, if any (production env contributes the production host).
    /// 3. Each custom domain attached to that environment, when
    ///    `include_custom_domains_inline` is true.
    ///
    /// `all_environments` is the full project env list used for the collision
    /// check in (1). Callers without env context can pass an empty slice.
    pub fn primary_ingress_hosts(
        &self,
        project: &Project,
        deployment_group: &str,
        environment_for_group: Option<&crate::db::models::Environment>,
        custom_domains_for_env: &[CustomDomain],
        include_custom_domains_inline: bool,
        all_environments: &[crate::db::models::Environment],
    ) -> Vec<IngressUrl> {
        let mut hosts: Vec<IngressUrl> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        if let Some(dg_url) = self.resolved_deployment_group_url(project, deployment_group) {
            let parsed = Self::parse_ingress_url(&dg_url);
            if self.host_conflicts_with_other_env(
                project,
                deployment_group,
                &parsed.host,
                all_environments,
            ) {
                warn!(
                    project = %project.name,
                    deployment_group = %deployment_group,
                    host = %parsed.host,
                    "Suppressing deployment-group URL: collides with env URL of an environment whose primary deployment group is different"
                );
            } else if seen.insert(parsed.host.clone()) {
                hosts.push(parsed);
            }
        }

        if let Some(env) = environment_for_group {
            if let Some(env_url) = self.resolved_environment_url(project, env) {
                let parsed = Self::parse_ingress_url(&env_url);
                if seen.insert(parsed.host.clone()) {
                    hosts.push(parsed);
                }
            }
        }

        if include_custom_domains_inline {
            for cd in custom_domains_for_env {
                if seen.insert(cd.domain.clone()) {
                    hosts.push(IngressUrl {
                        host: cd.domain.clone(),
                        path_prefix: None,
                    });
                }
            }
        }

        hosts
    }

    pub fn filter_valid_custom_domains(
        &self,
        custom_domains: &[CustomDomain],
    ) -> Vec<CustomDomain> {
        use crate::server::custom_domains::validation;

        custom_domains
            .iter()
            .filter(|domain| {
                match validation::validate_custom_domain(
                    &domain.domain,
                    &self.production_ingress_url_template,
                    self.staging_ingress_url_template.as_deref(),
                    self.environment_ingress_url_template.as_deref(),
                    None,
                ) {
                    Ok(()) => true,
                    Err(reason) => {
                        warn!(
                            domain_id = %domain.id,
                            project_id = %domain.project_id,
                            domain = %domain.domain,
                            "Ignoring custom domain that conflicts with project default domain pattern: {}",
                            reason
                        );
                        false
                    }
                }
            })
            .cloned()
            .collect()
    }

    // ── Deployment URLs ────────────────────────────────────────────────

    fn host_to_url(&self, host: &str) -> String {
        let host_with_port = if let Some(port) = self.ingress_port {
            format!("{}:{}", host, port)
        } else {
            host.to_string()
        };
        format!("{}://{}", self.ingress_schema, host_with_port)
    }

    pub fn compute_deployment_urls(
        &self,
        project: &Project,
        deployment: &Deployment,
        environment: Option<&crate::db::models::Environment>,
        all_environments: &[crate::db::models::Environment],
        custom_domains: &[CustomDomain],
    ) -> super::controller::DeploymentUrls {
        let default_url_host = self.full_ingress_url(project, deployment);
        let default_url = format!("{}://{}", self.ingress_schema, default_url_host);

        let environment_for_group = environment.filter(|env| {
            env.primary_deployment_group.as_deref() == Some(&deployment.deployment_group)
        });

        let environment_url = environment_for_group.and_then(|env| {
            self.resolved_environment_url(project, env).map(|url_host| {
                let full_host = self.full_ingress_url_from_host(&url_host);
                format!("{}://{}", self.ingress_schema, full_host)
            })
        });

        let valid_custom_domains = self.filter_valid_custom_domains(custom_domains);
        let env_id = environment_for_group.map(|e| e.id);
        let domains_for_env: Vec<&CustomDomain> = valid_custom_domains
            .iter()
            .filter(|cd| env_id == Some(cd.environment_id))
            .collect();

        let starred = domains_for_env.iter().find(|d| d.is_primary);
        let primary_url = if let Some(starred) = starred {
            self.host_to_url(&starred.domain)
        } else {
            environment_url
                .clone()
                .unwrap_or_else(|| default_url.clone())
        };

        let custom_domain_urls: Vec<String> = domains_for_env
            .iter()
            .map(|d| self.host_to_url(&d.domain))
            .collect();

        let templated_hosts = self.primary_ingress_hosts(
            project,
            &deployment.deployment_group,
            environment_for_group,
            &[],
            false,
            all_environments,
        );
        let mut all_urls: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for host in &templated_hosts {
            let url = self.host_to_url(&host.host);
            if seen.insert(url.clone()) {
                all_urls.push(url);
            }
        }
        for url in &custom_domain_urls {
            if seen.insert(url.clone()) {
                all_urls.push(url.clone());
            }
        }

        super::controller::DeploymentUrls {
            default_url,
            primary_url,
            custom_domain_urls,
            all_urls,
        }
    }

    pub fn compute_project_urls(
        &self,
        project: &Project,
        deployment_group: &str,
        custom_domains: &[CustomDomain],
    ) -> super::controller::DeploymentUrls {
        let url_host = self.resolved_ingress_url_for_group(project, deployment_group);
        let full_host = self.full_ingress_url_from_host(&url_host);
        let default_url = format!("{}://{}", self.ingress_schema, full_host);

        let valid_custom_domains = self.filter_valid_custom_domains(custom_domains);

        // No environment context here — keep the historical "default group hosts custom
        // domains" rule to preserve preview-URL semantics for callers that haven't yet
        // adopted the env-aware path.
        let (custom_domain_urls, primary_url) =
            if deployment_group == crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP {
                let starred = valid_custom_domains.iter().find(|d| d.is_primary);
                let primary = if let Some(starred) = starred {
                    self.host_to_url(&starred.domain)
                } else {
                    default_url.clone()
                };

                let urls: Vec<String> = valid_custom_domains
                    .iter()
                    .map(|d| self.host_to_url(&d.domain))
                    .collect();
                (urls, primary)
            } else {
                (Vec::new(), default_url.clone())
            };

        let mut all_urls = vec![default_url.clone()];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        seen.insert(default_url.clone());
        for url in &custom_domain_urls {
            if seen.insert(url.clone()) {
                all_urls.push(url.clone());
            }
        }

        super::controller::DeploymentUrls {
            default_url,
            primary_url,
            custom_domain_urls,
            all_urls,
        }
    }

    // ── Labels ─────────────────────────────────────────────────────────

    pub fn common_labels(
        project: &Project,
        environment_name: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_MANAGED_BY.to_string(), "rise".to_string());
        labels.insert(LABEL_PROJECT.to_string(), project.name.clone());
        if let Some(env) = environment_name {
            labels.insert(
                LABEL_ENVIRONMENT.to_string(),
                Self::sanitize_label_value(env),
            );
        }
        labels
    }

    pub fn group_labels(
        project: &Project,
        deployment: &Deployment,
        environment_name: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut labels = Self::common_labels(project, environment_name);
        labels.insert(
            LABEL_DEPLOYMENT_GROUP.to_string(),
            Self::sanitize_label_value(&deployment.deployment_group),
        );
        labels
    }

    pub fn deployment_labels(
        project: &Project,
        deployment: &Deployment,
        environment_name: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut labels = Self::common_labels(project, environment_name);
        labels.insert(
            LABEL_DEPLOYMENT_GROUP.to_string(),
            Self::sanitize_label_value(&deployment.deployment_group),
        );
        labels.insert(
            LABEL_DEPLOYMENT_ID.to_string(),
            deployment.deployment_id.clone(),
        );
        labels.insert(LABEL_DEPLOYMENT_UUID.to_string(), deployment.id.to_string());
        labels
    }

    /// Labels for the multi-container path — adds `rise.dev/container` so each
    /// container's Pods are isolated by selector. Never used for legacy
    /// single-container rollouts (changing `spec.selector` is forbidden by K8s).
    pub fn deployment_labels_with_container(
        project: &Project,
        deployment: &Deployment,
        environment_name: Option<&str>,
        container_name: &str,
    ) -> BTreeMap<String, String> {
        let mut labels = Self::deployment_labels(project, deployment, environment_name);
        labels.insert(LABEL_CONTAINER.to_string(), container_name.to_string());
        labels
    }

    /// Per-container variant of `deployment_name`. Legacy containers keep the
    /// unsuffixed name; multi-container deployments suffix with `-<container>`.
    pub fn deployment_name_for_container(
        project: &Project,
        deployment: &Deployment,
        runtime: &ContainerRuntime<'_>,
    ) -> String {
        if runtime.is_legacy {
            Self::deployment_name(project, deployment)
        } else {
            format!(
                "{}-{}",
                Self::deployment_name(project, deployment),
                runtime.name
            )
        }
    }

    /// Per-container Service name. Legacy keeps the group-only name (Ingress
    /// references it as today); multi-container deployments suffix `-<container>`.
    pub fn service_name_for_container(
        project: &Project,
        deployment: &Deployment,
        runtime: &ContainerRuntime<'_>,
    ) -> String {
        if runtime.is_legacy {
            Self::service_name(project, deployment)
        } else {
            format!(
                "{}-{}",
                Self::service_name(project, deployment),
                runtime.name
            )
        }
    }

    /// Auto-injected env vars exposing each routable sibling container's
    /// in-cluster Service address. One entry per container with `http_port`:
    /// `RISE_CONTAINER_HOST__<NAME> = <service>:<port>`, where `<NAME>` is the
    /// container name uppercased with dashes mapped to underscores.
    ///
    /// Each container also sees its own entry — handy for code that doesn't
    /// know which container it's in, and the Service self-loop is harmless.
    /// Order matches `container_specs` so test fixtures stay deterministic.
    /// Multi-container path only: legacy deployments have a single group-level
    /// Service with no sibling addresses to inject.
    pub fn auto_container_host_env_vars(
        deployment: &Deployment,
        container_specs: &[crate::server::deployment::models::ContainerSpec],
    ) -> Vec<EnvVar> {
        let group = Self::escaped_group_name(&deployment.deployment_group);
        container_specs
            .iter()
            .filter_map(|spec| {
                let port = spec.http_port?;
                let key = format!(
                    "RISE_CONTAINER_HOST__{}",
                    spec.name.to_uppercase().replace('-', "_")
                );
                Some(EnvVar {
                    name: key,
                    value: Some(format!("{group}-{}:{port}", spec.name)),
                    ..Default::default()
                })
            })
            .collect()
    }

    // ── Resource spec builders ─────────────────────────────────────────

    pub fn create_namespace(&self, project: &Project, namespace_prefix: &str) -> Namespace {
        let mut labels = Self::common_labels(project, None);
        for (k, v) in &self.namespace_labels {
            labels.insert(k.clone(), v.clone());
        }

        let annotations = if !self.namespace_annotations.is_empty() {
            Some(
                self.namespace_annotations
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            )
        } else {
            None
        };

        Namespace {
            metadata: ObjectMeta {
                name: Some(Self::namespace_name(project, namespace_prefix)),
                labels: Some(labels),
                annotations,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn create_service_account(
        &self,
        project: &Project,
        environment_name: &str,
        namespace: &str,
    ) -> ServiceAccount {
        ServiceAccount {
            metadata: ObjectMeta {
                name: Some(Self::environment_service_account_name(environment_name)),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::common_labels(project, Some(environment_name))),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn create_dockerconfigjson_secret(
        &self,
        name: &str,
        namespace: &str,
        registry_host: &str,
        credentials: &RegistryCredentials,
    ) -> anyhow::Result<Secret> {
        use base64::Engine;

        let auths_entry = match credentials.auth_method {
            RegistryAuthMethod::LoginCredentials => {
                let auth = base64::engine::general_purpose::STANDARD
                    .encode(format!("{}:{}", credentials.username, credentials.password));
                serde_json::json!({
                    "username": credentials.username,
                    "password": credentials.password,
                    "auth": auth,
                })
            }
            RegistryAuthMethod::RegistryToken => {
                serde_json::json!({ "registrytoken": credentials.password })
            }
        };

        let docker_config = serde_json::json!({ "auths": { registry_host: auths_entry } });
        let docker_config_bytes = docker_config.to_string().into_bytes();

        let mut data = BTreeMap::new();
        data.insert(
            ".dockerconfigjson".to_string(),
            k8s_openapi::ByteString(docker_config_bytes),
        );

        let mut annotations = BTreeMap::new();
        annotations.insert(
            ANNOTATION_LAST_REFRESH.to_string(),
            chrono::Utc::now().to_rfc3339(),
        );

        Ok(Secret {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                annotations: Some(annotations),
                ..Default::default()
            },
            type_: Some("kubernetes.io/dockerconfigjson".to_string()),
            data: Some(data),
            ..Default::default()
        })
    }

    pub fn create_deployment_env_secret(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        environment_name: Option<&str>,
        env_secret_hash: &str,
        data: BTreeMap<String, ByteString>,
    ) -> Secret {
        let mut annotations = BTreeMap::new();
        annotations.insert(
            ANNOTATION_ENV_SECRET_HASH.to_string(),
            env_secret_hash.to_string(),
        );

        Secret {
            metadata: ObjectMeta {
                name: Some(Self::deployment_env_secret_name(project, deployment)),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::deployment_labels(
                    project,
                    deployment,
                    environment_name,
                )),
                annotations: Some(annotations),
                ..Default::default()
            },
            type_: Some("Opaque".to_string()),
            data: Some(data),
            ..Default::default()
        }
    }

    /// Build the per-deployment workload-identity Secret.
    ///
    /// Carries the bootstrap `credential` plus one `token-<filename>` entry per
    /// auto-minted workload token. `refreshed_at` is recorded in the
    /// `rise.dev/last-refresh` annotation and drives token-staleness checks.
    #[allow(clippy::too_many_arguments)]
    pub fn create_identity_secret(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        environment_name: Option<&str>,
        credential: &str,
        tokens: &BTreeMap<String, String>,
        refreshed_at: chrono::DateTime<chrono::Utc>,
    ) -> Secret {
        let mut data: BTreeMap<String, ByteString> = BTreeMap::new();
        data.insert(
            IDENTITY_CREDENTIAL_KEY.to_string(),
            ByteString(credential.as_bytes().to_vec()),
        );
        for (filename, jwt) in tokens {
            data.insert(
                format!("{}{}", IDENTITY_TOKEN_KEY_PREFIX, filename),
                ByteString(jwt.as_bytes().to_vec()),
            );
        }

        let mut annotations = BTreeMap::new();
        annotations.insert(
            ANNOTATION_LAST_REFRESH.to_string(),
            refreshed_at.to_rfc3339(),
        );

        Secret {
            metadata: ObjectMeta {
                name: Some(Self::deployment_identity_secret_name(project, deployment)),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::deployment_labels(
                    project,
                    deployment,
                    environment_name,
                )),
                annotations: Some(annotations),
                ..Default::default()
            },
            type_: Some("Opaque".to_string()),
            data: Some(data),
            ..Default::default()
        }
    }

    fn create_identity_volume(identity: &IdentityMount) -> Volume {
        let mut items = vec![KeyToPath {
            key: IDENTITY_CREDENTIAL_KEY.to_string(),
            path: IDENTITY_CREDENTIAL_KEY.to_string(),
            ..Default::default()
        }];
        for filename in &identity.token_filenames {
            items.push(KeyToPath {
                key: format!("{}{}", IDENTITY_TOKEN_KEY_PREFIX, filename),
                path: format!("{}/{}", IDENTITY_TOKENS_SUBDIR, filename),
                ..Default::default()
            });
        }

        Volume {
            name: IDENTITY_VOLUME_NAME.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(identity.secret_name.clone()),
                items: Some(items),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn create_identity_volume_mount() -> VolumeMount {
        VolumeMount {
            name: IDENTITY_VOLUME_NAME.to_string(),
            mount_path: IDENTITY_MOUNT_PATH.to_string(),
            read_only: Some(true),
            ..Default::default()
        }
    }

    pub fn create_service(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        http_port: u16,
        environment_name: Option<&str>,
    ) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some(Self::service_name(project, deployment)),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::common_labels(project, environment_name)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".to_string()),
                selector: Some(Self::deployment_labels(
                    project,
                    deployment,
                    environment_name,
                )),
                ports: Some(vec![ServicePort {
                    name: Some("http".to_string()),
                    port: 80,
                    target_port: Some(
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                            http_port as i32,
                        ),
                    ),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    pub fn create_backend_service_externalname(
        &self,
        project: &Project,
        namespace: &str,
        external_name: &str,
    ) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some("rise-backend".to_string()),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::common_labels(project, None)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ExternalName".to_string()),
                external_name: Some(external_name.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    pub fn create_backend_service_clusterip(
        &self,
        project: &Project,
        namespace: &str,
        port: u16,
    ) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some("rise-backend".to_string()),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::common_labels(project, None)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".to_string()),
                ports: Some(vec![ServicePort {
                    name: Some("http".to_string()),
                    port: port as i32,
                    target_port: Some(
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(port as i32),
                    ),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    pub fn create_backend_endpoints(
        &self,
        project: &Project,
        namespace: &str,
        ip: &str,
        port: u16,
    ) -> k8s_openapi::api::core::v1::Endpoints {
        use k8s_openapi::api::core::v1::{EndpointAddress, EndpointPort, EndpointSubset};

        k8s_openapi::api::core::v1::Endpoints {
            metadata: ObjectMeta {
                name: Some("rise-backend".to_string()),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::common_labels(project, None)),
                ..Default::default()
            },
            subsets: Some(vec![EndpointSubset {
                addresses: Some(vec![EndpointAddress {
                    ip: ip.to_string(),
                    ..Default::default()
                }]),
                ports: Some(vec![EndpointPort {
                    name: Some("http".to_string()),
                    port: port as i32,
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
        }
    }

    pub fn create_network_policy(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        environment_name: Option<&str>,
    ) -> NetworkPolicy {
        let (egress_rules, policy_types) = match &self.network_policy.egress {
            None => (None, vec!["Ingress".to_string()]),
            Some(rules) => (
                Some(normalize_network_policy_label_selectors_in_egress(
                    rules.clone(),
                )),
                vec!["Ingress".to_string(), "Egress".to_string()],
            ),
        };

        let ingress_rules = normalize_network_policy_label_selectors_in_ingress(
            self.network_policy.ingress.clone(),
        );

        NetworkPolicy {
            metadata: ObjectMeta {
                name: Some(Self::network_policy_name(project, deployment)),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::common_labels(project, environment_name)),
                ..Default::default()
            },
            spec: Some(NetworkPolicySpec {
                pod_selector: Some(LabelSelector {
                    match_labels: Some(Self::group_labels(project, deployment, environment_name)),
                    ..Default::default()
                }),
                policy_types: Some(policy_types),
                egress: egress_rules,
                ingress: Some(ingress_rules),
            }),
        }
    }

    // ── Pod security & resources ───────────────────────────────────────

    fn create_pod_security_context(&self) -> Option<PodSecurityContext> {
        if !self.pod_security_enabled {
            return None;
        }
        Some(PodSecurityContext {
            run_as_non_root: Some(true),
            seccomp_profile: Some(SeccompProfile {
                type_: "RuntimeDefault".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn create_container_security_context(&self) -> Option<SecurityContext> {
        if !self.pod_security_enabled {
            return None;
        }
        Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            run_as_non_root: Some(true),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                ..Default::default()
            }),
            read_only_root_filesystem: Some(false),
            ..Default::default()
        })
    }

    fn create_resource_requirements(
        &self,
        cpu: &str,
        memory: &str,
    ) -> Option<ResourceRequirements> {
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

        // Single-knob: same value for both request and limit
        Some(ResourceRequirements {
            requests: Some({
                let mut map = BTreeMap::new();
                map.insert("cpu".to_string(), Quantity(cpu.to_string()));
                map.insert("memory".to_string(), Quantity(memory.to_string()));
                map
            }),
            limits: Some({
                let mut map = BTreeMap::new();
                map.insert("cpu".to_string(), Quantity(cpu.to_string()));
                map.insert("memory".to_string(), Quantity(memory.to_string()));
                map
            }),
            ..Default::default()
        })
    }

    fn create_http_probe(&self, port: i32, probe_type: ProbeType) -> Option<Probe> {
        self.create_http_probe_with_override(port, probe_type, None)
    }

    /// Build a probe, merging per-container overrides on top of the server's
    /// `HealthProbeConfig` defaults. `override_spec` with `disabled = true`
    /// returns `None`; individual override fields fall through to defaults
    /// when `None`.
    fn create_http_probe_with_override(
        &self,
        port: i32,
        probe_type: ProbeType,
        override_spec: Option<&crate::server::deployment::models::HealthCheckSpec>,
    ) -> Option<Probe> {
        if matches!(override_spec, Some(o) if o.disabled) {
            return None;
        }

        let defaults = self.health_probes.as_ref().cloned().unwrap_or_else(|| {
            crate::server::settings::HealthProbeConfig {
                liveness_enabled: true,
                readiness_enabled: true,
                path: "/".to_string(),
                initial_delay_seconds: 10,
                period_seconds: 10,
                timeout_seconds: 5,
                failure_threshold: 3,
            }
        });

        let enabled = match probe_type {
            ProbeType::Liveness => override_spec
                .and_then(|o| o.liveness_enabled)
                .unwrap_or(defaults.liveness_enabled),
            ProbeType::Readiness => override_spec
                .and_then(|o| o.readiness_enabled)
                .unwrap_or(defaults.readiness_enabled),
        };

        if !enabled {
            return None;
        }

        let path_source = override_spec
            .and_then(|o| o.path.as_deref())
            .unwrap_or(defaults.path.as_str());
        let path = if path_source.is_empty() || !path_source.starts_with('/') {
            warn!(
                "Invalid health probe path '{}', using default '/'",
                path_source
            );
            "/".to_string()
        } else {
            path_source.to_string()
        };

        Some(Probe {
            http_get: Some(HTTPGetAction {
                path: Some(path),
                port: k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(port),
                scheme: Some("HTTP".to_string()),
                ..Default::default()
            }),
            initial_delay_seconds: Some(
                override_spec
                    .and_then(|o| o.initial_delay_seconds)
                    .unwrap_or(defaults.initial_delay_seconds),
            ),
            period_seconds: Some(
                override_spec
                    .and_then(|o| o.period_seconds)
                    .unwrap_or(defaults.period_seconds),
            ),
            timeout_seconds: Some(
                override_spec
                    .and_then(|o| o.timeout_seconds)
                    .unwrap_or(defaults.timeout_seconds),
            ),
            failure_threshold: Some(
                override_spec
                    .and_then(|o| o.failure_threshold)
                    .unwrap_or(defaults.failure_threshold),
            ),
            success_threshold: Some(1),
            ..Default::default()
        })
    }

    fn create_extra_service_token_volume(&self) -> Option<Volume> {
        if self.extra_service_token_audiences.is_empty() {
            return None;
        }

        let mut token_names: Vec<_> = self.extra_service_token_audiences.keys().cloned().collect();
        token_names.sort();

        let sources = token_names
            .into_iter()
            .map(|name| VolumeProjection {
                service_account_token: Some(ServiceAccountTokenProjection {
                    audience: Some(
                        self.extra_service_token_audiences
                            .get(&name)
                            .expect("token name collected from map keys")
                            .clone(),
                    ),
                    path: name,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();

        Some(Volume {
            name: EXTRA_SERVICE_TOKENS_VOLUME_NAME.to_string(),
            projected: Some(ProjectedVolumeSource {
                sources: Some(sources),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn create_extra_service_token_volume_mount(&self) -> Option<VolumeMount> {
        if self.extra_service_token_audiences.is_empty() {
            return None;
        }
        Some(VolumeMount {
            name: EXTRA_SERVICE_TOKENS_VOLUME_NAME.to_string(),
            mount_path: EXTRA_SERVICE_TOKENS_MOUNT_PATH.to_string(),
            read_only: Some(true),
            ..Default::default()
        })
    }

    // ── K8s Deployment ─────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn create_k8s_deployment(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        image: &str,
        http_port: u16,
        env_vars: Vec<EnvVar>,
        secret_env_name: Option<String>,
        secret_env_hash: Option<String>,
        service_account_name: Option<String>,
        environment_name: Option<&str>,
        identity: Option<IdentityMount>,
    ) -> K8sDeployment {
        let mut volumes: Vec<Volume> = Vec::new();
        let mut volume_mounts: Vec<VolumeMount> = Vec::new();

        if let Some(volume) = self.create_extra_service_token_volume() {
            volumes.push(volume);
        }
        if let Some(mount) = self.create_extra_service_token_volume_mount() {
            volume_mounts.push(mount);
        }

        // The bootstrap credential and auto-minted tokens are exposed purely as
        // files under a standard path (IDENTITY_MOUNT_PATH); no env vars are
        // injected, mirroring the `extra_service_token_audiences` mount.
        if let Some(ref identity) = identity {
            volumes.push(Self::create_identity_volume(identity));
            volume_mounts.push(Self::create_identity_volume_mount());
        }

        let volumes = (!volumes.is_empty()).then_some(volumes);
        let volume_mounts = (!volume_mounts.is_empty()).then_some(volume_mounts);

        K8sDeployment {
            metadata: ObjectMeta {
                name: Some(Self::deployment_name(project, deployment)),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::deployment_labels(
                    project,
                    deployment,
                    environment_name,
                )),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(deployment.replicas),
                min_ready_seconds: None,
                selector: LabelSelector {
                    match_labels: Some(Self::deployment_labels(
                        project,
                        deployment,
                        environment_name,
                    )),
                    ..Default::default()
                },
                strategy: Some(k8s_openapi::api::apps::v1::DeploymentStrategy {
                    type_: Some("RollingUpdate".to_string()),
                    rolling_update: Some(k8s_openapi::api::apps::v1::RollingUpdateDeployment {
                        max_surge: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(1),
                        ),
                        max_unavailable: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(0),
                        ),
                    }),
                }),
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(Self::deployment_labels(
                            project,
                            deployment,
                            environment_name,
                        )),
                        annotations: secret_env_hash.map(|hash| {
                            let mut annotations = BTreeMap::new();
                            annotations.insert(ANNOTATION_ENV_SECRET_HASH.to_string(), hash);
                            annotations
                        }),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        security_context: self.create_pod_security_context(),
                        image_pull_secrets: {
                            // Use the explicitly-configured secret name if present.
                            // Fall back to the default constant only when the registry
                            // provider needs the controller to mint pull secrets.
                            // requires_pull_secret() == false means the cluster handles
                            // auth itself (e.g. node IAM), but an explicit
                            // image_pull_secret_name should still always be honoured.
                            let secret_name =
                                self.image_pull_secret_name.as_deref().or_else(|| {
                                    self.registry_provider
                                        .requires_pull_secret()
                                        .then_some(IMAGE_PULL_SECRET_NAME)
                                });
                            secret_name.map(|name| {
                                vec![LocalObjectReference {
                                    name: name.to_string(),
                                }]
                            })
                        },
                        containers: vec![Container {
                            name: LEGACY_CONTAINER_NAME.to_string(),
                            image: Some(image.to_string()),
                            ports: Some(vec![ContainerPort {
                                container_port: http_port as i32,
                                ..Default::default()
                            }]),
                            image_pull_policy: Some("Always".to_string()),
                            env: (!env_vars.is_empty()).then_some(env_vars),
                            env_from: secret_env_name.map(|name| {
                                vec![EnvFromSource {
                                    secret_ref: Some(SecretEnvSource {
                                        name,
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                }]
                            }),
                            security_context: self.create_container_security_context(),
                            resources: self
                                .create_resource_requirements(&deployment.cpu, &deployment.memory),
                            liveness_probe: self
                                .create_http_probe(http_port as i32, ProbeType::Liveness),
                            readiness_probe: self
                                .create_http_probe(http_port as i32, ProbeType::Readiness),
                            volume_mounts,
                            ..Default::default()
                        }],
                        volumes,
                        node_selector: if self.node_selector.is_empty() {
                            None
                        } else {
                            Some(self.node_selector.clone().into_iter().collect())
                        },
                        host_aliases: if self.host_aliases.is_empty() {
                            None
                        } else {
                            Some(
                                self.host_aliases
                                    .iter()
                                    .map(|(hostname, ip)| HostAlias {
                                        hostnames: Some(vec![hostname.clone()]),
                                        ip: ip.clone(),
                                    })
                                    .collect(),
                            )
                        },
                        service_account_name,
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Build a K8s Deployment for one container in a multi-container Rise
    /// deployment. Mirrors `create_k8s_deployment` but uses the
    /// `ContainerRuntime` for per-container fields (image, port, replicas,
    /// resources, probes) and adds a `rise.dev/container` selector label so
    /// each container's Pods are isolated.
    #[allow(clippy::too_many_arguments)]
    pub fn create_k8s_deployment_for_container(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        runtime: &ContainerRuntime<'_>,
        service_account_name: Option<String>,
        environment_name: Option<&str>,
        identity: Option<IdentityMount>,
    ) -> K8sDeployment {
        let mut volumes: Vec<Volume> = Vec::new();
        let mut volume_mounts: Vec<VolumeMount> = Vec::new();

        if let Some(volume) = self.create_extra_service_token_volume() {
            volumes.push(volume);
        }
        if let Some(mount) = self.create_extra_service_token_volume_mount() {
            volume_mounts.push(mount);
        }

        if let Some(ref identity) = identity {
            volumes.push(Self::create_identity_volume(identity));
            volume_mounts.push(Self::create_identity_volume_mount());
        }

        let volumes = (!volumes.is_empty()).then_some(volumes);
        let volume_mounts = (!volume_mounts.is_empty()).then_some(volume_mounts);

        let labels = Self::deployment_labels_with_container(
            project,
            deployment,
            environment_name,
            runtime.name,
        );

        let ports = runtime.http_port.map(|port| {
            vec![ContainerPort {
                container_port: port as i32,
                ..Default::default()
            }]
        });

        let (liveness, readiness) = runtime
            .http_port
            .map(|port| {
                (
                    self.create_http_probe_with_override(
                        port as i32,
                        ProbeType::Liveness,
                        runtime.health_check.as_ref(),
                    ),
                    self.create_http_probe_with_override(
                        port as i32,
                        ProbeType::Readiness,
                        runtime.health_check.as_ref(),
                    ),
                )
            })
            .unwrap_or((None, None));

        K8sDeployment {
            metadata: ObjectMeta {
                name: Some(Self::deployment_name_for_container(
                    project, deployment, runtime,
                )),
                namespace: Some(namespace.to_string()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(runtime.replicas),
                min_ready_seconds: None,
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                },
                strategy: Some(k8s_openapi::api::apps::v1::DeploymentStrategy {
                    type_: Some("RollingUpdate".to_string()),
                    rolling_update: Some(k8s_openapi::api::apps::v1::RollingUpdateDeployment {
                        max_surge: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(1),
                        ),
                        max_unavailable: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(0),
                        ),
                    }),
                }),
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels),
                        annotations: runtime.secret_env_hash.as_ref().map(|hash| {
                            let mut annotations = BTreeMap::new();
                            annotations
                                .insert(ANNOTATION_ENV_SECRET_HASH.to_string(), hash.clone());
                            annotations
                        }),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        security_context: self.create_pod_security_context(),
                        image_pull_secrets: {
                            let secret_name =
                                self.image_pull_secret_name.as_deref().or_else(|| {
                                    self.registry_provider
                                        .requires_pull_secret()
                                        .then_some(IMAGE_PULL_SECRET_NAME)
                                });
                            secret_name.map(|name| {
                                vec![LocalObjectReference {
                                    name: name.to_string(),
                                }]
                            })
                        },
                        containers: vec![Container {
                            name: runtime.name.to_string(),
                            image: Some(runtime.image.to_string()),
                            ports,
                            image_pull_policy: Some("Always".to_string()),
                            env: (!runtime.env_vars.is_empty()).then(|| runtime.env_vars.clone()),
                            env_from: runtime.secret_env_name.as_ref().map(|name| {
                                vec![EnvFromSource {
                                    secret_ref: Some(SecretEnvSource {
                                        name: name.clone(),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                }]
                            }),
                            security_context: self.create_container_security_context(),
                            resources: self
                                .create_resource_requirements(runtime.cpu, runtime.memory),
                            liveness_probe: liveness,
                            readiness_probe: readiness,
                            volume_mounts,
                            ..Default::default()
                        }],
                        volumes,
                        node_selector: if self.node_selector.is_empty() {
                            None
                        } else {
                            Some(self.node_selector.clone().into_iter().collect())
                        },
                        host_aliases: if self.host_aliases.is_empty() {
                            None
                        } else {
                            Some(
                                self.host_aliases
                                    .iter()
                                    .map(|(hostname, ip)| HostAlias {
                                        hostnames: Some(vec![hostname.clone()]),
                                        ip: ip.clone(),
                                    })
                                    .collect(),
                            )
                        },
                        service_account_name,
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Build a K8s Service for one routable container. Workers (no `http_port`)
    /// should never have this called for them — the caller should skip them.
    ///
    /// The Service exposes the container's `http_port` directly (Service port
    /// = target port). This keeps cross-container addresses intuitive — e.g.
    /// `default-redis:6379` actually reaches Redis — and works for non-HTTP
    /// services too. Ingresses reference the port by name (`"http"`), so
    /// changing the number doesn't affect ingress routing.
    pub fn create_service_for_container(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        runtime: &ContainerRuntime<'_>,
        environment_name: Option<&str>,
    ) -> Option<Service> {
        let port = runtime.http_port?;
        Some(Service {
            metadata: ObjectMeta {
                name: Some(Self::service_name_for_container(
                    project, deployment, runtime,
                )),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::common_labels(project, environment_name)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".to_string()),
                selector: Some(Self::deployment_labels_with_container(
                    project,
                    deployment,
                    environment_name,
                    runtime.name,
                )),
                ports: Some(vec![ServicePort {
                    name: Some("http".to_string()),
                    port: port as i32,
                    target_port: Some(
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(port as i32),
                    ),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    // ── Ingress ────────────────────────────────────────────────────────

    fn build_ingress_annotations(
        &self,
        project: &Project,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let mut annotations: BTreeMap<String, String> = self
            .ingress_annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let access_class = self
            .access_classes
            .get(&project.access_class)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Access class '{}' not configured. Available: {}",
                    project.access_class,
                    self.access_classes
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;

        match access_class.access_requirement {
            AccessRequirement::None => {}
            AccessRequirement::Authenticated | AccessRequirement::Member => {
                let auth_url = format!(
                    "{}/api/v1/auth/ingress?project={}",
                    self.auth_backend_url, project.name
                );

                let signin_url = if self.backend_address.is_some() {
                    format!(
                        "{}://$http_host/.rise/auth/signin?project={}&redirect=$scheme://$http_host$escaped_request_uri",
                        self.ingress_schema,
                        urlencoding::encode(&project.name)
                    )
                } else {
                    format!(
                        "{}/api/v1/auth/signin?project={}&redirect=$scheme://$http_host$escaped_request_uri",
                        self.auth_signin_url,
                        urlencoding::encode(&project.name)
                    )
                };

                annotations.insert("nginx.ingress.kubernetes.io/auth-url".to_string(), auth_url);
                annotations.insert(
                    "nginx.ingress.kubernetes.io/auth-signin".to_string(),
                    signin_url,
                );
                annotations.insert(
                    "nginx.ingress.kubernetes.io/auth-response-headers".to_string(),
                    "X-Auth-Request-Email,X-Auth-Request-User".to_string(),
                );
            }
        }

        for (key, value) in &access_class.custom_annotations {
            annotations.insert(key.clone(), value.clone());
        }

        Ok(annotations)
    }

    fn get_ingress_class_for_project(&self, project: &Project) -> anyhow::Result<&str> {
        let access_class = self
            .access_classes
            .get(&project.access_class)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Access class '{}' not configured. Available: {}",
                    project.access_class,
                    self.access_classes
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        Ok(&access_class.ingress_class)
    }

    fn build_ingress_paths(
        &self,
        service_name: &str,
        app_path: &str,
        app_path_type: &str,
    ) -> Vec<HTTPIngressPath> {
        let mut paths = vec![HTTPIngressPath {
            path: Some(app_path.to_string()),
            path_type: app_path_type.to_string(),
            backend: IngressBackend {
                service: Some(IngressServiceBackend {
                    name: service_name.to_string(),
                    port: Some(ServiceBackendPort {
                        name: Some("http".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
        }];

        self.append_rise_backend_path(&mut paths);
        paths
    }

    /// Multi-route variant of `build_ingress_paths`. Given a list of
    /// `(path, service_name)` pairs, emits one `HTTPIngressPath` per route in
    /// longest-prefix-first order (lexicographic tiebreaker for deterministic
    /// output across reconciles). The `/.rise` backend path is appended last.
    ///
    /// Routes whose path does not start with `/` are skipped with a warning;
    /// the request-time validator (`models::validate_containers_and_routes`)
    /// should make this unreachable, but emitting an invalid Ingress would
    /// take the whole project's traffic plane down — defence in depth.
    fn build_ingress_paths_multi(&self, routes: &[(String, String)]) -> Vec<HTTPIngressPath> {
        let mut sorted: Vec<(String, String)> = routes
            .iter()
            .filter(|(path, _)| {
                if path.starts_with('/') {
                    true
                } else {
                    warn!(
                        "Skipping invalid ingress route '{}': path must start with '/'",
                        path
                    );
                    false
                }
            })
            .cloned()
            .collect();
        sorted.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));

        let mut paths: Vec<HTTPIngressPath> = sorted
            .into_iter()
            .map(|(path, service_name)| HTTPIngressPath {
                path: Some(path),
                path_type: "Prefix".to_string(),
                backend: IngressBackend {
                    service: Some(IngressServiceBackend {
                        name: service_name,
                        port: Some(ServiceBackendPort {
                            name: Some("http".to_string()),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                },
            })
            .collect();

        self.append_rise_backend_path(&mut paths);
        paths
    }

    fn append_rise_backend_path(&self, paths: &mut Vec<HTTPIngressPath>) {
        if let Some(ref backend_addr) = self.backend_address {
            paths.push(HTTPIngressPath {
                path: Some("/.rise".to_string()),
                path_type: "Prefix".to_string(),
                backend: IngressBackend {
                    service: Some(IngressServiceBackend {
                        name: "rise-backend".to_string(),
                        port: Some(ServiceBackendPort {
                            number: Some(backend_addr.port as i32),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                },
            });
        }
    }

    /// Build the primary ingress resource for an active deployment in a group.
    ///
    /// `environment_for_group` is the environment whose `primary_deployment_group`
    /// equals this deployment's group (None when no env is mapped to the group).
    /// `inline_custom_domains` are folded into this ingress when non-empty; when
    /// `custom_domain_ingress_annotations` is configured, callers should pass an
    /// empty slice here and use `create_custom_domain_ingress` separately so the
    /// custom annotations only apply to those domains.
    #[allow(clippy::too_many_arguments)]
    pub fn create_primary_ingress(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        environment_for_group: Option<&crate::db::models::Environment>,
        inline_custom_domains: &[CustomDomain],
        environment_name: Option<&str>,
        all_environments: &[crate::db::models::Environment],
        // (path, service_name) pairs for multi-container routing. When None,
        // falls back to a single `/` → service_name(project, deployment) path
        // (legacy single-container behaviour). Callers should pre-filter:
        // passing `Some(&[])` is a multi-container-with-no-routable-pairs
        // signal (workers-only), for which we must NOT emit an ingress at all.
        multi_container_routes: Option<&[(String, String)]>,
    ) -> anyhow::Result<Option<Ingress>> {
        // Defensive guard: a multi-container deployment with zero routable
        // pairs (workers-only) would otherwise fall through to the legacy
        // `/` → group-service backend in the match below, pointing at a
        // Service phase 2 never emitted. Caller is responsible for skipping
        // this call entirely; this is belt-and-suspenders.
        if matches!(multi_container_routes, Some(routes) if routes.is_empty()) {
            debug_assert!(
                false,
                "create_primary_ingress called with empty multi_container_routes; caller should skip"
            );
            return Ok(None);
        }
        let mut hosts = self.primary_ingress_hosts(
            project,
            &deployment.deployment_group,
            environment_for_group,
            inline_custom_domains,
            !inline_custom_domains.is_empty(),
            all_environments,
        );

        // Historical fallback: when neither a DG host nor an env host applies
        // (e.g. the default group with no staging template and no production
        // env mapping), use the single-host computation so the deployment is
        // still reachable. Skip this fallback when the resulting host would
        // collide with another env's URL — nginx would reject it and we'd
        // rather emit no ingress than spin on admission failures.
        if hosts.is_empty() {
            let fallback = self.ingress_url_components(project, deployment);
            if self.host_conflicts_with_other_env(
                project,
                &deployment.deployment_group,
                &fallback.host,
                all_environments,
            ) {
                warn!(
                    project = %project.name,
                    deployment_group = %deployment.deployment_group,
                    host = %fallback.host,
                    "Skipping ingress: all candidate hosts collide with another env's URL"
                );
                return Ok(None);
            }
            hosts.push(fallback);
        }

        let mut annotations = self.build_ingress_annotations(project)?;

        // Annotations are per-ingress: if the DG host (first templated host) is
        // path-based, apply the rewrite-target annotation. Mixed strategies on
        // the same ingress are not supported by nginx-ingress today.
        if let Some(ref path) = hosts[0].path_prefix {
            annotations.insert(
                "nginx.ingress.kubernetes.io/rewrite-target".to_string(),
                "/$2".to_string(),
            );
            annotations.insert(
                "nginx.ingress.kubernetes.io/x-forwarded-prefix".to_string(),
                path.trim_end_matches('/').to_string(),
            );
        }

        let service_name = Self::service_name(project, deployment);

        let rules: Vec<IngressRule> = hosts
            .iter()
            .map(|host| {
                let paths = match multi_container_routes {
                    // Multi-container path: ignore per-host path_prefix (it was
                    // designed for the single-service-at-/ shape) and emit the
                    // user-declared route table verbatim. Per-host rewrite
                    // annotations are also skipped above for the multi case.
                    Some(routes) if !routes.is_empty() => self.build_ingress_paths_multi(routes),
                    _ => {
                        let (path, path_type) = if let Some(ref prefix) = host.path_prefix {
                            (
                                format!("{}(/|$)(.*)", prefix.trim_end_matches('/')),
                                "ImplementationSpecific",
                            )
                        } else {
                            ("/".to_string(), "Prefix")
                        };
                        self.build_ingress_paths(&service_name, &path, path_type)
                    }
                };
                IngressRule {
                    host: Some(host.host.clone()),
                    http: Some(HTTPIngressRuleValue { paths }),
                }
            })
            .collect();

        let templated_hosts: Vec<String> = hosts
            .iter()
            .filter(|h| !inline_custom_domains.iter().any(|cd| cd.domain == h.host))
            .map(|h| h.host.clone())
            .collect();

        let tls = self.build_primary_tls_config(&templated_hosts, inline_custom_domains);

        Ok(Some(Ingress {
            metadata: ObjectMeta {
                name: Some(Self::ingress_name(project, deployment)),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::common_labels(project, environment_name)),
                annotations: Some(annotations),
                ..Default::default()
            },
            spec: Some(IngressSpec {
                ingress_class_name: Some(self.get_ingress_class_for_project(project)?.to_string()),
                tls,
                rules: Some(rules),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    pub fn create_custom_domain_ingress(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        custom_domains: &[CustomDomain],
        environment_name: Option<&str>,
        // Multi-container routes ((path, service_name) pairs). None falls back
        // to `/` → legacy group service.
        multi_container_routes: Option<&[(String, String)]>,
    ) -> anyhow::Result<Ingress> {
        let mut annotations = self.build_ingress_annotations(project)?;

        for (k, v) in &self.custom_domain_ingress_annotations {
            annotations.insert(k.clone(), v.clone());
        }

        let service_name = Self::service_name(project, deployment);

        let mut rules = Vec::new();
        for domain in custom_domains {
            let paths = match multi_container_routes {
                Some(routes) if !routes.is_empty() => self.build_ingress_paths_multi(routes),
                _ => self.build_ingress_paths(&service_name, "/", "Prefix"),
            };
            rules.push(IngressRule {
                host: Some(domain.domain.clone()),
                http: Some(HTTPIngressRuleValue { paths }),
            });
        }

        let tls = self.build_custom_domain_tls_config(custom_domains);

        Ok(Ingress {
            metadata: ObjectMeta {
                name: Some(Self::custom_domain_ingress_name(project, deployment)),
                namespace: Some(namespace.to_string()),
                labels: Some(Self::common_labels(project, environment_name)),
                annotations: Some(annotations),
                ..Default::default()
            },
            spec: Some(IngressSpec {
                ingress_class_name: Some(self.get_ingress_class_for_project(project)?.to_string()),
                tls,
                rules: Some(rules),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn build_primary_tls_config(
        &self,
        templated_hosts: &[String],
        inline_custom_domains: &[CustomDomain],
    ) -> Option<Vec<k8s_openapi::api::networking::v1::IngressTLS>> {
        let mut entries: Vec<k8s_openapi::api::networking::v1::IngressTLS> = Vec::new();

        if !templated_hosts.is_empty() {
            if let Some(shared_secret) = self.ingress_tls_secret_name.as_ref() {
                entries.push(k8s_openapi::api::networking::v1::IngressTLS {
                    hosts: Some(templated_hosts.to_vec()),
                    secret_name: Some(shared_secret.clone()),
                });
            }
        }

        if !inline_custom_domains.is_empty() {
            if let Some(custom) = self.build_custom_domain_tls_config(inline_custom_domains) {
                entries.extend(custom);
            }
        }

        if entries.is_empty() {
            None
        } else {
            Some(entries)
        }
    }

    fn build_custom_domain_tls_config(
        &self,
        custom_domains: &[CustomDomain],
    ) -> Option<Vec<k8s_openapi::api::networking::v1::IngressTLS>> {
        if custom_domains.is_empty() {
            return None;
        }

        let mut tls_configs = Vec::new();

        match self.custom_domain_tls_mode {
            crate::server::settings::CustomDomainTlsMode::Shared => {
                let shared_secret = self.ingress_tls_secret_name.as_ref()?;
                let all_hosts: Vec<String> =
                    custom_domains.iter().map(|d| d.domain.clone()).collect();
                tls_configs.push(k8s_openapi::api::networking::v1::IngressTLS {
                    hosts: Some(all_hosts),
                    secret_name: Some(shared_secret.clone()),
                });
            }
            crate::server::settings::CustomDomainTlsMode::PerDomain => {
                for domain in custom_domains {
                    tls_configs.push(k8s_openapi::api::networking::v1::IngressTLS {
                        hosts: Some(vec![domain.domain.clone()]),
                        secret_name: Some(format!("tls-{}", domain.domain)),
                    });
                }
            }
        }

        Some(tls_configs)
    }

    // ── Image tag resolution ───────────────────────────────────────────

    /// Resolve the image reference for a deployment.
    /// For pre-built images, uses the pinned digest.
    /// For rollback deployments, uses the source deployment's tag.
    /// For regular builds, constructs from registry config.
    pub fn resolve_image(
        &self,
        project: &Project,
        deployment: &Deployment,
        source_deployment_id: Option<&str>,
    ) -> String {
        if let Some(ref image_digest) = deployment.image_digest {
            image_digest.clone()
        } else {
            let deployment_id_for_tag = source_deployment_id
                .unwrap_or(&deployment.deployment_id)
                .to_string();
            self.registry_provider.get_image_tag(
                &project.name,
                &deployment_id_for_tag,
                crate::server::registry::ImageTagType::Internal,
            )
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::server::registry::models::RegistryCredentials;
    use crate::server::registry::{ImageTagType, RegistryProvider};
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct TestRegistryProvider {
        requires_pull_secret: bool,
    }

    impl Default for TestRegistryProvider {
        fn default() -> Self {
            Self {
                requires_pull_secret: true,
            }
        }
    }

    #[async_trait]
    impl RegistryProvider for TestRegistryProvider {
        async fn get_credentials(
            &self,
            _repository: &str,
            _tags: &[&str],
        ) -> Result<RegistryCredentials> {
            unreachable!("not used in these tests")
        }

        async fn get_pull_credentials(&self) -> Result<(String, String)> {
            unreachable!("not used in these tests")
        }

        fn registry_host(&self) -> &str {
            "registry.example.test"
        }

        fn registry_url(&self) -> &str {
            "registry.example.test/rise"
        }

        fn get_image_tag(&self, repository: &str, tag: &str, _tag_type: ImageTagType) -> String {
            format!("registry.example.test/rise/{repository}:{tag}")
        }

        fn requires_pull_secret(&self) -> bool {
            self.requires_pull_secret
        }
    }

    fn test_resource_builder() -> ResourceBuilder {
        ResourceBuilder {
            production_ingress_url_template: "{project_name}.example.test".to_string(),
            staging_ingress_url_template: None,
            environment_ingress_url_template: None,
            ingress_port: None,
            ingress_schema: "https".to_string(),
            registry_provider: Arc::new(TestRegistryProvider::default()),
            auth_backend_url: "https://auth.example.test".to_string(),
            auth_signin_url: "https://signin.example.test".to_string(),
            backend_address: None,
            namespace_labels: std::collections::HashMap::new(),
            namespace_annotations: std::collections::HashMap::new(),
            ingress_annotations: std::collections::HashMap::new(),
            ingress_tls_secret_name: None,
            custom_domain_tls_mode: crate::server::settings::CustomDomainTlsMode::PerDomain,
            custom_domain_ingress_annotations: std::collections::HashMap::new(),
            node_selector: std::collections::HashMap::new(),
            image_pull_secret_name: None,
            access_classes: std::collections::HashMap::new(),
            host_aliases: std::collections::HashMap::new(),
            extra_service_token_audiences: std::collections::HashMap::new(),
            use_default_service_account_for_production: true,
            network_policy: crate::server::settings::NetworkPolicyConfig {
                ingress: vec![],
                egress: None,
            },
            pod_security_enabled: true,
            health_probes: None,
        }
    }

    fn test_project() -> Project {
        Project {
            id: uuid::Uuid::nil(),
            name: "demo".to_string(),
            status: crate::db::models::ProjectStatus::Running,
            access_class: "default".to_string(),
            owner_user_id: None,
            owner_team_id: None,
            finalizers: vec![],
            source_url: None,
            template_id: None,
            template_image: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn test_deployment() -> Deployment {
        Deployment {
            id: uuid::Uuid::nil(),
            deployment_id: "20260502-000000".to_string(),
            project_id: uuid::Uuid::nil(),
            created_by_id: uuid::Uuid::nil(),
            status: crate::db::models::DeploymentStatus::Healthy,
            deployment_group: "default".to_string(),
            environment_id: None,
            expires_at: None,
            termination_reason: None,
            completed_at: None,
            error_message: None,
            build_logs: None,
            controller_metadata: serde_json::Value::Null,
            image: None,
            image_digest: None,
            rolled_back_from_deployment_id: None,
            http_port: 8080,
            needs_reconcile: false,
            is_active: true,
            deploying_started_at: None,
            first_healthy_at: None,
            job_url: None,
            pull_request_url: None,
            git_repository_url: None,
            replicas: 1,
            cpu: "500m".to_string(),
            memory: "256Mi".to_string(),
            identity_credential_hash: None,
            identity_audiences: serde_json::json!({}),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn create_deployment_env_secret_uses_stable_name_and_labels() {
        let builder = test_resource_builder();
        let project = test_project();
        let deployment = test_deployment();

        let mut data = BTreeMap::new();
        data.insert("API_KEY".to_string(), ByteString(b"super-secret".to_vec()));

        let secret = builder.create_deployment_env_secret(
            &project,
            &deployment,
            "demo",
            None,
            "abc123",
            data.clone(),
        );

        assert_eq!(
            secret.metadata.name.as_deref(),
            Some("demo-20260502-000000-env")
        );
        assert_eq!(secret.metadata.namespace.as_deref(), Some("demo"));
        assert_eq!(
            secret
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get(LABEL_DEPLOYMENT_ID))
                .map(String::as_str),
            Some("20260502-000000")
        );
        assert_eq!(secret.type_.as_deref(), Some("Opaque"));
        assert_eq!(
            secret
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get(ANNOTATION_ENV_SECRET_HASH))
                .map(String::as_str),
            Some("abc123")
        );
        assert_eq!(secret.data, Some(data));
    }

    #[test]
    fn create_k8s_deployment_uses_env_from_for_secret_env() {
        let builder = test_resource_builder();
        let project = test_project();
        let deployment = test_deployment();

        let k8s_deployment = builder.create_k8s_deployment(
            &project,
            &deployment,
            "demo",
            "registry.example.test/rise/demo:20260502-000000",
            8080,
            vec![EnvVar {
                name: "PORT".to_string(),
                value: Some("8080".to_string()),
                ..Default::default()
            }],
            Some("demo-20260502-000000-env".to_string()),
            Some("abc123".to_string()),
            None,
            None,
            None,
        );

        let container = &k8s_deployment
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0];

        assert_eq!(container.env.as_ref().unwrap().len(), 1);
        assert_eq!(
            container.env_from.as_ref().unwrap()[0]
                .secret_ref
                .as_ref()
                .unwrap()
                .name
                .as_str(),
            "demo-20260502-000000-env"
        );
        assert_eq!(
            k8s_deployment
                .spec
                .as_ref()
                .unwrap()
                .template
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.annotations.as_ref())
                .and_then(|annotations| annotations.get(ANNOTATION_ENV_SECRET_HASH))
                .map(String::as_str),
            Some("abc123")
        );
    }

    #[test]
    fn create_k8s_deployment_omits_empty_env_and_env_from() {
        let builder = test_resource_builder();
        let project = test_project();
        let deployment = test_deployment();

        let k8s_deployment = builder.create_k8s_deployment(
            &project,
            &deployment,
            "demo",
            "registry.example.test/rise/demo:20260502-000000",
            8080,
            vec![],
            None,
            None,
            None,
            None,
            None,
        );

        let container = &k8s_deployment
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers[0];

        assert!(container.env.is_none());
        assert!(container.env_from.is_none());
    }

    #[test]
    fn create_k8s_deployment_mounts_identity_secret() {
        let builder = test_resource_builder();
        let k8s_deployment = builder.create_k8s_deployment(
            &test_project(),
            &test_deployment(),
            "demo",
            "registry.example.test/rise/demo:latest",
            8080,
            vec![],
            None,
            None,
            None,
            None,
            Some(IdentityMount {
                secret_name: "demo-20260502-000000-identity".to_string(),
                token_filenames: vec!["aws".to_string()],
            }),
        );

        let pod = pod_spec_from_deployment(&k8s_deployment);
        let volume = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == IDENTITY_VOLUME_NAME)
            .expect("identity volume present");
        let secret = volume.secret.as_ref().unwrap();
        assert_eq!(
            secret.secret_name.as_deref(),
            Some("demo-20260502-000000-identity")
        );
        let items = secret.items.as_ref().unwrap();
        assert!(items.iter().any(|i| i.path == "credential"));
        assert!(items.iter().any(|i| i.path == "tokens/aws"));

        let container = &pod.containers[0];
        let mount = container
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == IDENTITY_VOLUME_NAME)
            .expect("identity volume mount present");
        assert_eq!(mount.mount_path, IDENTITY_MOUNT_PATH);
        assert_eq!(mount.read_only, Some(true));

        // Identity is exposed purely as mounted files — no env vars injected.
        assert!(container.env.is_none());
    }

    #[test]
    fn create_k8s_deployment_omits_identity_when_absent() {
        let builder = test_resource_builder();
        let k8s_deployment = builder.create_k8s_deployment(
            &test_project(),
            &test_deployment(),
            "demo",
            "registry.example.test/rise/demo:latest",
            8080,
            vec![],
            None,
            None,
            None,
            None,
            None,
        );

        let pod = pod_spec_from_deployment(&k8s_deployment);
        let has_identity_volume = pod
            .volumes
            .as_ref()
            .map(|vols| vols.iter().any(|v| v.name == IDENTITY_VOLUME_NAME))
            .unwrap_or(false);
        assert!(!has_identity_volume);
    }

    fn pod_spec_from_deployment(k8s_deployment: &K8sDeployment) -> &PodSpec {
        k8s_deployment
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
    }

    fn make_deployment(
        image_pull_secret_name: Option<&str>,
        requires_pull_secret: bool,
    ) -> K8sDeployment {
        let mut builder = test_resource_builder();
        builder.registry_provider = Arc::new(TestRegistryProvider {
            requires_pull_secret,
        });
        builder.image_pull_secret_name = image_pull_secret_name.map(str::to_string);
        builder.create_k8s_deployment(
            &test_project(),
            &test_deployment(),
            "demo",
            "registry.example.test/rise/demo:latest",
            8080,
            vec![],
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn image_pull_secret_explicit_name_honoured_when_requires_pull_secret_false() {
        let d = make_deployment(Some("my-registry-creds"), false);
        let secrets = &pod_spec_from_deployment(&d).image_pull_secrets;
        assert_eq!(
            secrets
                .as_ref()
                .and_then(|s| s.first())
                .map(|s| s.name.as_str()),
            Some("my-registry-creds"),
        );
    }

    #[test]
    fn image_pull_secret_explicit_name_honoured_when_requires_pull_secret_true() {
        let d = make_deployment(Some("my-registry-creds"), true);
        let secrets = &pod_spec_from_deployment(&d).image_pull_secrets;
        assert_eq!(
            secrets
                .as_ref()
                .and_then(|s| s.first())
                .map(|s| s.name.as_str()),
            Some("my-registry-creds"),
        );
    }

    #[test]
    fn image_pull_secret_defaults_to_constant_when_requires_pull_secret_true_and_no_explicit_name()
    {
        let d = make_deployment(None, true);
        let secrets = &pod_spec_from_deployment(&d).image_pull_secrets;
        assert_eq!(
            secrets
                .as_ref()
                .and_then(|s| s.first())
                .map(|s| s.name.as_str()),
            Some(IMAGE_PULL_SECRET_NAME),
        );
    }

    #[test]
    fn image_pull_secret_absent_when_requires_pull_secret_false_and_no_explicit_name() {
        let d = make_deployment(None, false);
        let secrets = &pod_spec_from_deployment(&d).image_pull_secrets;
        assert!(secrets.is_none());
    }

    // ── Multi-host ingress tests ────────────────────────────────────────────

    fn test_environment(
        name: &str,
        is_production: bool,
        primary_group: &str,
    ) -> crate::db::models::Environment {
        crate::db::models::Environment {
            id: uuid::Uuid::nil(),
            project_id: uuid::Uuid::nil(),
            name: name.to_string(),
            primary_deployment_group: Some(primary_group.to_string()),
            is_production,
            color: "green".to_string(),
            min_replicas: None,
            max_replicas: None,
            min_cpu: None,
            max_cpu: None,
            min_memory: None,
            max_memory: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn test_custom_domain(domain: &str, is_primary: bool) -> CustomDomain {
        CustomDomain {
            id: uuid::Uuid::new_v4(),
            project_id: uuid::Uuid::nil(),
            environment_id: uuid::Uuid::nil(),
            domain: domain.to_string(),
            is_primary,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn dg_host_omitted_for_default_group_without_staging_template() {
        let builder = test_resource_builder();
        let project = test_project();
        assert_eq!(
            builder.resolved_deployment_group_url(&project, "default"),
            None
        );
    }

    #[test]
    fn dg_host_synthesized_for_non_default_without_staging_template() {
        let builder = test_resource_builder();
        let project = test_project();
        assert_eq!(
            builder.resolved_deployment_group_url(&project, "mr-42"),
            Some("demo-mr-42.example.test".to_string())
        );
    }

    #[test]
    fn dg_host_uses_staging_template_when_set() {
        let mut builder = test_resource_builder();
        builder.staging_ingress_url_template =
            Some("{project_name}-{deployment_group}.preview.example.test".to_string());
        let project = test_project();
        assert_eq!(
            builder.resolved_deployment_group_url(&project, "default"),
            Some("demo-default.preview.example.test".to_string())
        );
        assert_eq!(
            builder.resolved_deployment_group_url(&project, "mr/42"),
            Some("demo-mr--42.preview.example.test".to_string())
        );
    }

    #[test]
    fn primary_hosts_default_group_prod_env_includes_production_and_custom() {
        let mut builder = test_resource_builder();
        builder.staging_ingress_url_template =
            Some("{project_name}-{deployment_group}.preview.example.test".to_string());
        let project = test_project();
        let prod_env = test_environment("production", true, "default");
        let domains = vec![test_custom_domain("api.example.com", true)];

        let hosts = builder.primary_ingress_hosts(
            &project,
            "default",
            Some(&prod_env),
            &domains,
            true,
            &[],
        );
        let host_strs: Vec<&str> = hosts.iter().map(|h| h.host.as_str()).collect();

        assert!(host_strs.contains(&"demo-default.preview.example.test"));
        assert!(host_strs.contains(&"demo.example.test"));
        assert!(host_strs.contains(&"api.example.com"));
    }

    #[test]
    fn primary_hosts_non_default_group_with_staging_env() {
        let mut builder = test_resource_builder();
        builder.staging_ingress_url_template =
            Some("{project_name}-{deployment_group}.preview.example.test".to_string());
        builder.environment_ingress_url_template =
            Some("{environment}.{project_name}.example.test".to_string());
        let project = test_project();
        let staging_env = test_environment("staging", false, "mr-42");

        let hosts =
            builder.primary_ingress_hosts(&project, "mr-42", Some(&staging_env), &[], false, &[]);
        let host_strs: Vec<&str> = hosts.iter().map(|h| h.host.as_str()).collect();

        assert_eq!(
            host_strs,
            vec![
                "demo-mr-42.preview.example.test",
                "staging.demo.example.test"
            ]
        );
    }

    #[test]
    fn primary_hosts_dedup_when_dg_and_env_collide() {
        // For the default group with no staging template, no DG host is emitted —
        // the env host (production) is the only templated host.
        let builder = test_resource_builder();
        let project = test_project();
        let prod_env = test_environment("production", true, "default");

        let hosts =
            builder.primary_ingress_hosts(&project, "default", Some(&prod_env), &[], false, &[]);
        let host_strs: Vec<&str> = hosts.iter().map(|h| h.host.as_str()).collect();
        assert_eq!(host_strs, vec!["demo.example.test"]);
    }

    #[test]
    fn primary_hosts_no_env_returns_only_dg_host() {
        let mut builder = test_resource_builder();
        builder.staging_ingress_url_template =
            Some("{project_name}-{deployment_group}.preview.example.test".to_string());
        let project = test_project();

        let hosts = builder.primary_ingress_hosts(&project, "mr-42", None, &[], false, &[]);
        let host_strs: Vec<&str> = hosts.iter().map(|h| h.host.as_str()).collect();
        assert_eq!(host_strs, vec!["demo-mr-42.preview.example.test"]);
    }

    #[test]
    fn primary_hosts_suppress_dg_when_it_collides_with_other_env_url() {
        // Reproduces the convertx case: a deployment group named "staging"
        // would emit DG host "staging--demo.preview.example.test", which
        // collides with the env URL of the "staging" environment whose
        // primary deployment group is "staging-grp".
        let mut builder = test_resource_builder();
        builder.staging_ingress_url_template =
            Some("{deployment_group}--{project_name}.preview.example.test".to_string());
        builder.environment_ingress_url_template =
            Some("{environment}--{project_name}.preview.example.test".to_string());
        let project = test_project();
        let staging_env = test_environment("staging", false, "staging-grp");

        // Group "staging" has no env_for_group (no env's primary_dg == "staging")
        // and its DG URL collides with staging-env's URL. Hosts must be empty.
        let hosts = builder.primary_ingress_hosts(
            &project,
            "staging",
            None,
            &[],
            false,
            std::slice::from_ref(&staging_env),
        );
        assert!(
            hosts.is_empty(),
            "DG URL for 'staging' group should be suppressed; got {:?}",
            hosts
        );

        // Group "staging-grp" emits both its DG URL and the staging env URL.
        let hosts = builder.primary_ingress_hosts(
            &project,
            "staging-grp",
            Some(&staging_env),
            &[],
            false,
            std::slice::from_ref(&staging_env),
        );
        let host_strs: Vec<&str> = hosts.iter().map(|h| h.host.as_str()).collect();
        assert_eq!(
            host_strs,
            vec![
                "staging-grp--demo.preview.example.test",
                "staging--demo.preview.example.test"
            ]
        );
    }

    #[test]
    fn create_primary_ingress_emits_multiple_rules_and_tls_entries() {
        let mut builder = test_resource_builder();
        builder.staging_ingress_url_template =
            Some("{project_name}-{deployment_group}.preview.example.test".to_string());
        builder.ingress_tls_secret_name = Some("shared-wildcard".to_string());
        builder.access_classes.insert(
            "default".to_string(),
            crate::server::settings::AccessClass {
                display_name: "Public".to_string(),
                description: "Public access".to_string(),
                ingress_class: "nginx".to_string(),
                access_requirement: crate::server::settings::AccessRequirement::None,
                custom_annotations: std::collections::HashMap::new(),
            },
        );
        let project = test_project();
        let mut deployment = test_deployment();
        deployment.deployment_group = "default".to_string();
        let prod_env = test_environment("production", true, "default");
        let domains = vec![test_custom_domain("api.example.com", true)];

        let ingress = builder
            .create_primary_ingress(
                &project,
                &deployment,
                "demo",
                Some(&prod_env),
                &domains,
                Some("production"),
                &[],
                None,
            )
            .expect("primary ingress")
            .expect("ingress should be emitted");

        let spec = ingress.spec.expect("spec");
        let rules = spec.rules.expect("rules");
        let hosts: Vec<String> = rules.iter().filter_map(|r| r.host.clone()).collect();
        assert!(hosts.contains(&"demo-default.preview.example.test".to_string()));
        assert!(hosts.contains(&"demo.example.test".to_string()));
        assert!(hosts.contains(&"api.example.com".to_string()));

        let tls = spec.tls.expect("tls");
        // One entry for templated hosts (shared cert) + one per custom domain (PerDomain mode)
        assert!(tls
            .iter()
            .any(|t| t.secret_name.as_deref() == Some("shared-wildcard")));
        assert!(tls
            .iter()
            .any(|t| t.secret_name.as_deref() == Some("tls-api.example.com")));
    }

    #[test]
    fn create_primary_ingress_returns_none_when_all_hosts_collide() {
        // The "staging" deployment group has no env that names it as primary,
        // and its DG URL collides with the staging env's URL (whose primary is
        // a different group). With no other host to fall back to, the ingress
        // is suppressed entirely.
        let mut builder = test_resource_builder();
        builder.staging_ingress_url_template =
            Some("{deployment_group}--{project_name}.preview.example.test".to_string());
        builder.environment_ingress_url_template =
            Some("{environment}--{project_name}.preview.example.test".to_string());
        builder.access_classes.insert(
            "default".to_string(),
            crate::server::settings::AccessClass {
                display_name: "Public".to_string(),
                description: "Public access".to_string(),
                ingress_class: "nginx".to_string(),
                access_requirement: crate::server::settings::AccessRequirement::None,
                custom_annotations: std::collections::HashMap::new(),
            },
        );
        let project = test_project();
        let mut deployment = test_deployment();
        deployment.deployment_group = "staging".to_string();
        let staging_env = test_environment("staging", false, "staging-grp");

        let result = builder
            .create_primary_ingress(
                &project,
                &deployment,
                "demo",
                None,
                &[],
                Some("staging"),
                std::slice::from_ref(&staging_env),
                None,
            )
            .expect("call ok");

        assert!(
            result.is_none(),
            "expected no ingress when all candidate hosts collide"
        );
    }

    #[test]
    fn auto_container_host_env_vars_emits_one_entry_per_routable_container() {
        use crate::server::deployment::models::ContainerSpec;
        let deployment = test_deployment(); // deployment_group = "default"
        let specs = vec![
            ContainerSpec {
                name: "frontend".to_string(),
                image: Some("img".to_string()),
                http_port: Some(8080),
                replicas: Some(1),
                cpu: None,
                memory: None,
                env_overrides: vec![],
                health_check: None,
            },
            ContainerSpec {
                name: "api".to_string(),
                image: Some("img".to_string()),
                http_port: Some(8080),
                replicas: Some(1),
                cpu: None,
                memory: None,
                env_overrides: vec![],
                health_check: None,
            },
            ContainerSpec {
                // Worker: no http_port, no Service, no env entry emitted.
                name: "worker".to_string(),
                image: Some("img".to_string()),
                http_port: None,
                replicas: Some(1),
                cpu: None,
                memory: None,
                env_overrides: vec![],
                health_check: None,
            },
            ContainerSpec {
                // Dashes in the name get mapped to underscores in the env key.
                name: "side-car".to_string(),
                image: Some("img".to_string()),
                http_port: Some(9000),
                replicas: Some(1),
                cpu: None,
                memory: None,
                env_overrides: vec![],
                health_check: None,
            },
        ];

        let vars = ResourceBuilder::auto_container_host_env_vars(&deployment, &specs);

        let pairs: Vec<(String, String)> = vars
            .into_iter()
            .map(|v| (v.name, v.value.unwrap_or_default()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                (
                    "RISE_CONTAINER_HOST__FRONTEND".to_string(),
                    "default-frontend:8080".to_string()
                ),
                (
                    "RISE_CONTAINER_HOST__API".to_string(),
                    "default-api:8080".to_string()
                ),
                (
                    "RISE_CONTAINER_HOST__SIDE_CAR".to_string(),
                    "default-side-car:9000".to_string()
                ),
            ],
            "expected one entry per routable container in spec order, with worker skipped"
        );
    }

    #[test]
    fn auto_container_host_env_vars_uses_escaped_group_name() {
        use crate::server::deployment::models::ContainerSpec;
        let mut deployment = test_deployment();
        deployment.deployment_group = "staging".to_string();
        let specs = vec![ContainerSpec {
            name: "api".to_string(),
            image: Some("img".to_string()),
            http_port: Some(8080),
            replicas: Some(1),
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        }];

        let vars = ResourceBuilder::auto_container_host_env_vars(&deployment, &specs);

        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].name, "RISE_CONTAINER_HOST__API");
        assert_eq!(vars[0].value.as_deref(), Some("staging-api:8080"));
    }

    #[test]
    fn create_service_for_container_exposes_http_port_directly() {
        // Per-container Services use port == http_port (not the legacy 80) so
        // cross-container addresses like default-redis:6379 actually reach the
        // backing pods. Ingress paths reference the port by name ("http") and
        // are unaffected.
        let builder = test_resource_builder();
        let project = test_project();
        let deployment = test_deployment();
        let runtime = ContainerRuntime {
            name: "redis",
            image: "redis:7-alpine",
            http_port: Some(6379),
            replicas: 1,
            cpu: "",
            memory: "",
            env_vars: Vec::new(),
            secret_env_name: None,
            secret_env_hash: None,
            health_check: None,
            is_legacy: false,
        };

        let service = builder
            .create_service_for_container(&project, &deployment, "ns", &runtime, None)
            .expect("Service emitted for routable container");

        let spec = service.spec.expect("spec present");
        let ports = spec.ports.expect("ports present");
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, 6379, "Service port should match http_port");
        assert_eq!(ports[0].name.as_deref(), Some("http"));
        assert_eq!(
            ports[0].target_port,
            Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(6379)),
        );
    }
}

/// Normalize a `LabelSelector` to match Kubernetes API server behavior:
/// empty `matchLabels` maps are stripped (K8s normalizes `{matchLabels: {}}`
/// to `{}`), avoiding perpetual diffs with Metacontroller's last-applied state.
fn normalize_label_selector(mut sel: LabelSelector) -> LabelSelector {
    if sel.match_labels.as_ref().is_some_and(|m| m.is_empty()) {
        sel.match_labels = None;
    }
    sel
}

fn normalize_network_policy_label_selectors_in_ingress(
    rules: Vec<k8s_openapi::api::networking::v1::NetworkPolicyIngressRule>,
) -> Vec<k8s_openapi::api::networking::v1::NetworkPolicyIngressRule> {
    rules
        .into_iter()
        .map(|mut rule| {
            if let Some(ref mut from) = rule.from {
                for peer in from.iter_mut() {
                    if let Some(sel) = peer.pod_selector.take() {
                        peer.pod_selector = Some(normalize_label_selector(sel));
                    }
                    if let Some(sel) = peer.namespace_selector.take() {
                        peer.namespace_selector = Some(normalize_label_selector(sel));
                    }
                }
            }
            rule
        })
        .collect()
}

fn normalize_network_policy_label_selectors_in_egress(
    rules: Vec<k8s_openapi::api::networking::v1::NetworkPolicyEgressRule>,
) -> Vec<k8s_openapi::api::networking::v1::NetworkPolicyEgressRule> {
    rules
        .into_iter()
        .map(|mut rule| {
            if let Some(ref mut to) = rule.to {
                for peer in to.iter_mut() {
                    if let Some(sel) = peer.pod_selector.take() {
                        peer.pod_selector = Some(normalize_label_selector(sel));
                    }
                    if let Some(sel) = peer.namespace_selector.take() {
                        peer.namespace_selector = Some(normalize_label_selector(sel));
                    }
                }
            }
            rule
        })
        .collect()
}
