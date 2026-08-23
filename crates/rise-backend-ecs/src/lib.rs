//! Amazon ECS deployment backend for Rise (Fargate launch type).
//!
//! Like the Docker backend — and unlike Kubernetes, which delegates to
//! Metacontroller — this backend owns an in-process reconcile loop
//! ([`reconciler::EcsReconciler`]) that converges the cluster with the database.
//! [`EcsBackend`] itself implements [`DeploymentBackend`] and serves only the
//! synchronous HTTP-handler operations: URL computation and a no-op environment
//! cleanup (the reconciler collects a deleted environment's services on its next
//! tick).
//!
//! The design is ADR-0004. The shape worth knowing before reading further:
//!
//! - **One ECS service per (deployment, container spec)**, one task-definition
//!   family per (project, group, container) with a revision per deploy. Two
//!   deployments of a group therefore run as two services, which is what lets a
//!   cutover overlap them behind one Traefik service.
//! - **Drift is answered by `UpdateService`**, not by destroying anything: ECS
//!   performs the rolling replacement itself, so there is no remove-then-create
//!   window (which would be far more painful here than on Docker, where a
//!   container starts in a second rather than tens of them).
//! - **Secrets live in SSM**, never in the task definition — see [`ssm`].
//! - **Routing is Traefik's ECS provider**, reading the `dockerLabels` the
//!   task-definition builder stamps, with readiness gated on Traefik's
//!   `serverStatus` exactly as on Docker.

pub mod client;
pub mod reconciler;
pub mod service;
pub mod sizing;
pub mod ssm;
pub mod tags;
pub mod task_definition;

use anyhow::Result;
use async_trait::async_trait;
use rise_backend_core::models::{Deployment, Project};
use rise_backend_core::{DeploymentBackend, DeploymentStore, DeploymentUrlBuilder, DeploymentUrls};
use std::sync::Arc;

/// Slim ECS backend wrapping an ECS client + `DeploymentUrlBuilder`.
///
/// Provides URL computation and environment cleanup for the HTTP handlers.
/// Reconciliation, health and termination run in [`reconciler::EcsReconciler`],
/// spawned at startup.
pub struct EcsBackend {
    ecs: aws_sdk_ecs::Client,
    cluster: String,
    url_builder: Arc<DeploymentUrlBuilder>,
    store: Arc<dyn DeploymentStore>,
}

impl EcsBackend {
    pub fn new(
        ecs: aws_sdk_ecs::Client,
        cluster: String,
        url_builder: Arc<DeploymentUrlBuilder>,
        store: Arc<dyn DeploymentStore>,
    ) -> Self {
        Self {
            ecs,
            cluster,
            url_builder,
            store,
        }
    }

    /// Verify the configured cluster exists and is reachable, so a
    /// misconfiguration surfaces at startup rather than as a silently idle
    /// reconcile loop.
    pub async fn test_connection(&self) -> Result<()> {
        let out = self
            .ecs
            .describe_clusters()
            .clusters(&self.cluster)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to reach the ECS API (cluster {:?}): {}",
                    self.cluster,
                    aws_error_detail(&e)
                )
            })?;

        let found = out
            .clusters()
            .iter()
            .any(|c| c.status().is_some_and(|s| s == "ACTIVE"));
        if !found {
            anyhow::bail!(
                "ECS cluster {:?} was not found or is not ACTIVE. Check \
                 `deployment_controller.cluster` and the configured region.",
                self.cluster
            );
        }
        Ok(())
    }
}

/// Fail startup when the ECR registry lives in a different AWS account from
/// the ECS cluster.
///
/// Rise creates ECR repositories with tags and scan-on-push only — it writes no
/// repository policy, and no ECR call names a `registryId`. Identity-based
/// permissions on the execution role are not sufficient for a cross-account
/// pull, so a mismatch here means every task fails with
/// `CannotPullContainerError` and there is no configuration that fixes it.
/// Worse, `registry.account_id` is used purely for string formatting, so the
/// mismatch is otherwise invisible: repositories get created in whichever
/// account the credentials belong to while every image reference points at the
/// configured one.
pub async fn verify_ecr_same_account(
    sts: &aws_sdk_sts::Client,
    registry_account_id: &str,
) -> Result<()> {
    let identity = sts.get_caller_identity().send().await.map_err(|e| {
        anyhow::anyhow!(
            "Failed to resolve the ECS credentials' AWS account via sts:GetCallerIdentity, \
             needed to check that registry.account_id ({registry_account_id}) matches: {}",
            aws_error_detail(&e)
        )
    })?;

    let Some(account) = identity.account() else {
        anyhow::bail!(
            "sts:GetCallerIdentity returned no account, so the ECS credentials' account \
             could not be compared against registry.account_id ({registry_account_id})"
        );
    };

    if account != registry_account_id {
        anyhow::bail!(
            "registry.account_id is {registry_account_id} but the ECS deployment \
             controller's credentials belong to account {account}. Cross-account ECR is \
             not supported: Rise writes no ECR repository policy, so the task execution \
             role in {account} cannot be granted a pull. Point both at the same account, \
             or host the registry where the cluster runs."
        );
    }
    Ok(())
}

/// Concise detail for an AWS SDK error: prefer the service's own message
/// (`ProvideErrorMetadata`) over the SDK's wrapper text, which otherwise reads
/// "service error" and buries the actual cause — the AWS equivalent of the
/// `ImagePullError` wrapper the Docker backend needs for the same reason.
pub(crate) fn aws_error_detail<E>(err: &E) -> String
where
    E: aws_sdk_ecs::error::ProvideErrorMetadata + std::fmt::Debug,
{
    match (err.code(), err.message()) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code.to_string(),
        (None, Some(message)) => message.to_string(),
        (None, None) => format!("{err:?}"),
    }
}

#[async_trait]
impl DeploymentBackend for EcsBackend {
    async fn get_deployment_urls(
        &self,
        deployment: &Deployment,
        project: &Project,
    ) -> Result<DeploymentUrls> {
        let environment = if let Some(env_id) = deployment.environment_id {
            self.store.find_environment(env_id).await?
        } else {
            None
        };
        let all_environments = self.store.list_environments_for_project(project.id).await?;
        let custom_domains = self.store.list_project_custom_domains(project.id).await?;

        Ok(self.url_builder.compute_deployment_urls(
            project,
            deployment,
            environment.as_ref(),
            &all_environments,
            &custom_domains,
        ))
    }

    async fn get_project_urls(
        &self,
        project: &Project,
        deployment_group: &str,
    ) -> Result<DeploymentUrls> {
        let custom_domains = self.store.list_project_custom_domains(project.id).await?;
        Ok(self
            .url_builder
            .compute_project_urls(project, deployment_group, &custom_domains))
    }

    async fn cleanup_environment(&self, project: &Project, environment_name: &str) -> Result<()> {
        // Services of a deleted environment fall out of the desired set and are
        // collected on the reconciler's next tick, so no direct cleanup here.
        tracing::debug!(
            project = %project.name,
            environment = %environment_name,
            "ECS backend: environment cleanup deferred to reconcile loop"
        );
        Ok(())
    }
}
