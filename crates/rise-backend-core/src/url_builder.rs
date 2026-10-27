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

use crate::backend::DeploymentUrls;
use crate::custom_domain::validate_custom_domain;
use crate::group::{normalize_deployment_group, DEFAULT_DEPLOYMENT_GROUP};
use crate::log_once::warn_once;
use crate::models::{CustomDomain, Deployment, Environment, Project};
use crate::providers::{ImageTagType, RegistryProvider};

/// A deployment group's environment, that environment's valid custom
/// domains, and the hosts the group is routed under.
struct GroupRouting<'a> {
    environment: Option<&'a Environment>,
    custom_domains: Vec<CustomDomain>,
    hosts: Vec<String>,
}

/// Holds the ingress-template + registry configuration needed to resolve a
/// deployment's hostnames, URLs, and image. Pure spec computation — no client.
///
/// Every ingress template resolves to a bare hostname: settings validation
/// rejects templates with a path, so each app owns its host outright.
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

    /// `host` with the configured ingress port appended, if any.
    pub fn host_with_port(&self, host: &str) -> String {
        match self.ingress_port {
            Some(port) => format!("{}:{}", host, port),
            None => host.to_string(),
        }
    }

    pub fn full_ingress_url(&self, project: &Project, deployment: &Deployment) -> String {
        let host = self.resolved_ingress_url(project, deployment);
        self.host_with_port(&host)
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
            self.resolved_environment_url(project, env).as_deref() == Some(host)
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
    ) -> Vec<String> {
        let mut hosts: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        if let Some(dg_host) = self.resolved_deployment_group_url(project, deployment_group) {
            if self.host_conflicts_with_other_env(
                project,
                deployment_group,
                &dg_host,
                all_environments,
            ) {
                warn_once!(
                    format!("dg-host-suppressed:{}:{}:{}", project.id, deployment_group, parsed.host),
                    project = %project.name,
                    deployment_group = %deployment_group,
                    host = %dg_host,
                    "Suppressing deployment-group URL: collides with env URL of an environment whose primary deployment group is different"
                );
            } else if seen.insert(dg_host.clone()) {
                hosts.push(dg_host);
            }
        }

        if let Some(env) = environment_for_group {
            if let Some(env_host) = self.resolved_environment_url(project, env) {
                if seen.insert(env_host.clone()) {
                    hosts.push(env_host);
                }
            }
        }

        if include_custom_domains_inline {
            for cd in custom_domains_for_env {
                if seen.insert(cd.domain.clone()) {
                    hosts.push(cd.domain.clone());
                }
            }
        }

        hosts
    }

    /// The environment whose `primary_deployment_group` is `deployment_group`.
    fn environment_for_group<'a>(
        deployment_group: &str,
        all_environments: &'a [Environment],
    ) -> Option<&'a Environment> {
        all_environments
            .iter()
            .find(|env| env.primary_deployment_group.as_deref() == Some(deployment_group))
    }

    /// `environment`'s custom domains that pass [`Self::is_valid_custom_domain`].
    fn valid_domains_for_environment(
        &self,
        environment: Option<&Environment>,
        custom_domains: &[CustomDomain],
    ) -> Vec<CustomDomain> {
        let Some(environment) = environment else {
            return Vec::new();
        };
        custom_domains
            .iter()
            .filter(|domain| {
                domain.environment_id == environment.id && self.is_valid_custom_domain(domain)
            })
            .cloned()
            .collect()
    }

    /// The public hosts `deployment_group`'s active deployment is routed
    /// under: [`Self::primary_ingress_hosts`] with the environment whose
    /// `primary_deployment_group` is this group, and only that environment's
    /// valid custom domains, so a production custom domain never lands on a
    /// preview group's route. Unlike Kubernetes, there is no fallback host
    /// when none applies.
    pub fn public_hosts_for_group(
        &self,
        project: &Project,
        deployment_group: &str,
        all_environments: &[Environment],
        custom_domains: &[CustomDomain],
    ) -> Vec<String> {
        self.group_routing(project, deployment_group, all_environments, custom_domains)
            .hosts
    }

    /// The one derivation behind both [`Self::public_hosts_for_group`] and
    /// [`Self::urls_for_group`], so a group's routes and reported URLs agree.
    fn group_routing<'a>(
        &self,
        project: &Project,
        deployment_group: &str,
        all_environments: &'a [Environment],
        custom_domains: &[CustomDomain],
    ) -> GroupRouting<'a> {
        let environment = Self::environment_for_group(deployment_group, all_environments);
        let custom_domains = self.valid_domains_for_environment(environment, custom_domains);
        let hosts = self.primary_ingress_hosts(
            project,
            deployment_group,
            environment,
            &custom_domains,
            true,
            all_environments,
        );
        GroupRouting {
            environment,
            custom_domains,
            hosts,
        }
    }

    pub fn filter_valid_custom_domains(
        &self,
        custom_domains: &[CustomDomain],
    ) -> Vec<CustomDomain> {
        custom_domains
            .iter()
            .filter(|domain| self.is_valid_custom_domain(domain))
            .cloned()
            .collect()
    }

    /// Whether `domain` stays clear of the ingress URL templates; logs why not.
    fn is_valid_custom_domain(&self, domain: &CustomDomain) -> bool {
        match validate_custom_domain(
            &domain.domain,
            &self.production_ingress_url_template,
            self.staging_ingress_url_template.as_deref(),
            self.environment_ingress_url_template.as_deref(),
            None,
        ) {
            Ok(()) => true,
            Err(reason) => {
                warn_once!(
                    format!("custom-domain-ignored:{}:{}", domain.id, domain.domain),
                    domain_id = %domain.id,
                    project_id = %domain.project_id,
                    domain = %domain.domain,
                    "Ignoring custom domain that conflicts with project default domain pattern: {}",
                    reason
                );
                false
            }
        }
    }

    // ── Deployment URLs ────────────────────────────────────────────────

    fn host_to_url(&self, host: &str) -> String {
        format!("{}://{}", self.ingress_schema, self.host_with_port(host))
    }

    /// The URLs `deployment_group` is reachable at: its default template URL,
    /// its primary URL, its environment's custom domains and every URL it is
    /// routed under.
    ///
    /// Everything derives from the group — the environment whose
    /// `primary_deployment_group` it is, and that environment's custom domains
    /// — exactly as [`Self::public_hosts_for_group`] derives the routed hosts,
    /// so a deployment's reported URLs and its routes always agree. A
    /// deployment's URLs are its group's.
    pub fn urls_for_group(
        &self,
        project: &Project,
        deployment_group: &str,
        all_environments: &[Environment],
        custom_domains: &[CustomDomain],
    ) -> DeploymentUrls {
        let host = self.resolved_ingress_url_for_group(project, deployment_group);
        let default_url = self.host_to_url(&host);

        let GroupRouting {
            environment,
            custom_domains: domains_for_env,
            hosts,
        } = self.group_routing(project, deployment_group, all_environments, custom_domains);

        let environment_url = environment.and_then(|env| {
            self.resolved_environment_url(project, env)
                .map(|env_host| self.host_to_url(&env_host))
        });

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

        // Exactly the hosts the group is routed under, as URLs.
        let all_urls: Vec<String> = hosts.iter().map(|host| self.host_to_url(host)).collect();

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

#[cfg(test)]
mod tests {
    use crate::test_helpers::{custom_domain, environment, project, url_builder};

    #[test]
    fn public_hosts_scope_custom_domains_to_the_groups_environment() {
        let builder = url_builder();
        let project = project("docs-app", "private");
        let production = environment(&project, "production", Some("default"));
        let staging = environment(&project, "staging", Some("main"));
        let environments = vec![production.clone(), staging.clone()];
        let domains = vec![
            custom_domain(&production, "docs.customer.com"),
            // Matches the production template: stale, never routed.
            custom_domain(&production, "other-app.example.test"),
            custom_domain(&staging, "staging.customer.com"),
        ];

        assert_eq!(
            builder.public_hosts_for_group(&project, "default", &environments, &domains),
            vec![
                "docs-app-default.preview.example.test",
                "docs-app.example.test",
                "docs.customer.com",
            ]
        );
        assert_eq!(
            builder.public_hosts_for_group(&project, "main", &environments, &domains),
            vec![
                "docs-app-main.preview.example.test",
                "docs-app-staging.example.test",
                "staging.customer.com",
            ]
        );
        assert_eq!(
            builder.public_hosts_for_group(&project, "pr/123", &environments, &domains),
            vec!["docs-app-pr--123.preview.example.test"]
        );
    }
}
