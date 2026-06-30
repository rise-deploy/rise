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
    LocalObjectReference, Namespace, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    ProjectedVolumeSource, ResourceRequirements, SeccompProfile, Secret, SecretEnvSource,
    SecurityContext, Service, ServiceAccount, ServiceAccountTokenProjection, ServicePort,
    ServiceSpec, Volume, VolumeMount, VolumeProjection,
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

/// Parsed ingress URL components
#[derive(Debug, Clone)]
struct IngressUrl {
    host: String,
    path_prefix: Option<String>,
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
    pub namespace_format: String,
}

/// Format a namespace name using the given format string and project name.
///
/// Standalone function for use in contexts where a `ResourceBuilder` instance
/// is not available (e.g., CRD backfill at startup).
pub fn format_namespace_name(format: &str, project_name: &str) -> String {
    format.replace("{project_name}", project_name)
}

impl ResourceBuilder {
    // ── Naming helpers ─────────────────────────────────────────────────

    pub fn namespace_name(&self, project: &Project) -> String {
        format_namespace_name(&self.namespace_format, &project.name)
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

    pub fn compute_deployment_urls(
        &self,
        project: &Project,
        deployment: &Deployment,
        environment: Option<&crate::db::models::Environment>,
        custom_domains: &[CustomDomain],
    ) -> super::controller::DeploymentUrls {
        let default_url_host = self.full_ingress_url(project, deployment);
        let default_url = format!("{}://{}", self.ingress_schema, default_url_host);

        let environment_url = environment
            .filter(|env| {
                env.primary_deployment_group.as_deref() == Some(&deployment.deployment_group)
            })
            .and_then(|env| {
                self.resolved_environment_url(project, env).map(|url_host| {
                    let full_host = self.full_ingress_url_from_host(&url_host);
                    format!("{}://{}", self.ingress_schema, full_host)
                })
            });

        let is_production_primary = environment
            .map(|env| {
                env.is_production
                    && env.primary_deployment_group.as_deref() == Some(&deployment.deployment_group)
            })
            .unwrap_or(
                deployment.deployment_group
                    == crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP,
            );

        let custom_domains = self.filter_valid_custom_domains(custom_domains);

        let (custom_domain_urls, primary_url) = if is_production_primary {
            let starred = custom_domains.iter().find(|d| d.is_primary);
            let primary = if let Some(starred) = starred {
                let host = if let Some(port) = self.ingress_port {
                    format!("{}:{}", starred.domain, port)
                } else {
                    starred.domain.clone()
                };
                format!("{}://{}", self.ingress_schema, host)
            } else {
                environment_url
                    .clone()
                    .unwrap_or_else(|| default_url.clone())
            };

            let urls: Vec<String> = custom_domains
                .iter()
                .map(|domain| {
                    let url_host = if let Some(port) = self.ingress_port {
                        format!("{}:{}", domain.domain, port)
                    } else {
                        domain.domain.clone()
                    };
                    format!("{}://{}", self.ingress_schema, url_host)
                })
                .collect();
            (urls, primary)
        } else {
            let primary = environment_url
                .clone()
                .unwrap_or_else(|| default_url.clone());
            (Vec::new(), primary)
        };

        super::controller::DeploymentUrls {
            default_url,
            primary_url,
            custom_domain_urls,
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

        let custom_domains = self.filter_valid_custom_domains(custom_domains);

        let (custom_domain_urls, primary_url) =
            if deployment_group == crate::server::deployment::models::DEFAULT_DEPLOYMENT_GROUP {
                let starred = custom_domains.iter().find(|d| d.is_primary);
                let primary = if let Some(starred) = starred {
                    let host = if let Some(port) = self.ingress_port {
                        format!("{}:{}", starred.domain, port)
                    } else {
                        starred.domain.clone()
                    };
                    format!("{}://{}", self.ingress_schema, host)
                } else {
                    default_url.clone()
                };

                let urls: Vec<String> = custom_domains
                    .iter()
                    .map(|domain| {
                        let url_host = if let Some(port) = self.ingress_port {
                            format!("{}:{}", domain.domain, port)
                        } else {
                            domain.domain.clone()
                        };
                        format!("{}://{}", self.ingress_schema, url_host)
                    })
                    .collect();
                (urls, primary)
            } else {
                (Vec::new(), default_url.clone())
            };

        super::controller::DeploymentUrls {
            default_url,
            primary_url,
            custom_domain_urls,
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

    // ── Resource spec builders ─────────────────────────────────────────

    pub fn create_namespace(&self, project: &Project) -> Namespace {
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
                name: Some(self.namespace_name(project)),
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
        entries: &[(&str, &RegistryCredentials)],
    ) -> anyhow::Result<Secret> {
        use base64::Engine;

        let mut auths = serde_json::Map::new();
        for (registry_host, credentials) in entries {
            // Empty credentials don't go into the dockerconfigjson — kubelet
            // would either reject them outright (empty auth header) or attempt
            // an anonymous pull regardless. Skipping them lets ambient cluster
            // auth (e.g. node-level pull secrets) take over for that host.
            if credentials.username.is_empty() && credentials.password.is_empty() {
                continue;
            }

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
            auths.insert((*registry_host).to_string(), auths_entry);
        }

        let docker_config = serde_json::json!({ "auths": serde_json::Value::Object(auths) });
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
        let config = self.health_probes.as_ref().cloned().unwrap_or_else(|| {
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
            ProbeType::Liveness => config.liveness_enabled,
            ProbeType::Readiness => config.readiness_enabled,
        };

        if !enabled {
            return None;
        }

        let path = if config.path.is_empty() || !config.path.starts_with('/') {
            warn!(
                "Invalid health probe path '{}', using default '/'",
                config.path
            );
            "/".to_string()
        } else {
            config.path.clone()
        };

        Some(Probe {
            http_get: Some(HTTPGetAction {
                path: Some(path),
                port: k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(port),
                scheme: Some("HTTP".to_string()),
                ..Default::default()
            }),
            initial_delay_seconds: Some(config.initial_delay_seconds),
            period_seconds: Some(config.period_seconds),
            timeout_seconds: Some(config.timeout_seconds),
            failure_threshold: Some(config.failure_threshold),
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
    ) -> K8sDeployment {
        let volumes = self
            .create_extra_service_token_volume()
            .map(|volume| vec![volume]);
        let volume_mounts = self
            .create_extra_service_token_volume_mount()
            .map(|mount| vec![mount]);

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
                            // Pod's imagePullSecrets is a list. We add up to two:
                            //
                            // 1. The explicitly-configured `image_pull_secret_name`
                            //    (typically an externally-managed Secret like an
                            //    ESO ClusterExternalSecret), if set.
                            // 2. The controller-minted IMAGE_PULL_SECRET_NAME
                            //    when the provider needs it (`requires_pull_secret()`).
                            //
                            // Both can be present simultaneously — e.g. ECR provider
                            // mints scoped-per-project ECR creds AND the operator
                            // points at an ESO-managed JFrog secret. kubelet picks
                            // the right one based on the image's host.
                            let mut secrets: Vec<LocalObjectReference> = Vec::new();
                            if let Some(name) = self.image_pull_secret_name.as_deref() {
                                secrets.push(LocalObjectReference {
                                    name: name.to_string(),
                                });
                            }
                            if self.registry_provider.requires_pull_secret() {
                                secrets.push(LocalObjectReference {
                                    name: IMAGE_PULL_SECRET_NAME.to_string(),
                                });
                            }
                            (!secrets.is_empty()).then_some(secrets)
                        },
                        containers: vec![Container {
                            name: "app".to_string(),
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

        paths
    }

    pub fn create_primary_ingress(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        environment_name: Option<&str>,
    ) -> anyhow::Result<Ingress> {
        let url_components = self.ingress_url_components(project, deployment);

        let mut annotations = self.build_ingress_annotations(project)?;

        if let Some(ref path) = url_components.path_prefix {
            annotations.insert(
                "nginx.ingress.kubernetes.io/rewrite-target".to_string(),
                "/$2".to_string(),
            );
            annotations.insert(
                "nginx.ingress.kubernetes.io/x-forwarded-prefix".to_string(),
                path.trim_end_matches('/').to_string(),
            );
        }

        let (ingress_path, path_type) = if let Some(ref path) = url_components.path_prefix {
            let pattern = format!("{}(/|$)(.*)", path.trim_end_matches('/'));
            (pattern, "ImplementationSpecific")
        } else {
            ("/".to_string(), "Prefix")
        };

        let service_name = Self::service_name(project, deployment);
        let primary_paths = self.build_ingress_paths(&service_name, &ingress_path, path_type);

        let rules = vec![IngressRule {
            host: Some(url_components.host.clone()),
            http: Some(HTTPIngressRuleValue {
                paths: primary_paths,
            }),
        }];

        let tls = self.build_primary_tls_config(&url_components.host);

        Ok(Ingress {
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
        })
    }

    pub fn create_custom_domain_ingress(
        &self,
        project: &Project,
        deployment: &Deployment,
        namespace: &str,
        custom_domains: &[CustomDomain],
        environment_name: Option<&str>,
    ) -> anyhow::Result<Ingress> {
        let mut annotations = self.build_ingress_annotations(project)?;

        for (k, v) in &self.custom_domain_ingress_annotations {
            annotations.insert(k.clone(), v.clone());
        }

        let service_name = Self::service_name(project, deployment);

        let mut rules = Vec::new();
        for domain in custom_domains {
            let paths = self.build_ingress_paths(&service_name, "/", "Prefix");
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
        primary_host: &str,
    ) -> Option<Vec<k8s_openapi::api::networking::v1::IngressTLS>> {
        let shared_secret = self.ingress_tls_secret_name.as_ref()?;
        Some(vec![k8s_openapi::api::networking::v1::IngressTLS {
            hosts: Some(vec![primary_host.to_string()]),
            secret_name: Some(shared_secret.clone()),
        }])
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
    ///
    /// Priority: `image_digest`, then `image_path`, then reconstruct via
    /// `registry_provider.get_image_tag` (fallback for rows persisted before
    /// `image_path` existed).
    pub fn resolve_image(
        &self,
        project: &Project,
        deployment: &Deployment,
        source_deployment_id: Option<&str>,
    ) -> String {
        if let Some(ref image_digest) = deployment.image_digest {
            return image_digest.clone();
        }

        if let Some(ref image_path) = deployment.image_path {
            return image_path.clone();
        }

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
            _tag: &str,
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
            namespace_format: "{project_name}".to_string(),
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
            image_path: None,
            rolled_back_from_deployment_id: None,
            http_port: 8080,
            needs_reconcile: false,
            is_active: true,
            deploying_started_at: None,
            first_healthy_at: None,
            job_url: None,
            pull_request_url: None,
            replicas: 1,
            cpu: "500m".to_string(),
            memory: "256Mi".to_string(),
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

    /// Opt-in JFrog wiring on rise-dev: ECR provider still mints scoped
    /// per-project ECR creds (controller-minted IMAGE_PULL_SECRET_NAME) AND
    /// the operator references an externally-managed JFrog secret via
    /// `image_pull_secret_name`. Pod must list both so kubelet can pull from
    /// either registry depending on the image's host.
    #[test]
    fn image_pull_secrets_include_minted_alongside_explicit() {
        let d = make_deployment(Some("eso-managed-primary"), true);
        let names: Vec<&str> = pod_spec_from_deployment(&d)
            .image_pull_secrets
            .as_ref()
            .expect("expected both explicit and minted references")
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["eso-managed-primary", IMAGE_PULL_SECRET_NAME]);
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
