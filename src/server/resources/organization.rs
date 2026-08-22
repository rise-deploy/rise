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

use rise_resource_api::{DeleteOutcome, ResourceStore, StoreError};
use rise_resource_store_postgres::PgSession;
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
///
/// The count and the delete run on the same `session`, so they cannot see
/// different states of the world, and `store` must be built over that same
/// session or even that much is lost.
///
/// TODO(multi-org): that is *not* mutual exclusion against the typed writers.
/// PostgreSQL only checks a serializable transaction's predicate reads against
/// writers that are themselves serializable, and every typed link write —
/// `set_team_organization`, `set_project_organization`, `ensure_user_membership`
/// — runs at `READ COMMITTED`. A typed insert committing after this
/// transaction's snapshot is therefore invisible to the count and aborts
/// nothing, and the row is orphaned. Acceptable today because (a) the install
/// is single-default-Org, so any racing insert re-links on the next bootstrap
/// pass, and (b) Org deletes are a rare admin action. Before a second Org can
/// be created in production, serialize delete vs. typed insert by taking
/// `pg_advisory_xact_lock` keyed on the Org UID in this function *and* in every
/// `set_team_organization` / `set_project_organization` / `ensure_user_membership`
/// call site — an advisory lock blocks regardless of isolation level, which is
/// what predicate locking cannot do here.
pub(crate) async fn delete_organization_guarded(
    store: &Arc<dyn ResourceStore>,
    session: &PgSession,
    uid: Uuid,
) -> Result<DeleteOutcome, OrganizationDeleteError> {
    let count = {
        // Scoped: a transaction-scoped session lends out one connection, and
        // `store.delete` below needs it back.
        let mut connection = session
            .acquire()
            .await
            .map_err(OrganizationDeleteError::Store)?;
        organization_links::count_typed_children_for_organization(&mut *connection, uid)
            .await
            .map_err(OrganizationDeleteError::Db)?
    };
    if count > 0 {
        return Err(OrganizationDeleteError::HasChildren { count });
    }
    store
        .delete(uid)
        .await
        .map_err(OrganizationDeleteError::Store)
}
