use crate::models::{Deployment, DeploymentStatus};
use anyhow::{bail, Result};

/// Check if a deployment status is terminal (no further transitions allowed)
pub fn is_terminal(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Cancelled
            | DeploymentStatus::Stopped
            | DeploymentStatus::Superseded
            | DeploymentStatus::Failed
            | DeploymentStatus::Expired
    )
}

/// Check if a deployment is in an active running state
pub fn is_active(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Healthy | DeploymentStatus::Unhealthy
    )
}

/// Check if a deployment still needs an image push.
///
/// Returns `true` for the pre-"Pushed" states (`Pending`, `Building`, `Pushing`)
/// where the CLI may still need registry push credentials.
pub fn needs_image_push(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Pending | DeploymentStatus::Building | DeploymentStatus::Pushing
    )
}

/// Check if a deployment can be cancelled
/// Only deployments in pre-infrastructure states can be cancelled
pub fn is_cancellable(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Pending
            | DeploymentStatus::Building
            | DeploymentStatus::Pushing
            | DeploymentStatus::Pushed
            | DeploymentStatus::Deploying
    )
}

/// Check if a deployment can be terminated
/// Only deployments with running infrastructure can be terminated
pub fn is_terminable(status: &DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Healthy | DeploymentStatus::Unhealthy
    )
}

/// Check if a deployment can be used as the source for a new rollback/redeploy.
///
/// A deployment is reusable once its image is known to be available:
/// - digest-pinned images are reusable immediately
/// - rollback deployments are reusable because they point at an earlier image
/// - locally built images become reusable once they reached `Pushed`
/// - failed/cancelled deployments are reusable only after rollout actually started
pub fn can_create_from(deployment: &Deployment) -> bool {
    if deployment.image_digest.is_some() || deployment.rolled_back_from_deployment_id.is_some() {
        return true;
    }

    if matches!(
        deployment.status,
        DeploymentStatus::Pushed
            | DeploymentStatus::Deploying
            | DeploymentStatus::Healthy
            | DeploymentStatus::Unhealthy
            | DeploymentStatus::Terminating
            | DeploymentStatus::Stopped
            | DeploymentStatus::Superseded
            | DeploymentStatus::Expired
    ) {
        return true;
    }

    matches!(
        deployment.status,
        DeploymentStatus::Cancelling | DeploymentStatus::Cancelled | DeploymentStatus::Failed
    ) && deployment.deploying_started_at.is_some()
}

/// Check if a state transition is valid
pub fn is_valid_transition(from: &DeploymentStatus, to: &DeploymentStatus) -> bool {
    use DeploymentStatus::*;

    match (from, to) {
        // Same status is always valid (allows updated_at refresh)
        (from, to) if from == to => true,

        // Can't transition from terminal states
        (from, _) if is_terminal(from) => false,

        // Pre-Infrastructure (Cancellation Path)
        (Pending | Building | Pushing | Pushed | Deploying, Cancelling) => true,
        (Cancelling, Cancelled) => true,

        // Build/Deploy Path
        (Pending, Building) => true,
        (Building, Pushing) => true,
        (Building, Pushed) => true, // Allow skipping Pushing state if status update fails
        (Pushing, Pushed) => true,
        (Pushed, Deploying) => true,

        // Deployment outcomes
        (Deploying, Healthy) => true, // Health checks pass
        (Deploying, Failed) => true,  // Health checks fail

        // Post-Infrastructure (Running State)
        (Healthy, Unhealthy) => true, // Health degradation
        (Unhealthy, Healthy) => true, // Health recovery
        (Unhealthy, Failed) => true,  // Timeout without recovery

        // Post-Infrastructure (Termination Path)
        (Healthy | Unhealthy, Terminating) => true,
        (Terminating, Stopped) => true, // User-initiated termination
        (Terminating, Superseded) => true, // Replaced by newer deployment
        (Terminating, Expired) => true, // Deployment expired

        // Build/Deploy failures (before reaching Healthy)
        (Pending | Building | Pushing | Pushed, Failed) => true,

        // All other transitions are invalid
        _ => false,
    }
}

/// Validate a state transition and return an error if invalid
pub fn validate_transition(from: &DeploymentStatus, to: &DeploymentStatus) -> Result<()> {
    if !is_valid_transition(from, to) {
        bail!(
            "Invalid deployment state transition from '{}' to '{}'",
            from,
            to
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use DeploymentStatus::*;

    fn deployment(status: DeploymentStatus) -> Deployment {
        Deployment {
            id: Uuid::nil(),
            deployment_id: "20260315-000000".to_string(),
            project_id: Uuid::nil(),
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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_terminal_states() {
        assert!(is_terminal(&Cancelled));
        assert!(is_terminal(&Stopped));
        assert!(is_terminal(&Superseded));
        assert!(is_terminal(&Failed));

        assert!(!is_terminal(&Pending));
        assert!(!is_terminal(&Healthy));
        assert!(!is_terminal(&Cancelling));
    }

    #[test]
    fn test_active_states() {
        assert!(is_active(&Healthy));
        assert!(is_active(&Unhealthy));

        assert!(!is_active(&Deploying));
        assert!(!is_active(&Failed));
    }

    #[test]
    fn test_needs_image_push_states() {
        assert!(needs_image_push(&Pending));
        assert!(needs_image_push(&Building));
        assert!(needs_image_push(&Pushing));

        assert!(!needs_image_push(&Pushed));
        assert!(!needs_image_push(&Deploying));
        assert!(!needs_image_push(&Healthy));
        assert!(!needs_image_push(&Failed));
        assert!(!needs_image_push(&Cancelled));
    }

    #[test]
    fn test_cancellable_states() {
        assert!(is_cancellable(&Pending));
        assert!(is_cancellable(&Building));
        assert!(is_cancellable(&Pushing));
        assert!(is_cancellable(&Pushed));
        assert!(is_cancellable(&Deploying));

        assert!(!is_cancellable(&Healthy));
        assert!(!is_cancellable(&Unhealthy));
        assert!(!is_cancellable(&Cancelled));
    }

    #[test]
    fn test_terminable_states() {
        assert!(is_terminable(&Healthy));
        assert!(is_terminable(&Unhealthy));

        assert!(!is_terminable(&Deploying));
        assert!(!is_terminable(&Stopped));
    }

    #[test]
    fn test_can_create_from_when_image_is_known_available() {
        assert!(can_create_from(&deployment(Pushed)));
        assert!(can_create_from(&deployment(Healthy)));
        assert!(can_create_from(&deployment(Stopped)));
        assert!(can_create_from(&deployment(Superseded)));
    }

    #[test]
    fn test_can_create_from_rejects_pre_push_states_without_image() {
        assert!(!can_create_from(&deployment(Pending)));
        assert!(!can_create_from(&deployment(Building)));
        assert!(!can_create_from(&deployment(Pushing)));
    }

    #[test]
    fn test_can_create_from_requires_rollout_for_failed_or_cancelled_builds() {
        let failed_before_rollout = deployment(Failed);
        assert!(!can_create_from(&failed_before_rollout));

        let mut failed_after_rollout = deployment(Failed);
        failed_after_rollout.deploying_started_at = Some(chrono::Utc::now());
        assert!(can_create_from(&failed_after_rollout));

        let mut cancelled_after_rollout = deployment(Cancelled);
        cancelled_after_rollout.deploying_started_at = Some(chrono::Utc::now());
        assert!(can_create_from(&cancelled_after_rollout));
    }

    #[test]
    fn test_can_create_from_allows_digest_and_rollback_sources_immediately() {
        let mut digest_pinned = deployment(Pending);
        digest_pinned.image_digest = Some("registry.example/app@sha256:abc".to_string());
        assert!(can_create_from(&digest_pinned));

        let mut rollback_source = deployment(Failed);
        rollback_source.rolled_back_from_deployment_id = Some(Uuid::new_v4());
        assert!(can_create_from(&rollback_source));
    }

    #[test]
    fn test_valid_cancellation_path() {
        // Can cancel from pre-infrastructure states
        assert!(is_valid_transition(&Pending, &Cancelling));
        assert!(is_valid_transition(&Building, &Cancelling));
        assert!(is_valid_transition(&Deploying, &Cancelling));

        // Cancelling always succeeds to Cancelled
        assert!(is_valid_transition(&Cancelling, &Cancelled));

        // Cannot transition from Cancelling to Failed
        assert!(!is_valid_transition(&Cancelling, &Failed));
    }

    #[test]
    fn test_valid_termination_path() {
        // Can terminate from post-infrastructure states
        assert!(is_valid_transition(&Healthy, &Terminating));
        assert!(is_valid_transition(&Unhealthy, &Terminating));

        // Terminating succeeds to Stopped or Superseded
        assert!(is_valid_transition(&Terminating, &Stopped));
        assert!(is_valid_transition(&Terminating, &Superseded));

        // Cannot transition from Terminating to Failed
        assert!(!is_valid_transition(&Terminating, &Failed));
    }

    #[test]
    fn test_healthy_unhealthy_cannot_be_cancelled() {
        // Healthy/Unhealthy cannot go to Cancelled
        assert!(!is_valid_transition(&Healthy, &Cancelled));
        assert!(!is_valid_transition(&Unhealthy, &Cancelled));

        // They must use Terminating
        assert!(is_valid_transition(&Healthy, &Terminating));
        assert!(is_valid_transition(&Unhealthy, &Terminating));
    }

    #[test]
    fn test_deployment_path() {
        // Normal deployment flow
        assert!(is_valid_transition(&Pending, &Building));
        assert!(is_valid_transition(&Building, &Pushing));
        assert!(is_valid_transition(&Pushing, &Pushed));
        assert!(is_valid_transition(&Pushed, &Deploying));
        assert!(is_valid_transition(&Deploying, &Healthy));

        // Allow skipping Pushing if status update fails
        assert!(is_valid_transition(&Building, &Pushed));

        // Health state transitions
        assert!(is_valid_transition(&Healthy, &Unhealthy));
        assert!(is_valid_transition(&Unhealthy, &Healthy));
        assert!(is_valid_transition(&Unhealthy, &Failed));
    }

    #[test]
    fn test_terminal_states_no_transitions() {
        // Cannot transition from terminal states
        assert!(!is_valid_transition(&Cancelled, &Pending));
        assert!(!is_valid_transition(&Stopped, &Healthy));
        assert!(!is_valid_transition(&Superseded, &Deploying));
        assert!(!is_valid_transition(&Failed, &Healthy));
    }

    #[test]
    fn test_invalid_transitions() {
        // Cannot skip states in deployment path
        assert!(!is_valid_transition(&Pending, &Deploying));
        assert!(!is_valid_transition(&Building, &Healthy));

        // Cannot go back in deployment path
        assert!(!is_valid_transition(&Deploying, &Building));
        assert!(!is_valid_transition(&Healthy, &Pending));
    }

    #[test]
    fn test_same_status_transitions() {
        // Same status transitions should be valid (allows updated_at refresh)
        assert!(is_valid_transition(&Pending, &Pending));
        assert!(is_valid_transition(&Building, &Building));
        assert!(is_valid_transition(&Healthy, &Healthy));
        assert!(is_valid_transition(&Unhealthy, &Unhealthy));
        assert!(is_valid_transition(&Failed, &Failed));
        assert!(is_valid_transition(&Stopped, &Stopped));
    }
}
