//! HTTP handlers for the generic resource API.
//!
//! The API is operator-only in v1 (`auth.operator_users`), except for the
//! controller-specific status/finalizer endpoints, which authenticate via the
//! `AnyAuth` extractor and are further gated to controllers listed in the
//! collection's `allowed_status_controller_ids` (default-deny on an empty
//! list). `Orphan` deletion and reparent are additionally gated to operators
//! who are also listed in `auth.admin_users`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rise_resource_api::{CreateResourceRequest, ResourceScope, UpdateResourceRequest};
use rise_resource_store::{
    CollectionInfo, CreateResourceParams, DeleteOutcome, PathSegment, PropagationPolicy,
    ResourceRow, ResourceStore, UpdateResourceParams,
};
use serde::Deserialize;
use uuid::Uuid;

use super::error_map::store_error_to_server_error;
use super::models::{
    row_to_resource, row_to_resource_with_api_version, ControllerFinalizerUpdate,
    ControllerStatusUpdate, ReparentRequest, ResourceList,
};
use super::path::{
    parse_identifier, parse_resource_path, AncestorRef, CollectionRef, ResourcePath, Subresource,
};
use crate::db::models::User;
use crate::server::auth::context::{AnyAuth, AuthContext};
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
    collection: &CollectionRef,
) -> Result<ResolvedCollection, ServerError> {
    let info = store
        .resolve_collection_version(&collection.group, &collection.version, &collection.plural)
        .await
        .map_err(store_error_to_server_error)?
        .ok_or_else(|| {
            ServerError::not_found(format!(
                "unknown collection '{}/{}/{}'",
                collection.group, collection.version, collection.plural
            ))
        })?;
    Ok(ResolvedCollection {
        collection: collection.plural.clone(),
        info,
    })
}

/// Resolve a leaf resource by building the full ancestor + leaf segment chain
/// and calling `store.resolve_path` once.
///
/// `ancestor_segs` must be the already-resolved ancestor segments (from
/// `resolve_ancestors`). Appends the leaf segment for `identifier` and passes
/// the complete chain to the store so ancestry validation is correct even for
/// depth-2+ paths.
async fn resolve_item(
    store: &Arc<dyn ResourceStore>,
    ancestor_segs: Vec<PathSegment>,
    info: &CollectionInfo,
    identifier: &str,
) -> Result<ResourceRow, ServerError> {
    let mut segs = ancestor_segs;
    segs.push(path_segment(
        &info.served_api_versions,
        &info.kind,
        identifier,
    )?);
    let chain = store
        .resolve_path(&segs)
        .await
        .map_err(store_error_to_server_error)?;
    chain
        .into_iter()
        .last()
        .ok_or_else(|| ServerError::not_found(format!("resource '{identifier}' not found")))
}

fn path_segment(
    api_versions: &[String],
    kind: &str,
    raw: &str,
) -> Result<PathSegment, ServerError> {
    match parse_identifier(
        api_versions.first().map(String::as_str).unwrap_or_default(),
        kind,
        raw,
    )? {
        PathSegment::Name { name, .. } => Ok(PathSegment::Name {
            api_versions: api_versions.to_vec(),
            kind: kind.to_string(),
            name,
        }),
        PathSegment::Uid { uid, .. } => Ok(PathSegment::Uid {
            api_versions: api_versions.to_vec(),
            kind: kind.to_string(),
            uid,
        }),
    }
}

/// Check that a collection's declared scope is consistent with its depth in the URL path.
///
/// Root-scoped collections must appear at the top level (no parent); any other
/// scope must appear under a parent. This check is driven by the collection's
/// own scope declaration and does not hardcode a particular non-root scope variant.
fn enforce_collection_depth(
    info: &CollectionInfo,
    has_ancestors: bool,
    collection: &str,
) -> Result<(), ServerError> {
    let is_root_scoped = info.scope == ResourceScope::Root;
    if !has_ancestors && !is_root_scoped {
        return Err(ServerError::bad_request(format!(
            "collection '{collection}' is not root-scoped but appears at root level"
        )));
    }
    if has_ancestors && is_root_scoped {
        return Err(ServerError::bad_request(format!(
            "collection '{collection}' is root-scoped and cannot appear under a parent resource"
        )));
    }
    Ok(())
}

fn enforce_parent_type(
    info: &CollectionInfo,
    parent: Option<&ResourceRow>,
    collection: &str,
) -> Result<(), ServerError> {
    match (&info.parent, parent) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(ServerError::bad_request(format!(
            "collection '{collection}' is root-scoped and cannot appear under a parent resource"
        ))),
        (Some(_), None) => Err(ServerError::bad_request(format!(
            "collection '{collection}' requires a parent resource"
        ))),
        (Some(expected), Some(parent)) => {
            if parent.api_version == expected.api_version && parent.kind == expected.kind {
                Ok(())
            } else {
                Err(ServerError::bad_request(format!(
                    "collection '{collection}' requires parent '{}/{}', got '{}/{}'",
                    expected.api_version, expected.kind, parent.api_version, parent.kind
                )))
            }
        }
    }
}

/// Authorize a controller token for status/finalizer writes against a
/// collection. The collection's `allowed_status_controller_ids` is the gate;
/// an empty list is default-deny. Built-in collections currently carry an
/// empty list, so controllers cannot write their status until a future phase
/// wires controller ownership for built-ins.
fn enforce_controller_allowed(
    info: &CollectionInfo,
    collection: &str,
    controller_id: &str,
) -> Result<(), ServerError> {
    if info
        .allowed_status_controller_ids
        .iter()
        .any(|id| id == controller_id)
    {
        return Ok(());
    }
    tracing::warn!(
        controller_id = %controller_id,
        kind = %info.kind,
        "Controller status/finalizer write denied — controller not in collection's allowed_status_controller_ids"
    );
    Err(ServerError::forbidden(format!(
        "controller '{controller_id}' is not authorized to write status or finalizers \
         for collection '{collection}'"
    )))
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
// Ancestor resolution helper
// -----------------------------------------------------------------------------

/// Resolve a sequence of ancestor `AncestorRef` values to `PathSegment`s.
///
/// Each ancestor's scope is checked against its position in the path: the first
/// ancestor must be root-scoped; any further ancestors must not be root-scoped.
/// The returned segments can be passed directly to `ResourceStore::resolve_path`.
async fn resolve_ancestors(
    store: &Arc<dyn ResourceStore>,
    ancestors: &[AncestorRef],
) -> Result<(Vec<PathSegment>, Option<ResourceRow>), ServerError> {
    let mut segs = Vec::with_capacity(ancestors.len());
    for (i, anc) in ancestors.iter().enumerate() {
        let resolved = resolve_collection(store, &anc.collection).await?;
        enforce_collection_depth(&resolved.info, i > 0, &resolved.collection)?;
        let parent = if segs.is_empty() {
            None
        } else {
            store
                .resolve_path(&segs)
                .await
                .map_err(store_error_to_server_error)?
                .into_iter()
                .last()
        };
        enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
        segs.push(path_segment(
            &resolved.info.served_api_versions,
            &resolved.info.kind,
            &anc.identifier,
        )?);
    }
    let parent = if segs.is_empty() {
        None
    } else {
        store
            .resolve_path(&segs)
            .await
            .map_err(store_error_to_server_error)?
            .into_iter()
            .last()
    };
    Ok((segs, parent))
}

// -----------------------------------------------------------------------------
// Dispatch handlers
// -----------------------------------------------------------------------------

pub async fn dispatch_get(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    auth: AuthContext,
    Query(q): Query<OrphansQuery>,
) -> Result<Response, ServerError> {
    let path = parse_resource_path(&raw)?;
    match path {
        ResourcePath::Orphans => {
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
            let items: Vec<rise_resource_api::Resource> =
                rows.iter().map(row_to_resource).collect();
            Ok(Json(serde_json::json!({ "items": items })).into_response())
        }
        ResourcePath::List {
            ancestors,
            collection,
        } => {
            let _user = require_operator(&state, &auth)?;
            let (_ancestor_segs, parent) =
                resolve_ancestors(&state.resource_store, &ancestors).await?;
            let parent_uid = parent.as_ref().map(|r| r.uid);
            let resolved = resolve_collection(&state.resource_store, &collection).await?;
            enforce_collection_depth(&resolved.info, !ancestors.is_empty(), &resolved.collection)?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let rows = state
                .resource_store
                .list_versions(
                    &resolved.info.served_api_versions,
                    &resolved.info.kind,
                    parent_uid,
                )
                .await
                .map_err(store_error_to_server_error)?;
            Ok(Json(ResourceList {
                api_version: resolved.info.api_version.clone(),
                kind: resolved.info.kind,
                items: rows
                    .iter()
                    .map(|row| row_to_resource_with_api_version(row, &resolved.info.api_version))
                    .collect(),
            })
            .into_response())
        }
        ResourcePath::Item {
            ancestors,
            collection,
            identifier,
        } => {
            let _user = require_operator(&state, &auth)?;
            let (ancestor_segs, parent) =
                resolve_ancestors(&state.resource_store, &ancestors).await?;
            let resolved = resolve_collection(&state.resource_store, &collection).await?;
            enforce_collection_depth(&resolved.info, !ancestors.is_empty(), &resolved.collection)?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let row = resolve_item(
                &state.resource_store,
                ancestor_segs,
                &resolved.info,
                &identifier,
            )
            .await?;
            Ok(Json(row_to_resource_with_api_version(
                &row,
                &resolved.info.api_version,
            ))
            .into_response())
        }
        ResourcePath::Subresource { .. } => Err(ServerError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "GET is not supported for subresource paths",
        )),
    }
}

pub async fn dispatch_post(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    auth: AuthContext,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ServerError> {
    let path = parse_resource_path(&raw)?;
    match path {
        ResourcePath::List {
            ancestors,
            collection,
        } => {
            let user = require_operator(&state, &auth)?;
            let body: CreateResourceRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let (_ancestor_segs, parent) =
                resolve_ancestors(&state.resource_store, &ancestors).await?;
            let parent_uid = parent.as_ref().map(|r| r.uid);
            let resolved = resolve_collection(&state.resource_store, &collection).await?;
            enforce_collection_depth(&resolved.info, !ancestors.is_empty(), &resolved.collection)?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let (status, resource) =
                create_resource(&state, &resolved, parent_uid, body, &user).await?;
            Ok((status, resource).into_response())
        }
        ResourcePath::Subresource {
            ancestors,
            collection,
            identifier,
            subresource: Subresource::Reparent,
        } => {
            let user = require_admin_operator(&state, &auth)?;
            let body: ReparentRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let (ancestor_segs, parent) =
                resolve_ancestors(&state.resource_store, &ancestors).await?;
            let resolved = resolve_collection(&state.resource_store, &collection).await?;
            enforce_collection_depth(&resolved.info, !ancestors.is_empty(), &resolved.collection)?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let row = resolve_item(
                &state.resource_store,
                ancestor_segs,
                &resolved.info,
                &identifier,
            )
            .await?;
            let resp = apply_reparent(&state, &resolved.info, &row, body, &user).await?;
            Ok(resp.into_response())
        }
        _ => Err(ServerError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "POST is only valid for collection paths and reparent",
        )),
    }
}

pub async fn dispatch_put(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    auth: AnyAuth,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ServerError> {
    let path = parse_resource_path(&raw)?;
    match path {
        ResourcePath::Item {
            ancestors,
            collection,
            identifier,
        } => {
            // Controller tokens must not update items
            let auth_ctx = match &auth {
                AnyAuth::User(ctx) => ctx,
                AnyAuth::Controller(_) => {
                    return Err(ServerError::forbidden(
                        "controller tokens cannot update resource items",
                    ));
                }
            };
            let user = require_operator(&state, auth_ctx)?;
            let body: UpdateResourceRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let (ancestor_segs, parent) =
                resolve_ancestors(&state.resource_store, &ancestors).await?;
            let resolved = resolve_collection(&state.resource_store, &collection).await?;
            enforce_collection_depth(&resolved.info, !ancestors.is_empty(), &resolved.collection)?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let row = resolve_item(
                &state.resource_store,
                ancestor_segs,
                &resolved.info,
                &identifier,
            )
            .await?;
            let resp = update_resource(&state, &resolved, &row, &identifier, body, &user).await?;
            Ok(resp.into_response())
        }
        ResourcePath::Subresource {
            ancestors,
            collection,
            identifier,
            subresource,
        } => {
            // User tokens cannot update status/finalizers
            let controller = match &auth {
                AnyAuth::Controller(ctrl) => ctrl,
                AnyAuth::User(_) => {
                    return Err(ServerError::unauthorized(
                        "controller authentication required for status/finalizer updates",
                    ));
                }
            };
            let (ancestor_segs, parent) =
                resolve_ancestors(&state.resource_store, &ancestors).await?;
            let resolved = resolve_collection(&state.resource_store, &collection).await?;
            enforce_collection_depth(&resolved.info, !ancestors.is_empty(), &resolved.collection)?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            enforce_controller_allowed(
                &resolved.info,
                &resolved.collection,
                &controller.0.identity_id,
            )?;
            let row = resolve_item(
                &state.resource_store,
                ancestor_segs,
                &resolved.info,
                &identifier,
            )
            .await?;
            match subresource {
                Subresource::Status => {
                    let body: ControllerStatusUpdate =
                        serde_json::from_value(body).map_err(|e| {
                            ServerError::bad_request(format!("invalid request body: {e}"))
                        })?;
                    let resp = apply_controller_status(
                        &state,
                        controller,
                        &row,
                        body,
                        &resolved.info.api_version,
                    )
                    .await?;
                    Ok(resp.into_response())
                }
                Subresource::Finalizers => {
                    let body: ControllerFinalizerUpdate =
                        serde_json::from_value(body).map_err(|e| {
                            ServerError::bad_request(format!("invalid request body: {e}"))
                        })?;
                    let resp = apply_controller_finalizers(
                        &state,
                        controller,
                        &row,
                        body,
                        &resolved.info.api_version,
                    )
                    .await?;
                    Ok(resp.into_response())
                }
                Subresource::Reparent => Err(ServerError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "PUT is not valid for reparent",
                )),
            }
        }
        _ => Err(ServerError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "PUT is only valid for item and subresource paths",
        )),
    }
}

pub async fn dispatch_delete(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    auth: AuthContext,
    Query(q): Query<DeleteQuery>,
) -> Result<Response, ServerError> {
    let path = parse_resource_path(&raw)?;
    match path {
        ResourcePath::Item {
            ancestors,
            collection,
            identifier,
        } => {
            let policy = parse_propagation(q.propagation_policy)?;
            let user = if matches!(policy, PropagationPolicy::Orphan) {
                require_admin_operator(&state, &auth)?
            } else {
                require_operator(&state, &auth)?
            };
            let (ancestor_segs, parent) =
                resolve_ancestors(&state.resource_store, &ancestors).await?;
            let resolved = resolve_collection(&state.resource_store, &collection).await?;
            enforce_collection_depth(&resolved.info, !ancestors.is_empty(), &resolved.collection)?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let row = resolve_item(
                &state.resource_store,
                ancestor_segs,
                &resolved.info,
                &identifier,
            )
            .await?;
            let resp =
                delete_resource(&state, &row, policy, &user, &resolved.info.api_version).await?;
            Ok(resp.into_response())
        }
        _ => Err(ServerError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "DELETE is only valid for item paths",
        )),
    }
}

// -----------------------------------------------------------------------------
// Shared helpers (write paths)
// -----------------------------------------------------------------------------

async fn create_resource(
    state: &AppState,
    resolved: &ResolvedCollection,
    parent_uid: Option<Uuid>,
    body: CreateResourceRequest,
    user: &User,
) -> Result<(StatusCode, Json<rise_resource_api::Resource>), ServerError> {
    // POST creates have no URL name segment — only validate type identity.
    if body.api_version != resolved.info.api_version {
        return Err(ServerError::bad_request(format!(
            "body apiVersion '{}' does not match collection ({})",
            body.api_version, resolved.info.api_version
        )));
    }
    if body.kind != resolved.info.kind {
        return Err(ServerError::bad_request(format!(
            "body kind '{}' does not match collection ({})",
            body.kind, resolved.info.kind
        )));
    }

    let annotations: BTreeMap<String, String> = body.metadata.annotations.clone();
    let spec = serde_json::to_value(&body.spec)
        .map_err(|e| ServerError::bad_request(format!("invalid spec: {e}")))?;

    let params = CreateResourceParams {
        api_version: resolved.info.storage_api_version.clone(),
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

    tracing::info!(
        target: "rise::audit",
        actor = %user.email,
        uid = %row.uid,
        api_version = %row.api_version,
        kind = %row.kind,
        name = %row.name,
        parent_uid = ?row.parent_uid,
        "resource.created"
    );
    Ok((
        StatusCode::CREATED,
        Json(row_to_resource_with_api_version(
            &row,
            &resolved.info.api_version,
        )),
    ))
}

async fn update_resource(
    state: &AppState,
    resolved: &ResolvedCollection,
    row: &ResourceRow,
    url_name: &str,
    body: UpdateResourceRequest,
    user: &User,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    assert_body_matches(
        &resolved.info,
        &body.api_version,
        &body.kind,
        &body.metadata.name,
        url_name,
    )?;

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
        api_version: Some(resolved.info.storage_api_version.clone()),
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

    tracing::info!(
        target: "rise::audit",
        actor = %user.email,
        uid = %updated.uid,
        api_version = %updated.api_version,
        kind = %updated.kind,
        name = %updated.name,
        revision = updated.revision,
        "resource.updated"
    );
    Ok(Json(row_to_resource_with_api_version(
        &updated,
        &resolved.info.api_version,
    )))
}

async fn delete_resource(
    state: &AppState,
    row: &ResourceRow,
    policy: PropagationPolicy,
    user: &User,
    response_api_version: &str,
) -> Result<Json<serde_json::Value>, ServerError> {
    let outcome = state
        .resource_store
        .delete(row.uid, policy)
        .await
        .map_err(store_error_to_server_error)?;

    let event = match policy {
        PropagationPolicy::Orphan => "resource.orphan_delete",
        PropagationPolicy::Cascade => "resource.deleted",
    };
    tracing::info!(
        target: "rise::audit",
        actor = %user.email,
        uid = %row.uid,
        api_version = %row.api_version,
        kind = %row.kind,
        name = %row.name,
        propagation_policy = ?policy,
        "{event}"
    );

    let body = match outcome {
        DeleteOutcome::Deleted => serde_json::json!({"deleted": true, "uid": row.uid}),
        DeleteOutcome::MarkedForDeletion(marked) => serde_json::json!({
            "deleted": false,
            "markedForDeletion": true,
            "resource": row_to_resource_with_api_version(&marked, response_api_version),
        }),
    };
    Ok(Json(body))
}

async fn apply_controller_status(
    state: &AppState,
    controller: &ControllerAuthContext,
    row: &ResourceRow,
    body: ControllerStatusUpdate,
    response_api_version: &str,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = state
        .resource_store
        .update_controller_status(row.uid, &controller.0.identity_id, body.status)
        .await
        .map_err(store_error_to_server_error)?;
    Ok(Json(row_to_resource_with_api_version(
        &updated,
        response_api_version,
    )))
}

async fn apply_controller_finalizers(
    state: &AppState,
    controller: &ControllerAuthContext,
    row: &ResourceRow,
    body: ControllerFinalizerUpdate,
    response_api_version: &str,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = state
        .resource_store
        .update_controller_finalizers(row.uid, &controller.0.identity_id, &body.add, &body.remove)
        .await
        .map_err(store_error_to_server_error)?;
    Ok(Json(row_to_resource_with_api_version(
        &updated,
        response_api_version,
    )))
}

async fn apply_reparent(
    state: &AppState,
    info: &CollectionInfo,
    row: &ResourceRow,
    body: ReparentRequest,
    user: &User,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = state
        .resource_store
        .reparent(row.uid, body.new_parent_uid, info.scope.clone())
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
    Ok(Json(row_to_resource_with_api_version(
        &updated,
        &info.api_version,
    )))
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
            storage_api_version: "rise.dev/v1alpha1".into(),
            served_api_versions: vec!["rise.dev/v1alpha1".into()],
            kind: "Organization".into(),
            scope: ResourceScope::Root,
            parent: None,
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

    fn collection_info(allowed: Vec<String>) -> CollectionInfo {
        CollectionInfo {
            api_version: "example.dev/v1".into(),
            storage_api_version: "example.dev/v1".into(),
            served_api_versions: vec!["example.dev/v1".into()],
            kind: "Widget".into(),
            scope: ResourceScope::Root,
            parent: None,
            spec_validator: std::sync::Arc::new(rise_resource_store::NoOpValidator),
            allowed_status_controller_ids: allowed,
        }
    }

    #[test]
    fn enforce_controller_allowed_permits_listed_controller() {
        let info = collection_info(vec!["controller.example.com".into()]);
        assert!(enforce_controller_allowed(&info, "widgets", "controller.example.com").is_ok());
    }

    #[test]
    fn enforce_controller_allowed_rejects_unlisted_controller() {
        let info = collection_info(vec!["controller.example.com".into()]);
        let err = enforce_controller_allowed(&info, "widgets", "other.example.com").unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert!(err.message.contains("not authorized"));
    }

    #[test]
    fn enforce_controller_allowed_default_denies_empty_allowlist() {
        // Built-in collections carry an empty allowlist — default-deny.
        let info = collection_info(vec![]);
        let err =
            enforce_controller_allowed(&info, "widgets", "controller.example.com").unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }
}
