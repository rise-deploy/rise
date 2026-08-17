//! Backend-agnostic deployment URL + image resolution.
//!
//! [`DeploymentUrlBuilder`] turns the ingress URL templates and registry
//! configuration into the concrete hostnames, URLs, and image references a
//! deployment is reachable at. It holds no Kubernetes (or Docker) types, so
//! both backends share one source of truth: the K8s `ResourceBuilder` composes
//! one of these and delegates its URL/image methods to it, and the Docker
//! controller holds one directly.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::warn;

use crate::backend::DeploymentUrls;
use crate::custom_domain::validate_custom_domain;
use crate::group::{normalize_deployment_group, DEFAULT_DEPLOYMENT_GROUP};
use crate::models::{CustomDomain, Deployment, Environment, Project};
use crate::providers::{ImageTagType, RegistryProvider};

/// Parsed ingress URL components.
#[derive(Debug, Clone)]
pub struct IngressUrl {
    pub host: String,
    pub path_prefix: Option<String>,
}

/// Holds the ingress-template + registry configuration needed to resolve a
/// deployment's hostnames, URLs, and image. Pure spec computation — no client.
#[derive(Clone)]
pub struct DeploymentUrlBuilder {
    pub production_ingress_url_template: String,
    pub staging_ingress_url_template: Option<String>,
    pub environment_ingress_url_template: Option<String>,
    pub ingress_port: Option<u16>,
    pub ingress_schema: String,
    pub registry_provider: Arc<dyn RegistryProvider>,
}

impl DeploymentUrlBuilder {
    // ── Naming helpers ─────────────────────────────────────────────────

    pub fn sanitize_label_value(value: &str) -> String {
        normalize_deployment_group(value)
    }

    pub fn escaped_group_name(deployment_group: &str) -> String {
        if deployment_group == DEFAULT_DEPLOYMENT_GROUP {
            "default".to_string()
        } else {
            Self::sanitize_label_value(deployment_group)
        }
    }

    // ── URL resolution ─────────────────────────────────────────────────

    pub fn parse_ingress_url(url: &str) -> IngressUrl {
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
        if deployment_group == DEFAULT_DEPLOYMENT_GROUP {
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
        } else if deployment_group != DEFAULT_DEPLOYMENT_GROUP {
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
        environment: &Environment,
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

    pub fn full_ingress_url_from_host(&self, url: &str) -> String {
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

    pub fn full_ingress_url(&self, project: &Project, deployment: &Deployment) -> String {
        let url = self.resolved_ingress_url(project, deployment);
        self.full_ingress_url_from_host(&url)
    }

    pub fn ingress_url_components(&self, project: &Project, deployment: &Deployment) -> IngressUrl {
        let url = self.resolved_ingress_url(project, deployment);
        Self::parse_ingress_url(&url)
    }

    /// Returns `true` if `host` would also be emitted as the env URL of some
    /// environment whose `primary_deployment_group` is *not* `deployment_group`.
    /// That's the cross-ingress collision case: a deployment-group's name
    /// happens to match an environment name, so its DG URL would steal a host
    /// that "belongs" to the env's primary group.
    pub fn host_conflicts_with_other_env(
        &self,
        project: &Project,
        deployment_group: &str,
        host: &str,
        all_environments: &[Environment],
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
        environment_for_group: Option<&Environment>,
        custom_domains_for_env: &[CustomDomain],
        include_custom_domains_inline: bool,
        all_environments: &[Environment],
    ) -> Vec<IngressUrl> {
        let mut hosts: Vec<IngressUrl> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

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
        custom_domains
            .iter()
            .filter(|domain| {
                match validate_custom_domain(
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
        environment: Option<&Environment>,
        all_environments: &[Environment],
        custom_domains: &[CustomDomain],
    ) -> DeploymentUrls {
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
        let mut seen: HashSet<String> = HashSet::new();
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

        DeploymentUrls {
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
    ) -> DeploymentUrls {
        let url_host = self.resolved_ingress_url_for_group(project, deployment_group);
        let full_host = self.full_ingress_url_from_host(&url_host);
        let default_url = format!("{}://{}", self.ingress_schema, full_host);

        let valid_custom_domains = self.filter_valid_custom_domains(custom_domains);

        // No environment context here — keep the historical "default group hosts custom
        // domains" rule to preserve preview-URL semantics for callers that haven't yet
        // adopted the env-aware path.
        let (custom_domain_urls, primary_url) = if deployment_group == DEFAULT_DEPLOYMENT_GROUP {
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
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(default_url.clone());
        for url in &custom_domain_urls {
            if seen.insert(url.clone()) {
                all_urls.push(url.clone());
            }
        }

        DeploymentUrls {
            default_url,
            primary_url,
            custom_domain_urls,
            all_urls,
        }
    }

    // ── Image resolution ───────────────────────────────────────────────

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
                ImageTagType::Internal,
            )
        }
    }
}
