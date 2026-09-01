//! The `OrganizationView` implementation backing every deployment backend.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rise_backend_core::{resolve_namespace_prefix, DeploymentStore, OrganizationView};
use tracing::error;
use uuid::Uuid;

/// How long a loaded Organization stays cached. A change to either field
/// propagates within roughly one resync window.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Well above any realistic Organization count; bounds memory if abused.
const CACHE_CAPACITY: u64 = 1024;

/// The per-Organization fields backends read on every reconcile, loaded in one
/// store read rather than one per field.
#[derive(Debug, Clone)]
struct OrgSnapshot {
    controller_class: Option<String>,
    namespace_prefix: String,
}

/// Reads Organizations through the resource API behind a short-TTL cache, so a
/// steady stream of resyncs hits the store at most once per Org per window.
///
/// A missing Organization surfaces as `Err` and is never cached — `try_get_with`
/// only retains successful loads, so a transient absence does not get memoised.
pub struct CachedOrgView {
    resource_store: Arc<dyn rise_resource_api::ResourceApi>,
    deployment_store: Arc<dyn DeploymentStore>,
    cache: moka::future::Cache<Uuid, OrgSnapshot>,
}

impl CachedOrgView {
    pub fn new(
        resource_store: Arc<dyn rise_resource_api::ResourceApi>,
        deployment_store: Arc<dyn DeploymentStore>,
    ) -> Self {
        Self {
            resource_store,
            deployment_store,
            cache: moka::future::Cache::builder()
                .time_to_live(CACHE_TTL)
                .max_capacity(CACHE_CAPACITY)
                .build(),
        }
    }

    async fn load(&self, uid: Uuid) -> anyhow::Result<OrgSnapshot> {
        let store = self.resource_store.clone();
        self.cache
            .try_get_with(uid, async move {
                let row = store
                    .get(uid)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to load Organization {uid}: {e}"))?
                    .ok_or_else(|| anyhow::anyhow!("Organization {uid} is missing"))?;
                let spec: rise_resource_api::OrganizationSpec =
                    serde_json::from_value(row.spec.clone()).map_err(|e| {
                        anyhow::anyhow!("Organization {uid} has malformed spec: {e}")
                    })?;
                let annotations: BTreeMap<String, String> =
                    serde_json::from_value(row.metadata.clone()).unwrap_or_default();
                Ok::<_, anyhow::Error>(OrgSnapshot {
                    controller_class: spec.deployment_controller_class,
                    namespace_prefix: resolve_namespace_prefix(&annotations, &row.discriminator),
                })
            })
            .await
            .map_err(|e| anyhow::anyhow!("Failed to load Organization {uid}: {e}"))
    }
}

#[async_trait]
impl OrganizationView for CachedOrgView {
    async fn organization_uid_for_project(&self, project_id: Uuid) -> anyhow::Result<Uuid> {
        self.deployment_store
            .organization_uid_for_project(project_id)
            .await?
            .ok_or_else(|| {
                error!(
                    %project_id,
                    "Project has no organization linkage — bootstrap validation should have \
                     caught this; refusing to reconcile"
                );
                anyhow::anyhow!(
                    "project {project_id} is missing organization_resource_uid; bootstrap must \
                     backfill before reconciliation"
                )
            })
    }

    async fn controller_class(&self, org_uid: Uuid) -> anyhow::Result<Option<String>> {
        Ok(self.load(org_uid).await?.controller_class)
    }

    async fn namespace_prefix(&self, org_uid: Uuid) -> anyhow::Result<String> {
        Ok(self.load(org_uid).await?.namespace_prefix)
    }
}
