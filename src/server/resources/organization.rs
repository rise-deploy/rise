//! Organization-specific resource operations that need application-layer
//! invariants beyond what the generic resource store enforces.
//!
//! Today this is just `delete_organization_guarded`: an Organization carries
//! soft links from `users`, `teams`, and `projects` (via
//! `organization_resource_uid` or the `user_organization_memberships` join
//! table). Those typed rows are not in `resource_store.resources`, so the
//! store's generic child-detection cannot see them — calling
//! `ResourceStore::delete` on an Organization UID directly would orphan
//! every linked row. This module is the canonical entry point for code that
//! needs to delete an Organization.

use std::sync::Arc;

use rise_resource_store::{DeleteOutcome, ResourceStore, StoreError};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::organization_links;

/// Failure modes for [`delete_organization_guarded`].
///
/// Callers are expected to map each variant to a domain-appropriate
/// response — for the HTTP API that means `HasChildren` → 409, `Db` →
/// 500, `Store` → whatever `store_error_to_server_error` would have done.
pub(crate) enum OrganizationDeleteError {
    /// Typed children (memberships, teams, projects) still link to this
    /// Organization.
    HasChildren { count: i64 },
    /// Counting the typed children failed.
    Db(anyhow::Error),
    /// The resource store rejected the delete after the guard passed.
    Store(StoreError),
}

/// Delete an Organization resource only after confirming no typed children
/// reference it. The single canonical entry point — direct
/// `ResourceStore::delete` calls on an Organization UID bypass this guard
/// and will orphan rows in `user_organization_memberships`, `teams`, and
/// `projects`.
pub(crate) async fn delete_organization_guarded(
    store: &Arc<dyn ResourceStore>,
    pool: &PgPool,
    uid: Uuid,
) -> Result<DeleteOutcome, OrganizationDeleteError> {
    let count = organization_links::count_typed_children_for_organization(pool, uid)
        .await
        .map_err(OrganizationDeleteError::Db)?;
    if count > 0 {
        return Err(OrganizationDeleteError::HasChildren { count });
    }
    store
        .delete(uid)
        .await
        .map_err(OrganizationDeleteError::Store)
}
