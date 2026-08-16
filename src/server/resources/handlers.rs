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
#[cfg(test)]
use rise_resource_api::NoOpValidator;
use rise_resource_api::{
    CollectionInfo, CreateResourceParams, CreateResourceRequest, DeleteOutcome, PathSegment,
    ResourceRow, ResourceStore, UpdateResourceParams, UpdateResourceRequest,
    CASCADE_DELETION_FINALIZER, MAX_PARENT_CHAIN_DEPTH,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error_map::store_error_to_server_error;
use super::models::{
    row_to_resource, row_to_resource_with_api_version, ControllerFinalizerUpdate,
    ControllerStatusUpdate, ResourceList,
};
use super::path::{
    parse_resource_path, parse_uid_token, CollectionRef, RawResourcePath, Subresource, UID_PREFIX,
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
/// and the operator allowlist.
#[derive(Clone)]
pub(crate) struct ResourceApiCtx {
    store: Arc<dyn ResourceStore>,
    operator_users: Arc<Vec<String>>,
    /// DB pool used by application-layer guards (e.g. blocking Organization
    /// deletion while typed children — users-via-memberships, teams, projects —
    /// still reference it via `organization_resource_uid`). Required: the
    /// delete-guard must run on every dispatch, so dropping the pool would
    /// silently bypass it.
    db_pool: sqlx::PgPool,
}

impl ResourceApiCtx {
    fn from_state(state: &AppState) -> Self {
        Self {
            store: state.resource_store.clone(),
            operator_users: state.operator_users.clone(),
            db_pool: state.db_pool.clone(),
        }
    }

    fn is_operator(&self, email: &str) -> bool {
        crate::server::auth::admin::is_operator_user(&self.operator_users, email)
    }
}

fn response_resource(
    row: &ResourceRow,
    response_api_version: &str,
) -> Result<rise_resource_api::Resource, ServerError> {
    let converted = if response_api_version == row.api_version {
        row_to_resource(row)
    } else {
        row_to_resource_with_api_version(row, response_api_version)
    };
    converted.map_err(|error| {
        ServerError::internal_anyhow(
            anyhow::Error::new(error),
            "stored resource could not be converted to an API response",
        )
        .with_context("resource_uid", row.uid.to_string())
        .with_context("resource_kind", row.kind.clone())
        .with_context("resource_name", row.name.clone())
        .with_context("stored_api_version", row.api_version.clone())
        .with_context("response_api_version", response_api_version.to_owned())
    })
}

// -----------------------------------------------------------------------------
// Authorization helpers
// -----------------------------------------------------------------------------

/// Require an operator-authenticated user. Service-account/controller tokens
/// and non-operator users get 401/403 respectively.
///
/// Admin users do **not** bypass this check. Unlike the typed project/team
/// APIs — where admins skip permission checks — the generic resource API is
/// operator-gated only; admin access here is intentionally deferred (see
/// `ROADMAP.md`). Do not add an `is_admin` shortcut.
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
/// `ancestor_segs` must be the already-built ancestor segments (from
/// `classify_path`). Appends the leaf segment for `identifier` and passes the
/// complete chain to the store so ancestry validation is correct even for
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
    if raw.starts_with(UID_PREFIX) {
        Ok(PathSegment::Uid {
            api_versions: api_versions.to_vec(),
            kind: kind.to_string(),
            uid: parse_uid_token(raw)?,
        })
    } else {
        Ok(PathSegment::Name {
            api_versions: api_versions.to_vec(),
            kind: kind.to_string(),
            name: raw.to_string(),
        })
    }
}

/// Extract the API group from an `apiVersion` string.
///
/// `apiVersion` is `<group>/<version>` (e.g. `rise.dev/v1alpha1` → `rise.dev`).
/// If no `/` is present the whole string is treated as the group.
fn api_group(api_version: &str) -> &str {
    api_version.split('/').next().unwrap_or(api_version)
}

/// Resolve a leaf resource addressed by `uid:` form.
///
/// A UID is globally unique, so the ancestor chain is redundant and is not
/// consulted — this resolves the row directly and verifies it belongs to the
/// addressed collection (matching kind and a declared `api_version`). A missing
/// UID, or a row of the wrong kind, is a 404.
async fn resolve_item_by_uid(
    store: &Arc<dyn ResourceStore>,
    resolved: &ResolvedCollection,
    uid: Uuid,
) -> Result<ResourceRow, ServerError> {
    let row = store
        .get(uid)
        .await
        .map_err(store_error_to_server_error)?
        .ok_or_else(|| ServerError::not_found(format!("resource 'uid:{uid}' not found")))?;
    if row.kind != resolved.info.kind
        || !resolved
            .info
            .declared_api_versions
            .contains(&row.api_version)
    {
        return Err(ServerError::not_found(format!(
            "resource 'uid:{uid}' is not a member of collection '{}'",
            resolved.collection
        )));
    }
    Ok(row)
}

/// Walk the `ResourceDefinition` parent chain of `leaf`, returning the ancestor
/// collections root-most first. `chain.len()` is the leaf kind's parent-chain
/// depth `D`.
///
/// Each ancestor is resolved by `(group, kind)` via `resolve_collection_by_kind`.
/// An ancestor kind with no registered collection is a 404. Registration
/// rejects cyclic `parent` graphs, but the walk is still capped at
/// `MAX_PARENT_CHAIN_DEPTH` as a hard backstop.
async fn resolve_parent_chain(
    store: &Arc<dyn ResourceStore>,
    leaf: &CollectionInfo,
) -> Result<Vec<CollectionInfo>, ServerError> {
    let mut chain: Vec<CollectionInfo> = Vec::new();
    let mut current = leaf.parent.clone();
    while let Some(parent_ref) = current {
        if chain.len() >= MAX_PARENT_CHAIN_DEPTH {
            return Err(ServerError::internal(format!(
                "ResourceDefinition parent chain for kind '{}' exceeds the maximum depth \
                 of {}; the parent graph may contain a cycle",
                leaf.kind, MAX_PARENT_CHAIN_DEPTH,
            )));
        }
        let group = api_group(&parent_ref.api_version);
        let info = store
            .resolve_collection_by_kind(group, &parent_ref.kind)
            .await
            .map_err(store_error_to_server_error)?
            .ok_or_else(|| {
                ServerError::not_found(format!(
                    "parent collection '{}/{}' declared by kind '{}' is not registered",
                    group, parent_ref.kind, leaf.kind
                ))
            })?;
        current = info.parent.clone();
        chain.push(info);
    }
    chain.reverse();
    Ok(chain)
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
) -> Result<(), ServerError> {
    if !info
        .served_api_versions
        .contains(&body_api_version.to_string())
    {
        return Err(ServerError::bad_request(format!(
            "body apiVersion '{body_api_version}' is not a served version of collection \
             '{}' (served: {})",
            info.kind,
            info.served_api_versions.join(", ")
        )));
    }
    if body_kind != info.kind {
        return Err(ServerError::bad_request(format!(
            "body kind '{body_kind}' does not match collection ({})",
            info.kind
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeletionBlockersResponse {
    resource_uid: Uuid,
    cascade_finalizer_present: bool,
    blockers: Vec<DeletionBlockerResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeletionBlockerResponse {
    relationship: &'static str,
    api_version: String,
    kind: String,
    name: String,
    uid: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_owner_deletion: Option<bool>,
    deletion_timestamp: Option<chrono::DateTime<chrono::Utc>>,
    finalizers: Vec<String>,
}

// -----------------------------------------------------------------------------
// Store-aware path classification
// -----------------------------------------------------------------------------

/// How to resolve the leaf resource row of an item/subresource path.
enum LeafRef {
    /// Named form: `D` ancestor name segments plus the leaf name.
    Named {
        ancestor_segs: Vec<PathSegment>,
        name: String,
    },
    /// `uid:` form: a globally-unique UID, no ancestor segments.
    Uid(Uuid),
}

/// A resource path classified against the leaf kind's parent-chain depth.
enum ResolvedPath {
    /// `pending-deletion`: resources tombstoned and awaiting GC.
    PendingDeletion,
    /// A collection listing — `D` ancestor name segments, no leaf.
    List {
        resolved: ResolvedCollection,
        ancestor_segs: Vec<PathSegment>,
    },
    /// A single item.
    Item {
        resolved: ResolvedCollection,
        leaf: LeafRef,
    },
    /// A subresource operation on an item.
    Subresource {
        resolved: ResolvedCollection,
        leaf: LeafRef,
        subresource: Subresource,
    },
}

/// Build the "wrong number of segments" 400 for a named path.
fn segment_count_error(resolved: &ResolvedCollection, depth: usize, got: usize) -> ServerError {
    if depth == 0 {
        ServerError::bad_request(format!(
            "collection '{}' is root-scoped: an item path takes 1 name segment, \
             a subresource path takes 2 (got {got})",
            resolved.info.kind
        ))
    } else {
        ServerError::bad_request(format!(
            "collection '{}' has parent-chain depth {depth}: a list expects {depth} ancestor \
             name segment(s), an item {}, a subresource {} (the item name plus a subresource \
             keyword); got {got}",
            resolved.info.kind,
            depth + 1,
            depth + 2,
        ))
    }
}

/// Classify a parsed resource path against the resource store.
///
/// `parse_resource_path` cannot decide list-vs-item-vs-subresource: that needs
/// the leaf kind's parent-chain depth `D`. This resolves the leaf collection,
/// then either short-circuits the `uid:` form (a UID is globally unique, so the
/// ancestor chain is irrelevant — no parent-chain walk) or walks the parent
/// chain to learn `D` and classifies the named segments against it.
async fn classify_path(
    store: &Arc<dyn ResourceStore>,
    raw: RawResourcePath,
) -> Result<ResolvedPath, ServerError> {
    let (collection, segments) = match raw {
        RawResourcePath::PendingDeletion => return Ok(ResolvedPath::PendingDeletion),
        RawResourcePath::Collection {
            collection,
            segments,
        } => (collection, segments),
    };

    // Structural check (pure, no I/O): a `uid:` token in a non-first position
    // is always invalid, regardless of which collection is being addressed.
    // Validate before the DB call so a malformed path is rejected cheaply.
    if !segments.first().is_some_and(|s| s.starts_with(UID_PREFIX)) {
        if let Some(pos) = segments.iter().position(|s| s.starts_with(UID_PREFIX)) {
            return Err(ServerError::bad_request(format!(
                "uid: token at segment {pos} is invalid; a uid: identifier may only appear as \
                 the sole identifier segment, with no ancestor names"
            )));
        }
    }

    let resolved = resolve_collection(store, &collection).await?;

    // `uid:` form: a `uid:` token as the sole identifier segment. A UID is
    // globally unique, so the ancestor chain is redundant — the parent-chain
    // walk is skipped entirely and the form resolves even when an ancestor's
    // ResourceDefinition is absent.
    if segments.first().is_some_and(|s| s.starts_with(UID_PREFIX)) {
        let uid = parse_uid_token(&segments[0])?;
        return match segments.len() {
            1 => Ok(ResolvedPath::Item {
                resolved,
                leaf: LeafRef::Uid(uid),
            }),
            2 => {
                let subresource = Subresource::from_keyword(&segments[1]).ok_or_else(|| {
                    ServerError::bad_request(format!(
                        "expected a subresource keyword after a uid: identifier, got '{}'",
                        segments[1]
                    ))
                })?;
                Ok(ResolvedPath::Subresource {
                    resolved,
                    leaf: LeafRef::Uid(uid),
                    subresource,
                })
            }
            _ => Err(ServerError::bad_request(
                "a uid: identifier may be followed only by an optional subresource keyword",
            )),
        };
    }

    // Derive the parent-chain depth `D` and classify the segment count.
    let ancestors = resolve_parent_chain(store, &resolved.info).await?;
    let depth = ancestors.len();
    let n = segments.len();
    if n < depth {
        return Err(segment_count_error(&resolved, depth, n));
    }

    let mut ancestor_segs = Vec::with_capacity(depth);
    for (ancestor, name) in ancestors.iter().zip(&segments) {
        ancestor_segs.push(path_segment(
            &ancestor.declared_api_versions,
            &ancestor.kind,
            name,
        )?);
    }

    match n - depth {
        0 => Ok(ResolvedPath::List {
            resolved,
            ancestor_segs,
        }),
        1 => Ok(ResolvedPath::Item {
            resolved,
            leaf: LeafRef::Named {
                ancestor_segs,
                name: segments[depth].clone(),
            },
        }),
        2 => {
            let subresource = Subresource::from_keyword(&segments[depth + 1]).ok_or_else(|| {
                ServerError::bad_request(format!(
                    "expected a subresource keyword ({}) after the item name, got '{}'",
                    Subresource::KEYWORDS,
                    segments[depth + 1],
                ))
            })?;
            Ok(ResolvedPath::Subresource {
                resolved,
                leaf: LeafRef::Named {
                    ancestor_segs,
                    name: segments[depth].clone(),
                },
                subresource,
            })
        }
        _ => Err(segment_count_error(&resolved, depth, n)),
    }
}

/// Resolve the parent row for a list: the last ancestor in the chain. Returns
/// `None` when there are no ancestor segments (a root-scoped leaf collection).
async fn resolve_parent_row(
    store: &Arc<dyn ResourceStore>,
    ancestor_segs: &[PathSegment],
) -> Result<Option<ResourceRow>, ServerError> {
    if ancestor_segs.is_empty() {
        return Ok(None);
    }
    let chain = store
        .resolve_path(ancestor_segs)
        .await
        .map_err(store_error_to_server_error)?;
    if chain.is_empty() {
        return Err(ServerError::not_found("parent resource not found"));
    }
    Ok(chain.into_iter().last())
}

/// Resolve the leaf resource row for an item/subresource path.
async fn resolve_leaf(
    store: &Arc<dyn ResourceStore>,
    resolved: &ResolvedCollection,
    leaf: &LeafRef,
) -> Result<ResourceRow, ServerError> {
    match leaf {
        LeafRef::Named {
            ancestor_segs,
            name,
        } => resolve_item(store, ancestor_segs.clone(), &resolved.info, name).await,
        LeafRef::Uid(uid) => resolve_item_by_uid(store, resolved, *uid).await,
    }
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
    let raw_path = parse_resource_path(&raw)?;
    // Every GET path on the generic resource API is operator-only; authorize
    // before any store I/O so collection existence is not probeable.
    let user = require_operator(ctx, &auth)?;
    match classify_path(&ctx.store, raw_path).await? {
        ResolvedPath::PendingDeletion => {
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
            let items = rows
                .iter()
                .map(|row| response_resource(row, &row.api_version))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Json(serde_json::json!({ "items": items })).into_response())
        }
        ResolvedPath::List {
            resolved,
            ancestor_segs,
        } => {
            let parent_uid = resolve_parent_row(&ctx.store, &ancestor_segs)
                .await?
                .map(|r| r.uid);
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
                    .map(|row| response_resource(row, &resolved.info.api_version))
                    .collect::<Result<Vec<_>, _>>()?,
            })
            .into_response())
        }
        ResolvedPath::Item { resolved, leaf } => {
            let row = resolve_leaf(&ctx.store, &resolved, &leaf).await?;
            Ok(Json(response_resource(&row, &resolved.info.api_version)?).into_response())
        }
        ResolvedPath::Subresource {
            resolved,
            leaf,
            subresource: Subresource::DeletionBlockers,
        } => {
            let row = resolve_leaf(&ctx.store, &resolved, &leaf).await?;
            let report = ctx
                .store
                .list_deletion_blockers(row.uid)
                .await
                .map_err(store_error_to_server_error)?;
            let row = report.resource;
            let blockers = report
                .blockers
                .into_iter()
                .map(|blocker| {
                    let (relationship, block_owner_deletion) = match blocker.relationship {
                        rise_resource_api::DeletionBlockerRelationship::StructuralChild => {
                            ("structuralChild", None)
                        }
                        rise_resource_api::DeletionBlockerRelationship::OwnerReference => {
                            ("ownerReference", Some(true))
                        }
                    };
                    DeletionBlockerResponse {
                        relationship,
                        api_version: blocker.api_version,
                        kind: blocker.kind,
                        name: blocker.name,
                        uid: blocker.uid,
                        block_owner_deletion,
                        deletion_timestamp: blocker.deletion_timestamp,
                        finalizers: blocker.finalizers,
                    }
                })
                .collect::<Vec<_>>();
            tracing::info!(
                target: "rise::audit",
                actor = %user.email,
                uid = %row.uid,
                api_version = %row.api_version,
                kind = %row.kind,
                name = %row.name,
                blocker_count = blockers.len(),
                "resource.deletion_blockers_listed"
            );
            Ok(Json(DeletionBlockersResponse {
                resource_uid: row.uid,
                cascade_finalizer_present: row
                    .finalizers
                    .iter()
                    .any(|finalizer| finalizer == CASCADE_DELETION_FINALIZER),
                blockers,
            })
            .into_response())
        }
        ResolvedPath::Subresource { .. } => Err(ServerError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "GET is only supported for the deletion-blockers subresource",
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
    let raw_path = parse_resource_path(&raw)?;
    // Every POST path on the generic resource API is operator-only.
    let user = require_operator(ctx, &auth)?;
    match classify_path(&ctx.store, raw_path).await? {
        ResolvedPath::List {
            resolved,
            ancestor_segs,
        } => {
            let body: CreateResourceRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let parent_uid = resolve_parent_row(&ctx.store, &ancestor_segs)
                .await?
                .map(|r| r.uid);
            let (status, resource) =
                create_resource(ctx, &resolved, parent_uid, body, &user).await?;
            Ok((status, resource).into_response())
        }
        _ => Err(ServerError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "POST is only valid for collection paths",
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
    // Parse the path first (pure, no I/O) to return 400 on a malformed path
    // before performing any auth check, matching the GET/POST/DELETE handlers.
    // Store the operator result so it can be reused inside the Item branch
    // without a second call.
    let raw_path = parse_resource_path(&raw)?;
    let operator_user = if let AnyAuth::User(auth_ctx) = &auth {
        Some(require_operator(ctx, auth_ctx)?)
    } else {
        None
    };
    // `classify_path` performs store I/O (resolving the collection) before the
    // controller token is rejected for item-level paths. This means a controller
    // token can observe whether a collection (ResourceDefinition) exists. This
    // is acceptable: controllers can already probe collection existence via GET
    // requests to listing paths, so the information is not meaningfully secret.
    match classify_path(&ctx.store, raw_path).await? {
        ResolvedPath::Item { resolved, leaf } => {
            // Controller tokens must not update items
            let user = match &auth {
                AnyAuth::User(_auth_ctx) => {
                    // Already validated by the early gate above; reuse the result.
                    operator_user.expect("operator_user is Some for AnyAuth::User")
                }
                AnyAuth::Controller(_) => {
                    return Err(ServerError::forbidden(
                        "controller tokens cannot update resource items",
                    ));
                }
            };
            let body: UpdateResourceRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let row = resolve_leaf(&ctx.store, &resolved, &leaf).await?;
            let resp = update_resource(ctx, &resolved, &row, &leaf, body, &user).await?;
            Ok(resp.into_response())
        }
        ResolvedPath::Subresource {
            resolved,
            leaf,
            subresource,
        } => {
            let row = resolve_leaf(&ctx.store, &resolved, &leaf).await?;
            match auth {
                AnyAuth::Controller(ref controller) => {
                    enforce_controller_allowed(
                        &resolved.info,
                        &resolved.collection,
                        &controller.0.identity_id,
                    )?;
                    match subresource {
                        Subresource::Status => {
                            let body: ControllerStatusUpdate = serde_json::from_value(body)
                                .map_err(|e| {
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
                            let body: ControllerFinalizerUpdate = serde_json::from_value(body)
                                .map_err(|e| {
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
                        Subresource::DeletionBlockers => Err(ServerError::new(
                            StatusCode::METHOD_NOT_ALLOWED,
                            "deletion-blockers is a read-only subresource",
                        )),
                    }
                }
                AnyAuth::User(_) => {
                    // Operators have unrestricted access to all subresources.
                    let user = operator_user.expect("operator_user is Some for AnyAuth::User");
                    match subresource {
                        Subresource::Status => {
                            let body: ControllerStatusUpdate = serde_json::from_value(body)
                                .map_err(|e| {
                                    ServerError::bad_request(format!("invalid request body: {e}"))
                                })?;
                            let resp = apply_operator_status(
                                ctx,
                                &user,
                                &row,
                                body,
                                &resolved.info.api_version,
                            )
                            .await?;
                            Ok(resp.into_response())
                        }
                        Subresource::Finalizers => {
                            let body: ControllerFinalizerUpdate = serde_json::from_value(body)
                                .map_err(|e| {
                                    ServerError::bad_request(format!("invalid request body: {e}"))
                                })?;
                            let resp = apply_operator_finalizers(
                                ctx,
                                &user,
                                &row,
                                body,
                                &resolved.info.api_version,
                            )
                            .await?;
                            Ok(resp.into_response())
                        }
                        Subresource::DeletionBlockers => Err(ServerError::new(
                            StatusCode::METHOD_NOT_ALLOWED,
                            "deletion-blockers is a read-only subresource",
                        )),
                    }
                }
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
    let raw_path = parse_resource_path(&raw)?;
    // Every DELETE path on the generic resource API is operator-only.
    let user = require_operator(ctx, &auth)?;
    match classify_path(&ctx.store, raw_path).await? {
        ResolvedPath::Item { resolved, leaf } => {
            let row = resolve_leaf(&ctx.store, &resolved, &leaf).await?;
            delete_resource(ctx, &row, &user, &resolved.info.api_version).await
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
    // Reject writes to non-storage versions until version conversion is implemented.
    if resolved.info.api_version != resolved.info.storage_api_version {
        return Err(ServerError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "writes to version '{}' are not supported; the storage version is '{}' \
                 (version conversion is not yet implemented — use the storage version for writes)",
                resolved.info.api_version, resolved.info.storage_api_version
            ),
        ));
    }

    // POST creates have no URL name segment — only validate type identity.
    if !resolved
        .info
        .served_api_versions
        .contains(&body.api_version)
    {
        return Err(ServerError::bad_request(format!(
            "body apiVersion '{}' is not a served version of collection '{}' (served: {})",
            body.api_version,
            resolved.info.kind,
            resolved.info.served_api_versions.join(", ")
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
        labels: body.metadata.labels,
        api_version: resolved.info.storage_api_version.clone(),
        kind: body.kind,
        name: body.metadata.name,
        parent_uid,
        annotations,
        finalizers: body.metadata.finalizers,
        owner_references: body.metadata.owner_references,
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
        Json(response_resource(&row, &resolved.info.api_version)?),
    ))
}

async fn update_resource(
    ctx: &ResourceApiCtx,
    resolved: &ResolvedCollection,
    row: &ResourceRow,
    leaf: &LeafRef,
    body: UpdateResourceRequest,
    user: &User,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    // Reject writes to non-storage versions until version conversion is implemented.
    if resolved.info.api_version != resolved.info.storage_api_version {
        return Err(ServerError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "writes to version '{}' are not supported; the storage version is '{}' \
                 (version conversion is not yet implemented — use the storage version for writes)",
                resolved.info.api_version, resolved.info.storage_api_version
            ),
        ));
    }

    assert_body_matches(&resolved.info, &body.api_version, &body.kind)?;

    // For named-form URLs the body name must match the URL segment. For
    // `uid:` URLs the URL segment doesn't constrain the name, but the body
    // name must still match the stored row (checked below).
    if let LeafRef::Named {
        name: url_leaf_name,
        ..
    } = leaf
    {
        if body.metadata.name != *url_leaf_name {
            return Err(ServerError::bad_request(format!(
                "body metadata.name '{}' does not match URL name '{}'",
                body.metadata.name, url_leaf_name
            )));
        }
    }

    if body.metadata.name != row.name {
        return Err(ServerError::bad_request(format!(
            "body metadata.name '{}' does not match stored name '{}'; resources cannot be renamed via PUT",
            body.metadata.name, row.name
        )));
    }

    let annotations: BTreeMap<String, String> = body.metadata.annotations.clone();
    let spec = serde_json::to_value(&body.spec)
        .map_err(|e| ServerError::bad_request(format!("invalid spec: {e}")))?;

    let params = UpdateResourceParams {
        labels: body.metadata.labels,
        api_version: Some(resolved.info.storage_api_version.clone()),
        revision: body.metadata.revision,
        annotations,
        finalizers: body.metadata.finalizers,
        owner_references: body.metadata.owner_references,
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
    Ok(Json(response_resource(
        &updated,
        &resolved.info.api_version,
    )?))
}

async fn delete_resource(
    ctx: &ResourceApiCtx,
    row: &ResourceRow,
    user: &User,
    response_api_version: &str,
) -> Result<Response, ServerError> {
    // Organization deletes route through the canonical guard so the typed
    // soft-links (users via `user_organization_memberships`, teams, projects)
    // are never orphaned. See `super::organization` module docs.
    let outcome = if row.kind == rise_resource_api::ORGANIZATION_KIND {
        use super::organization::{delete_organization_guarded, OrganizationDeleteError};
        match delete_organization_guarded(&ctx.store, &ctx.db_pool, row.uid).await {
            Ok(outcome) => outcome,
            Err(OrganizationDeleteError::HasChildren { count }) => {
                return Err(ServerError::new(
                    StatusCode::CONFLICT,
                    format!(
                        "Organization '{}' has {count} typed child(ren) (users-via-memberships, teams, projects); \
                         delete or reassign them before deleting the organization",
                        row.name,
                    ),
                ));
            }
            Err(OrganizationDeleteError::Db(e)) => {
                return Err(ServerError::internal_anyhow(
                    e,
                    "Failed to count typed children for organization",
                ));
            }
            Err(OrganizationDeleteError::Store(e)) => {
                return Err(store_error_to_server_error(e));
            }
        }
    } else {
        ctx.store
            .delete(row.uid)
            .await
            .map_err(store_error_to_server_error)?
    };

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

    match outcome {
        DeleteOutcome::Deleted => Ok((
            StatusCode::OK,
            Json(serde_json::json!({"deleted": true, "uid": row.uid})),
        )
            .into_response()),
        DeleteOutcome::MarkedForDeletion(marked) => {
            let resource = response_resource(&marked, response_api_version)?;
            Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "deleted": false,
                    "markedForDeletion": true,
                    "resource": resource,
                })),
            )
                .into_response())
        }
    }
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
    tracing::info!(
        target: "rise::audit",
        actor = %controller.0.identity_id,
        uid = %row.uid,
        api_version = %row.api_version,
        kind = %row.kind,
        name = %row.name,
        "resource.controller_status_updated"
    );
    Ok(Json(response_resource(&updated, response_api_version)?))
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
    tracing::info!(
        target: "rise::audit",
        actor = %controller.0.identity_id,
        uid = %row.uid,
        api_version = %row.api_version,
        kind = %row.kind,
        name = %row.name,
        "resource.controller_finalizers_updated"
    );
    Ok(Json(response_resource(&updated, response_api_version)?))
}

async fn apply_operator_status(
    ctx: &ResourceApiCtx,
    user: &User,
    row: &ResourceRow,
    body: ControllerStatusUpdate,
    response_api_version: &str,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = ctx
        .store
        .operator_update_status(row.uid, &user.email, body.status)
        .await
        .map_err(store_error_to_server_error)?;
    tracing::info!(
        target: "rise::audit",
        actor = %user.email,
        uid = %row.uid,
        api_version = %row.api_version,
        kind = %row.kind,
        name = %row.name,
        "resource.operator_status_updated"
    );
    Ok(Json(response_resource(&updated, response_api_version)?))
}

async fn apply_operator_finalizers(
    ctx: &ResourceApiCtx,
    user: &User,
    row: &ResourceRow,
    body: ControllerFinalizerUpdate,
    response_api_version: &str,
) -> Result<Json<rise_resource_api::Resource>, ServerError> {
    let updated = ctx
        .store
        .operator_update_finalizers(row.uid, &user.email, &body.add, &body.remove)
        .await
        .map_err(store_error_to_server_error)?;
    tracing::info!(
        target: "rise::audit",
        actor = %user.email,
        uid = %row.uid,
        api_version = %row.api_version,
        kind = %row.kind,
        name = %row.name,
        "resource.operator_finalizers_updated"
    );
    Ok(Json(response_resource(&updated, response_api_version)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_body_matches_validates_api_version_and_kind() {
        let info = CollectionInfo {
            api_version: "rise.dev/v1alpha1".into(),
            storage_api_version: "rise.dev/v1alpha1".into(),
            served_api_versions: vec!["rise.dev/v1alpha1".into()],
            declared_api_versions: vec!["rise.dev/v1alpha1".into()],
            kind: "Organization".into(),
            parent: None,
            spec_validator: std::sync::Arc::new(NoOpValidator),
            allowed_status_controller_ids: vec![],
        };

        // Matching apiVersion and kind must be accepted.
        assert!(assert_body_matches(&info, "rise.dev/v1alpha1", "Organization",).is_ok());

        // Wrong apiVersion.
        assert!(assert_body_matches(&info, "wrong/v1", "Organization",).is_err());

        // Wrong kind.
        assert!(assert_body_matches(&info, "rise.dev/v1alpha1", "Widget",).is_err());
    }

    fn collection_info(allowed: Vec<String>) -> CollectionInfo {
        CollectionInfo {
            api_version: "example.dev/v1".into(),
            storage_api_version: "example.dev/v1".into(),
            served_api_versions: vec!["example.dev/v1".into()],
            declared_api_versions: vec!["example.dev/v1".into()],
            kind: "Widget".into(),
            parent: None,
            spec_validator: std::sync::Arc::new(NoOpValidator),
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

    #[test]
    fn malformed_stored_row_maps_to_contextual_internal_error() {
        let now = chrono::Utc::now();
        let row = ResourceRow {
            labels: Default::default(),
            uid: Uuid::new_v4(),
            api_version: "example.dev/v1".into(),
            kind: "Widget".into(),
            parent_uid: None,
            name: "widget-a".into(),
            discriminator: "abcd1234".into(),
            metadata: serde_json::json!({"invalid": 42}),
            spec: serde_json::json!({}),
            status: serde_json::json!({}),
            revision: 1,
            finalizers: vec![],
            owner_references: vec![],
            deletion_timestamp: None,
            created_at: now,
            updated_at: now,
        };

        let error = response_resource(&row, "example.dev/v1").unwrap_err();
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(error.source.is_some());
        assert!(error
            .context
            .iter()
            .any(|(key, value)| *key == "resource_uid" && value == &row.uid.to_string()));
        assert!(error
            .context
            .iter()
            .any(|(key, value)| *key == "response_api_version" && value == "example.dev/v1"));
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
    use rise_resource_store_postgres::PgResourceStore;
    use serde_json::{json, Value};

    const OPERATOR: &str = "operator@example.com";
    const PLAIN_USER: &str = "plain-user@example.com";

    /// Build a `ResourceApiCtx` over a real `PgResourceStore`. The resource
    /// store schema is layered on top of the root migrations `#[sqlx::test]`
    /// already ran.
    async fn ctx(pool: sqlx::PgPool) -> ResourceApiCtx {
        rise_resource_store_postgres::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        ResourceApiCtx {
            store: Arc::new(PgResourceStore::new(pool.clone())),
            operator_users: Arc::new(vec![OPERATOR.into()]),
            db_pool: pool,
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
            "versions": [
                {"name": "v1", "served": true, "storage": true},
                {"name": "v2", "served": true, "storage": false},
            ],
            "allowedStatusControllerIds": allowed,
        });
        ctx.store
            .register_resource_definition(CreateResourceParams {
                labels: Default::default(),
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: "widgets.example.dev".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec,
                validator: None,
            })
            .await
            .expect("register widgets RD");
    }

    /// Register an Organization-scoped `gadgets` collection whose declared
    /// parent is the built-in `rise.dev/v1alpha1` `Organization` (depth 1).
    /// `allowed` becomes the collection's `allowedStatusControllerIds`.
    async fn register_gadget_rd(ctx: &ResourceApiCtx, allowed: &[&str]) {
        let allowed: Vec<String> = allowed.iter().map(|s| s.to_string()).collect();
        let spec = json!({
            "group": "example.dev",
            "kind": "Gadget",
            "plural": "gadgets",
            "parent": {"apiVersion": "rise.dev/v1alpha1", "kind": "Organization"},
            "versions": [{"name": "v1", "served": true, "storage": true}],
            "allowedStatusControllerIds": allowed,
        });
        ctx.store
            .register_resource_definition(CreateResourceParams {
                labels: Default::default(),
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: "gadgets.example.dev".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec,
                validator: None,
            })
            .await
            .expect("register gadgets RD");
    }

    /// Register a `gizmos` collection whose declared parent is the `Gadget`
    /// collection — a depth-2 chain (`Gizmo` → `Gadget` → `Organization`).
    async fn register_gizmo_rd(ctx: &ResourceApiCtx) {
        let spec = json!({
            "group": "example.dev",
            "kind": "Gizmo",
            "plural": "gizmos",
            "parent": {"apiVersion": "example.dev/v1", "kind": "Gadget"},
            "versions": [{"name": "v1", "served": true, "storage": true}],
            "allowedStatusControllerIds": [],
        });
        ctx.store
            .register_resource_definition(CreateResourceParams {
                labels: Default::default(),
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: "gizmos.example.dev".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec,
                validator: None,
            })
            .await
            .expect("register gizmos RD");
    }

    /// POST `body` to `path`, asserting a 201, and return the created JSON.
    async fn create_at(ctx: &ResourceApiCtx, path: &str, body: Value) -> Value {
        let resp = dispatch_post_inner(ctx, path.to_string(), auth(OPERATOR), body)
            .await
            .expect("create resource");
        let (status, created) = read(resp).await;
        assert_eq!(status, StatusCode::CREATED, "unexpected create status");
        created
    }

    /// Parse `metadata.uid` from a resource JSON body.
    fn uid_of(resource: &Value) -> Uuid {
        resource["metadata"]["uid"]
            .as_str()
            .expect("uid")
            .parse()
            .expect("parse uid")
    }

    /// Create an Organization through the generic API.
    async fn create_org(ctx: &ResourceApiCtx, name: &str) {
        create_at(
            ctx,
            "rise.dev/v1alpha1/organizations",
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "Organization",
                "metadata": {"name": name},
                "spec": {"displayName": name},
            }),
        )
        .await;
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
            "example.dev/v1/widgets".to_string(),
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
    // Labels
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn labels_round_trip_through_the_http_surface(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        let resp = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "labeled",
                    "labels": {"rise.dev/owner": "group:platform"}
                },
                "spec": {"size": "large"},
            }),
        )
        .await
        .expect("create labeled widget");
        let (status, created) = read(resp).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            created["metadata"]["labels"]["rise.dev/owner"],
            "group:platform"
        );

        // The stored labels come back on a subsequent read.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets/labeled".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get labeled widget");
        let (_, fetched) = read(resp).await;
        assert_eq!(
            fetched["metadata"]["labels"]["rise.dev/owner"],
            "group:platform"
        );

        // PUT replaces the map wholesale, like annotations.
        let resp = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/labeled".to_string(),
            any_user(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "labeled",
                    "revision": fetched["metadata"]["revision"],
                    "labels": {"squad": "infra"}
                },
                "spec": {"size": "large"},
            }),
        )
        .await
        .expect("update labeled widget");
        let (status, updated) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["metadata"]["labels"]["squad"], "infra");
        assert!(updated["metadata"]["labels"]
            .get("rise.dev/owner")
            .is_none());

        // A resource with no labels omits the key rather than sending an empty
        // object, matching how ownerReferences is projected.
        let bare = create_widget(&ctx, "example.dev/v1", "bare").await;
        assert!(bare["metadata"].get("labels").is_none());

        // An invalid key is a 400, not a 500.
        let err = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {"name": "bad-label", "labels": {"not a key/x": "v"}},
                "spec": {},
            }),
        )
        .await
        .expect_err("invalid label key must be rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
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
            "example.dev/v1/widgets".to_string(),
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
            "example.dev/v1/widgets".to_string(),
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
    // Auth tier: status/finalizers — operators can write, non-operators cannot
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn status_subresource_rejects_non_operator_user_with_403(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &["controller.example.com"]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        // A non-operator user hitting a subresource path must be rejected with 403.
        let err = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/w1/status".to_string(),
            any_user(PLAIN_USER),
            json!({"status": {"phase": "Ready"}}),
        )
        .await
        .expect_err("non-operator user must be rejected for status writes");
        assert_eq!(err.status, StatusCode::FORBIDDEN);

        // Same for finalizers.
        let err = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/w1/finalizers".to_string(),
            any_user(PLAIN_USER),
            json!({"add": ["x/y"], "remove": []}),
        )
        .await
        .expect_err("non-operator user must be rejected for finalizer writes");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn status_subresource_allows_operator_user(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &["controller.example.com"]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        // An operator user can write status and finalizers without a controller token.
        let resp = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/w1/status".to_string(),
            any_user(OPERATOR),
            json!({"status": {"phase": "Ready"}}),
        )
        .await
        .expect("operator status write must succeed");
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/w1/finalizers".to_string(),
            any_user(OPERATOR),
            json!({"add": ["some.controller/cleanup"], "remove": []}),
        )
        .await
        .expect("operator finalizer write must succeed");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[sqlx::test]
    async fn status_subresource_allows_listed_controller(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &["controller.example.com"]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;

        let resp = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/w1/status".to_string(),
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
            "example.dev/v1/widgets/w1/status".to_string(),
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
            "example.dev/v1/widgets/w1".to_string(),
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
            "example.dev/v1/widgets".to_string(),
            any_user(OPERATOR),
            json!({}),
        )
        .await
        .expect_err("PUT on a collection must be 405");
        assert_eq!(err.status, StatusCode::METHOD_NOT_ALLOWED);

        // DELETE is not valid for a collection path.
        let err = dispatch_delete_inner(&ctx, "example.dev/v1/widgets".to_string(), auth(OPERATOR))
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
            "example.dev/v1/widgets/w1/status".to_string(),
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

        // POST is only valid for collection paths.
        let err = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets/w1".to_string(),
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
            "example.dev/v1/nonexistents".to_string(),
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
        // Fewer than three leading segments cannot name a {group}/{version}/{plural}.
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
            "example.dev/v1/widgets/".to_string(),
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
            "example.dev/v1/widgets/missing".to_string(),
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
            "example.dev/v2/widgets/w1".to_string(),
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
            "example.dev/v2/widgets".to_string(),
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
    async fn create_via_undefined_version_yields_404(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        // The `widgets` plural exists but `v3` is not a version it declares. An
        // undefined (or unserved) version is not addressable — 404, as
        // Kubernetes returns for an unserved apiVersion.
        let err = dispatch_post_inner(
            &ctx,
            "example.dev/v3/widgets".to_string(),
            auth(OPERATOR),
            widget_body("example.dev/v3", "w1"),
        )
        .await
        .expect_err("undefined version must not resolve to a collection");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn write_to_non_storage_version_yields_422(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        // `register_widget_rd` registers v1 as storage, v2 as served non-storage.
        register_widget_rd(&ctx, &[]).await;

        // POST to the non-storage served version v2 must be rejected with 422.
        let err = dispatch_post_inner(
            &ctx,
            "example.dev/v2/widgets".to_string(),
            auth(OPERATOR),
            widget_body("example.dev/v2", "w1"),
        )
        .await
        .expect_err("write to non-storage version must be 422");
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);

        // Seed a widget at the storage version directly so the PUT test has a row.
        ctx.store
            .create(CreateResourceParams {
                labels: Default::default(),
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                name: "w1".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: serde_json::json!({"size": "large"}),
                validator: None,
            })
            .await
            .expect("create widget at storage v1");

        // PUT to the non-storage served version v2 must also be rejected.
        let err = dispatch_put_inner(
            &ctx,
            "example.dev/v2/widgets/w1".to_string(),
            any_user(OPERATOR),
            json!({
                "apiVersion": "example.dev/v2",
                "kind": "Widget",
                "metadata": {"name": "w1", "revision": 1},
                "spec": {"size": "medium"},
            }),
        )
        .await
        .expect_err("PUT to non-storage version must be 422");
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
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
            "example.dev/v1/widgets".to_string(),
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
            "example.dev/v1/widgets/w1".to_string(),
            any_user(OPERATOR),
            update(revision),
        )
        .await
        .expect("first update");
        assert_eq!(resp.status(), StatusCode::OK);

        // Re-using the now-stale revision must conflict (409), not 500.
        let err = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/w1".to_string(),
            any_user(OPERATOR),
            update(revision),
        )
        .await
        .expect_err("stale revision must conflict");
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    // -------------------------------------------------------------------------
    // Parent-chain classification through the dispatch layer
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn nested_ancestor_name_not_found_yields_404(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_gadget_rd(&ctx, &[]).await;

        // `gadgets` is Organization-scoped (depth 1). The ancestor *type* is now
        // derived from the ResourceDefinition graph and cannot be mistyped in
        // the URL, so the only failure mode is an ancestor *name* with no row.
        let err = dispatch_get_inner(
            &ctx,
            "example.dev/v1/gadgets/no-such-org".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("a missing ancestor organization must be a 404");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn excess_segments_for_root_collection_yields_400(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        // `widgets` is root-scoped (depth 0): a path may carry at most an item
        // name plus a subresource keyword. More name segments than that is a
        // segment-count 400.
        let err = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets/w1/extra/segments".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("too many segments for a depth-0 collection must be a 400");
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
            "example.dev/v1/widgets/lifecycle".to_string(),
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
            "example.dev/v1/widgets/lifecycle".to_string(),
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
            "example.dev/v1/widgets/lifecycle".to_string(),
            auth(OPERATOR),
        )
        .await
        .expect("delete");
        assert_eq!(resp.status(), StatusCode::OK);

        // Gone.
        let err = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets/lifecycle".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("deleted resource must be gone");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    // -------------------------------------------------------------------------
    // Application-layer guards for typed-table linkage
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn delete_organization_with_linked_team_yields_409(pool: sqlx::PgPool) {
        // PR5 guard: an Organization with typed children (teams/projects via
        // `organization_resource_uid`) cannot be deleted. Those rows do not
        // live in the `resources` table, so the generic child check cannot
        // catch them — we enforce it at the application layer.

        let ctx = ctx(pool.clone()).await;

        // Create an Organization through the store (operator-only API,
        // bypassing the typed `delete_resource` path until we exercise it).
        let org_spec = json!({
            "displayName": "Multi-Tenancy Test",
        });
        let org = ctx
            .store
            .create(CreateResourceParams {
                labels: Default::default(),
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: rise_resource_api::ORGANIZATION_KIND.to_string(),
                name: "guard-test".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: org_spec,
                validator: Some(Arc::new(
                    rise_resource_store_postgres::OrganizationValidator,
                )),
            })
            .await
            .expect("create organization");

        // Seed a team in the typed table that points at this Organization.
        let user_id =
            sqlx::query_scalar::<_, Uuid>("INSERT INTO users (email) VALUES ($1) RETURNING id")
                .bind(format!("guard-{}@example.com", Uuid::new_v4()))
                .fetch_one(&pool)
                .await
                .expect("seed user");
        let team_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO teams (name, organization_resource_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(format!("guard-team-{}", Uuid::new_v4().simple()))
        .bind(org.uid)
        .fetch_one(&pool)
        .await
        .expect("seed team");

        // Attempt to delete the Organization — the guard must surface 409.
        let err = dispatch_delete_inner(
            &ctx,
            format!(
                "{}/{}/{}/{}",
                rise_resource_api::API_VERSION_V1ALPHA1
                    .split_once('/')
                    .unwrap()
                    .0,
                rise_resource_api::API_VERSION_V1ALPHA1
                    .split_once('/')
                    .unwrap()
                    .1,
                rise_resource_api::ORGANIZATION_COLLECTION,
                org.name,
            ),
            auth(OPERATOR),
        )
        .await
        .expect_err("organization with typed children must be 409");
        assert_eq!(err.status, StatusCode::CONFLICT);

        // Verify the row is still there.
        assert!(ctx.store.get(org.uid).await.unwrap().is_some());

        // Cleanup the linkage. After clearing it, the delete should succeed.
        sqlx::query("DELETE FROM teams WHERE id = $1")
            .bind(team_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();

        let resp = dispatch_delete_inner(
            &ctx,
            format!(
                "{}/{}/{}/{}",
                rise_resource_api::API_VERSION_V1ALPHA1
                    .split_once('/')
                    .unwrap()
                    .0,
                rise_resource_api::API_VERSION_V1ALPHA1
                    .split_once('/')
                    .unwrap()
                    .1,
                rise_resource_api::ORGANIZATION_COLLECTION,
                org.name,
            ),
            auth(OPERATOR),
        )
        .await
        .expect("delete must succeed once the linked team is gone");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[sqlx::test]
    async fn delete_organization_with_only_membership_yields_409(pool: sqlx::PgPool) {
        // Regression guard for commit 15a9144: an Organization whose only
        // typed-table child is a `user_organization_memberships` row (no team,
        // no project) must still surface 409. Without it, deleting the Org
        // would orphan the membership row (no FK back to
        // `resource_store.resources`).

        let ctx = ctx(pool.clone()).await;

        // Create an Organization through the store (operator-only API,
        // bypassing the typed `delete_resource` path until we exercise it).
        let org_spec = json!({
            "displayName": "Membership Guard Test",
        });
        let org = ctx
            .store
            .create(CreateResourceParams {
                labels: Default::default(),
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: rise_resource_api::ORGANIZATION_KIND.to_string(),
                name: "membership-guard-test".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: org_spec,
                validator: Some(Arc::new(
                    rise_resource_store_postgres::OrganizationValidator,
                )),
            })
            .await
            .expect("create organization");

        // Seed a user, then link it to the Organization via the memberships
        // join table. No team or project rows reference this Organization.
        let user_id =
            sqlx::query_scalar::<_, Uuid>("INSERT INTO users (email) VALUES ($1) RETURNING id")
                .bind(format!("membership-{}@example.com", Uuid::new_v4()))
                .fetch_one(&pool)
                .await
                .expect("seed user");
        sqlx::query(
            "INSERT INTO user_organization_memberships (user_id, organization_resource_uid) \
             VALUES ($1, $2)",
        )
        .bind(user_id)
        .bind(org.uid)
        .execute(&pool)
        .await
        .expect("seed membership");

        // Attempt to delete the Organization — the guard must surface 409.
        let err = dispatch_delete_inner(
            &ctx,
            format!(
                "{}/{}/{}/{}",
                rise_resource_api::API_VERSION_V1ALPHA1
                    .split_once('/')
                    .unwrap()
                    .0,
                rise_resource_api::API_VERSION_V1ALPHA1
                    .split_once('/')
                    .unwrap()
                    .1,
                rise_resource_api::ORGANIZATION_COLLECTION,
                org.name,
            ),
            auth(OPERATOR),
        )
        .await
        .expect_err("organization with only-membership children must be 409");
        assert_eq!(err.status, StatusCode::CONFLICT);

        // Verify the row is still there.
        assert!(ctx.store.get(org.uid).await.unwrap().is_some());

        // Cleanup the linkage. After clearing it, the delete should succeed.
        sqlx::query(
            "DELETE FROM user_organization_memberships \
             WHERE user_id = $1 AND organization_resource_uid = $2",
        )
        .bind(user_id)
        .bind(org.uid)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();

        let resp = dispatch_delete_inner(
            &ctx,
            format!(
                "{}/{}/{}/{}",
                rise_resource_api::API_VERSION_V1ALPHA1
                    .split_once('/')
                    .unwrap()
                    .0,
                rise_resource_api::API_VERSION_V1ALPHA1
                    .split_once('/')
                    .unwrap()
                    .1,
                rise_resource_api::ORGANIZATION_COLLECTION,
                org.name,
            ),
            auth(OPERATOR),
        )
        .await
        .expect("delete must succeed once the membership is gone");
        assert_eq!(resp.status(), StatusCode::OK);
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
            "example.dev/v1/widgets".to_string(),
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

    #[sqlx::test]
    async fn deletion_blockers_reports_blocking_relationships(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        let owner = ctx
            .store
            .create(CreateResourceParams {
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: rise_resource_api::ORGANIZATION_KIND.to_string(),
                name: "blocker-owner".to_string(),
                spec: json!({"displayName": "Blocker Owner"}),
                ..Default::default()
            })
            .await
            .expect("create owner");
        let blocking_reference = rise_resource_api::OwnerReference::new(
            &owner.api_version,
            &owner.kind,
            &owner.name,
            owner.uid,
        )
        .expect("owner reference")
        .with_block_owner_deletion(true);
        let dependent = ctx
            .store
            .create(CreateResourceParams {
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                name: "blocker-dependent".to_string(),
                owner_references: vec![blocking_reference],
                finalizers: vec!["controller.example.com/cleanup".to_string()],
                spec: json!({}),
                ..Default::default()
            })
            .await
            .expect("create dependent");

        ctx.store.delete(owner.uid).await.expect("delete owner");

        let resp = dispatch_get_inner(
            &ctx,
            "rise.dev/v1alpha1/organizations/blocker-owner/deletion-blockers".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("list deletion blockers");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["resourceUid"], owner.uid.to_string());
        assert_eq!(body["cascadeFinalizerPresent"], true);
        let blockers = body["blockers"].as_array().expect("blockers array");
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0]["relationship"], "ownerReference");
        assert_eq!(blockers[0]["uid"], dependent.uid.to_string());
        assert_eq!(blockers[0]["blockOwnerDeletion"], true);
        assert!(blockers[0]["deletionTimestamp"].is_string());
        assert_eq!(
            blockers[0]["finalizers"],
            json!(["controller.example.com/cleanup"])
        );

        let err = dispatch_get_inner(
            &ctx,
            "rise.dev/v1alpha1/organizations/blocker-owner/deletion-blockers".to_string(),
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
            "versions": [
                {"name": "v1", "served": false, "storage": true},
                {"name": "v2", "served": true, "storage": false},
            ],
            "allowedStatusControllerIds": [],
        });
        ctx.store
            .register_resource_definition(CreateResourceParams {
                labels: Default::default(),
                api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
                kind: RESOURCE_DEFINITION_KIND.to_string(),
                name: "widgets.example.dev".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec,
                validator: None,
            })
            .await
            .expect("register widgets RD");

        // Create directly via the store at the (non-served) storage version v1.
        // Writing to v2 via the HTTP dispatch layer is rejected (Change 4 — writes
        // to non-storage versions are not supported), so we seed the row directly.
        ctx.store
            .create(CreateResourceParams {
                labels: Default::default(),
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                name: "w1".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: serde_json::json!({"size": "large"}),
                validator: None,
            })
            .await
            .expect("create widget at storage v1");

        // GET via v2 must find the row stored at the non-served v1.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v2/widgets/w1".to_string(),
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
            "example.dev/v2/widgets".to_string(),
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
    // Parent-chain depth: list / item / subresource at depth 1 and 2
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn depth_1_chain_classifies_list_item_subresource(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_gadget_rd(&ctx, &["controller.example.com"]).await;
        create_org(&ctx, "acme").await;

        // Create a gadget under acme — POST to the depth-1 list path.
        let gadget = create_at(
            &ctx,
            "example.dev/v1/gadgets/acme",
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Gadget",
                "metadata": {"name": "g1"},
                "spec": {},
            }),
        )
        .await;
        assert_eq!(gadget["metadata"]["name"], "g1");

        // List — exactly D (= 1) ancestor name segments, no leaf.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/gadgets/acme".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("list gadgets under acme");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().expect("items").len(), 1);

        // Item — D + 1 segments.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/gadgets/acme/g1".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get gadget");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["metadata"]["name"], "g1");

        // Subresource — D + 2 segments, controller-authenticated status write.
        let resp = dispatch_put_inner(
            &ctx,
            "example.dev/v1/gadgets/acme/g1/status".to_string(),
            any_controller("controller.example.com"),
            json!({"status": {"phase": "Ready"}}),
        )
        .await
        .expect("controller status write on a depth-1 subresource");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[sqlx::test]
    async fn depth_2_chain_resolves_list_and_item(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_gadget_rd(&ctx, &[]).await;
        register_gizmo_rd(&ctx).await;
        create_org(&ctx, "acme").await;
        create_at(
            &ctx,
            "example.dev/v1/gadgets/acme",
            json!({"apiVersion": "example.dev/v1", "kind": "Gadget", "metadata": {"name": "g1"}, "spec": {}}),
        )
        .await;
        create_at(
            &ctx,
            "example.dev/v1/gizmos/acme/g1",
            json!({"apiVersion": "example.dev/v1", "kind": "Gizmo", "metadata": {"name": "z1"}, "spec": {}}),
        )
        .await;

        // Item — 2 ancestor names (org, gadget) + the leaf name.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/gizmos/acme/g1/z1".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get gizmo at depth 2");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["metadata"]["name"], "z1");

        // List — exactly 2 ancestor names, no leaf.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/gizmos/acme/g1".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("list gizmos at depth 2");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().expect("items").len(), 1);
    }

    // -------------------------------------------------------------------------
    // UID addressing and segment-count edge cases
    // -------------------------------------------------------------------------

    #[sqlx::test]
    async fn uid_form_resolves_nested_resource_without_ancestors(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        register_gadget_rd(&ctx, &[]).await;
        create_org(&ctx, "acme").await;
        let gadget = create_at(
            &ctx,
            "example.dev/v1/gadgets/acme",
            json!({"apiVersion": "example.dev/v1", "kind": "Gadget", "metadata": {"name": "g1"}, "spec": {}}),
        )
        .await;
        let uid = uid_of(&gadget);

        // The `uid:` form addresses the gadget globally: no `acme` ancestor name
        // and no parent-chain walk, even though the gadget is nested.
        let resp = dispatch_get_inner(
            &ctx,
            format!("example.dev/v1/gadgets/uid:{uid}"),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get nested gadget by uid alone");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["metadata"]["name"], "g1");

        // A UID resolved under the wrong collection is a 404 (kind mismatch).
        let err = dispatch_get_inner(
            &ctx,
            format!("example.dev/v1/widgets/uid:{uid}"),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("a Gadget uid under the widgets collection must be 404");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn uid_token_mid_chain_yields_400(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_gadget_rd(&ctx, &[]).await;

        // A `uid:` token is valid only as the sole identifier segment; following
        // an ancestor name it is a 400 — and the parent-chain walk is not even
        // reached.
        let err = dispatch_get_inner(
            &ctx,
            format!("example.dev/v1/gadgets/acme/uid:{}", Uuid::new_v4()),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("uid: after an ancestor name must be 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn leaf_resource_named_status_resolves_as_item(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "status").await;

        // `widgets` is depth 0, so `widgets/status` is D + 1 segments: the
        // trailing `status` is the item name, not a subresource keyword.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets/status".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get widget literally named 'status'");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["metadata"]["name"], "status");
    }

    // -------------------------------------------------------------------------
    // ResourceDefinition update via HTTP PUT dispatch
    // -------------------------------------------------------------------------

    /// PUT a `ResourceDefinition` through `dispatch_put_inner` — this exercises
    /// the `update_resource_definition` code path that all the other tests bypass
    /// by calling `store.register_resource_definition` directly.
    #[sqlx::test]
    async fn update_resource_definition_via_put(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        // Step 1: GET the RD through the HTTP layer to obtain its current revision.
        let resp = dispatch_get_inner(
            &ctx,
            "rise.dev/v1alpha1/resourcedefinitions/widgets.example.dev".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get ResourceDefinition");
        let (status, rd_body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rd_body["metadata"]["name"], "widgets.example.dev");
        let revision = rd_body["metadata"]["revision"].as_i64().unwrap();

        // Step 2: PUT an updated version — add a `v3` served (non-storage) version.
        let updated_spec = json!({
            "group": "example.dev",
            "kind": "Widget",
            "plural": "widgets",
            "versions": [
                {"name": "v1", "served": true, "storage": true},
                {"name": "v2", "served": true, "storage": false},
                {"name": "v3", "served": true, "storage": false},
            ],
            "allowedStatusControllerIds": [],
        });
        let resp = dispatch_put_inner(
            &ctx,
            "rise.dev/v1alpha1/resourcedefinitions/widgets.example.dev".to_string(),
            any_user(OPERATOR),
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": RESOURCE_DEFINITION_KIND,
                "metadata": {"name": "widgets.example.dev", "revision": revision},
                "spec": updated_spec,
            }),
        )
        .await
        .expect("update ResourceDefinition via PUT");

        // Step 3: Assert the response is 200 and the returned body reflects the update.
        let (status, put_body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(put_body["metadata"]["name"], "widgets.example.dev");
        assert_eq!(put_body["apiVersion"], "rise.dev/v1alpha1");
        assert_eq!(put_body["kind"], RESOURCE_DEFINITION_KIND);
        // The new version should be visible in the returned spec.
        let versions = put_body["spec"]["versions"]
            .as_array()
            .expect("versions array");
        assert_eq!(versions.len(), 3, "expected 3 versions after update");
    }
}
