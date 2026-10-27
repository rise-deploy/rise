//! A project's routing inputs and the one place they are turned into a
//! deployment group's hosts and URLs.
//!
//! A group's public hosts and its reported URLs both follow from two
//! project-wide lists: the project's environments and its custom domains.
//! [`ProjectRouting`] holds those lists, and every consumer goes through it:
//! the Docker and ECS reconcilers (loaded once per project pass and threaded
//! into each deployment's work, rather than re-read per deployment), the
//! Kubernetes sync webhook, and each backend's URL lookups for the HTTP API.

use anyhow::Result;
use uuid::Uuid;

use crate::backend::DeploymentUrls;
use crate::models::{CustomDomain, Environment, Project};
use crate::store::DeploymentStore;
use crate::url_builder::DeploymentUrlBuilder;

/// The project's environments and custom domains.
#[derive(Debug, Clone, Default)]
pub struct ProjectRouting {
    pub environments: Vec<Environment>,
    pub custom_domains: Vec<CustomDomain>,
}

impl ProjectRouting {
    /// Read both lists for `project_id` — one store call each.
    pub async fn load(store: &dyn DeploymentStore, project_id: Uuid) -> Result<Self> {
        Ok(Self {
            environments: store.list_environments_for_project(project_id).await?,
            custom_domains: store.list_project_custom_domains(project_id).await?,
        })
    }

    /// The project's environment `id`, e.g. a deployment's `environment_id`.
    pub fn environment(&self, id: Uuid) -> Option<&Environment> {
        self.environments.iter().find(|env| env.id == id)
    }

    /// The public hosts `deployment_group` is routed under (see
    /// [`DeploymentUrlBuilder::public_hosts_for_group`]).
    pub fn hosts_for_group(
        &self,
        url_builder: &DeploymentUrlBuilder,
        project: &Project,
        deployment_group: &str,
    ) -> Vec<String> {
        url_builder.public_hosts_for_group(
            project,
            deployment_group,
            &self.environments,
            &self.custom_domains,
        )
    }

    /// The URLs `deployment_group` — and so each of its deployments — is
    /// reachable at (see [`DeploymentUrlBuilder::urls_for_group`]).
    pub fn urls_for_group(
        &self,
        url_builder: &DeploymentUrlBuilder,
        project: &Project,
        deployment_group: &str,
    ) -> DeploymentUrls {
        url_builder.urls_for_group(
            project,
            deployment_group,
            &self.environments,
            &self.custom_domains,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::events::{EventSource, ForwardedEvent};
    use crate::models::{Deployment, DeploymentEnvVar, DeploymentStatus, TerminationReason};
    use crate::observation::{ContainerObservation, DerivedEvent};
    use crate::store::SupersessionOutcome;
    use crate::test_helpers;

    /// Serves fixed environment / custom-domain lists and counts how often
    /// each is read. Every other method is unreachable: loading the routing
    /// inputs must touch nothing else.
    #[derive(Default)]
    struct CountingStore {
        environments: Vec<Environment>,
        custom_domains: Vec<CustomDomain>,
        environment_reads: AtomicUsize,
        custom_domain_reads: AtomicUsize,
    }

    #[async_trait]
    impl DeploymentStore for CountingStore {
        async fn list_environments_for_project(&self, _: Uuid) -> Result<Vec<Environment>> {
            self.environment_reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.environments.clone())
        }
        async fn list_project_custom_domains(&self, _: Uuid) -> Result<Vec<CustomDomain>> {
            self.custom_domain_reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.custom_domains.clone())
        }

        async fn list_projects(&self, _: Option<Uuid>) -> Result<Vec<Project>> {
            unreachable!()
        }
        async fn find_project(&self, _: Uuid) -> Result<Option<Project>> {
            unreachable!()
        }
        async fn find_project_by_name(&self, _: &str) -> Result<Option<Project>> {
            unreachable!()
        }
        async fn list_active_projects(&self) -> Result<Vec<Project>> {
            unreachable!()
        }
        async fn update_project_calculated_status(&self, _: Uuid) -> Result<Project> {
            unreachable!()
        }
        async fn organization_uid_for_project(&self, _: Uuid) -> Result<Option<Uuid>> {
            unreachable!()
        }
        async fn find_environment(&self, _: Uuid) -> Result<Option<Environment>> {
            unreachable!()
        }
        async fn list_deployment_env_vars(&self, _: Uuid) -> Result<Vec<DeploymentEnvVar>> {
            unreachable!()
        }
        async fn find_deployment(&self, _: Uuid) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn list_non_terminal_deployments_for_project(
            &self,
            _: Uuid,
        ) -> Result<Vec<Deployment>> {
            unreachable!()
        }
        async fn list_active_deployments_for_project(&self, _: Uuid) -> Result<Vec<Deployment>> {
            unreachable!()
        }
        async fn find_non_terminal_deployments_for_project_and_group(
            &self,
            _: Uuid,
            _: &str,
        ) -> Result<Vec<Deployment>> {
            unreachable!()
        }
        async fn update_deployment_status(
            &self,
            _: Uuid,
            _: DeploymentStatus,
        ) -> Result<Deployment> {
            unreachable!()
        }
        async fn update_deployment_controller_metadata(
            &self,
            _: Uuid,
            _: &serde_json::Value,
        ) -> Result<Deployment> {
            unreachable!()
        }
        async fn mark_deployment_failed(&self, _: Uuid, _: &str) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn mark_deployment_cancelling(&self, _: Uuid) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn mark_deployment_cancelled(&self, _: Uuid) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn mark_deployment_stopped(&self, _: Uuid) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn mark_deployment_superseded(&self, _: Uuid) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn mark_deployment_expired(&self, _: Uuid) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn mark_deployment_healthy(&self, _: Uuid) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn mark_deployment_unhealthy(
            &self,
            _: Uuid,
            _: String,
        ) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn list_container_observations(&self, _: Uuid) -> Result<Vec<ContainerObservation>> {
            unreachable!()
        }
        async fn record_container_observations(
            &self,
            _: Uuid,
            _: EventSource,
            _: &[ContainerObservation],
            _: &[DerivedEvent],
        ) -> Result<()> {
            unreachable!()
        }
        async fn forward_backend_events(
            &self,
            _: Uuid,
            _: EventSource,
            _: &[ForwardedEvent],
        ) -> Result<u64> {
            unreachable!()
        }
        async fn mark_deployment_terminating(
            &self,
            _: Uuid,
            _: TerminationReason,
            _: Option<&str>,
        ) -> Result<Option<Deployment>> {
            unreachable!()
        }
        async fn mark_deployment_healthy_and_supersede(
            &self,
            _: Uuid,
            _: Uuid,
            _: &str,
        ) -> Result<SupersessionOutcome> {
            unreachable!()
        }
        async fn mark_deployment_as_active(&self, _: Uuid, _: Uuid, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn set_identity_credential_hash(&self, _: Uuid, _: &str) -> Result<()> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn one_load_serves_every_group_of_the_project() {
        let project = test_helpers::project("myapp", "public");
        let production = test_helpers::environment(&project, "production", Some("default"));
        let staging = test_helpers::environment(&project, "staging", Some("staging"));
        let store = CountingStore {
            custom_domains: vec![test_helpers::custom_domain(&production, "app.example.org")],
            environments: vec![production.clone(), staging.clone()],
            ..Default::default()
        };
        let url_builder = test_helpers::url_builder();

        let routing = ProjectRouting::load(&store, project.id).await.unwrap();

        for group in ["default", "staging", "mr/7"] {
            let hosts = routing.hosts_for_group(&url_builder, &project, group);
            let urls = routing.urls_for_group(&url_builder, &project, group);
            // A group's reported URLs are exactly the hosts it is routed under.
            assert_eq!(
                urls.all_urls,
                hosts
                    .iter()
                    .map(|host| format!("https://{host}"))
                    .collect::<Vec<_>>(),
                "group {group}"
            );
        }
        assert!(routing
            .hosts_for_group(&url_builder, &project, "default")
            .contains(&"app.example.org".to_string()));
        assert_eq!(
            routing.environment(staging.id).map(|env| env.name.as_str()),
            Some("staging")
        );

        // Three groups, two consumers each — still one read per list.
        assert_eq!(store.environment_reads.load(Ordering::SeqCst), 1);
        assert_eq!(store.custom_domain_reads.load(Ordering::SeqCst), 1);
    }
}
