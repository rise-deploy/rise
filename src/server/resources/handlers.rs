//! HTTP handlers for the generic resource API.
//!
//! The API is operator-only in v1 (`auth.operator_users`), except for the
//! controller-specific status/finalizer endpoints, which authenticate via the
//! `ControllerAuthContext` extractor. `Orphan` deletion and reparent are
//! additionally gated to operators who are also listed in `auth.admin_users`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use rise_resource_api::{
    CreateResourceRequest, ResourceScope, UpdateResourceRequest, ORGANIZATION_KIND,
};
use rise_resource_store::{
    CollectionInfo, CreateResourceParams, DeleteOutcome, PathSegment, PropagationPolicy,
    ResourceRow, ResourceStore, UpdateResourceParams,
};
use serde::Deserialize;
use uuid::Uuid;

use super::error_map::store_error_to_server_error;
use super::models::{
    row_to_resource, ControllerFinalizerUpdate, ControllerStatusUpdate, ReparentRequest,
    ResourceList,
};
use super::path::parse_identifier;
use crate::db::models::User;
use crate::server::auth::context::AuthContext;
use crate::server::auth::controller::ControllerAuthContext;
use crate::server::error::ServerError;
use crate::server::state::AppState;

// -----------------------------------------------------------------------------
// Authorization helpers
// -----------------------------------------------------------------------------

/// Require an operator-authenticated user. Service-account/controller tokens
/// and non-operator users get 401/403 respectively.
fn require_operator(state: &AppState, auth: &AuthContext) -> Result<User, ServerError> {
    let user = auth.user()?.clone();
    if !state.is_operator(&user.email) {
        tracing::warn!(
            user_email = %user.email,
            "Generic resource API access denied — user is not an Operator"
        );
        return Err(ServerError::forbidden(
            "Operator role required for the generic resource API",
        ));
    }
    Ok(user)
}

/// Require an operator who is also an admin. Used for break-glass operations
/// (`Orphan` deletion and reparent) per the PR 4 plan.
fn require_admin_operator(state: &AppState, auth: &AuthContext) -> Result<User, ServerError> {
    let user = require_operator(state, auth)?;
    if !state.is_admin(&user.email) {
        tracing::warn!(
            user_email = %user.email,
            "Break-glass resource operation denied — Operator is not also listed as an admin"
        );
        return Err(ServerError::forbidden(
            "This operation requires Operator + admin privileges",
        ));
    }
    Ok(user)
}

// -----------------------------------------------------------------------------
// Path resolution
// -----------------------------------------------------------------------------

struct ResolvedCollection {
    collection: String,
    info: CollectionInfo,
}

async fn resolve_collection(
    store: &Arc<dyn ResourceStore>,
    collection: &str,
) -> Result<ResolvedCollection, ServerError> {
    let info = store
        .resolve_collection(collection)
        .await
        .map_err(store_error_to_server_error)?
        .ok_or_else(|| ServerError::not_found(format!("unknown collection '{collection}'")))?;
    Ok(ResolvedCollection {
        collection: collection.to_string(),
        info,
    })
}

/// Resolve the organization parent of an org-scoped collection. Returns the
/// org's UID for use as `parent_uid` in subsequent operations.
async fn resolve_organization_parent(
    store: &Arc<dyn ResourceStore>,
    org_token: &str,
) -> Result<Uuid, ServerError> {
    let segment = parse_identifier(ORGANIZATION_KIND, org_token)?;
    let chain = store
        .resolve_path(&[segment])
        .await
        .map_err(store_error_to_server_error)?;
    let org = chain
        .into_iter()
        .next()
        .ok_or_else(|| ServerError::not_found(format!("organization '{org_token}' not found")))?;
    Ok(org.uid)
}

/// Resolve a leaf resource within a scope by name-or-uid identifier.
async fn resolve_leaf(
    store: &Arc<dyn ResourceStore>,
    info: &CollectionInfo,
    parent_uid: Option<Uuid>,
    identifier: &str,
) -> Result<ResourceRow, ServerError> {
    let mut segments = Vec::new();
    if let Some(pid) = parent_uid {
        segments.push(PathSegment::Uid {
            kind: ORGANIZATION_KIND.to_string(),
            uid: pid,
        });
    }
    segments.push(parse_identifier(&info.kind, identifier)?);
    let chain = store
        .resolve_path(&segments)
        .await
        .map_err(store_error_to_server_error)?;
    chain
        .into_iter()
        .last()
        .ok_or_else(|| ServerError::not_found(format!("resource '{identifier}' not found")))
}

fn enforce_scope(
    info: &CollectionInfo,
    expected: ResourceScope,
    collection: &str,
) -> Result<(), ServerError> {
    if info.scope != expected {
        let expected_label = match expected {
            ResourceScope::Root => "root",
            ResourceScope::Organization => "organization",
        };
        return Err(ServerError::bad_request(format!(
            "collection '{collection}' is not {expected_label}-scoped"
        )));
    }
    Ok(())
}

fn assert_body_matches(
    info: &CollectionInfo,
    body_api_version: &str,
    body_kind: &str,
    body_name: &str,
    url_name: &str,
) -> Result<(), ServerError> {
    if body_api_version != info.api_version {
        return Err(ServerError::bad_request(format!(
            "body apiVersion '{body_api_version}' does not match collection ({})",
            info.api_version
        )));
    }
    if body_kind != info.kind {
        return Err(ServerError::bad_request(format!(
            "body kind '{body_kind}' does not match collection ({})",
            info.kind
        )));
    }
    // The URL identifier may be `uid:<uuid>`, in which case it does not
    // constrain the name. For plain-name URLs, the body's name must match.
    if !url_name.starts_with("uid:") && body_name != url_name {
        return Err(ServerError::bad_request(format!(
            "body metadata.name '{body_name}' does not match URL name '{url_name}'"
        )));
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Query types
// -----------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteQuery {
    #[serde(default)]
    pub propagation_policy: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphansQuery {
    /// Optional parent UID to scope the list to a single subtree.
    #[serde(default)]
    pub parent: Option<Uuid>,
}

fn parse_propagation(value: Option<String>) -> Result<PropagationPolicy, ServerError> {
    match value.as_deref().map(str::trim) {
        None | Some("") | Some("Cascade") => Ok(PropagationPolicy::Cascade),
        Some("Orphan") => Ok(PropagationPolicy::Orphan),
        Some(other) => Err(ServerError::bad_request(format!(
            "invalid propagationPolicy '{other}' (expected 'Cascade' or 'Orphan')"
        ))),
    }
}

// -----------------------------------------------------------------------------
// Root-scoped CRUD
// -----------------------------------------------------------------------------

pub async fn list_root(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(collection): Path<String>,
) -> Result<Json<ResourceList>, ServerError> {
    let _user = require_operator(&state, &auth)?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(&resolved.info, ResourceScope::Root, &resolved.collection)?;
    let rows = state
        .resource_store
        .list(&resolved.info.kind, None)
        .await
        .map_err(store_error_to_server_error)?;
    Ok(Json(ResourceList {
        api_version: resolved.info.api_version,
        kind: resolved.info.kind,
        items: rows.iter().map(row_to_resource).collect(),
    }))
}

pub async fn create_root(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(collection): Path<String>,
    Json(body): Json<CreateResourceRequest>,
) -> Result<(StatusCode, Json<rise_resource_api::Resource>), ServerError> {
    let _user = require_operator(&state, &auth)?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(&resolved.info, ResourceScope::Root, &resolved.collection)?;
    create_resource(&state, &resolved, None, body).await
}

pub async fn get_root(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((collection, name)): Path<(String, String)>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let _user = require_operator(&state, &auth)?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(&resolved.info, ResourceScope::Root, &resolved.collection)?;
    let row = resolve_leaf(&state.resource_store, &resolved.info, None, &name).await?;
    Ok(Json(row_to_resource(&row)))
}

pub async fn update_root(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((collection, name)): Path<(String, String)>,
    Json(body): Json<UpdateResourceRequest>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let _user = require_operator(&state, &auth)?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(&resolved.info, ResourceScope::Root, &resolved.collection)?;
    update_resource(&state, &resolved, None, &name, body).await
}

pub async fn delete_root(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((collection, name)): Path<(String, String)>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<serde_json::Value>, ServerError> {
    let policy = parse_propagation(q.propagation_policy)?;
    let user = if matches!(policy, PropagationPolicy::Orphan) {
        require_admin_operator(&state, &auth)?
    } else {
        require_operator(&state, &auth)?
    };
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(&resolved.info, ResourceScope::Root, &resolved.collection)?;
    delete_resource(&state, &resolved, None, &name, policy, &user).await
}

// -----------------------------------------------------------------------------
// Organization-scoped CRUD
// -----------------------------------------------------------------------------

pub async fn list_org(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((org, collection)): Path<(String, String)>,
) -> Result<Json<ResourceList>, ServerError> {
    let _user = require_operator(&state, &auth)?;
    let org_uid = resolve_organization_parent(&state.resource_store, &org).await?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(
        &resolved.info,
        ResourceScope::Organization,
        &resolved.collection,
    )?;
    let rows = state
        .resource_store
        .list(&resolved.info.kind, Some(org_uid))
        .await
        .map_err(store_error_to_server_error)?;
    Ok(Json(ResourceList {
        api_version: resolved.info.api_version,
        kind: resolved.info.kind,
        items: rows.iter().map(row_to_resource).collect(),
    }))
}

pub async fn create_org(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((org, collection)): Path<(String, String)>,
    Json(body): Json<CreateResourceRequest>,
) -> Result<(StatusCode, Json<rise_resource_api::Resource>), ServerError> {
    let _user = require_operator(&state, &auth)?;
    let org_uid = resolve_organization_parent(&state.resource_store, &org).await?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(
        &resolved.info,
        ResourceScope::Organization,
        &resolved.collection,
    )?;
    create_resource(&state, &resolved, Some(org_uid), body).await
}

pub async fn get_org(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((org, collection, name)): Path<(String, String, String)>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let _user = require_operator(&state, &auth)?;
    let org_uid = resolve_organization_parent(&state.resource_store, &org).await?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(
        &resolved.info,
        ResourceScope::Organization,
        &resolved.collection,
    )?;
    let row = resolve_leaf(&state.resource_store, &resolved.info, Some(org_uid), &name).await?;
    Ok(Json(row_to_resource(&row)))
}

pub async fn update_org(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((org, collection, name)): Path<(String, String, String)>,
    Json(body): Json<UpdateResourceRequest>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let _user = require_operator(&state, &auth)?;
    let org_uid = resolve_organization_parent(&state.resource_store, &org).await?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(
        &resolved.info,
        ResourceScope::Organization,
        &resolved.collection,
    )?;
    update_resource(&state, &resolved, Some(org_uid), &name, body).await
}

pub async fn delete_org(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((org, collection, name)): Path<(String, String, String)>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<serde_json::Value>, ServerError> {
    let policy = parse_propagation(q.propagation_policy)?;
    let user = if matches!(policy, PropagationPolicy::Orphan) {
        require_admin_operator(&state, &auth)?
    } else {
        require_operator(&state, &auth)?
    };
    let org_uid = resolve_organization_parent(&state.resource_store, &org).await?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(
        &resolved.info,
        ResourceScope::Organization,
        &resolved.collection,
    )?;
    delete_resource(&state, &resolved, Some(org_uid), &name, policy, &user).await
}

// -----------------------------------------------------------------------------
// Controller status / finalizer endpoints
// -----------------------------------------------------------------------------

pub async fn update_status_root(
    State(state): State<AppState>,
    controller: ControllerAuthContext,
    Path((collection, name)): Path<(String, String)>,
    Json(body): Json<ControllerStatusUpdate>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(&resolved.info, ResourceScope::Root, &resolved.collection)?;
    let row = resolve_leaf(&state.resource_store, &resolved.info, None, &name).await?;
    apply_controller_status(&state, &controller, &row, body).await
}

pub async fn update_status_org(
    State(state): State<AppState>,
    controller: ControllerAuthContext,
    Path((org, collection, name)): Path<(String, String, String)>,
    Json(body): Json<ControllerStatusUpdate>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let org_uid = resolve_organization_parent(&state.resource_store, &org).await?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(
        &resolved.info,
        ResourceScope::Organization,
        &resolved.collection,
    )?;
    let row = resolve_leaf(&state.resource_store, &resolved.info, Some(org_uid), &name).await?;
    apply_controller_status(&state, &controller, &row, body).await
}

pub async fn update_finalizers_root(
    State(state): State<AppState>,
    controller: ControllerAuthContext,
    Path((collection, name)): Path<(String, String)>,
    Json(body): Json<ControllerFinalizerUpdate>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(&resolved.info, ResourceScope::Root, &resolved.collection)?;
    let row = resolve_leaf(&state.resource_store, &resolved.info, None, &name).await?;
    apply_controller_finalizers(&state, &controller, &row, body).await
}

pub async fn update_finalizers_org(
    State(state): State<AppState>,
    controller: ControllerAuthContext,
    Path((org, collection, name)): Path<(String, String, String)>,
    Json(body): Json<ControllerFinalizerUpdate>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let org_uid = resolve_organization_parent(&state.resource_store, &org).await?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    enforce_scope(
        &resolved.info,
        ResourceScope::Organization,
        &resolved.collection,
    )?;
    let row = resolve_leaf(&state.resource_store, &resolved.info, Some(org_uid), &name).await?;
    apply_controller_finalizers(&state, &controller, &row, body).await
}

// -----------------------------------------------------------------------------
// Break-glass endpoints
// -----------------------------------------------------------------------------

pub async fn list_orphans(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<OrphansQuery>,
) -> Result<Json<serde_json::Value>, ServerError> {
    let user = require_admin_operator(&state, &auth)?;
    let rows = state
        .resource_store
        .list_orphans(q.parent)
        .await
        .map_err(store_error_to_server_error)?;
    tracing::info!(
        target: "rise::audit",
        actor = %user.email,
        parent = ?q.parent,
        count = rows.len(),
        "resource.orphans_listed"
    );
    let items: Vec<rise_resource_api::Resource> = rows.iter().map(row_to_resource).collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

pub async fn reparent_root(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((collection, name)): Path<(String, String)>,
    Json(body): Json<ReparentRequest>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let user = require_admin_operator(&state, &auth)?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    let row = resolve_leaf(&state.resource_store, &resolved.info, None, &name).await?;
    apply_reparent(&state, &row, body, &user).await
}

pub async fn reparent_org(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((org, collection, name)): Path<(String, String, String)>,
    Json(body): Json<ReparentRequest>,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let user = require_admin_operator(&state, &auth)?;
    let org_uid = resolve_organization_parent(&state.resource_store, &org).await?;
    let resolved = resolve_collection(&state.resource_store, &collection).await?;
    let row = resolve_leaf(&state.resource_store, &resolved.info, Some(org_uid), &name).await?;
    apply_reparent(&state, &row, body, &user).await
}

// -----------------------------------------------------------------------------
// Shared helpers (write paths)
// -----------------------------------------------------------------------------

async fn create_resource(
    state: &AppState,
    resolved: &ResolvedCollection,
    parent_uid: Option<Uuid>,
    body: CreateResourceRequest,
) -> Result<(StatusCode, Json<rise_resource_api::Resource>), ServerError> {
    assert_body_matches(
        &resolved.info,
        &body.api_version,
        &body.kind,
        &body.metadata.name,
        &body.metadata.name,
    )?;

    let annotations: BTreeMap<String, String> = body.metadata.annotations.clone();
    let spec = serde_json::to_value(&body.spec)
        .map_err(|e| ServerError::bad_request(format!("invalid spec: {e}")))?;

    let params = CreateResourceParams {
        api_version: body.api_version,
        kind: body.kind,
        name: body.metadata.name,
        parent_uid,
        annotations,
        finalizers: body.metadata.finalizers,
        spec,
        validator: Some(resolved.info.spec_validator.clone()),
    };

    // ResourceDefinitions must go through the dedicated entry point so the
    // projection table is kept in sync. The store rejects regular `create()`
    // calls for that kind, but we still route here explicitly.
    let row = if resolved.info.kind == rise_resource_api::RESOURCE_DEFINITION_KIND {
        state
            .resource_store
            .register_resource_definition(params)
            .await
            .map_err(store_error_to_server_error)?
    } else {
        state
            .resource_store
            .create(params)
            .await
            .map_err(store_error_to_server_error)?
    };

    Ok((StatusCode::CREATED, Json(row_to_resource(&row))))
}

async fn update_resource(
    state: &AppState,
    resolved: &ResolvedCollection,
    parent_uid: Option<Uuid>,
    url_name: &str,
    body: UpdateResourceRequest,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    assert_body_matches(
        &resolved.info,
        &body.api_version,
        &body.kind,
        &body.metadata.name,
        url_name,
    )?;

    let row = resolve_leaf(&state.resource_store, &resolved.info, parent_uid, url_name).await?;
    if body.metadata.name != row.name {
        return Err(ServerError::bad_request(format!(
            "renaming a resource via PUT is not supported (stored name '{}', body '{}')",
            row.name, body.metadata.name
        )));
    }

    let annotations: BTreeMap<String, String> = body.metadata.annotations.clone();
    let spec = serde_json::to_value(&body.spec)
        .map_err(|e| ServerError::bad_request(format!("invalid spec: {e}")))?;

    let params = UpdateResourceParams {
        revision: body.metadata.revision,
        annotations,
        finalizers: body.metadata.finalizers,
        spec,
        validator: Some(resolved.info.spec_validator.clone()),
    };

    let updated = if resolved.info.kind == rise_resource_api::RESOURCE_DEFINITION_KIND {
        state
            .resource_store
            .update_resource_definition(row.uid, params)
            .await
            .map_err(store_error_to_server_error)?
    } else {
        state
            .resource_store
            .update(row.uid, params)
            .await
            .map_err(store_error_to_server_error)?
    };

    Ok(Json(row_to_resource(&updated)))
}

async fn delete_resource(
    state: &AppState,
    resolved: &ResolvedCollection,
    parent_uid: Option<Uuid>,
    url_name: &str,
    policy: PropagationPolicy,
    user: &User,
) -> Result<Json<serde_json::Value>, ServerError> {
    let row = resolve_leaf(&state.resource_store, &resolved.info, parent_uid, url_name).await?;
    let outcome = state
        .resource_store
        .delete(row.uid, policy)
        .await
        .map_err(store_error_to_server_error)?;

    if matches!(policy, PropagationPolicy::Orphan) {
        tracing::info!(
            target: "rise::audit",
            actor = %user.email,
            uid = %row.uid,
            kind = %row.kind,
            name = %row.name,
            "resource.orphan_delete"
        );
    }

    let body = match outcome {
        DeleteOutcome::Deleted => serde_json::json!({"deleted": true, "uid": row.uid}),
        DeleteOutcome::MarkedForDeletion(marked) => serde_json::json!({
            "deleted": false,
            "markedForDeletion": true,
            "resource": row_to_resource(&marked),
        }),
    };
    Ok(Json(body))
}

async fn apply_controller_status(
    state: &AppState,
    controller: &ControllerAuthContext,
    row: &ResourceRow,
    body: ControllerStatusUpdate,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = state
        .resource_store
        .update_controller_status(row.uid, &controller.0.identity_id, body.status)
        .await
        .map_err(store_error_to_server_error)?;
    Ok(Json(row_to_resource(&updated)))
}

async fn apply_controller_finalizers(
    state: &AppState,
    controller: &ControllerAuthContext,
    row: &ResourceRow,
    body: ControllerFinalizerUpdate,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = state
        .resource_store
        .update_controller_finalizers(row.uid, &controller.0.identity_id, &body.add, &body.remove)
        .await
        .map_err(store_error_to_server_error)?;
    Ok(Json(row_to_resource(&updated)))
}

async fn apply_reparent(
    state: &AppState,
    row: &ResourceRow,
    body: ReparentRequest,
    user: &User,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = state
        .resource_store
        .reparent(row.uid, body.new_parent_uid)
        .await
        .map_err(store_error_to_server_error)?;
    tracing::info!(
        target: "rise::audit",
        actor = %user.email,
        uid = %row.uid,
        kind = %row.kind,
        name = %row.name,
        old_parent_uid = ?row.parent_uid,
        new_parent_uid = ?body.new_parent_uid,
        "resource.reparent"
    );
    Ok(Json(row_to_resource(&updated)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_propagation_defaults_to_cascade() {
        assert!(matches!(
            parse_propagation(None).unwrap(),
            PropagationPolicy::Cascade
        ));
        assert!(matches!(
            parse_propagation(Some("".into())).unwrap(),
            PropagationPolicy::Cascade
        ));
        assert!(matches!(
            parse_propagation(Some("Cascade".into())).unwrap(),
            PropagationPolicy::Cascade
        ));
    }

    #[test]
    fn parses_propagation_orphan() {
        assert!(matches!(
            parse_propagation(Some("Orphan".into())).unwrap(),
            PropagationPolicy::Orphan
        ));
    }

    #[test]
    fn rejects_unknown_propagation() {
        let err = parse_propagation(Some("Foreground".into())).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("invalid propagationPolicy"));
    }

    #[test]
    fn assert_body_matches_uid_url_does_not_enforce_name() {
        let info = CollectionInfo {
            api_version: "rise.dev/v1alpha1".into(),
            kind: "Organization".into(),
            scope: ResourceScope::Root,
            spec_validator: std::sync::Arc::new(rise_resource_store::NoOpValidator),
            allowed_status_controller_ids: vec![],
        };

        // UID-form URL: body name need not match (typical UID-prefixed paths).
        assert!(assert_body_matches(
            &info,
            "rise.dev/v1alpha1",
            "Organization",
            "acme",
            "uid:00000000-0000-0000-0000-000000000000",
        )
        .is_ok());

        // Name-form URL: body name must match.
        assert!(
            assert_body_matches(&info, "rise.dev/v1alpha1", "Organization", "acme", "other",)
                .is_err()
        );

        // Wrong apiVersion.
        assert!(assert_body_matches(&info, "wrong/v1", "Organization", "acme", "acme",).is_err());

        // Wrong kind.
        assert!(
            assert_body_matches(&info, "rise.dev/v1alpha1", "Widget", "acme", "acme",).is_err()
        );
    }
}
