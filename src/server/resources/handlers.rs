//! HTTP handlers for the generic resource API.
//!
//! Every user-authenticated path is authorized through the choke point in
//! `crate::server::authz` — one `(verb, ResourceKind, subresource?)` decision
//! per resource, evaluated against that resource's own ancestry and effective
//! labels (ADR-0001 §4), plus the write-time grant gate on any change that
//! delegates authority (§5). Operators reach everything because the seeded
//! `system-admin` binding says so, not because of a check in front of the API.
//!
//! The controller-specific status/finalizer endpoints are the exception: they
//! authenticate via the `AnyAuth` extractor and remain gated to controllers
//! listed in the collection's `allowed_status_controller_ids` (default-deny on
//! an empty list). Controllers become ordinary principals — and that allowlist
//! goes away — when Controller identity resources go live.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rise_authz::engine::{ListCandidate, ListDecision, ResourceTree};
#[cfg(test)]
use rise_resource_api::NoOpValidator;
use rise_resource_api::{
    CollectionInfo, CreateResourceParams, CreateResourceRequest, DeleteOutcome, PathSegment,
    ResourceRow, ResourceStore, SubresourceName, UpdateResourceParams, UpdateResourceRequest, Verb,
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
use crate::server::auth::context::{AnyAuth, AuthContext};
use crate::server::auth::controller::ControllerAuthContext;
use crate::server::authz::{
    change_for_create, change_for_delete, change_for_update, label_changes, node_for, node_for_new,
    project_list_item, AuthorizationChangeSet, AuthorizationContext, ReadGranularity,
    ResourceAuthorizer,
};
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
    /// Pool-backed store for read paths and for resolving a request's shape
    /// before it enters a transaction. Write paths use the transaction-scoped
    /// store on their `AuthorizationContext` instead, so the facts a check reads
    /// and the row it authorizes are the same ones the write commits.
    store: Arc<dyn ResourceStore>,
    authz: ResourceAuthorizer,
}

impl ResourceApiCtx {
    fn from_state(state: &AppState) -> Self {
        Self {
            store: state.resource_store.clone(),
            authz: state.resource_authorizer.clone(),
        }
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
    /// Blockers the caller may not `list`. Counted rather than named, so the
    /// report never reads as "nothing is blocking this" when something is.
    hidden_blockers: usize,
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
    // Resolving the principal is not yet an authorization decision, so which
    // collections exist is visible to any authenticated caller. That matches
    // discovery being a property of the registry rather than of any one
    // resource (ADR-0001 §8); what a collection *contains* is authorized per
    // item below.
    let authz = ctx.authz.read_context(&auth).await?;
    match classify_path(&ctx.store, raw_path).await? {
        ResolvedPath::PendingDeletion => {
            let limit = q.limit.unwrap_or(100).clamp(1, 1000);
            let rows = ctx
                .store
                .list_pending_collection(limit)
                .await
                .map_err(store_error_to_server_error)?;
            // A diagnostics listing spanning every kind is still a collection:
            // each row is evaluated on its own ancestry and labels, and one the
            // caller cannot `list` is omitted rather than refused.
            let mut items = Vec::new();
            for row in &rows {
                // A row whose ancestry cannot be resolved cannot be authorized
                // either, and defaulting to visible would be the fail-open
                // direction. It is skipped and named in the log rather than
                // failing the whole listing, which would hide every other
                // draining resource behind one anomaly.
                let target = match authz.tree(row.uid).await {
                    Ok(target) => target,
                    Err(error) => {
                        tracing::warn!(
                            uid = %row.uid,
                            kind = %row.kind,
                            "Skipping a tombstoned resource whose ancestry could not be \
                             resolved: {}",
                            error.message
                        );
                        continue;
                    }
                };
                if !authz.allows(&target, Verb::List, None).await? {
                    continue;
                }
                let granularity = read_granularity(authz.allows(&target, Verb::Get, None).await?);
                items.push(project_list_item(
                    &resource_response(row, &row.api_version, &target)?,
                    granularity,
                )?);
            }
            tracing::info!(
                target: "rise::audit",
                actor = %authz.actor(),
                count = items.len(),
                "resource.pending_deletion_listed"
            );
            Ok(Json(serde_json::json!({ "items": items })).into_response())
        }
        ResolvedPath::List {
            resolved,
            ancestor_segs,
        } => {
            // A collection under a scope that does not exist answers the same
            // way as one the caller may see nothing in: empty. Answering 404
            // here would make the ancestor path itself enumerable by name —
            // which organizations exist, which projects they contain — while
            // the per-item filter below carefully masks their contents
            // (ADR-0001 §4).
            let parent = match resolve_parent_row(&ctx.store, &ancestor_segs).await {
                Ok(parent) => parent,
                Err(error) if error.status == StatusCode::NOT_FOUND => {
                    return Ok(Json(ResourceList {
                        api_version: resolved.info.api_version.clone(),
                        kind: resolved.info.kind,
                        items: Vec::new(),
                    })
                    .into_response());
                }
                Err(error) => return Err(error),
            };
            let parent_uid = parent.map(|r| r.uid);
            let ancestors = match parent_uid {
                None => Vec::new(),
                Some(uid) => authz.tree(uid).await?.nodes().to_vec(),
            };
            let rows = ctx
                .store
                .list_versions(
                    &resolved.info.declared_api_versions,
                    &resolved.info.kind,
                    parent_uid,
                )
                .await
                .map_err(store_error_to_server_error)?;
            let candidates = rows
                .iter()
                .map(|row| {
                    Ok(ListCandidate {
                        uid: row.uid,
                        node: node_for(row)?,
                    })
                })
                .collect::<Result<Vec<_>, ServerError>>()?;
            // One decision per item, against that item's own effective labels
            // (ADR-0001 §4). Items the caller cannot `list` are omitted and
            // their existence masked: no applicable grant yields an empty
            // collection, never a 403 confirming the scope is populated.
            let decisions: BTreeMap<Uuid, ListDecision> = authz
                .filter_list(&ancestors, &candidates)
                .await?
                .into_iter()
                .map(|decision| (decision.uid, decision))
                .collect();
            let mut items = Vec::new();
            for (row, candidate) in rows.iter().zip(&candidates) {
                // Matched by UID rather than by position: a decision that does
                // not name this row is no decision at all, and the item is
                // omitted. Positional alignment holds today, but reading it
                // back from the answer is what keeps a future change to the
                // engine's filter from silently pairing one item's row with
                // another item's verdict.
                let Some(decision) = decisions.get(&row.uid).filter(|d| d.listable) else {
                    continue;
                };
                let target = ResourceTree::with_leaf(&ancestors, candidate.node.clone());
                items.push(project_list_item(
                    &resource_response(row, &resolved.info.api_version, &target)?,
                    read_granularity(decision.readable),
                )?);
            }
            Ok(Json(ResourceList {
                api_version: resolved.info.api_version.clone(),
                kind: resolved.info.kind,
                items,
            })
            .into_response())
        }
        ResolvedPath::Item { resolved, leaf } => {
            let row = resolve_leaf(&ctx.store, &resolved, &leaf).await?;
            let target = authz.tree(row.uid).await?;
            authz.require(&target, Verb::Get, None).await?;
            Ok(Json(resource_response(
                &row,
                &resolved.info.api_version,
                &target,
            )?)
            .into_response())
        }
        ResolvedPath::Subresource {
            resolved,
            leaf,
            subresource: Subresource::DeletionBlockers,
        } => {
            let row = resolve_leaf(&ctx.store, &resolved, &leaf).await?;
            let target = authz.tree(row.uid).await?;
            authz
                .require(
                    &target,
                    Verb::Get,
                    Some(&subresource_name(Subresource::DeletionBlockers)?),
                )
                .await?;
            let report = ctx
                .store
                .list_deletion_blockers(row.uid)
                .await
                .map_err(store_error_to_server_error)?;
            let row = report.resource;
            // The blockers are a collection like any other, so they are filtered
            // per item (ADR-0001 §4) — this subresource is a separate grant from
            // `list` on the kinds beneath, and a caller holding only the former
            // must not receive a complete inventory of an Organization's
            // children by name and UID. What is withheld is still counted: a
            // report that silently omits blockers would read as "nothing is
            // blocking this", which is worse than saying how many are hidden.
            let mut blockers = Vec::new();
            let mut hidden = 0usize;
            for blocker in report.blockers {
                let visible = match authz.tree(blocker.uid).await {
                    Ok(target) => authz.allows(&target, Verb::List, None).await?,
                    // A blocker whose ancestry will not resolve cannot be
                    // authorized; it is counted, never named.
                    Err(_) => false,
                };
                if !visible {
                    hidden += 1;
                    continue;
                }
                let (relationship, block_owner_deletion) = match blocker.relationship {
                    rise_resource_api::DeletionBlockerRelationship::StructuralChild => {
                        ("structuralChild", None)
                    }
                    rise_resource_api::DeletionBlockerRelationship::OwnerReference => {
                        ("ownerReference", Some(true))
                    }
                };
                blockers.push(DeletionBlockerResponse {
                    relationship,
                    api_version: blocker.api_version,
                    kind: blocker.kind,
                    name: blocker.name,
                    uid: blocker.uid,
                    block_owner_deletion,
                    deletion_timestamp: blocker.deletion_timestamp,
                    finalizers: blocker.finalizers,
                });
            }
            tracing::info!(
                target: "rise::audit",
                actor = %authz.actor(),
                uid = %row.uid,
                api_version = %row.api_version,
                kind = %row.kind,
                name = %row.name,
                blocker_count = blockers.len(),
                hidden_blocker_count = hidden,
                "resource.deletion_blockers_listed"
            );
            Ok(Json(DeletionBlockersResponse {
                resource_uid: row.uid,
                cascade_finalizer_present: row
                    .finalizers
                    .iter()
                    .any(|finalizer| finalizer == CASCADE_DELETION_FINALIZER),
                blockers,
                hidden_blockers: hidden,
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
    // Parsed once up front: a malformed path is a 400 regardless of who is
    // asking, and it is the only work in the loop that cannot change between
    // attempts.
    parse_resource_path(&raw)?;
    let mut attempt = 1;
    loop {
        let write = ctx.authz.begin_write(&auth).await?;
        let outcome = match create_once(ctx, write.context(), &raw, body.clone()).await {
            Ok(response) => write.commit().await.map(|()| response),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(response) => return Ok(response),
            Err(error) if ResourceAuthorizer::should_retry(&error, attempt) => attempt += 1,
            Err(error) => return Err(error),
        }
    }
}

async fn create_once(
    ctx: &ResourceApiCtx,
    authz: &AuthorizationContext,
    raw: &str,
    body: serde_json::Value,
) -> Result<Response, ServerError> {
    let raw_path = parse_resource_path(raw)?;
    match classify_path(authz.store(), raw_path).await? {
        ResolvedPath::List {
            resolved,
            ancestor_segs,
        } => {
            let body: CreateResourceRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let parent = resolve_parent_row(authz.store(), &ancestor_segs).await?;
            let (status, resource) =
                create_resource(ctx, authz, &resolved, parent.as_ref(), body).await?;
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
    // before any authentication or store work, matching the other handlers.
    parse_resource_path(&raw)?;

    // A controller token writes only through the status/finalizer subresources,
    // authorized by the collection's controller allowlist rather than by RBAC: a
    // Controller is not a principal until its identity resource exists, so there
    // is nothing for the engine to evaluate.
    let user_auth = match &auth {
        AnyAuth::User(auth_ctx) => auth_ctx.clone(),
        AnyAuth::Controller(_) => return dispatch_put_controller(ctx, raw, auth, body).await,
    };

    let mut attempt = 1;
    loop {
        let write = ctx.authz.begin_write(&user_auth).await?;
        let outcome = match update_once(ctx, write.context(), &raw, body.clone()).await {
            Ok(response) => write.commit().await.map(|()| response),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(response) => return Ok(response),
            Err(error) if ResourceAuthorizer::should_retry(&error, attempt) => attempt += 1,
            Err(error) => return Err(error),
        }
    }
}

async fn update_once(
    ctx: &ResourceApiCtx,
    authz: &AuthorizationContext,
    raw: &str,
    body: serde_json::Value,
) -> Result<Response, ServerError> {
    let raw_path = parse_resource_path(raw)?;
    match classify_path(authz.store(), raw_path).await? {
        ResolvedPath::Item { resolved, leaf } => {
            let body: UpdateResourceRequest = serde_json::from_value(body)
                .map_err(|e| ServerError::bad_request(format!("invalid request body: {e}")))?;
            let row = resolve_leaf(authz.store(), &resolved, &leaf).await?;
            let resp = update_resource(ctx, authz, &resolved, &row, &leaf, body).await?;
            Ok(resp.into_response())
        }
        ResolvedPath::Subresource {
            resolved,
            leaf,
            subresource,
        } => {
            let row = resolve_leaf(authz.store(), &resolved, &leaf).await?;
            let target = authz.tree(row.uid).await?;
            let name = subresource_name(subresource.clone())?;
            match subresource {
                Subresource::Status => {
                    authz.require(&target, Verb::Update, Some(&name)).await?;
                    let body: ControllerStatusUpdate =
                        serde_json::from_value(body).map_err(|e| {
                            ServerError::bad_request(format!("invalid request body: {e}"))
                        })?;
                    let resp =
                        apply_user_status(authz, &row, body, &resolved.info.api_version, &target)
                            .await?;
                    Ok(resp.into_response())
                }
                Subresource::Finalizers => {
                    authz.require(&target, Verb::Update, Some(&name)).await?;
                    let body: ControllerFinalizerUpdate =
                        serde_json::from_value(body).map_err(|e| {
                            ServerError::bad_request(format!("invalid request body: {e}"))
                        })?;
                    let resp = apply_user_finalizers(
                        authz,
                        &row,
                        body,
                        &resolved.info.api_version,
                        &target,
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
        _ => Err(ServerError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "PUT is only valid for item and subresource paths",
        )),
    }
}

/// The controller-token half of `PUT`: status and finalizer writes gated by the
/// collection's `allowed_status_controller_ids`.
async fn dispatch_put_controller(
    ctx: &ResourceApiCtx,
    raw: String,
    auth: AnyAuth,
    body: serde_json::Value,
) -> Result<Response, ServerError> {
    let AnyAuth::Controller(controller) = auth else {
        return Err(ServerError::unauthorized("Not authenticated"));
    };
    let raw_path = parse_resource_path(&raw)?;
    // `classify_path` performs store I/O (resolving the collection) before the
    // controller token is rejected for item-level paths. This means a controller
    // token can observe whether a collection (ResourceDefinition) exists. This
    // is acceptable: controllers can already probe collection existence via GET
    // requests to listing paths, so the information is not meaningfully secret.
    match classify_path(&ctx.store, raw_path).await? {
        ResolvedPath::Item { .. } => Err(ServerError::forbidden(
            "controller tokens cannot update resource items",
        )),
        ResolvedPath::Subresource {
            resolved,
            leaf,
            subresource,
        } => {
            let row = resolve_leaf(&ctx.store, &resolved, &leaf).await?;
            enforce_controller_allowed(
                &resolved.info,
                &resolved.collection,
                &controller.0.identity_id,
            )?;
            match subresource {
                Subresource::Status => {
                    let body: ControllerStatusUpdate =
                        serde_json::from_value(body).map_err(|e| {
                            ServerError::bad_request(format!("invalid request body: {e}"))
                        })?;
                    let resp = apply_controller_status(
                        ctx,
                        &controller,
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
                        &controller,
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
    parse_resource_path(&raw)?;
    let mut attempt = 1;
    loop {
        let write = ctx.authz.begin_write(&auth).await?;
        let outcome = match delete_once(ctx, write.context(), &raw).await {
            Ok(response) => write.commit().await.map(|()| response),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(response) => return Ok(response),
            Err(error) if ResourceAuthorizer::should_retry(&error, attempt) => attempt += 1,
            Err(error) => return Err(error),
        }
    }
}

async fn delete_once(
    ctx: &ResourceApiCtx,
    authz: &AuthorizationContext,
    raw: &str,
) -> Result<Response, ServerError> {
    let raw_path = parse_resource_path(raw)?;
    match classify_path(authz.store(), raw_path).await? {
        ResolvedPath::Item { resolved, leaf } => {
            let row = resolve_leaf(authz.store(), &resolved, &leaf).await?;
            delete_resource(ctx, authz, &row, &resolved.info.api_version).await
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

/// The response envelope for one resource, with `effectiveLabels` resolved from
/// the ancestry authorization already walked (ADR-0001 §6.1 — one computation
/// serving both the display value and the `labelSelector` match).
fn resource_response(
    row: &ResourceRow,
    response_api_version: &str,
    target: &ResourceTree,
) -> Result<rise_resource_api::Resource, ServerError> {
    let mut resource = response_resource(row, response_api_version)?;
    resource.metadata.effective_labels = target.effective_labels();
    Ok(resource)
}

/// Report an `anyhow` failure from inside the write transaction, preserving a
/// lost serialization race as retryable.
///
/// Helpers that predate the store contract report `anyhow::Error`, which has no
/// place to carry "replay me". Recovering the SQLSTATE keeps the retry loop
/// working for statements that do not speak `StoreError`.
fn serialization_aware_internal(error: anyhow::Error, message: &str) -> ServerError {
    if error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<sqlx::Error>())
        .any(rise_resource_store_postgres::is_serialization_failure)
    {
        return store_error_to_server_error(rise_resource_api::StoreError::Serialization);
    }
    ServerError::internal_anyhow(error, message.to_owned())
}

/// Queue one write's audit record, to be emitted when the transaction commits.
///
/// The record is deferred rather than logged inline because a serialization
/// retry rolls the write back: a `resource.created` line for a create that never
/// happened would make the trail worse than useless.
fn audit_write(
    authz: &AuthorizationContext,
    row: &ResourceRow,
    event: &'static str,
    revision: Option<i64>,
) {
    let actor = authz.actor().to_owned();
    let uid = row.uid;
    let api_version = row.api_version.clone();
    let kind = row.kind.clone();
    let name = row.name.clone();
    let parent_uid = row.parent_uid;
    authz.audit(move || {
        tracing::info!(
            target: "rise::audit",
            actor = %actor,
            uid = %uid,
            api_version = %api_version,
            kind = %kind,
            name = %name,
            parent_uid = ?parent_uid,
            revision = ?revision,
            "{event}"
        );
    });
}

/// The body a write returns, at the granularity the caller may read.
///
/// A write verb is not a read grant: ADR-0001 §2 keeps main-resource and
/// subresource permissions separate, so a caller who may set `status` has not
/// thereby been given the `spec`. Anything the caller cannot `get` comes back
/// projected onto the same allowlist a `list`-only item uses — which still
/// echoes everything they just sent, because a write body is their own input.
async fn write_response(
    authz: &AuthorizationContext,
    row: &ResourceRow,
    response_api_version: &str,
    target: &ResourceTree,
) -> Result<serde_json::Value, ServerError> {
    let readable = authz.allows(target, Verb::Get, None).await?;
    project_list_item(
        &resource_response(row, response_api_version, target)?,
        read_granularity(readable),
    )
}

fn read_granularity(readable: bool) -> ReadGranularity {
    if readable {
        ReadGranularity::Full
    } else {
        ReadGranularity::ListOnly
    }
}

fn subresource_name(subresource: Subresource) -> Result<SubresourceName, ServerError> {
    subresource.keyword().parse().map_err(|error| {
        ServerError::internal(format!("subresource keyword is not a valid name: {error}"))
    })
}

/// Authorize the owner references a write newly attaches.
///
/// An owner reference grants the dependent no access (ADR-0001 §1), but it is
/// not inert: deleting the owner starts deletion of the dependent, and the
/// garbage collector finishes it. Attaching one therefore does two things that
/// need authority.
///
/// It *references* the owner from another resource's fields, which is ADR-0001
/// §2's `use` verb, checked at write time of the referencing resource. And when
/// the dependent already exists, the new edge makes it deletable through a
/// resource the caller may control — so attaching it to something is
/// indistinguishable from holding `delete` on it, and is refused unless the
/// caller actually does. Without that second half, `update` on a resource plus
/// `delete` on anything the caller owns compose into `delete` on the resource,
/// which is how a `Deny` on `delete` gets around.
///
/// A create is different: the dependent is the resource being brought into
/// being, and tying its lifetime to an owner is the creator's own choice.
///
/// Only references the write *introduces* are checked. Re-sending one already
/// stored, byte for byte, is an ordinary read-modify-write, and re-authorizing
/// it would make an unrelated update fail because of an edge someone else
/// attached. The comparison is on the whole reference rather than its UID:
/// raising `blockOwnerDeletion` on a stored edge strengthens it into a hold on
/// the owner's deletion, which is a change and has to be authorized like one.
async fn authorize_owner_references(
    authz: &AuthorizationContext,
    dependent: Option<&ResourceTree>,
    before: &[rise_resource_api::OwnerReference],
    after: &[rise_resource_api::OwnerReference],
) -> Result<(), ServerError> {
    let added: Vec<_> = after
        .iter()
        .filter(|reference| !before.contains(reference))
        .collect();
    if added.is_empty() {
        return Ok(());
    }
    if let Some(dependent) = dependent {
        authz.require(dependent, Verb::Delete, None).await?;
    }
    for reference in added {
        // The owner is addressed by UID, so a refusal tells a caller who cannot
        // see it only that they may not use it.
        let target = authz.tree(reference.uid()).await?;
        authz.require(&target, Verb::Use, None).await?;
    }
    Ok(())
}

/// `allowedStatusControllerIds` is an authorization decision, and the only one
/// the grant gate cannot express.
///
/// Every id on a `ResourceDefinition`'s list grants that controller `status` and
/// `finalizers` writes over every resource of the kind, in every organization —
/// but a controller is not a subject the engine can evaluate until its identity
/// resource exists, so there is no binding to diff and no recipient to compare a
/// writer against. Until then, changing the list stays operator authority:
/// otherwise an ordinary `update` on a `ResourceDefinition` would confer
/// authority no `RoleBinding` granted and no gate ever weighed. The allowlist
/// goes away entirely once Controller identities are live (`ROADMAP.md` §1).
fn require_operator_for_controller_allowlist(
    authz: &AuthorizationContext,
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> Result<(), ServerError> {
    let ids = |spec: &serde_json::Value| -> Vec<serde_json::Value> {
        spec.get("allowedStatusControllerIds")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default()
    };
    if ids(before) == ids(after) || authz.is_operator() {
        return Ok(());
    }
    Err(ServerError::forbidden(
        "changing allowedStatusControllerIds requires operator standing: it grants \
         controllers status and finalizer writes outside the authorization model",
    ))
}

/// Run the grant gate over every authorization-changing effect of one write.
///
/// The caller has already established ordinary write authority; this is the
/// separate question of whether they may delegate what the write confers
/// (ADR-0001 §5). It runs before the store call, so a refused write never
/// reaches referential-integrity validation and never reveals whether the
/// subject it named exists (§6.6, scenario 41).
async fn run_gate(
    authz: &AuthorizationContext,
    changes: AuthorizationChangeSet,
) -> Result<(), ServerError> {
    for gated in changes {
        authz
            .gate(&gated.operation, &gated.change, gated.disclosure)
            .await?;
    }
    Ok(())
}

async fn create_resource(
    ctx: &ResourceApiCtx,
    authz: &AuthorizationContext,
    resolved: &ResolvedCollection,
    parent: Option<&ResourceRow>,
    body: CreateResourceRequest,
) -> Result<(StatusCode, Json<serde_json::Value>), ServerError> {
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

    // A create is authorized against the resource it would produce, labels
    // included: nothing in evaluation distinguishes a resource that exists from
    // one being written (ADR-0001 §4).
    let leaf = node_for_new(
        &resolved.info.storage_api_version,
        &resolved.info.kind,
        &body.metadata.name,
        &body.metadata.labels,
    )?;
    let target = authz.tree_for_new(parent.map(|row| row.uid), leaf).await?;
    authz.require(&target, Verb::Create, None).await?;

    let annotations: BTreeMap<String, String> = body.metadata.annotations.clone();
    let spec = serde_json::to_value(&body.spec)
        .map_err(|e| ServerError::bad_request(format!("invalid spec: {e}")))?;

    if resolved.info.kind == rise_resource_api::RESOURCE_DEFINITION_KIND {
        require_operator_for_controller_allowlist(authz, &serde_json::Value::Null, &spec)?;
    }
    authorize_owner_references(authz, None, &[], &body.metadata.owner_references).await?;

    let mut changes = change_for_create(
        authz,
        &resolved.info.storage_api_version,
        &resolved.info.kind,
        &body.metadata.name,
        parent,
        &spec,
    )
    .await?;
    changes.extend(label_changes(
        &target,
        // No row yet, so no K-inheriting subtree to diff: the proposed leaf is
        // the whole affected set.
        None,
        &BTreeMap::new(),
        &body.metadata.labels,
        true,
    )?);
    run_gate(authz, changes).await?;

    let params = CreateResourceParams {
        labels: body.metadata.labels,
        api_version: resolved.info.storage_api_version.clone(),
        kind: body.kind,
        name: body.metadata.name,
        parent_uid: parent.map(|row| row.uid),
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
        authz
            .store()
            .register_resource_definition(params)
            .await
            .map_err(store_error_to_server_error)?
    } else {
        authz
            .store()
            .create(params)
            .await
            .map_err(store_error_to_server_error)?
    };

    audit_write(authz, &row, "resource.created", None);
    let _ = ctx;
    // Projected like every other write: a `create` grant is not a read grant,
    // and the stored row carries more than the caller sent — the server-assigned
    // UID, and for a policy kind the contextual normalization admission applied
    // to the spec.
    Ok((
        StatusCode::CREATED,
        Json(write_response(authz, &row, &resolved.info.api_version, &target).await?),
    ))
}

async fn update_resource(
    ctx: &ResourceApiCtx,
    authz: &AuthorizationContext,
    resolved: &ResolvedCollection,
    row: &ResourceRow,
    leaf: &LeafRef,
    body: UpdateResourceRequest,
) -> Result<Json<serde_json::Value>, ServerError> {
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

    // A main write preserves finalizers (ADR-0001 §2): only
    // `(update, Kind, finalizers)` may change them, and permissions never flow
    // implicitly between the main resource and a subresource. A
    // read-modify-write client carries the stored list back unchanged and is
    // unaffected; anything else is refused here rather than silently ignored,
    // so a client that meant to drop a finalizer learns it did not.
    if body.metadata.finalizers != row.finalizers {
        return Err(ServerError::forbidden(
            "a main-resource write cannot change metadata.finalizers; use the \
             finalizers subresource, which is a separate grant",
        ));
    }

    // The target carries the resource's *stored* labels: a label diff is
    // measured against the world as it stands, which is also the world the
    // writer's own authority is measured in (ADR-0001 §6.6).
    let target = authz.tree(row.uid).await?;
    authz.require(&target, Verb::Update, None).await?;

    let annotations: BTreeMap<String, String> = body.metadata.annotations.clone();
    let spec = serde_json::to_value(&body.spec)
        .map_err(|e| ServerError::bad_request(format!("invalid spec: {e}")))?;

    if resolved.info.kind == rise_resource_api::RESOURCE_DEFINITION_KIND {
        require_operator_for_controller_allowlist(authz, &row.spec, &spec)?;
    }
    authorize_owner_references(
        authz,
        Some(&target),
        &row.owner_references,
        &body.metadata.owner_references,
    )
    .await?;

    let mut changes = change_for_update(authz, row, &spec).await?;
    changes.extend(label_changes(
        &target,
        Some(row.uid),
        &row.labels,
        &body.metadata.labels,
        false,
    )?);
    run_gate(authz, changes).await?;

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
        authz
            .store()
            .update_resource_definition(row.uid, params)
            .await
            .map_err(store_error_to_server_error)?
    } else {
        authz
            .store()
            .update(row.uid, params)
            .await
            .map_err(store_error_to_server_error)?
    };

    audit_write(authz, &updated, "resource.updated", Some(updated.revision));
    let _ = ctx;
    // The written labels are the resource's own after this update, so the
    // response reports the value the next request will evaluate against.
    let updated_target = authz.tree(updated.uid).await?;
    Ok(Json(
        write_response(authz, &updated, &resolved.info.api_version, &updated_target).await?,
    ))
}

async fn delete_resource(
    ctx: &ResourceApiCtx,
    authz: &AuthorizationContext,
    row: &ResourceRow,
    response_api_version: &str,
) -> Result<Response, ServerError> {
    let target = authz.tree(row.uid).await?;
    authz.require(&target, Verb::Delete, None).await?;
    // Deleting policy is a grant: removing a Deny can enlarge what another
    // binding allows (ADR-0001 §5, scenario 31).
    run_gate(authz, change_for_delete(authz, row).await?).await?;

    // Organization deletes route through the canonical guard so the typed
    // soft-links (users via `user_organization_memberships`, teams, projects)
    // are never orphaned. See `super::organization` module docs. Both halves run
    // on this request's transaction, so a typed insert racing the count cannot
    // slip between the check and the delete.
    let outcome = if row.kind == rise_resource_api::ORGANIZATION_KIND {
        use super::organization::{delete_organization_guarded, OrganizationDeleteError};
        match delete_organization_guarded(authz.store(), authz.session(), row.uid).await {
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
                // The count runs inside the request's serializable transaction,
                // so it can lose the race like any other statement in it. It
                // reports `anyhow`, which carries no store classification, so
                // the SQLSTATE is recovered here rather than surfacing a lost
                // race as a hard 500 the caller cannot act on.
                return Err(serialization_aware_internal(
                    e,
                    "Failed to count typed children for organization",
                ));
            }
            Err(OrganizationDeleteError::Store(e)) => {
                return Err(store_error_to_server_error(e));
            }
        }
    } else {
        authz
            .store()
            .delete(row.uid)
            .await
            .map_err(store_error_to_server_error)?
    };

    // A single static event message keeps this audit log consistent with
    // `resource.created` / `resource.updated`.
    audit_write(authz, row, "resource.deleted", None);
    let _ = ctx;

    match outcome {
        DeleteOutcome::Deleted => Ok((
            StatusCode::OK,
            Json(serde_json::json!({"deleted": true, "uid": row.uid})),
        )
            .into_response()),
        DeleteOutcome::MarkedForDeletion(marked) => {
            let resource = write_response(authz, &marked, response_api_version, &target).await?;
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

/// A user's `status` write, authorized by `(update, Kind, status)`.
///
/// The value lands in the writer-keyed slot the store reserves for non-
/// controller writers, so a controller's own slot is never overwritten by a
/// human edit. That slot is still named for the operator tier it was introduced
/// for; ADR-0002's subresource execution model owns the field separation and is
/// where the naming is settled, not here.
async fn apply_user_status(
    authz: &AuthorizationContext,
    row: &ResourceRow,
    body: ControllerStatusUpdate,
    response_api_version: &str,
    target: &ResourceTree,
) -> Result<Json<serde_json::Value>, ServerError> {
    // Keyed on the caller's stable subject, never their email: the slot is
    // stored inside the resource document and served to every reader of it,
    // which makes it product data rather than an audit record (ADR-0001 §1 —
    // user identity is not email).
    let updated = authz
        .store()
        .operator_update_status(row.uid, authz.subject().as_ref(), body.status)
        .await
        .map_err(store_error_to_server_error)?;
    audit_write(authz, row, "resource.user_status_updated", None);
    Ok(Json(
        write_response(authz, &updated, response_api_version, target).await?,
    ))
}

/// A user's `finalizers` write, authorized by `(update, Kind, finalizers)`.
///
/// Finalizer keys in the reserved `system.rise.dev/*` namespace stay refused by
/// the store: those are lifecycle bookkeeping the garbage collector owns, and no
/// RBAC grant makes them writable.
async fn apply_user_finalizers(
    authz: &AuthorizationContext,
    row: &ResourceRow,
    body: ControllerFinalizerUpdate,
    response_api_version: &str,
    target: &ResourceTree,
) -> Result<Json<serde_json::Value>, ServerError> {
    let updated = authz
        .store()
        .operator_update_finalizers(row.uid, authz.subject().as_ref(), &body.add, &body.remove)
        .await
        .map_err(store_error_to_server_error)?;
    audit_write(authz, row, "resource.user_finalizers_updated", None);
    Ok(Json(
        write_response(authz, &updated, response_api_version, target).await?,
    ))
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
    use crate::db::models::User;
    use rise_resource_api::RESOURCE_DEFINITION_KIND;
    use rise_resource_store_postgres::PgResourceStore;
    use serde_json::{json, Value};

    const OPERATOR: &str = "operator@example.com";
    const PLAIN_USER: &str = "plain-user@example.com";

    /// Build a `ResourceApiCtx` over a real `PgResourceStore`, with `OPERATOR`
    /// on the operator email allowlist. The resource store schema is layered on
    /// top of the root migrations `#[sqlx::test]` already ran, and the baseline
    /// policy is seeded exactly as startup seeds it — without it an operator
    /// would still reach everything (the evaluator hardcodes that), but nothing
    /// else in the model would be present to reason about.
    async fn ctx(pool: sqlx::PgPool) -> ResourceApiCtx {
        ctx_with_operators(pool, vec![OPERATOR.into()], vec![]).await
    }

    async fn ctx_with_operators(
        pool: sqlx::PgPool,
        operator_users: Vec<String>,
        operator_idp_groups: Vec<String>,
    ) -> ResourceApiCtx {
        rise_resource_store_postgres::run_migrations(&pool)
            .await
            .expect("resource store migrations");
        let pg_store = Arc::new(PgResourceStore::new(pool.clone()));
        let store: Arc<dyn ResourceStore> = pg_store.clone();
        crate::server::policy_seed::run(&store)
            .await
            .expect("seed baseline policy");
        ResourceApiCtx {
            store,
            authz: ResourceAuthorizer::new(
                pg_store,
                pool,
                crate::server::authz::OperatorSelectors {
                    users: Arc::new(operator_users),
                    idp_groups: Arc::new(operator_idp_groups),
                },
            ),
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

    /// Grant every authenticated caller `statements`, platform-wide.
    ///
    /// One `PlatformRole` plus one `PlatformRoleBinding` on
    /// `system:authenticated` at wildcard scope — the only shape that reaches an
    /// ordinary principal today, since a binding naming a `user:` subject needs
    /// a `User` resource and those go live with identity resolution.
    async fn grant_authenticated(ctx: &ResourceApiCtx, name: &str, statements: Value) {
        create_at(
            ctx,
            "rise.dev/v1alpha1/platformroles",
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "PlatformRole",
                "metadata": {"name": name},
                "spec": {"statements": statements},
            }),
        )
        .await;
        create_at(
            ctx,
            "rise.dev/v1alpha1/platformrolebindings",
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "PlatformRoleBinding",
                "metadata": {"name": name},
                "spec": {
                    "subject": "system:authenticated",
                    "roleRef": {"kind": "PlatformRole", "name": name},
                },
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
    // Authorization: what stored policy grants, per resource
    // -------------------------------------------------------------------------

    /// A caller with no applicable `list` grant gets a masked-empty collection,
    /// not a 403 that would confirm the scope is populated (ADR-0001 §4).
    #[sqlx::test]
    async fn list_without_a_grant_is_masked_empty(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "one").await;

        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(PLAIN_USER),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("a list with no grant is empty, not refused");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().expect("items").len(), 0);
    }

    /// An item the caller holds no `get` on is refused. Unlike a collection,
    /// naming one resource exactly is not a scope the caller could be shown a
    /// filtered view of.
    #[sqlx::test]
    async fn item_without_a_grant_is_refused(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "one").await;

        let err = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets/one".to_string(),
            auth(PLAIN_USER),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("no get grant");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert!(err.message.contains("not authorized to get"));
    }

    /// A write is refused the same way, and the resource is left untouched.
    #[sqlx::test]
    async fn write_without_a_grant_is_refused(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        let err = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(PLAIN_USER),
            widget_body("example.dev/v1", "denied"),
        )
        .await
        .expect_err("no create grant");
        assert_eq!(err.status, StatusCode::FORBIDDEN);

        // Nothing was written: the refusal happened inside the transaction that
        // would have carried the create, and it rolled back.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("operator list");
        let (_, body) = read(resp).await;
        assert_eq!(body["items"].as_array().expect("items").len(), 0);
    }

    /// `list` without `get` returns the allowlisted projection: `apiVersion`,
    /// `kind`, and the documented `metadata` fields — never `spec` (ADR-0001
    /// §4, scenario 37). Adding `get` expands the same item to the full stored
    /// object (scenario 38).
    #[sqlx::test]
    async fn list_grant_projects_metadata_and_get_expands_it(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "one").await;
        grant_authenticated(
            &ctx,
            "widget-lister",
            json!([{
                "effect": "Allow",
                "kinds": ["example.dev/Widget"],
                "verbs": ["list"],
            }]),
        )
        .await;

        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(PLAIN_USER),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("list with a list grant");
        let (_, body) = read(resp).await;
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["metadata"]["name"], "one");
        assert!(
            items[0].get("spec").is_none(),
            "list-only projection must not carry spec: {}",
            items[0]
        );
        assert!(items[0]["metadata"].get("uid").is_none());

        // The same caller, now also holding `get`, sees the whole object.
        grant_authenticated(
            &ctx,
            "widget-reader",
            json!([{
                "effect": "Allow",
                "kinds": ["example.dev/Widget"],
                "verbs": ["get"],
            }]),
        )
        .await;
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(PLAIN_USER),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("list with list+get");
        let (_, body) = read(resp).await;
        let items = body["items"].as_array().expect("items");
        assert_eq!(items[0]["spec"]["size"], "large");
        assert!(items[0]["metadata"]["uid"].is_string());
    }

    /// `effectiveLabels` is resolved from the ancestor chain on every read, so a
    /// child with no value of its own reports the one it inherits (ADR-0001
    /// §6.1) — the same value a `labelSelector` matches against.
    #[sqlx::test]
    async fn effective_labels_resolve_through_the_ancestor_chain(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_gadget_rd(&ctx, &[]).await;
        create_at(
            &ctx,
            "rise.dev/v1alpha1/organizations",
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "Organization",
                "metadata": {"name": "acme", "labels": {"rise.dev/owner": "group:platform"}},
                "spec": {"displayName": "acme"},
            }),
        )
        .await;
        create_at(
            &ctx,
            "example.dev/v1/gadgets/acme",
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Gadget",
                "metadata": {"name": "inherits"},
                "spec": {},
            }),
        )
        .await;

        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/gadgets/acme/inherits".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get gadget");
        let (_, body) = read(resp).await;
        // The child sets no label of its own...
        assert!(body["metadata"].get("labels").is_none());
        // ...but reports the ancestor's value as in force.
        assert_eq!(
            body["metadata"]["effectiveLabels"]["rise.dev/owner"],
            "group:platform"
        );

        // A child's own value shadows the ancestor's rather than unioning.
        let resp = dispatch_post_inner(
            &ctx,
            "example.dev/v1/gadgets/acme".to_string(),
            auth(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Gadget",
                "metadata": {"name": "shadows", "labels": {"rise.dev/owner": "group:devops"}},
                "spec": {},
            }),
        )
        .await
        .expect("create shadowing gadget");
        let (_, created) = read(resp).await;
        assert_eq!(
            created["metadata"]["effectiveLabels"]["rise.dev/owner"],
            "group:devops"
        );
    }

    // -------------------------------------------------------------------------
    // Subresource boundaries a main write must not cross
    // -------------------------------------------------------------------------

    /// ADR-0001 §2: main writes preserve finalizers, and only
    /// `(update, Kind, finalizers)` may change them. Permissions never flow
    /// implicitly between the main resource and a subresource, so plain
    /// `update` — which every editor holds — must not be able to clear a
    /// finalizer another controller is holding a deletion with.
    #[sqlx::test]
    async fn a_main_write_cannot_change_finalizers(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &["controller.example.com"]).await;
        let created = create_widget(&ctx, "example.dev/v1", "held").await;
        let uid = uid_of(&created);
        ctx.store
            .update_controller_finalizers(
                uid,
                "controller.example.com",
                &["controller.example.com/cleanup".to_string()],
                &[],
            )
            .await
            .expect("controller adds its finalizer");

        let fetched = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets/held".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("read back");
        let (_, fetched) = read(fetched).await;

        // Omitting `finalizers` is not "leave unchanged" — the field defaults to
        // an empty list, so this request would clear the controller's hold.
        let err = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/held".to_string(),
            any_user(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {"name": "held", "revision": fetched["metadata"]["revision"]},
                "spec": {"size": "large"},
            }),
        )
        .await
        .expect_err("a main write must not drop finalizers");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert!(
            err.message.contains("finalizers subresource"),
            "{}",
            err.message
        );

        // Carrying the stored list back unchanged is an ordinary
        // read-modify-write and is accepted.
        let resp = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/held".to_string(),
            any_user(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "held",
                    "revision": fetched["metadata"]["revision"],
                    "finalizers": ["controller.example.com/cleanup"],
                },
                "spec": {"size": "small"},
            }),
        )
        .await
        .expect("an unchanged finalizer list is fine");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// A reserved `system.rise.dev/*` finalizer is lifecycle bookkeeping the
    /// store owns. Planting one through a create would make the resource
    /// undeletable through every route the API offers — the `finalizers`
    /// subresource refuses to remove a reserved name even for an operator.
    #[sqlx::test]
    async fn a_create_cannot_plant_a_reserved_finalizer(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;

        let err = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "undeletable",
                    "finalizers": ["system.rise.dev/cascade-deletion"],
                },
                "spec": {},
            }),
        )
        .await
        .expect_err("a reserved finalizer must be refused");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("reserved"), "{}", err.message);
    }

    /// A write verb is not a read grant (ADR-0001 §2). A caller who may set
    /// `status` has not been given the `spec`, so the response comes back at the
    /// granularity they may read.
    #[sqlx::test]
    async fn a_status_write_does_not_return_the_spec(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        create_widget(&ctx, "example.dev/v1", "w1").await;
        grant_authenticated(
            &ctx,
            "status-writer",
            json!([{
                "effect": "Allow",
                "kinds": ["example.dev/Widget"],
                "verbs": ["update"],
                "subresources": ["status"],
            }]),
        )
        .await;

        let resp = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/w1/status".to_string(),
            any_user(PLAIN_USER),
            json!({"status": {"phase": "Ready"}}),
        )
        .await
        .expect("the status grant permits the write");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["metadata"]["name"], "w1");
        assert!(
            body.get("spec").is_none(),
            "a status writer must not read the spec back: {body}"
        );
        assert!(body.get("status").is_none(), "{body}");

        // An operator holds `get`, so the same call returns the whole object.
        let resp = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/w1/status".to_string(),
            any_user(OPERATOR),
            json!({"status": {"phase": "Ready"}}),
        )
        .await
        .expect("operator status write");
        let (_, body) = read(resp).await;
        assert_eq!(body["spec"]["size"], "large");
    }

    /// `allowedStatusControllerIds` grants controllers status and finalizer
    /// writes outside the authorization model, so changing it stays operator
    /// authority until Controller identities make it expressible as policy.
    #[sqlx::test]
    async fn changing_the_controller_allowlist_requires_an_operator(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        grant_authenticated(
            &ctx,
            "rd-editor",
            json!([{
                "effect": "Allow",
                "kinds": ["rise.dev/ResourceDefinition"],
                "verbs": ["get", "list", "update"],
            }]),
        )
        .await;

        let fetched = dispatch_get_inner(
            &ctx,
            "rise.dev/v1alpha1/resourcedefinitions/widgets.example.dev".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("read the RD");
        let (_, fetched) = read(fetched).await;
        let mut spec = fetched["spec"].clone();
        spec["allowedStatusControllerIds"] = json!(["ci.example.com"]);

        let err = dispatch_put_inner(
            &ctx,
            "rise.dev/v1alpha1/resourcedefinitions/widgets.example.dev".to_string(),
            any_user(PLAIN_USER),
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "ResourceDefinition",
                "metadata": {
                    "name": "widgets.example.dev",
                    "revision": fetched["metadata"]["revision"],
                },
                "spec": spec,
            }),
        )
        .await
        .expect_err("only an operator may widen the controller allowlist");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert!(
            err.message.contains("allowedStatusControllerIds"),
            "{}",
            err.message
        );
    }

    // -------------------------------------------------------------------------
    // Owner references are a lifecycle edge, and attaching one needs authority
    // -------------------------------------------------------------------------

    /// An owner reference grants no access, but deleting the owner starts
    /// deletion of the dependent. Attaching one to an *existing* resource is
    /// therefore indistinguishable from holding `delete` on it: without the
    /// check, `update` on a resource plus `delete` on anything the caller owns
    /// compose into `delete` on the resource, which is how a `Deny` on `delete`
    /// gets around.
    #[sqlx::test]
    async fn attaching_an_owner_reference_needs_delete_on_the_dependent(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        let owner = create_widget(&ctx, "example.dev/v1", "owner").await;
        let victim = create_widget(&ctx, "example.dev/v1", "victim").await;
        // The caller may edit widgets and use the owner, but may not delete.
        grant_authenticated(
            &ctx,
            "widget-editor",
            json!([{
                "effect": "Allow",
                "kinds": ["example.dev/Widget"],
                "verbs": ["get", "list", "update", "use"],
            }]),
        )
        .await;

        let err = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/victim".to_string(),
            any_user(PLAIN_USER),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "victim",
                    "revision": victim["metadata"]["revision"],
                    "ownerReferences": [{
                        "apiVersion": "example.dev/v1",
                        "kind": "Widget",
                        "name": "owner",
                        "uid": owner["metadata"]["uid"],
                    }],
                },
                "spec": {"size": "large"},
            }),
        )
        .await
        .expect_err("attaching the edge needs delete on the dependent");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert!(err.message.contains("delete"), "{}", err.message);
    }

    /// Referencing a resource from another resource's fields is ADR-0001 §2's
    /// `use` verb, checked at write time of the referencing resource.
    #[sqlx::test]
    async fn attaching_an_owner_reference_needs_use_on_the_owner(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        let owner = create_widget(&ctx, "example.dev/v1", "owner").await;
        // Everything except `use` on the owner.
        grant_authenticated(
            &ctx,
            "widget-admin",
            json!([{
                "effect": "Allow",
                "kinds": ["example.dev/Widget"],
                "verbs": ["get", "list", "create", "update", "delete"],
            }]),
        )
        .await;

        let err = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(PLAIN_USER),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "dependent",
                    "ownerReferences": [{
                        "apiVersion": "example.dev/v1",
                        "kind": "Widget",
                        "name": "owner",
                        "uid": owner["metadata"]["uid"],
                    }],
                },
                "spec": {},
            }),
        )
        .await
        .expect_err("referencing the owner needs `use` on it");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert!(err.message.contains("use"), "{}", err.message);

        // An operator holds every verb, so the same create succeeds.
        let resp = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "dependent",
                    "ownerReferences": [{
                        "apiVersion": "example.dev/v1",
                        "kind": "Widget",
                        "name": "owner",
                        "uid": owner["metadata"]["uid"],
                    }],
                },
                "spec": {},
            }),
        )
        .await
        .expect("an operator may attach the edge");
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    /// The blockers are a collection, so they are filtered per item — a caller
    /// holding the subresource but no `list` on the children below must not
    /// receive an inventory of them by name and UID. What is withheld is
    /// counted, so the report never reads as "nothing is blocking this".
    #[sqlx::test]
    async fn deletion_blockers_are_filtered_per_item(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_gadget_rd(&ctx, &[]).await;
        create_org(&ctx, "acme").await;
        create_at(
            &ctx,
            "example.dev/v1/gadgets/acme",
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Gadget",
                "metadata": {"name": "child"},
                "spec": {},
            }),
        )
        .await;
        // The subresource, and nothing else — no `list` on Gadgets.
        grant_authenticated(
            &ctx,
            "blocker-reader",
            json!([{
                "effect": "Allow",
                "kinds": ["rise.dev/Organization"],
                "verbs": ["get"],
                "subresources": ["deletion-blockers"],
            }]),
        )
        .await;

        let resp = dispatch_get_inner(
            &ctx,
            "rise.dev/v1alpha1/organizations/acme/deletion-blockers".to_string(),
            auth(PLAIN_USER),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("the subresource grant permits the read");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["blockers"].as_array().expect("blockers").len(), 0);
        assert_eq!(body["hiddenBlockers"], 1);

        // An operator holds `list`, so the same call names the blocker.
        let resp = dispatch_get_inner(
            &ctx,
            "rise.dev/v1alpha1/organizations/acme/deletion-blockers".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("operator read");
        let (_, body) = read(resp).await;
        assert_eq!(body["blockers"][0]["name"], "child");
        assert_eq!(body["hiddenBlockers"], 0);
    }

    // -------------------------------------------------------------------------
    // The write-time grant gate
    // -------------------------------------------------------------------------

    /// §6.6's creation exception: a genuinely new resource may name its creator
    /// as owner without the general gate, because there is no prior owner to
    /// displace and nothing is delegated to anyone else.
    #[sqlx::test]
    async fn creation_may_label_a_new_resource_for_its_creator(pool: sqlx::PgPool) {
        let ctx = ctx(pool.clone()).await;
        register_widget_rd(&ctx, &[]).await;
        grant_authenticated(
            &ctx,
            "widget-author",
            json!([{
                "effect": "Allow",
                "kinds": ["example.dev/Widget"],
                "verbs": ["create", "get", "list"],
            }]),
        )
        .await;
        let caller = auth(PLAIN_USER);
        let subject = format!("user:{}", caller.user().unwrap().id);

        let resp = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            caller.clone(),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {"name": "mine", "labels": {"rise.dev/owner": subject}},
                "spec": {"size": "large"},
            }),
        )
        .await
        .expect("the creation exception carries this write");
        let (status, created) = read(resp).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            created["metadata"]["effectiveLabels"]["rise.dev/owner"],
            subject
        );

        // And the seeded ownership binding now reaches them: `resource-owner`
        // grants delete, which their own binding does not.
        let resp = dispatch_delete_inner(&ctx, "example.dev/v1/widgets/mine".to_string(), caller)
            .await
            .expect("ownership grants delete");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Scenario 41: an editor who would not hold the resulting grant cannot
    /// relabel ownership — to themselves or to anyone else. The refusal is the
    /// gate's and happens before the store resolves the named subject, so it is
    /// not an existence oracle for that subject either.
    #[sqlx::test]
    async fn relabelling_ownership_without_holding_it_is_refused(pool: sqlx::PgPool) {
        let ctx = ctx(pool.clone()).await;
        register_widget_rd(&ctx, &[]).await;
        // An editor: they may write the resource, but hold none of the
        // `resource-owner` set that owning it would confer (no `delete`).
        grant_authenticated(
            &ctx,
            "widget-editor",
            json!([{
                "effect": "Allow",
                "kinds": ["example.dev/Widget"],
                "verbs": ["create", "get", "list", "update"],
            }]),
        )
        .await;
        let caller = auth(PLAIN_USER);
        let subject = format!("user:{}", caller.user().unwrap().id);

        // Unowned, so the editor's access does not arrive through the label.
        let resp = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            caller.clone(),
            widget_body("example.dev/v1", "unowned"),
        )
        .await
        .expect("create an unowned widget");
        let (_, created) = read(resp).await;

        for value in [subject.as_str(), "user:u-someone-else"] {
            let err = dispatch_put_inner(
                &ctx,
                "example.dev/v1/widgets/unowned".to_string(),
                AnyAuth::User(caller.clone()),
                json!({
                    "apiVersion": "example.dev/v1",
                    "kind": "Widget",
                    "metadata": {
                        "name": "unowned",
                        "revision": created["metadata"]["revision"],
                        "labels": {"rise.dev/owner": value},
                    },
                    "spec": {"size": "large"},
                }),
            )
            .await
            .expect_err("the gate must refuse the redirect");
            assert_eq!(err.status, StatusCode::FORBIDDEN);
            assert!(
                err.message
                    .contains("would grant authority you do not hold"),
                "unexpected message for '{value}': {}",
                err.message
            );
        }

        // The label is unchanged: each refusal rolled its transaction back.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets/unowned".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("operator read");
        let (_, body) = read(resp).await;
        assert!(body["metadata"].get("labels").is_none());
    }

    /// The other half of §6.6: the resource's *current* owner may hand ownership
    /// on, even though their own access arrives through the very label they are
    /// replacing. The writer's side of the comparison is pinned to the old
    /// value, which is what makes a transfer expressible at all.
    #[sqlx::test]
    async fn an_owner_may_transfer_ownership(pool: sqlx::PgPool) {
        let ctx = ctx(pool.clone()).await;
        register_widget_rd(&ctx, &[]).await;
        grant_authenticated(
            &ctx,
            "widget-author",
            json!([{
                "effect": "Allow",
                "kinds": ["example.dev/Widget"],
                "verbs": ["create", "get", "list"],
            }]),
        )
        .await;
        let caller = auth(PLAIN_USER);
        let subject = format!("user:{}", caller.user().unwrap().id);

        let resp = dispatch_post_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            caller.clone(),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {"name": "mine", "labels": {"rise.dev/owner": subject}},
                "spec": {"size": "large"},
            }),
        )
        .await
        .expect("create with own ownership");
        let (_, created) = read(resp).await;

        // `update` here comes from ownership, not from the authenticated
        // binding, which grants only create/get/list.
        let resp = dispatch_put_inner(
            &ctx,
            "example.dev/v1/widgets/mine".to_string(),
            AnyAuth::User(caller),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Widget",
                "metadata": {
                    "name": "mine",
                    "revision": created["metadata"]["revision"],
                    "labels": {"rise.dev/owner": "user:u-successor"},
                },
                "spec": {"size": "large"},
            }),
        )
        .await
        .expect("an owner may hand ownership on");
        let (status, updated) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            updated["metadata"]["effectiveLabels"]["rise.dev/owner"],
            "user:u-successor"
        );
    }

    /// A writer who may create bindings still cannot hand out more than they
    /// hold: the delta the binding would confer is compared against their own
    /// effective policy over the same domain (ADR-0001 §5).
    #[sqlx::test]
    async fn grant_gate_refuses_delegating_authority_the_writer_lacks(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        // The caller may author platform bindings, and may list Widgets. That is
        // the whole of their authority.
        grant_authenticated(&ctx, "binding-author", json!([{
            "effect": "Allow",
            "kinds": ["rise.dev/PlatformRoleBinding", "rise.dev/PlatformRole", "example.dev/Widget"],
            "verbs": ["create", "get", "list"],
        }]))
        .await;

        // Binding the shipped org-admin baseline — every verb on every kind —
        // is far outside that.
        let err = dispatch_post_inner(
            &ctx,
            "rise.dev/v1alpha1/platformrolebindings".to_string(),
            auth(PLAIN_USER),
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "PlatformRoleBinding",
                "metadata": {"name": "escalate"},
                "spec": {
                    "subject": "system:authenticated",
                    "roleRef": {"kind": "PlatformRole", "name": "org-admin"},
                },
            }),
        )
        .await
        .expect_err("the gate must refuse the escalation");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert!(
            err.message
                .contains("would grant authority you do not hold"),
            "unexpected message: {}",
            err.message
        );

        // And nothing was stored — the refusal rolled its transaction back.
        let resp = dispatch_get_inner(
            &ctx,
            "rise.dev/v1alpha1/platformrolebindings/escalate".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await;
        assert_eq!(
            resp.expect_err("binding must not exist").status,
            StatusCode::NOT_FOUND
        );
    }

    /// The same writer may delegate authority they *do* hold: the delta is a
    /// subset of their own effective policy, so the gate passes.
    #[sqlx::test]
    async fn grant_gate_permits_delegating_authority_the_writer_holds(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_widget_rd(&ctx, &[]).await;
        grant_authenticated(&ctx, "binding-author", json!([{
            "effect": "Allow",
            "kinds": ["rise.dev/PlatformRoleBinding", "rise.dev/PlatformRole", "example.dev/Widget"],
            "verbs": ["create", "get", "list"],
        }]))
        .await;
        // A Role granting strictly less than the writer already holds. An
        // unbound Role body confers nothing, so authoring it is ungated
        // (scenario 30); binding it is what the gate weighs.
        create_at(
            &ctx,
            "rise.dev/v1alpha1/platformroles",
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "PlatformRole",
                "metadata": {"name": "widget-viewer"},
                "spec": {"statements": [{
                    "effect": "Allow",
                    "kinds": ["example.dev/Widget"],
                    "verbs": ["list"],
                }]},
            }),
        )
        .await;

        let resp = dispatch_post_inner(
            &ctx,
            "rise.dev/v1alpha1/platformrolebindings".to_string(),
            auth(PLAIN_USER),
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "PlatformRoleBinding",
                "metadata": {"name": "delegated"},
                "spec": {
                    "subject": "system:authenticated",
                    "roleRef": {"kind": "PlatformRole", "name": "widget-viewer"},
                },
            }),
        )
        .await
        .expect("the gate must permit a subset delegation");
        let (status, _) = read(resp).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    /// Operator standing can also be granted by IdP group. Unlike the email
    /// allowlist this reads the DB, so the user and their IdP-managed team must
    /// exist — and a team that is *not* IdP-managed must not grant it.
    ///
    /// The group lookup runs inside the request's own transaction, so this also
    /// pins that flipping `idp_managed` takes effect on the next request with
    /// nothing cached.
    #[sqlx::test]
    async fn operator_path_allows_operator_by_idp_group(pool: sqlx::PgPool) {
        let ctx = ctx_with_operators(pool.clone(), vec![], vec!["platform-operators".into()]).await;
        register_widget_rd(&ctx, &[]).await;

        let user = crate::db::users::create(&pool, "grouped@example.com")
            .await
            .unwrap();
        let auth_ctx = AuthContext::User(user.clone());

        // Same-named team that the IdP did not create grants nothing.
        let self_made = crate::db::teams::create(&pool, "platform-operators")
            .await
            .unwrap();
        crate::db::teams::add_member(
            &pool,
            self_made.id,
            user.id,
            crate::db::models::TeamRole::Owner,
        )
        .await
        .unwrap();

        // Written through the store: this install configures no operator email,
        // so there is no caller who could create it through the API yet.
        ctx.store
            .create(CreateResourceParams {
                labels: Default::default(),
                api_version: "example.dev/v1".to_string(),
                kind: "Widget".to_string(),
                name: "visible-to-operators".to_string(),
                parent_uid: None,
                annotations: BTreeMap::new(),
                finalizers: vec![],
                owner_references: vec![],
                spec: json!({}),
                validator: None,
            })
            .await
            .expect("seed a widget");

        // Not an operator, and no binding reaches them: the collection is masked
        // empty rather than refused (ADR-0001 §4).
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth_ctx.clone(),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("a list is never a 403");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["items"].as_array().expect("items").len(),
            0,
            "a self-created team must not grant operator standing"
        );

        // Once the team is IdP-managed, the same user is an operator and the
        // seeded system-admin binding reaches everything.
        crate::db::teams::set_idp_managed(&pool, self_made.id, true)
            .await
            .unwrap();

        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/widgets".to_string(),
            auth_ctx,
            PendingDeletionQuery::default(),
        )
        .await
        .expect("operator by IdP group must be allowed");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().expect("items").len(), 1);
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

    /// A *listing* under an ancestor that does not exist answers exactly as one
    /// the caller may see nothing in: empty. Distinguishing the two would make
    /// the ancestor path enumerable by name — which organizations exist, which
    /// projects they hold — right next to a per-item filter that carefully masks
    /// their contents (ADR-0001 §4).
    #[sqlx::test]
    async fn a_listing_under_a_missing_ancestor_is_masked_empty(pool: sqlx::PgPool) {
        let ctx = ctx(pool).await;
        register_gadget_rd(&ctx, &[]).await;

        // `gadgets` is Organization-scoped (depth 1). The ancestor *type* is
        // derived from the ResourceDefinition graph and cannot be mistyped in
        // the URL, so the only failure mode is an ancestor *name* with no row.
        let resp = dispatch_get_inner(
            &ctx,
            "example.dev/v1/gadgets/no-such-org".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("a listing is never a 404 for the ancestor");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().expect("items").len(), 0);

        // An *item* under the same missing ancestor is still a 404: the caller
        // named one resource exactly, and there is nothing to mask a collection
        // of.
        let err = dispatch_get_inner(
            &ctx,
            "example.dev/v1/gadgets/no-such-org/gadget".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect_err("a missing ancestor on an item path is a 404");
        assert_eq!(err.status, StatusCode::NOT_FOUND);

        // And a create under it still fails: you cannot write into a scope that
        // does not exist.
        let err = dispatch_post_inner(
            &ctx,
            "example.dev/v1/gadgets/no-such-org".to_string(),
            auth(OPERATOR),
            json!({
                "apiVersion": "example.dev/v1",
                "kind": "Gadget",
                "metadata": {"name": "orphan"},
                "spec": {},
            }),
        )
        .await
        .expect_err("a create under a missing ancestor must fail");
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

        // The diagnostics listing is a collection like any other: a caller with
        // no `list` grant on those kinds sees nothing rather than a 403 telling
        // them something is draining.
        let resp = dispatch_get_inner(
            &ctx,
            "pending-deletion".to_string(),
            auth(PLAIN_USER),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("a listing is never a 403");
        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().expect("items").len(), 0);
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
