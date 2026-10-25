//! Shared test fixtures for the runtime-agnostic reconcile machinery.
//!
//! Compiled for this crate's tests and, through the `test-helpers` feature
//! (enabled only from `[dev-dependencies]`), for every backend crate's tests,
//! so they build the same canonical fixtures instead of drifting copies:
//! `DesiredContainer` / `ActualContainer` slots, protected-id and health maps,
//! and `Project` / `Deployment` / `Environment` / `CustomDomain` models with a
//! matching `DeploymentUrlBuilder`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use super::desired::{DesiredContainer, DesiredRoute};
use super::diff::ActualContainer;
use super::models::{
    CustomDomain, Deployment, DeploymentStatus, Environment, Project, ProjectStatus,
};
use super::naming;
use super::providers::{ImageTagType, RegistryCredentials, RegistryProvider};
use super::url_builder::DeploymentUrlBuilder;

pub fn desired(container: &str, image: &str, hash: &str) -> DesiredContainer {
    DesiredContainer {
        project: "myapp".to_string(),
        project_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
        access_class: "public".to_string(),
        deployment_group: "default".to_string(),
        deployment_id: "20260101-120000".to_string(),
        deployment_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
        container: container.to_string(),
        environment: None,
        image: image.to_string(),
        port: Some(8080),
        cpu: "500m".to_string(),
        memory: "256Mi".to_string(),
        env: vec![],
        env_hash: hash.to_string(),
        routes: vec![DesiredRoute {
            hosts: vec!["myapp.rise.dev".to_string()],
            path_prefix: None,
            access: None,
        }],
        routable: true,
        // Fixed sentinel route-hash for diff tests; the reconciler computes
        // the real value via `route_hash_for`. Tests that exercise routing
        // drift override this and the matching actual label.
        route_hash: "rh-active".to_string(),
        // Seed generation (the diff resolves the real value before apply).
        generation: 1,
        replica: 0,
        health_path: Some("/".to_string()),
        health_check_interval_secs: None,
        health_check_timeout_secs: None,
    }
}

/// Stable identity key for the `desired()` helper's slot, used to build the
/// `identity` field of expected Create/Recreate actions.
pub fn identity_of(d: &DesiredContainer) -> String {
    super::diff::identity_key(
        &d.project,
        &d.deployment_group,
        &d.deployment_id,
        &d.container,
        d.replica,
    )
}

/// Empty protected-deployment-ids set for the common case where no
/// deployment failed desired computation.
pub fn no_protected() -> HashSet<String> {
    HashSet::new()
}

/// The resolved generation-ful name for the `desired()` slot at a given
/// generation (the name a Create/Recreate action carries).
pub fn name_of_gen(d: &DesiredContainer, generation: u32) -> String {
    naming::container_name(
        "rise",
        &d.project,
        &d.deployment_group,
        &d.deployment_id,
        &d.container,
        d.replica,
        generation,
    )
}

/// A live container belonging to the `desired()` helper's deployment, at a
/// given generation. Its name carries the `_g{generation}` suffix and it
/// carries the full identity-label set so the diff can match it.
pub fn actual_for_gen(
    d: &DesiredContainer,
    image: &str,
    env_hash: &str,
    generation: u32,
) -> ActualContainer {
    ActualContainer {
        id: "cid".to_string(),
        name: name_of_gen(d, generation),
        project: Some(d.project.clone()),
        deployment_group: Some(d.deployment_group.clone()),
        container: Some(d.container.clone()),
        deployment_id_label: Some(d.deployment_id.clone()),
        deployment_uuid_label: Some(d.deployment_uuid.clone()),
        generation,
        replica: d.replica,
        image_label: Some(image.to_string()),
        env_hash_label: Some(env_hash.to_string()),
        route_hash_label: Some("rh-active".to_string()),
        state: Some("running".to_string()),
    }
}

/// A live container at generation 1 (the common case for most diff tests).
pub fn actual_for(d: &DesiredContainer, image: &str, env_hash: &str) -> ActualContainer {
    actual_for_gen(d, image, env_hash, 1)
}

/// Build the `desired()` slot for a specific replica index.
pub fn desired_replica(replica: u32) -> DesiredContainer {
    let mut d = desired("app", "img:1", "h1");
    d.replica = replica;
    d
}

/// A live (matched) container for the given replica/state/image. Carries the
/// full identity-label set including the replica so the diff matches it.
pub fn actual_replica(replica: u32, state: &str, image: &str) -> ActualContainer {
    let d = desired_replica(replica);
    ActualContainer {
        id: format!("cid-r{replica}"),
        state: Some(state.to_string()),
        ..actual_for(&d, image, "h1")
    }
}

/// Health map marking every given identity healthy.
pub fn all_healthy(actual: &[ActualContainer]) -> HashMap<String, bool> {
    actual
        .iter()
        .filter_map(|a| a.identity().map(|id| (id, true)))
        .collect()
}

// ── Model fixtures ─────────────────────────────────────────────────────

/// A project named `name` with the given access class.
pub fn project(name: &str, access_class: &str) -> Project {
    let now = Utc::now();
    Project {
        id: Uuid::new_v4(),
        name: name.to_string(),
        status: ProjectStatus::Running,
        access_class: access_class.to_string(),
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

/// A healthy, active, single-container deployment of `project` in `group`.
pub fn deployment(project: &Project, deployment_id: &str, group: &str) -> Deployment {
    let now = Utc::now();
    Deployment {
        id: Uuid::new_v4(),
        deployment_id: deployment_id.to_string(),
        project_id: project.id,
        created_by_id: Uuid::nil(),
        status: DeploymentStatus::Healthy,
        deployment_group: group.to_string(),
        environment_id: Uuid::nil(),
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
        containers: None,
        routes: None,
        created_via: crate::models::CreatedVia::Deploy,
        promoted_from_deployment_id: None,
        promoted_from_environment_id: None,
        migration_version: None,
        unhealthy_since: None,
        rollback_guard_for_deployment_id: None,
        created_at: now,
        updated_at: now,
    }
}

/// An environment of `project` whose primary deployment group is `group`.
pub fn environment(project: &Project, name: &str, group: Option<&str>) -> Environment {
    let now = Utc::now();
    Environment {
        id: Uuid::new_v4(),
        project_id: project.id,
        name: name.to_string(),
        primary_deployment_group: group.map(str::to_string),
        is_production: name == "production",
        color: "green".to_string(),
        min_replicas: None,
        max_replicas: None,
        min_cpu: None,
        max_cpu: None,
        min_memory: None,
        max_memory: None,
        max_deployment_expiration: None,
        promotion_source_environment_id: None,
        rollback_guard_enabled: false,
        rollback_guard_unhealthy_timeout_seconds: 300,
        created_at: now,
        updated_at: now,
    }
}

/// A custom domain attached to `environment`.
pub fn custom_domain(environment: &Environment, domain: &str) -> CustomDomain {
    let now = Utc::now();
    CustomDomain {
        id: Uuid::new_v4(),
        project_id: environment.project_id,
        environment_id: environment.id,
        domain: domain.to_string(),
        is_primary: false,
        created_at: now,
        updated_at: now,
    }
}

struct TestRegistryProvider;

#[async_trait::async_trait]
impl RegistryProvider for TestRegistryProvider {
    async fn get_credentials(
        &self,
        _repository: &str,
        _tags: &[&str],
    ) -> anyhow::Result<RegistryCredentials> {
        unreachable!("test registry issues no credentials")
    }

    async fn get_pull_credentials(&self) -> anyhow::Result<(String, String)> {
        unreachable!("test registry issues no credentials")
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
}

/// A URL builder with production template `{project_name}.example.test`,
/// staging template `{project_name}-{deployment_group}.preview.example.test`
/// and environment template `{project_name}-{environment}.example.test`.
pub fn url_builder() -> DeploymentUrlBuilder {
    DeploymentUrlBuilder {
        production_ingress_url_template: "{project_name}.example.test".to_string(),
        staging_ingress_url_template: Some(
            "{project_name}-{deployment_group}.preview.example.test".to_string(),
        ),
        environment_ingress_url_template: Some(
            "{project_name}-{environment}.example.test".to_string(),
        ),
        ingress_port: None,
        ingress_schema: "https".to_string(),
        registry_provider: Arc::new(TestRegistryProvider),
    }
}
