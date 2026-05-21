//! HTTP handlers for the generic resource API.
//!
//! The API is operator-only in v1 (`auth.operator_users`), except for the
//! controller-specific status/finalizer endpoints, which authenticate via the
//! `AnyAuth` extractor and are further gated to controllers listed in the
//! collection's `allowed_status_controller_ids` (default-deny on an empty
//! list).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rise_resource_api::{CreateResourceRequest, UpdateResourceRequest};
use rise_resource_store::{
    CollectionInfo, CreateResourceParams, DeleteOutcome, PathSegment, ResourceRow, ResourceStore,
    UpdateResourceParams,
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
// Dispatch context
// -----------------------------------------------------------------------------

/// The slice of `AppState` the generic resource API actually consumes.
///
/// Each `dispatch_*` HTTP handler builds one of these from the full `AppState`
/// and delegates to a `dispatch_*_inner` function. Keeping the dispatch logic
/// behind this small context (rather than the 30-plus-field `AppState`) lets
/// the router/auth behaviour be exercised in tests with only a resource store
/// and the two role allowlists.
#[derive(Clone)]
pub(crate) struct ResourceApiCtx {
    store: Arc<dyn ResourceStore>,
    operator_users: Arc<Vec<String>>,
}

impl ResourceApiCtx {
    fn from_state(state: &AppState) -> Self {
        Self {
            store: state.resource_store.clone(),
            operator_users: state.operator_users.clone(),
        }
    }

    fn is_operator(&self, email: &str) -> bool {
        crate::server::auth::admin::is_operator_user(&self.operator_users, email)
    }
}

// -----------------------------------------------------------------------------
// Authorization helpers
// -----------------------------------------------------------------------------

/// Require an operator-authenticated user. Service-account/controller tokens
/// and non-operator users get 401/403 respectively.
fn require_operator(ctx: &ResourceApiCtx, auth: &AuthContext) -> Result<User, ServerError> {
    let user = auth.user()?.clone();
    if !ctx.is_operator(&user.email) {
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
        &info.declared_api_versions,
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

/// Extract the API group from an `apiVersion` string.
///
/// `apiVersion` is `<group>/<version>` (e.g. `rise.dev/v1alpha1` → `rise.dev`).
/// If no `/` is present the whole string is treated as the group.
fn api_group(api_version: &str) -> &str {
    api_version.split('/').next().unwrap_or(api_version)
}

/// Check that a collection's declared parent type is consistent with the parent
/// resolved from the URL path.
///
/// This single check covers every case: a root-scoped collection (no declared
/// parent) must have no parent in the path, and a collection that declares a
/// parent must appear under a resource of that parent's API **group** and
/// **kind**. The parent's version is intentionally ignored — a parent resource
/// may be stored at an older served version than the one the child's
/// `ResourceDefinition` declares, so an exact `api_version` match would wrongly
/// reject otherwise-valid children.
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
            let expected_group = api_group(&expected.api_version);
            let parent_group = api_group(&parent.api_version);
            if parent_group == expected_group && parent.kind == expected.kind {
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

/// Query parameters for `GET .../pending-deletion`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingDeletionQuery {
    /// Maximum number of tombstoned resources to return (default 100).
    #[serde(default)]
    pub limit: Option<i64>,
}

// -----------------------------------------------------------------------------
// Ancestor resolution helper
// -----------------------------------------------------------------------------

/// Resolve a sequence of ancestor `AncestorRef` values to `PathSegment`s.
///
/// Each ancestor's declared parent type is checked against its position in the
/// path: the first ancestor must be root-scoped; any further ancestor must sit
/// under a parent of its declared parent type. The returned segments can be
/// passed directly to `ResourceStore::resolve_path`.
///
/// The full ancestor path is resolved with a single `store.resolve_path` call
/// (rather than once per ancestor prefix), so the cost is O(n) row lookups
/// across one transaction instead of O(n^2) across n transactions.
async fn resolve_ancestors(
    store: &Arc<dyn ResourceStore>,
    ancestors: &[AncestorRef],
) -> Result<(Vec<PathSegment>, Option<ResourceRow>), ServerError> {
    // Step 1: resolve every ancestor collection and build the full segment
    // chain. No `resolve_path` calls happen in this loop.
    let mut segs = Vec::with_capacity(ancestors.len());
    let mut infos = Vec::with_capacity(ancestors.len());
    for anc in ancestors {
        let resolved = resolve_collection(store, &anc.collection).await?;
        segs.push(path_segment(
            &resolved.info.declared_api_versions,
            &resolved.info.kind,
            &anc.identifier,
        )?);
        infos.push(resolved);
    }

    if segs.is_empty() {
        return Ok((segs, None));
    }

    // Step 2: resolve the complete ancestor chain in one call. On success the
    // chain has exactly one row per segment; a missing ancestor surfaces as a
    // 404 from the store.
    let chain = store
        .resolve_path(&segs)
        .await
        .map_err(store_error_to_server_error)?;

    // Step 3: walk the chain to validate parent typing at each level. Ancestor
    // `i`'s parent is `chain[i - 1]`; ancestor 0 has no parent.
    for (i, resolved) in infos.iter().enumerate() {
        let parent = i.checked_sub(1).and_then(|p| chain.get(p));
        enforce_parent_type(&resolved.info, parent, &resolved.collection)?;
    }

    let parent = chain.into_iter().last();
    Ok((segs, parent))
}

// -----------------------------------------------------------------------------
// Dispatch handlers
// -----------------------------------------------------------------------------

pub async fn dispatch_get(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    auth: AuthContext,
    Query(q): Query<PendingDeletionQuery>,
) -> Result<Response, ServerError> {
    dispatch_get_inner(&ResourceApiCtx::from_state(&state), raw, auth, q).await
}

async fn dispatch_get_inner(
    ctx: &ResourceApiCtx,
    raw: String,
    auth: AuthContext,
    q: PendingDeletionQuery,
) -> Result<Response, ServerError> {
    let path = parse_resource_path(&raw)?;
    match path {
        ResourcePath::PendingDeletion => {
            let user = require_operator(ctx, &auth)?;
            let limit = q.limit.unwrap_or(100).clamp(1, 1000);
            let rows = ctx
                .store
                .list_pending_collection(limit)
                .await
                .map_err(store_error_to_server_error)?;
            tracing::info!(
                target: "rise::audit",
                actor = %user.email,
                count = rows.len(),
                "resource.pending_deletion_listed"
            );
            let items: Vec<rise_resource_api::Resource> =
                rows.iter().map(row_to_resource).collect();
            Ok(Json(serde_json::json!({ "items": items })).into_response())
        }
        ResourcePath::List {
            ancestors,
            collection,
        } => {
            let _user = require_operator(ctx, &auth)?;
            let (_ancestor_segs, parent) = resolve_ancestors(&ctx.store, &ancestors).await?;
            let parent_uid = parent.as_ref().map(|r| r.uid);
            let resolved = resolve_collection(&ctx.store, &collection).await?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let rows = ctx
                .store
                .list_versions(
                    &resolved.info.declared_api_versions,
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
            let _user = require_operator(ctx, &auth)?;
            let (ancestor_segs, parent) = resolve_ancestors(&ctx.store, &ancestors).await?;
            let resolved = resolve_collection(&ctx.store, &collection).await?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let row = resolve_item(&ctx.store, ancestor_segs, &resolved.info, &identifier).await?;
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
    dispatch_post_inner(&ResourceApiCtx::from_state(&state), raw, auth, body).await
}

async fn dispatch_post_inner(
    ctx: &ResourceApiCtx,
    raw: String,
    auth: AuthContext,
    body: serde_json::Value,
) -> Result<Response, ServerError> {
    let path = parse_resource_path(&raw)?;
    match path {
        ResourcePath::List {
            ancestors,
            collection,
        } => {
            let user = require_operator(ctx, &auth)?;
            let body: CreateResourceRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let (_ancestor_segs, parent) = resolve_ancestors(&ctx.store, &ancestors).await?;
            let parent_uid = parent.as_ref().map(|r| r.uid);
            let resolved = resolve_collection(&ctx.store, &collection).await?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let (status, resource) =
                create_resource(ctx, &resolved, parent_uid, body, &user).await?;
            Ok((status, resource).into_response())
        }
        ResourcePath::Subresource {
            ancestors,
            collection,
            identifier,
            subresource: Subresource::Reparent,
        } => {
            let user = require_operator(ctx, &auth)?;
            let body: ReparentRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let (ancestor_segs, parent) = resolve_ancestors(&ctx.store, &ancestors).await?;
            let resolved = resolve_collection(&ctx.store, &collection).await?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let row = resolve_item(&ctx.store, ancestor_segs, &resolved.info, &identifier).await?;
            let resp = apply_reparent(ctx, &resolved.info, &row, body, &user).await?;
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
    dispatch_put_inner(&ResourceApiCtx::from_state(&state), raw, auth, body).await
}

async fn dispatch_put_inner(
    ctx: &ResourceApiCtx,
    raw: String,
    auth: AnyAuth,
    body: serde_json::Value,
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
                AnyAuth::User(c) => c,
                AnyAuth::Controller(_) => {
                    return Err(ServerError::forbidden(
                        "controller tokens cannot update resource items",
                    ));
                }
            };
            let user = require_operator(ctx, auth_ctx)?;
            let body: UpdateResourceRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let (ancestor_segs, parent) = resolve_ancestors(&ctx.store, &ancestors).await?;
            let resolved = resolve_collection(&ctx.store, &collection).await?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let row = resolve_item(&ctx.store, ancestor_segs, &resolved.info, &identifier).await?;
            let resp = update_resource(ctx, &resolved, &row, &identifier, body, &user).await?;
            Ok(resp.into_response())
        }
        ResourcePath::Subresource {
            ancestors,
            collection,
            identifier,
            subresource,
        } => {
            // `reparent` is POST-only; reject PUT before auth discrimination so
            // every caller gets a consistent 405 (not a 403 about controllers).
            if matches!(subresource, Subresource::Reparent) {
                return Err(ServerError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "PUT is not valid for reparent; use POST",
                ));
            }
            // User tokens cannot update status/finalizers. The caller is
            // authenticated (a user/SA token), so this is an authorization
            // failure (403), not an authentication failure (401).
            let controller = match &auth {
                AnyAuth::Controller(ctrl) => ctrl,
                AnyAuth::User(_) => {
                    return Err(ServerError::forbidden(
                        "controller authentication required for status/finalizer updates",
                    ));
                }
            };
            let (ancestor_segs, parent) = resolve_ancestors(&ctx.store, &ancestors).await?;
            let resolved = resolve_collection(&ctx.store, &collection).await?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            enforce_controller_allowed(
                &resolved.info,
                &resolved.collection,
                &controller.0.identity_id,
            )?;
            let row = resolve_item(&ctx.store, ancestor_segs, &resolved.info, &identifier).await?;
            match subresource {
                Subresource::Status => {
                    let body: ControllerStatusUpdate =
                        serde_json::from_value(body).map_err(|e| {
                            ServerError::bad_request(format!("invalid request body: {e}"))
                        })?;
                    let resp = apply_controller_status(
                        ctx,
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
                        ctx,
                        controller,
                        &row,
                        body,
                        &resolved.info.api_version,
                    )
                    .await?;
                    Ok(resp.into_response())
                }
                Subresource::Reparent => unreachable!("handled above"),
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
) -> Result<Response, ServerError> {
    dispatch_delete_inner(&ResourceApiCtx::from_state(&state), raw, auth).await
}

async fn dispatch_delete_inner(
    ctx: &ResourceApiCtx,
    raw: String,
    auth: AuthContext,
) -> Result<Response, ServerError> {
    let path = parse_resource_path(&raw)?;
    match path {
        ResourcePath::Item {
            ancestors,
            collection,
            identifier,
        } => {
            let user = require_operator(ctx, &auth)?;
            let (ancestor_segs, parent) = resolve_ancestors(&ctx.store, &ancestors).await?;
            let resolved = resolve_collection(&ctx.store, &collection).await?;
            enforce_parent_type(&resolved.info, parent.as_ref(), &resolved.collection)?;
            let row = resolve_item(&ctx.store, ancestor_segs, &resolved.info, &identifier).await?;
            let resp = delete_resource(ctx, &row, &user, &resolved.info.api_version).await?;
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
    ctx: &ResourceApiCtx,
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
        ctx.store
            .register_resource_definition(params)
            .await
            .map_err(store_error_to_server_error)?
    } else {
        ctx.store
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
    ctx: &ResourceApiCtx,
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
        ctx.store
            .update_resource_definition(row.uid, params)
            .await
            .map_err(store_error_to_server_error)?
    } else {
        ctx.store
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
    ctx: &ResourceApiCtx,
    row: &ResourceRow,
    user: &User,
    response_api_version: &str,
) -> Result<Json<serde_json::Value>, ServerError> {
    let outcome = ctx
        .store
        .delete(row.uid)
        .await
        .map_err(store_error_to_server_error)?;

    // A single static event message keeps this audit log consistent with
    // `resource.created` / `resource.updated`.
    tracing::info!(
        target: "rise::audit",
        actor = %user.email,
        uid = %row.uid,
        api_version = %row.api_version,
        kind = %row.kind,
        name = %row.name,
        "resource.deleted"
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
    ctx: &ResourceApiCtx,
    controller: &ControllerAuthContext,
    row: &ResourceRow,
    body: ControllerStatusUpdate,
    response_api_version: &str,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = ctx
        .store
        .update_controller_status(row.uid, &controller.0.identity_id, body.status)
        .await
        .map_err(store_error_to_server_error)?;
    Ok(Json(row_to_resource_with_api_version(
        &updated,
        response_api_version,
    )))
}

async fn apply_controller_finalizers(
    ctx: &ResourceApiCtx,
    controller: &ControllerAuthContext,
    row: &ResourceRow,
    body: ControllerFinalizerUpdate,
    response_api_version: &str,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = ctx
        .store
        .update_controller_finalizers(row.uid, &controller.0.identity_id, &body.add, &body.remove)
        .await
        .map_err(store_error_to_server_error)?;
    Ok(Json(row_to_resource_with_api_version(
        &updated,
        response_api_version,
    )))
}

async fn apply_reparent(
    ctx: &ResourceApiCtx,
    info: &CollectionInfo,
    row: &ResourceRow,
    body: ReparentRequest,
    user: &User,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = ctx
        .store
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
    Ok(Json(row_to_resource_with_api_version(
        &updated,
        &info.api_version,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rise_resource_api::{ResourceParentRef, ResourceScope};

    #[test]
    fn assert_body_matches_uid_url_does_not_enforce_name() {
        let info = CollectionInfo {
            api_version: "rise.dev/v1alpha1".into(),
            storage_api_version: "rise.dev/v1alpha1".into(),
            served_api_versions: vec!["rise.dev/v1alpha1".into()],
            declared_api_versions: vec!["rise.dev/v1alpha1".into()],
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
            declared_api_versions: vec!["example.dev/v1".into()],
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

    #[test]
    fn api_group_extracts_group_before_slash() {
        assert_eq!(api_group("rise.dev/v1alpha1"), "rise.dev");
        assert_eq!(api_group("example.dev/v2"), "example.dev");
        // No slash — the whole string is the group.
        assert_eq!(api_group("plaingroup"), "plaingroup");
        assert_eq!(api_group(""), "");
    }

    /// `CollectionInfo` for a child collection that declares `parent`.
    fn child_collection_info(parent: Option<ResourceParentRef>) -> CollectionInfo {
        CollectionInfo {
            api_version: "example.dev/v2".into(),
            storage_api_version: "example.dev/v2".into(),
            served_api_versions: vec!["example.dev/v2".into()],
            declared_api_versions: vec!["example.dev/v2".into()],
            kind: "Gadget".into(),
            scope: if parent.is_some() {
                ResourceScope::Organization
            } else {
                ResourceScope::Root
            },
            parent,
            spec_validator: std::sync::Arc::new(rise_resource_store::NoOpValidator),
            allowed_status_controller_ids: vec![],
        }
    }

    fn parent_row(api_version: &str, kind: &str) -> ResourceRow {
        ResourceRow {
            uid: Uuid::new_v4(),
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            parent_uid: None,
            name: "p".into(),
            discriminator: "abcd1234".into(),
            metadata: serde_json::json!({}),
            spec: serde_json::json!({}),
            status: serde_json::json!({}),
            revision: 1,
            finalizers: vec![],
            deletion_timestamp: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn enforce_parent_type_root_scoped_accepts_no_parent() {
        let info = child_collection_info(None);
        assert!(enforce_parent_type(&info, None, "gadgets").is_ok());
    }

    #[test]
    fn enforce_parent_type_root_scoped_rejects_a_parent() {
        let info = child_collection_info(None);
        let parent = parent_row("rise.dev/v1alpha1", "Organization");
        let err = enforce_parent_type(&info, Some(&parent), "gadgets").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn enforce_parent_type_scoped_requires_a_parent() {
        let info = child_collection_info(Some(ResourceParentRef {
            api_version: "rise.dev/v1alpha1".into(),
            kind: "Organization".into(),
        }));
        let err = enforce_parent_type(&info, None, "gadgets").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("requires a parent"));
    }

    #[test]
    fn enforce_parent_type_matches_on_group_and_kind() {
        let info = child_collection_info(Some(ResourceParentRef {
            api_version: "rise.dev/v1alpha1".into(),
            kind: "Organization".into(),
        }));
        // Exact group + kind match.
        let parent = parent_row("rise.dev/v1alpha1", "Organization");
        assert!(enforce_parent_type(&info, Some(&parent), "gadgets").is_ok());
    }

    #[test]
    fn enforce_parent_type_ignores_parent_version() {
        // Fix B: a parent stored at an older served version than the one the
        // child's ResourceDefinition declares must still be accepted — only the
        // group + kind matter.
        let info = child_collection_info(Some(ResourceParentRef {
            api_version: "example.dev/v2".into(),
            kind: "Folder".into(),
        }));
        let parent = parent_row("example.dev/v1", "Folder");
        assert!(
            enforce_parent_type(&info, Some(&parent), "gadgets").is_ok(),
            "parent at older version example.dev/v1 should satisfy declared example.dev/v2"
        );
    }

    #[test]
    fn enforce_parent_type_rejects_wrong_group() {
        let info = child_collection_info(Some(ResourceParentRef {
            api_version: "rise.dev/v1alpha1".into(),
            kind: "Organization".into(),
        }));
        // Same kind, different group — must be rejected.
        let parent = parent_row("other.dev/v1alpha1", "Organization");
        let err = enforce_parent_type(&info, Some(&parent), "gadgets").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("requires parent"));
    }

    #[test]
    fn enforce_parent_type_rejects_wrong_kind() {
        let info = child_collection_info(Some(ResourceParentRef {
            api_version: "rise.dev/v1alpha1".into(),
            kind: "Organization".into(),
        }));
        // Same group, different kind — must be rejected.
        let parent = parent_row("rise.dev/v1alpha1", "Project");
        let err = enforce_parent_type(&info, Some(&parent), "gadgets").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }
}

/// DB-backed tests that drive the generic resource API through the
/// `dispatch_*_inner` functions — the same code path the four Axum handlers
/// run, minus only the `State`/`Path`/`Query`/`Json` extraction.
///
/// `AppState` has ~30 fields and no test constructor (it wires AWS, OAuth and
/// Kubernetes clients), so a full `Router` + `AppState` is impractical here and
/// no other `src/server/` test builds one. Instead these tests construct a
/// `ResourceApiCtx` — the slice of `AppState` the resource API actually uses (a
/// real `PgResourceStore` plus the two role allowlists) — and exercise routing,
/// auth tiers, versioning and error mapping end to end against Postgres.
#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use rise_resource_api::RESOURCE_DEFINITION_KIND;
    use rise_resource_store::PgResourceStore;
    use serde_json::{json, Value};

    const OPERATOR: &str = "operator@example.com";
    const PLAIN_USER: &str = "plain-user@example.com";

    /// Build a `ResourceApiCtx` over a real `PgResourceStore`. The resource
    /// store schema is layered on top of the root migrations `#[sqlx::test]`
    /// already ran.
    async fn ctx(pool: sqlx::PgPool) -> ResourceApiCtx {
        rise_resource_store::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        ResourceApiCtx {
            store: Arc::new(PgResourceStore::new(pool)),
            operator_users: Arc::new(vec![OPERATOR.into()]),
        }
    }

    /// A `User`-backed `AuthContext`. `User` rows do not need to exist in the
    /// DB — the resource API authorizes purely on the email allowlists.
    fn auth(email: &str) -> AuthContext {
        AuthContext::User(User {
            id: Uuid::new_v4(),
            email: email.to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
    }

    /// An `AnyAuth` carrying a user token (operator).
    fn any_user(email: &str) -> AnyAuth {
        AnyAuth::User(auth(email))
    }

    /// An `AnyAuth` carrying a controller token with the given controller id.
    fn any_controller(id: &str) -> AnyAuth {
        AnyAuth::Controller(ControllerAuthContext(
            crate::server::auth::controller::VerifiedControllerToken {
                identity_id: id.to_string(),
                issuer: "https://issuer.example.com".into(),
                claims: json!({}),
            },
        ))
    }

    /// Read a `Response` into `(status, json_body)`.
    async fn read(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("response body");
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, body)
    }

    /// Register a root-scoped `widgets` collection (group `example.dev`) served
    /// at both `v1` and `v2`, with `v1` as the storage version. `allowed`
    /// becomes the collection's `allowedStatusControllerIds`.
    async fn register_widget_rd(ctx: &ResourceApiCtx, allowed: &[&str]) {
        let allowed: Vec<String> = allowed.iter().map(|s| s.to_string()).collect();
        let spec = json!({
            "group": "example.dev",
            "kind": "Widget",
            "plural": "widgets",
            "scope": "root",
            "versions": [
                {"name": "v1", "served": true, "storage": true},
                {"name": "v2", "served": true, "storage": false},
            ],
            "allowedStatusControllerIds": allowed,
        });
        ctx.store
            .register_resource_definition(CreateResourceParams {
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: "widgets.example.dev".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec,
                validator: None,
            })
            .await
            .expect("register widgets RD");
    }

    /// Register an Organization-scoped `gadgets` collection whose declared
    /// parent is the built-in `rise.dev/v1alpha1` `Organization`.
    async fn register_gadget_rd(ctx: &ResourceApiCtx) {
        let spec = json!({
            "group": "example.dev",
            "kind": "Gadget",
            "plural": "gadgets",
            "scope": "organization",
            "parent": {"apiVersion": "rise.dev/v1alpha1", "kind": "Organization"},
            "versions": [{"name": "v1", "served": true, "storage": true}],
            "allowedStatusControllerIds": [],
        });
        ctx.store
            .register_resource_definition(CreateResourceParams {
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: "gadgets.example.dev".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec,
                validator: None,
            })
            .await
            .expect("register gadgets RD");
    }

    /// JSON body for creating a widget at the given served apiVersion.
    fn widget_body(api_version: &str, name: &str) -> Value {
        json!({
            "apiVersion": api_version,
            "kind": "Widget",
            "metadata": {"name": name},
            "spec": {"size": "large"},
        })
    }

    /// POST a widget and return the created resource JSON.
    async fn create_widget(ctx: &ResourceApiCtx, api_version: &str, name: &str) -> Value {
        let resp = dispatch_post_inner(
            ctx,
            "apis/example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            widget_body(api_version, name),
        )
        .await
        .expect("create widget");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::CREATED, "unexpected create status");
        body
    }

    // -------------------------------------------------------------------------
    // Auth tier: operator-only paths
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn operator_path_rejects_non_operator(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        let err = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/widgets".to_string(),
            auth(PLAIN_USER),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("non-operator must be rejected");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn operator_path_allows_operator(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        let resp = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("operator list");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["apiVersion"], "example.dev/v1");
        assert_eq!(body["kind"], "Widget");
    }

    // -------------------------------------------------------------------------
    // Auth tier: reparent is a normal operator operation
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn reparent_allowed_for_operator(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        // Any operator may reparent — there is no separate admin tier.
        let resp = dispatch_post_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1/reparent".to_string(),
            auth(OPERATOR),
            json!({"newParentUid": null}),
        )
        .await
        .expect("operator may reparent");
        let (status, _) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[sqlx::test]
    async fn reparent_rejects_non_operator(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        let err = dispatch_post_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1/reparent".to_string(),
            auth(PLAIN_USER),
            json!({"newParentUid": null}),
        )
        .await
        .expect_err("a non-operator must not reparent");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    // -------------------------------------------------------------------------
    // Auth tier: status/finalizers require a controller token (Fix E)
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn status_subresource_rejects_user_token_with_403(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &["controller.example.com"]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        // Fix E: an authenticated user hitting a controller subresource is an
        // authorization failure (403), not an authentication failure (401).
        let err = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1/status".to_string(),
            any_user(OPERATOR),
            json!({"status": {"phase": "Ready"}}),
        )
        .await
        .expect_err("user token must be rejected for status writes");
        assert_eq!(err.status, StatusCode::FORBIDDEN);

        // Same for finalizers.
        let err = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1/finalizers".to_string(),
            any_user(OPERATOR),
            json!({"add": ["x/y"], "remove": []}),
        )
        .await
        .expect_err("user token must be rejected for finalizer writes");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn status_subresource_allows_listed_controller(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &["controller.example.com"]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        let resp = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1/status".to_string(),
            any_controller("controller.example.com"),
            json!({"status": {"phase": "Ready"}}),
        )
        .await
        .expect("listed controller status write");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[sqlx::test]
    async fn status_subresource_rejects_unlisted_controller_with_403(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        // Allowlist contains a different controller id.
        register_widget_rd(&ctx, &["controller.example.com"]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        let err = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1/status".to_string(),
            any_controller("other.example.com"),
            json!({"status": {"phase": "Ready"}}),
        )
        .await
        .expect_err("unlisted controller must be rejected");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn item_update_rejects_controller_token_with_403(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        // A controller token must not be able to PUT a full item.
        let err = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1".to_string(),
            any_controller("controller.example.com"),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {"name": "w1", "revision": 1},
                "spec": {"size": "small"},
            }),
        )
        .await
        .expect_err("controller token must not update items");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    // -------------------------------------------------------------------------
    // Routing / method correctness
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn wrong_method_on_collection_yields_405(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        // PUT is not valid for a collection path.
        let err = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets".to_string(),
            any_user(OPERATOR),
            json!({}),
        )
        .await
        .expect_err("PUT on a collection must be 405");
        assert_eq!(err.status, StatusCode::METHOD_NOT_ALLOWED);

        // DELETE is not valid for a collection path.
        let err = dispatch_delete_inner(
            &ctx,
            "apis/example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
        )
        .await
        .expect_err("DELETE on a collection must be 405");
        assert_eq!(err.status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[sqlx::test]
    async fn get_on_subresource_yields_405(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        let err = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1/status".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("GET on a subresource must be 405");
        assert_eq!(err.status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[sqlx::test]
    async fn post_on_item_yields_405(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        // POST is only valid for collection paths and reparent.
        let err = dispatch_post_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1".to_string(),
            auth(OPERATOR),
            json!({}),
        )
        .await
        .expect_err("POST on an item must be 405");
        assert_eq!(err.status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[sqlx::test]
    async fn unknown_collection_yields_404(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        // No RD registered for `nonexistents`.
        let err = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/nonexistents".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("unknown collection must be 404");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn malformed_path_yields_400(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        // An unversioned collection path (no `apis/` prefix) is malformed.
        let err = dispatch_get_inner(
            &ctx,
            "widgets/w1".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("malformed path must be 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        // A trailing-slash empty segment is also malformed.
        let err = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/widgets/".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("empty path segment must be 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn item_not_found_yields_404(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        // Collection exists, item does not.
        let err = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/widgets/missing".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("missing item must be 404");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    // -------------------------------------------------------------------------
    // Versioned behaviour
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn resource_created_via_v1_listed_via_v2_keeps_requested_version(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        // Create through the v1 served version.
        let created = create_widget(&ctx, "example.dev/v1", "w1").await;
        assert_eq!(created["apiVersion"], "example.dev/v1");

        // GET the same resource through the v2 served version.
        let resp = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v2/widgets/w1".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get widget via v2");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["apiVersion"], "example.dev/v2",
            "GET via v2 must report the requested apiVersion"
        );
        assert_eq!(body["metadata"]["name"], "w1");

        // LIST through v2 must also report v2 on the envelope and items.
        let resp = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v2/widgets".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("list widgets via v2");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["apiVersion"], "example.dev/v2");
        assert_eq!(body["items"][0]["apiVersion"], "example.dev/v2");
    }

    #[sqlx::test]
    async fn create_via_undefined_version_yields_400(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        // The `widgets` plural exists but `v3` is not a version it declares.
        // The store distinguishes a truly unknown collection (404) from a known
        // collection at an undefined/unserved version (a 400 — the request
        // names a version the ResourceDefinition does not expose).
        let err = dispatch_post_inner(
            &ctx,
            "apis/example.dev/v3/widgets".to_string(),
            auth(OPERATOR),
            widget_body("example.dev/v3", "w1"),
        )
        .await
        .expect_err("undefined version must not resolve to a collection");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    // -------------------------------------------------------------------------
    // Conflicts surface as 409, not 500
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn duplicate_name_create_yields_409(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "dup").await;

        // Creating a second widget with the same name in the same scope.
        let err = dispatch_post_inner(
            &ctx,
            "apis/example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            widget_body("example.dev/v1", "dup"),
        )
        .await
        .expect_err("duplicate name must conflict");
        assert_eq!(
            err.status,
            StatusCode::CONFLICT,
            "name conflict must be 409, not 500"
        );
    }

    #[sqlx::test]
    async fn stale_revision_update_yields_409(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        let created = create_widget(&ctx, "example.dev/v1", "w1").await;
        let revision = created["metadata"]["revision"].as_i64().unwrap();

        // First update at the current revision succeeds.
        let update = |rev: i64| {
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {"name": "w1", "revision": rev},
                "spec": {"size": "medium"},
            })
        };
        let resp = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1".to_string(),
            any_user(OPERATOR),
            update(revision),
        )
        .await
        .expect("first update");
        assert_eq!(resp.status(), StatusCode::OK);

        // Re-using the now-stale revision must conflict (409), not 500.
        let err = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1".to_string(),
            any_user(OPERATOR),
            update(revision),
        )
        .await
        .expect_err("stale revision must conflict");
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    // -------------------------------------------------------------------------
    // Parent typing through the dispatch layer (Fix B / Fix C)
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn nested_collection_requires_correct_parent_type(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        register_gadget_rd(&ctx).await;

        // A root-scoped `widgets` resource cannot be a parent for the
        // Organization-scoped `gadgets` collection — wrong parent kind => 400.
        create_widget(&ctx, "example.dev/v1", "w1").await;
        let err = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1/apis/example.dev/v1/gadgets".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("gadgets under a Widget parent must be a 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn root_scoped_collection_rejects_a_parent(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "parent").await;

        // `widgets` is root-scoped; nesting it under another resource is a 400.
        let err = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/widgets/parent/apis/example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("root-scoped collection under a parent must be a 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    // -------------------------------------------------------------------------
    // Full lifecycle through the dispatch layer
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn create_get_update_delete_lifecycle(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        // Create.
        let created = create_widget(&ctx, "example.dev/v1", "lifecycle").await;
        assert_eq!(created["metadata"]["name"], "lifecycle");
        let revision = created["metadata"]["revision"].as_i64().unwrap();

        // Get.
        let resp = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/widgets/lifecycle".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["spec"]["size"], "large");

        // Update.
        let resp = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets/lifecycle".to_string(),
            any_user(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {"name": "lifecycle", "revision": revision},
                "spec": {"size": "extra-large"},
            }),
        )
        .await
        .expect("update");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["spec"]["size"], "extra-large");

        // Delete (cascade).
        let resp = dispatch_delete_inner(
            &ctx,
            "apis/example.dev/v1/widgets/lifecycle".to_string(),
            auth(OPERATOR),
        )
        .await
        .expect("delete");
        assert_eq!(resp.status(), StatusCode::OK);

        // Gone.
        let err = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v1/widgets/lifecycle".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("deleted resource must be gone");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    // -------------------------------------------------------------------------
    // Body / type-identity validation
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn create_with_mismatched_kind_yields_400(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        let err = dispatch_post_inner(
            &ctx,
            "apis/example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Gadget",
                "metadata": {"name": "w1"},
                "spec": {},
            }),
        )
        .await
        .expect_err("body kind mismatch must be 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    // -------------------------------------------------------------------------
    // Pending-deletion listing
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn pending_deletion_lists_tombstoned_resources(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        let created = create_widget(&ctx, "example.dev/v1", "w1").await;
        let uid: Uuid = created["metadata"]["uid"]
            .as_str()
            .expect("uid")
            .parse()
            .expect("parse uid");

        // A finalizer makes delete tombstone the row rather than hard-delete it.
        ctx.store
            .update_controller_finalizers(
                uid,
                "controller.example.com",
                &["controller.example.com/cleanup".to_string()],
                &[],
            )
            .await
            .expect("add finalizer");
        ctx.store.delete(uid).await.expect("delete");

        // The tombstoned widget shows up in the pending-deletion listing.
        let resp = dispatch_get_inner(
            &ctx,
            "pending-deletion".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("pending-deletion list");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["metadata"]["name"], "w1");
        assert!(items[0]["metadata"]["deletionTimestamp"].is_string());

        // A non-operator is rejected.
        let err = dispatch_get_inner(
            &ctx,
            "pending-deletion".to_string(),
            auth(PLAIN_USER),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("non-operator must be rejected");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    // -------------------------------------------------------------------------
    // Version-independent lookup
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn resource_reachable_when_storage_version_not_served(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;

        // v1 is the storage version but is NOT served; v2 is served. Rows are
        // stored at v1, so lookups must search declared (not just served)
        // versions or the resource becomes unreachable.
        let spec = json!({
            "group": "example.dev",
            "kind": "Widget",
            "plural": "widgets",
            "scope": "root",
            "versions": [
                {"name": "v1", "served": false, "storage": true},
                {"name": "v2", "served": true, "storage": false},
            ],
            "allowedStatusControllerIds": [],
        });
        ctx.store
            .register_resource_definition(CreateResourceParams {
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: "widgets.example.dev".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                spec,
                validator: None,
            })
            .await
            .expect("register widgets RD");

        // Create through the served v2 endpoint — the row lands at storage v1.
        let resp = dispatch_post_inner(
            &ctx,
            "apis/example.dev/v2/widgets".to_string(),
            auth(OPERATOR),
            widget_body("example.dev/v2", "w1"),
        )
        .await
        .expect("create widget via v2");
        let (status, created) = read(resp).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["apiVersion"], "example.dev/v2");

        // GET via v2 must find the row stored at the non-served v1.
        let resp = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v2/widgets/w1".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get widget via v2");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["apiVersion"], "example.dev/v2");
        assert_eq!(body["metadata"]["name"], "w1");

        // LIST via v2 must include it too.
        let resp = dispatch_get_inner(
            &ctx,
            "apis/example.dev/v2/widgets".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("list widgets via v2");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().expect("items array").len(), 1);
        assert_eq!(body["items"][0]["apiVersion"], "example.dev/v2");
    }

    // -------------------------------------------------------------------------
    // Method correctness: reparent is POST-only
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn put_on_reparent_yields_405(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        // reparent is POST-only; a PUT must be 405 for every caller, not a 403
        // about controller authentication.
        let err = dispatch_put_inner(
            &ctx,
            "apis/example.dev/v1/widgets/w1/reparent".to_string(),
            any_user(OPERATOR),
            json!({"newParentUid": null}),
        )
        .await
        .expect_err("PUT on reparent must be 405");
        assert_eq!(err.status, StatusCode::METHOD_NOT_ALLOWED);
    }
}
