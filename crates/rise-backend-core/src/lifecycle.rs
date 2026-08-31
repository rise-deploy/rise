//! The deployment status-transition state machine shared by every backend.
//!
//! `perform_status_transition`, `complete_termination` and
//! `handle_deployment_became_healthy` used to be copied — near-verbatim, drifting
//! a little more with each copy — into the Kubernetes webhook, the Docker
//! reconciler and the ECS reconciler. None of it depends on how a backend runs
//! workloads: it is pure [`DeploymentStore`] bookkeeping (timeouts, expiry,
//! cancellation, termination, activation/supersession), so it lives here once.
//!
//! What deliberately stays OUT of this module, and duplicated per backend: the
//! runtime-observed readiness/health loop (K8s pod status, Docker container
//! inspection, ECS task/`serverStatus` polling) that *decides* a deployment is
//! healthy. Those diverge on genuine runtime semantics. This module only
//! covers what happens once that decision has been made, or needs no runtime
//! observation at all.

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use tracing::{info, warn};

use crate::models::{Deployment, DeploymentStatus, Project, TerminationReason};
use crate::runtime::{DEPLOYING_TIMEOUT_MINUTES, PRE_PUSHED_TIMEOUT_MINUTES};
use crate::state_machine;
use crate::store::{DeploymentStore, SupersessionOutcome};

/// Backend-specific cleanup run when a deployment is superseded by a newly
/// healthy one in the same group. Default no-op.
///
/// The only implementor today is the ECS backend, which eagerly deletes the
/// superseded deployment's SSM parameters here — a later rollback must
/// re-create them from the database rather than read a stale value. Docker
/// and Kubernetes have no equivalent out-of-band state to clean up eagerly;
/// their storage (a container's own environment, a K8s Secret) is retired
/// through the ordinary diff/GC path instead.
#[async_trait]
pub trait SupersededHook: Send + Sync {
    async fn on_superseded(&self, project: &Project, superseded: &Deployment) -> Result<()>;
}

/// Drive the time- and state-based transitions that do not depend on the
/// runtime: pre-push and deploying timeouts, expiry, cancellation, and
/// termination completion. Identical in shape and thresholds on every
/// backend, so a deployment's lifecycle reads the same regardless of which
/// one runs it.
///
/// Deliberately does NOT touch `Deploying`/`Healthy`/`Unhealthy` beyond the
/// deploying-timeout and expiry checks: deciding whether a deployment IS
/// healthy is each backend's own runtime-observed readiness loop, called
/// separately (see the module docs).
pub async fn perform_status_transition(
    store: &dyn DeploymentStore,
    project: &Project,
    deployment: &Deployment,
) -> Result<()> {
    let now = Utc::now();

    match deployment.status {
        DeploymentStatus::Pending | DeploymentStatus::Building | DeploymentStatus::Pushing => {
            // The CLI drives these; only time them out.
            let elapsed = now.signed_duration_since(deployment.created_at);
            if elapsed > chrono::Duration::minutes(PRE_PUSHED_TIMEOUT_MINUTES) {
                let msg = format!(
                    "Deployment timed out after {} minutes in {} state. This usually \
                     indicates the CLI was interrupted during build/push.",
                    PRE_PUSHED_TIMEOUT_MINUTES, deployment.status
                );
                warn!(deployment_id = %deployment.deployment_id, "{}", msg);
                if store
                    .mark_deployment_failed(deployment.id, &msg)
                    .await?
                    .is_some()
                {
                    store.update_project_calculated_status(project.id).await?;
                }
            }
        }
        DeploymentStatus::Cancelling => {
            info!(
                deployment_id = %deployment.deployment_id,
                "Cancelling deployment — marking as Cancelled"
            );
            if store
                .mark_deployment_cancelled(deployment.id)
                .await?
                .is_some()
            {
                store.update_project_calculated_status(project.id).await?;
            }
        }
        DeploymentStatus::Terminating => {
            complete_termination(store, project, deployment).await?;
        }
        DeploymentStatus::Pushed => {
            info!(
                deployment_id = %deployment.deployment_id,
                "Deployment image pushed, transitioning to Deploying"
            );
            store
                .update_deployment_status(deployment.id, DeploymentStatus::Deploying)
                .await?;
            store.update_project_calculated_status(project.id).await?;
        }
        DeploymentStatus::Deploying => {
            if let Some(started) = deployment.deploying_started_at {
                let elapsed = now.signed_duration_since(started);
                if elapsed > chrono::Duration::minutes(DEPLOYING_TIMEOUT_MINUTES) {
                    let msg = format!(
                        "Deployment timed out after {} seconds in Deploying state",
                        elapsed.num_seconds()
                    );
                    warn!(deployment_id = %deployment.deployment_id, "{}", msg);
                    if store
                        .mark_deployment_failed(deployment.id, &msg)
                        .await?
                        .is_some()
                    {
                        store.update_project_calculated_status(project.id).await?;
                    }
                }
            }
        }
        _ => {}
    }

    // Expiration applies to every non-terminal deployment, whatever the match
    // above did. Reads `deployment.status` as it was on entry — a deployment
    // this call just moved to Terminating/Cancelling above is still excluded,
    // since a fresh Terminating/Cancelling instance never carries `expires_at`
    // as the reason it's terminating.
    if let Some(expires_at) = deployment.expires_at {
        if expires_at <= now
            && !matches!(
                deployment.status,
                DeploymentStatus::Terminating | DeploymentStatus::Cancelling
            )
        {
            info!(
                deployment_id = %deployment.deployment_id,
                "Deployment has expired, marking as Terminating"
            );
            if store
                .mark_deployment_terminating(deployment.id, TerminationReason::Expired, None)
                .await?
                .is_some()
            {
                store.update_project_calculated_status(project.id).await?;
            }
        }
    }

    Ok(())
}

/// Move a `Terminating` deployment to the terminal state its
/// `termination_reason` names.
pub async fn complete_termination(
    store: &dyn DeploymentStore,
    project: &Project,
    deployment: &Deployment,
) -> Result<()> {
    let applied = match deployment.termination_reason {
        Some(TerminationReason::Superseded) => {
            store.mark_deployment_superseded(deployment.id).await?
        }
        Some(TerminationReason::UserStopped) => {
            store.mark_deployment_stopped(deployment.id).await?
        }
        Some(TerminationReason::Expired) => store.mark_deployment_expired(deployment.id).await?,
        Some(TerminationReason::Failed) => {
            store
                .mark_deployment_failed(
                    deployment.id,
                    deployment
                        .error_message
                        .as_deref()
                        .unwrap_or("Deployment failed"),
                )
                .await?
        }
        Some(TerminationReason::Cancelled) => {
            store.mark_deployment_cancelled(deployment.id).await?
        }
        None => {
            // Missing termination reason resolves to Stopped.
            store.mark_deployment_stopped(deployment.id).await?
        }
    }
    .is_some();

    if applied {
        store.update_project_calculated_status(project.id).await?;
    }
    Ok(())
}

/// Mark a deployment healthy, supersede the group's previous active
/// deployment (and any other stragglers), and mark this one active.
///
/// `hook` runs (best-effort — its error is logged, never propagated) right
/// after the previous active deployment is marked Terminating. Pass `None`
/// where a backend has no such cleanup.
pub async fn handle_deployment_became_healthy(
    store: &dyn DeploymentStore,
    hook: Option<&dyn SupersededHook>,
    project: &Project,
    deployment: &Deployment,
) -> Result<()> {
    // Atomically: mark this deployment Healthy, and if some other deployment
    // is currently the group's active (Healthy) one, mark it Terminating in
    // the same database transaction. A crash between "mark new healthy" and
    // "mark old terminating" can then never leave both non-terminal.
    let outcome: SupersessionOutcome = store
        .mark_deployment_healthy_and_supersede(
            deployment.id,
            project.id,
            &deployment.deployment_group,
        )
        .await?;

    if !outcome.became_healthy {
        // The deployment moved to a protected status (e.g. a concurrent stop
        // request already moved it to Terminating) between the reconciler's
        // readiness read and this call. A routine "became healthy"
        // transition must not resurrect or override that decision.
        return Ok(());
    }

    if let Some(old_active) = outcome.superseded {
        info!(
            "Deployment {} replacing {} in group '{}', marking old as Terminating",
            deployment.deployment_id, old_active.deployment_id, deployment.deployment_group
        );
        if let Some(hook) = hook {
            if let Err(e) = hook.on_superseded(project, &old_active).await {
                warn!(
                    deployment_id = %old_active.deployment_id,
                    "Post-supersession cleanup failed: {:?}", e
                );
            }
        }
    }

    // Clean up other active (Healthy/Unhealthy) deployments in the group. On
    // the status, not on `is_active`: the flag is set in the write below, so a
    // replica that died between marking a deployment healthy and flagging it
    // active would otherwise leave a sibling that is serving traffic and
    // would never be retired. Self-healing: this also catches an old-active
    // deployment currently sitting in `Unhealthy` (not covered by the atomic
    // write above, which only supersedes a `Healthy` predecessor) the next
    // time some deployment in the group becomes healthy.
    let others = store
        .find_non_terminal_deployments_for_project_and_group(
            project.id,
            &deployment.deployment_group,
        )
        .await?;
    for other in others {
        if other.id != deployment.id
            && state_machine::is_active(&other.status)
            && !state_machine::is_terminal(&other.status)
        {
            info!(
                "Cleaning up non-active deployment {} in group '{}', marking as Terminating",
                other.deployment_id, deployment.deployment_group
            );
            store
                .mark_deployment_terminating(
                    other.id,
                    TerminationReason::Superseded,
                    Some(deployment.deployment_id.as_str()),
                )
                .await?;
        }
    }

    store
        .mark_deployment_as_active(deployment.id, project.id, &deployment.deployment_group)
        .await?;
    store.update_project_calculated_status(project.id).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ProjectStatus;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use uuid::Uuid;

    fn project() -> Project {
        let now = Utc::now();
        Project {
            id: Uuid::new_v4(),
            name: "myapp".to_string(),
            status: ProjectStatus::Running,
            access_class: "public".to_string(),
            owner_user_id: None,
            owner_team_id: None,
            finalizers: Vec::new(),
            source_url: None,
            template_id: None,
            template_image: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn deployment(status: DeploymentStatus) -> Deployment {
        let now = Utc::now();
        Deployment {
            id: Uuid::new_v4(),
            deployment_id: "20260101-120000".to_string(),
            project_id: Uuid::new_v4(),
            created_by_id: Uuid::nil(),
            status,
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
            is_active: false,
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
            containers: None,
            routes: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// A minimal in-memory `DeploymentStore` recording every call, just
    /// enough to assert the transition logic without a database.
    #[derive(Default)]
    struct FakeStore {
        calls: Mutex<Vec<String>>,
        active_in_group: Mutex<HashMap<(Uuid, String), Deployment>>,
        siblings_in_group: Mutex<HashMap<(Uuid, String), Vec<Deployment>>>,
        /// When set, `mark_deployment_healthy_and_supersede` reports
        /// `became_healthy: false` without touching `calls` — simulating a
        /// deployment that already moved to a protected status.
        reject_healthy: Mutex<bool>,
    }

    #[async_trait]
    impl DeploymentStore for FakeStore {
        async fn list_projects(&self, _owner_user_id: Option<Uuid>) -> Result<Vec<Project>> {
            unimplemented!()
        }
        async fn find_project(&self, _id: Uuid) -> Result<Option<Project>> {
            unimplemented!()
        }
        async fn find_project_by_name(&self, _name: &str) -> Result<Option<Project>> {
            unimplemented!()
        }
        async fn list_active_projects(&self) -> Result<Vec<Project>> {
            unimplemented!()
        }
        async fn update_project_calculated_status(&self, _project_id: Uuid) -> Result<Project> {
            self.calls
                .lock()
                .unwrap()
                .push("update_project_status".to_string());
            Ok(project())
        }
        async fn organization_uid_for_project(&self, _project_id: Uuid) -> Result<Option<Uuid>> {
            unimplemented!()
        }
        async fn find_environment(&self, _id: Uuid) -> Result<Option<crate::models::Environment>> {
            unimplemented!()
        }
        async fn list_environments_for_project(
            &self,
            _project_id: Uuid,
        ) -> Result<Vec<crate::models::Environment>> {
            unimplemented!()
        }
        async fn list_project_custom_domains(
            &self,
            _project_id: Uuid,
        ) -> Result<Vec<crate::models::CustomDomain>> {
            unimplemented!()
        }
        async fn list_deployment_env_vars(
            &self,
            _deployment_id: Uuid,
        ) -> Result<Vec<crate::models::DeploymentEnvVar>> {
            unimplemented!()
        }
        async fn find_deployment(&self, _id: Uuid) -> Result<Option<Deployment>> {
            unimplemented!()
        }
        async fn list_non_terminal_deployments_for_project(
            &self,
            _project_id: Uuid,
        ) -> Result<Vec<Deployment>> {
            unimplemented!()
        }
        async fn find_non_terminal_deployments_for_project_and_group(
            &self,
            project_id: Uuid,
            group: &str,
        ) -> Result<Vec<Deployment>> {
            Ok(self
                .siblings_in_group
                .lock()
                .unwrap()
                .get(&(project_id, group.to_string()))
                .cloned()
                .unwrap_or_default())
        }
        async fn update_deployment_status(
            &self,
            _id: Uuid,
            status: DeploymentStatus,
        ) -> Result<Deployment> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("update_status:{status}"));
            Ok(deployment(status))
        }
        async fn update_deployment_controller_metadata(
            &self,
            _id: Uuid,
            _metadata: &serde_json::Value,
        ) -> Result<Deployment> {
            unimplemented!()
        }
        async fn mark_deployment_failed(
            &self,
            _id: Uuid,
            error_message: &str,
        ) -> Result<Option<Deployment>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("failed:{error_message}"));
            Ok(Some(deployment(DeploymentStatus::Failed)))
        }
        async fn mark_deployment_cancelling(&self, _id: Uuid) -> Result<Option<Deployment>> {
            unimplemented!()
        }
        async fn mark_deployment_cancelled(&self, _id: Uuid) -> Result<Option<Deployment>> {
            self.calls.lock().unwrap().push("cancelled".to_string());
            Ok(Some(deployment(DeploymentStatus::Cancelled)))
        }
        async fn mark_deployment_stopped(&self, _id: Uuid) -> Result<Option<Deployment>> {
            self.calls.lock().unwrap().push("stopped".to_string());
            Ok(Some(deployment(DeploymentStatus::Stopped)))
        }
        async fn mark_deployment_superseded(&self, _id: Uuid) -> Result<Option<Deployment>> {
            self.calls.lock().unwrap().push("superseded".to_string());
            Ok(Some(deployment(DeploymentStatus::Superseded)))
        }
        async fn mark_deployment_expired(&self, _id: Uuid) -> Result<Option<Deployment>> {
            self.calls.lock().unwrap().push("expired".to_string());
            Ok(Some(deployment(DeploymentStatus::Expired)))
        }
        async fn mark_deployment_healthy(&self, _id: Uuid) -> Result<Option<Deployment>> {
            self.calls.lock().unwrap().push("healthy".to_string());
            Ok(Some(deployment(DeploymentStatus::Healthy)))
        }
        async fn mark_deployment_unhealthy(
            &self,
            _id: Uuid,
            _reason: String,
        ) -> Result<Option<Deployment>> {
            unimplemented!()
        }
        async fn list_container_observations(
            &self,
            _deployment_id: Uuid,
        ) -> Result<Vec<crate::observation::ContainerObservation>> {
            Ok(Vec::new())
        }

        async fn record_container_observations(
            &self,
            _deployment_id: Uuid,
            _source: crate::events::EventSource,
            _observations: &[crate::observation::ContainerObservation],
            _events: &[crate::observation::DerivedEvent],
        ) -> Result<()> {
            Ok(())
        }

        async fn mark_deployment_terminating(
            &self,
            id: Uuid,
            reason: TerminationReason,
            _superseded_by: Option<&str>,
        ) -> Result<Option<Deployment>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("terminating:{id}:{reason:?}"));
            let mut d = deployment(DeploymentStatus::Terminating);
            d.id = id;
            d.termination_reason = Some(reason);
            Ok(Some(d))
        }
        async fn mark_deployment_healthy_and_supersede(
            &self,
            deployment_id: Uuid,
            project_id: Uuid,
            deployment_group: &str,
        ) -> Result<SupersessionOutcome> {
            if *self.reject_healthy.lock().unwrap() {
                return Ok(SupersessionOutcome {
                    became_healthy: false,
                    superseded: None,
                });
            }
            self.calls.lock().unwrap().push("healthy".to_string());
            let superseded = self
                .active_in_group
                .lock()
                .unwrap()
                .get(&(project_id, deployment_group.to_string()))
                .filter(|d| d.id != deployment_id && d.status == DeploymentStatus::Healthy)
                .cloned();
            if let Some(old) = &superseded {
                self.calls.lock().unwrap().push(format!(
                    "terminating:{}:{:?}",
                    old.id,
                    TerminationReason::Superseded
                ));
            }
            Ok(SupersessionOutcome {
                became_healthy: true,
                superseded,
            })
        }
        async fn mark_deployment_as_active(
            &self,
            _deployment_id: Uuid,
            _project_id: Uuid,
            _deployment_group: &str,
        ) -> Result<()> {
            self.calls.lock().unwrap().push("mark_active".to_string());
            Ok(())
        }
        async fn set_identity_credential_hash(&self, _id: Uuid, _hash: &str) -> Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn pending_past_the_timeout_is_marked_failed() {
        let store = FakeStore::default();
        let project = project();
        let mut d = deployment(DeploymentStatus::Pending);
        d.created_at = Utc::now() - chrono::Duration::minutes(PRE_PUSHED_TIMEOUT_MINUTES + 1);

        perform_status_transition(&store, &project, &d)
            .await
            .unwrap();

        let calls = store.calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.starts_with("failed:")));
    }

    #[tokio::test]
    async fn pending_within_the_timeout_is_left_alone() {
        let store = FakeStore::default();
        let project = project();
        let d = deployment(DeploymentStatus::Pending); // created_at = now

        perform_status_transition(&store, &project, &d)
            .await
            .unwrap();

        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pushed_transitions_to_deploying() {
        let store = FakeStore::default();
        let project = project();
        let d = deployment(DeploymentStatus::Pushed);

        perform_status_transition(&store, &project, &d)
            .await
            .unwrap();

        let calls = store.calls.lock().unwrap();
        assert!(calls.contains(&"update_status:Deploying".to_string()));
    }

    #[tokio::test]
    async fn cancelling_is_marked_cancelled() {
        let store = FakeStore::default();
        let project = project();
        let d = deployment(DeploymentStatus::Cancelling);

        perform_status_transition(&store, &project, &d)
            .await
            .unwrap();

        assert!(store
            .calls
            .lock()
            .unwrap()
            .contains(&"cancelled".to_string()));
    }

    #[tokio::test]
    async fn terminating_completes_via_its_reason() {
        let store = FakeStore::default();
        let project = project();
        let mut d = deployment(DeploymentStatus::Terminating);
        d.termination_reason = Some(TerminationReason::UserStopped);

        perform_status_transition(&store, &project, &d)
            .await
            .unwrap();

        assert!(store.calls.lock().unwrap().contains(&"stopped".to_string()));
    }

    #[tokio::test]
    async fn missing_termination_reason_resolves_to_stopped() {
        let store = FakeStore::default();
        let project = project();
        let d = deployment(DeploymentStatus::Terminating); // termination_reason: None

        complete_termination(&store, &project, &d).await.unwrap();

        assert!(store.calls.lock().unwrap().contains(&"stopped".to_string()));
    }

    #[tokio::test]
    async fn expired_non_terminal_deployment_is_marked_terminating() {
        let store = FakeStore::default();
        let project = project();
        let mut d = deployment(DeploymentStatus::Healthy);
        d.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));

        perform_status_transition(&store, &project, &d)
            .await
            .unwrap();

        let calls = store.calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c.contains("terminating") && c.contains("Expired")));
    }

    #[tokio::test]
    async fn an_already_terminating_deployment_is_not_re_expired() {
        // A deployment already Terminating for some OTHER reason must not
        // also be re-marked terminating for expiry -- that would clobber the
        // termination reason mid-flight.
        let store = FakeStore::default();
        let project = project();
        let mut d = deployment(DeploymentStatus::Terminating);
        d.termination_reason = Some(TerminationReason::UserStopped);
        d.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));

        perform_status_transition(&store, &project, &d)
            .await
            .unwrap();

        let calls = store.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("terminating:"))
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn becoming_healthy_supersedes_the_previous_active_deployment() {
        let store = FakeStore::default();
        let project = project();
        let new = deployment(DeploymentStatus::Deploying);
        let mut old_active = deployment(DeploymentStatus::Healthy);
        old_active.deployment_group = new.deployment_group.clone();

        store.active_in_group.lock().unwrap().insert(
            (project.id, new.deployment_group.clone()),
            old_active.clone(),
        );

        handle_deployment_became_healthy(&store, None, &project, &new)
            .await
            .unwrap();

        let calls = store.calls.lock().unwrap();
        assert!(calls.contains(&"healthy".to_string()));
        assert!(calls
            .iter()
            .any(|c| c.starts_with(&format!("terminating:{}", old_active.id))));
        assert!(calls.contains(&"mark_active".to_string()));
    }

    #[tokio::test]
    async fn the_superseded_hook_runs_after_the_old_deployment_is_marked_terminating() {
        struct RecordingHook {
            called_with: Mutex<Option<Uuid>>,
        }
        #[async_trait]
        impl SupersededHook for RecordingHook {
            async fn on_superseded(
                &self,
                _project: &Project,
                superseded: &Deployment,
            ) -> Result<()> {
                *self.called_with.lock().unwrap() = Some(superseded.id);
                Ok(())
            }
        }

        let store = FakeStore::default();
        let project = project();
        let new = deployment(DeploymentStatus::Deploying);
        let mut old_active = deployment(DeploymentStatus::Healthy);
        old_active.deployment_group = new.deployment_group.clone();
        store.active_in_group.lock().unwrap().insert(
            (project.id, new.deployment_group.clone()),
            old_active.clone(),
        );
        let hook = RecordingHook {
            called_with: Mutex::new(None),
        };

        handle_deployment_became_healthy(&store, Some(&hook), &project, &new)
            .await
            .unwrap();

        assert_eq!(*hook.called_with.lock().unwrap(), Some(old_active.id));
    }

    #[tokio::test]
    async fn a_hook_error_is_swallowed_not_propagated() {
        struct FailingHook;
        #[async_trait]
        impl SupersededHook for FailingHook {
            async fn on_superseded(
                &self,
                _project: &Project,
                _superseded: &Deployment,
            ) -> Result<()> {
                anyhow::bail!("boom")
            }
        }

        let store = FakeStore::default();
        let project = project();
        let new = deployment(DeploymentStatus::Deploying);
        let mut old_active = deployment(DeploymentStatus::Healthy);
        old_active.deployment_group = new.deployment_group.clone();
        store
            .active_in_group
            .lock()
            .unwrap()
            .insert((project.id, new.deployment_group.clone()), old_active);

        let result =
            handle_deployment_became_healthy(&store, Some(&FailingHook), &project, &new).await;

        assert!(
            result.is_ok(),
            "a hook failure must not fail the whole transition"
        );
    }

    #[tokio::test]
    async fn straggler_siblings_are_superseded_by_status_not_by_is_active() {
        let store = FakeStore::default();
        let project = project();
        let new = deployment(DeploymentStatus::Deploying);
        let mut straggler = deployment(DeploymentStatus::Healthy);
        straggler.deployment_group = new.deployment_group.clone();
        straggler.is_active = false; // died before the write that flags it active

        store.siblings_in_group.lock().unwrap().insert(
            (project.id, new.deployment_group.clone()),
            vec![straggler.clone()],
        );

        handle_deployment_became_healthy(&store, None, &project, &new)
            .await
            .unwrap();

        let calls = store.calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c.starts_with(&format!("terminating:{}", straggler.id))));
    }

    #[tokio::test]
    async fn when_the_atomic_write_is_rejected_nothing_else_happens() {
        struct RecordingHook {
            called_with: Mutex<Option<Uuid>>,
        }
        #[async_trait]
        impl SupersededHook for RecordingHook {
            async fn on_superseded(
                &self,
                _project: &Project,
                superseded: &Deployment,
            ) -> Result<()> {
                *self.called_with.lock().unwrap() = Some(superseded.id);
                Ok(())
            }
        }

        let store = FakeStore::default();
        *store.reject_healthy.lock().unwrap() = true;
        let project = project();
        let new = deployment(DeploymentStatus::Deploying);
        let mut old_active = deployment(DeploymentStatus::Healthy);
        old_active.deployment_group = new.deployment_group.clone();
        store
            .active_in_group
            .lock()
            .unwrap()
            .insert((project.id, new.deployment_group.clone()), old_active);
        let hook = RecordingHook {
            called_with: Mutex::new(None),
        };

        handle_deployment_became_healthy(&store, Some(&hook), &project, &new)
            .await
            .unwrap();

        assert!(
            store.calls.lock().unwrap().is_empty(),
            "a rejected atomic write must trigger no hook, no straggler cleanup, no activation"
        );
        assert_eq!(*hook.called_with.lock().unwrap(), None);
    }
}
