//! Same-transaction resolution of the references a policy binding carries.
//!
//! ADR-0001 §4 makes both of a binding's references resolvable facts rather
//! than free-form strings: a `Scope` names a real node in the resource tree,
//! and a `roleRef` names a real policy object at a placement the binding is
//! allowed to reach. Resolving them here — inside the write transaction, under
//! `FOR SHARE` — is what makes a stored binding mean the same thing when the
//! authorization engine later reads it back.

use rise_resource_api::{
    BindingSubject, ResourceDefinitionSpec, Scope, StoreError, SubjectId, API_GROUP,
    CONTROLLER_KIND, GROUP_KIND, MAX_PARENT_CHAIN_DEPTH, ORGANIZATION_KIND, SERVICE_ACCOUNT_KIND,
    USER_KIND,
};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::builtin::BuiltInRegistry;
use crate::models::PgResourceRow;

/// One resolved node of a `Scope` path.
pub(crate) struct ScopeNode {
    pub(crate) uid: Uuid,
    pub(crate) kind: String,
    pub(crate) name: String,
}

/// A fully resolved, non-wildcard `Scope`: every ancestor root-first, then the
/// leaf the scope names.
pub(crate) struct ResolvedScope {
    nodes: Vec<ScopeNode>,
}

impl ResolvedScope {
    /// The outermost ancestor. For an org-contained scope this is the
    /// Organization the whole path hangs under, which is what both containment
    /// rules are stated against.
    pub(crate) fn root(&self) -> &ScopeNode {
        self.nodes
            .first()
            .expect("a resolved scope always has at least its leaf")
    }
}

/// Resolve a non-wildcard `Scope` against live rows, locking each node against
/// concurrent deletion for the rest of the transaction.
///
/// The caller handles `*` before calling: a wildcard names no node.
pub(crate) async fn resolve_scope(
    conn: &mut PgConnection,
    builtins: &BuiltInRegistry,
    scope: &Scope,
) -> Result<ResolvedScope, StoreError> {
    let resource_kind = scope
        .resource_kind()
        .ok_or_else(|| StoreError::Validation("a wildcard scope names no resource".into()))?;
    let chain = kind_chain(conn, builtins, resource_kind.group(), resource_kind.kind()).await?;

    let names: Vec<&str> = scope.names().collect();
    if names.len() != chain.len() {
        return Err(StoreError::Validation(format!(
            "scope '{scope}' must name {} ancestor(s) and then its own name; got {}",
            chain.len().saturating_sub(1),
            names.len()
        )));
    }

    // Root-first, so each step already holds its parent's UID. This is also the
    // order `validate_builtin_placement` leaves the parent chain locked in for
    // a built-in write, keeping one direction of traversal across admission.
    let mut nodes: Vec<ScopeNode> = Vec::with_capacity(chain.len());
    let mut parent_uid: Option<Uuid> = None;
    for ((group, kind), name) in chain.iter().zip(&names) {
        let row = live_row_by_name(conn, group, kind, name, parent_uid).await?;
        let Some(row) = row else {
            return Err(StoreError::Validation(format!(
                "scope '{scope}' must identify a live resource; no {group}/{kind} named '{name}' exists at that position"
            )));
        };
        parent_uid = Some(row.uid);
        nodes.push(ScopeNode {
            uid: row.uid,
            kind: row.kind,
            name: row.name,
        });
    }

    Ok(ResolvedScope { nodes })
}

/// Every `(group, kind)` from the scope's leaf up to its root ancestor,
/// returned root-first.
///
/// Built-in kinds resolve through the immutable registry; external kinds
/// through their `ResourceDefinition`. A scope may therefore target any
/// registered kind, which is what lets a binding reach product resources once
/// they migrate onto the generic store.
async fn kind_chain(
    conn: &mut PgConnection,
    builtins: &BuiltInRegistry,
    group: &str,
    kind: &str,
) -> Result<Vec<(String, String)>, StoreError> {
    let mut chain: Vec<(String, String)> = Vec::new();
    let mut current = (group.to_owned(), kind.to_owned());
    loop {
        if chain.len() >= MAX_PARENT_CHAIN_DEPTH {
            return Err(StoreError::Validation(format!(
                "scope kind '{group}/{kind}' exceeds the maximum parent chain depth"
            )));
        }
        let parent = parent_of_kind(conn, builtins, &current.0, &current.1).await?;
        chain.push(current);
        match parent {
            Some(parent) => current = parent,
            None => break,
        }
    }
    chain.reverse();
    Ok(chain)
}

/// The declared parent `(group, kind)` of one registered kind, or `None` when
/// the kind is root-scoped.
async fn parent_of_kind(
    conn: &mut PgConnection,
    builtins: &BuiltInRegistry,
    group: &str,
    kind: &str,
) -> Result<Option<(String, String)>, StoreError> {
    if let Some(registration) = builtins.lookup_by_group_kind(group, kind) {
        return registration
            .parent
            .as_ref()
            .map(|parent| split_group(&parent.api_version, &parent.kind))
            .transpose();
    }

    let row = sqlx::query(
        r#"
        SELECT resource.spec
        FROM resource_store.resource_definitions definition
        JOIN resource_store.resources resource ON resource.uid = definition.uid
        WHERE definition.group_name = $1 AND definition.kind = $2
        "#,
    )
    .bind(group)
    .bind(kind)
    .fetch_optional(&mut *conn)
    .await
    .map_err(StoreError::backend)?
    .ok_or_else(|| {
        StoreError::Validation(format!("scope names unknown ResourceKind '{group}/{kind}'"))
    })?;

    let spec: ResourceDefinitionSpec = serde_json::from_value(
        row.try_get("spec").map_err(StoreError::backend)?,
    )
    .map_err(|error| {
        StoreError::Validation(format!(
            "ResourceDefinition for '{group}/{kind}' has an invalid spec: {error}"
        ))
    })?;

    spec.parent
        .as_ref()
        .map(|parent| split_group(&parent.api_version, &parent.kind))
        .transpose()
}

fn split_group(api_version: &str, kind: &str) -> Result<(String, String), StoreError> {
    let (group, _) = api_version.split_once('/').ok_or_else(|| {
        StoreError::Validation(format!(
            "parent reference '{api_version}' is not '<group>/<version>'"
        ))
    })?;
    Ok((group.to_owned(), kind.to_owned()))
}

/// Look up one live row by API group, kind, name, and parent, holding it for
/// the rest of the transaction.
///
/// Matching on the API *group* rather than the full `api_version` mirrors
/// `get_by_name`: a scope is version-independent (ADR-0001 §8), so it resolves
/// the row whichever declared version it is stored at.
async fn live_row_by_name(
    conn: &mut PgConnection,
    group: &str,
    kind: &str,
    name: &str,
    parent_uid: Option<Uuid>,
) -> Result<Option<PgResourceRow>, StoreError> {
    let row = match parent_uid {
        None => {
            sqlx::query_as::<_, PgResourceRow>(
                r#"
                SELECT *
                FROM resource_store.resources
                WHERE split_part(api_version, '/', 1) = $1
                  AND kind = $2
                  AND name = $3
                  AND parent_uid IS NULL
                  AND deletion_timestamp IS NULL
                FOR SHARE
                "#,
            )
            .bind(group)
            .bind(kind)
            .bind(name)
            .fetch_optional(&mut *conn)
            .await
        }
        Some(parent_uid) => {
            sqlx::query_as::<_, PgResourceRow>(
                r#"
                SELECT *
                FROM resource_store.resources
                WHERE split_part(api_version, '/', 1) = $1
                  AND kind = $2
                  AND name = $3
                  AND parent_uid = $4
                  AND deletion_timestamp IS NULL
                FOR SHARE
                "#,
            )
            .bind(group)
            .bind(kind)
            .bind(name)
            .bind(parent_uid)
            .fetch_optional(&mut *conn)
            .await
        }
    };
    row.map_err(StoreError::backend)
}

/// Resolve one live policy object by name at an exact placement.
///
/// `parent_uid` is `None` for the root-parented `PlatformRole` and the owning
/// Organization's UID for an org `Role`. Placement is part of the lookup, not
/// a check after it: that is what makes ADR-0001's "references are never
/// resolved by bare-name fallback" structural — an org's own `Role` named
/// `resource-owner` is simply not a candidate for a `{ kind: PlatformRole }`
/// reference.
pub(crate) async fn resolve_policy_ref(
    conn: &mut PgConnection,
    api_version: &str,
    kind: &str,
    name: &str,
    parent_uid: Option<Uuid>,
) -> Result<PgResourceRow, StoreError> {
    let (group, _) = api_version.split_once('/').ok_or_else(|| {
        StoreError::Validation(format!(
            "built-in api_version '{api_version}' is not '<group>/<version>'"
        ))
    })?;
    live_row_by_name(conn, group, kind, name, parent_uid)
        .await?
        .ok_or_else(|| {
            StoreError::Validation(match parent_uid {
                Some(_) => format!("roleRef must identify a live {kind} named '{name}' in this binding's own Organization"),
                None => format!("roleRef must identify a live root-scoped {kind} named '{name}'"),
            })
        })
}

/// One resolved Organization, kept so a caller that both resolves an
/// org-native subject and checks containment reads the row once.
pub(crate) struct ResolvedOrganization {
    pub(crate) uid: Uuid,
    pub(crate) name: String,
}

/// Resolve a binding's literal subject against live rows.
///
/// `SubjectId` parsing is syntax only; ADR-0001 requires the identity a
/// literal subject names to exist at policy write time, so a typo in a Group
/// or User name fails the write instead of silently producing a binding that
/// can never match. Returns the subject's Organization when the subject is
/// org-native, because the platform containment rule is stated against it.
///
/// A dynamic template resolves per-resource at evaluation time and has nothing
/// to look up here. `system:authenticated` names a population rather than a
/// row; `system:operators` never reaches this point.
pub(crate) async fn resolve_binding_subject(
    conn: &mut PgConnection,
    subject: &BindingSubject,
) -> Result<Option<ResolvedOrganization>, StoreError> {
    let Some(literal) = subject.literal() else {
        return Ok(None);
    };
    let name = literal.name();
    match literal.kind() {
        "system" => Ok(None),
        "user" => {
            resolve_subject_row(conn, USER_KIND, name, None, literal).await?;
            Ok(None)
        }
        "controller" => {
            resolve_subject_row(conn, CONTROLLER_KIND, name, None, literal).await?;
            Ok(None)
        }
        // `org:<name>` is a virtual predicate over that org's members rather
        // than a stored identity, but the organization it predicates over must
        // still exist.
        "org" => {
            resolve_subject_row(conn, ORGANIZATION_KIND, name, None, literal).await?;
            Ok(None)
        }
        kind => {
            let organization = literal.organization().ok_or_else(|| {
                StoreError::Validation(format!("subject '{literal}' names no organization"))
            })?;
            let organization_row =
                resolve_subject_row(conn, ORGANIZATION_KIND, organization, None, literal).await?;
            let identity_kind = if kind == "group" {
                GROUP_KIND
            } else {
                SERVICE_ACCOUNT_KIND
            };
            resolve_subject_row(
                conn,
                identity_kind,
                name,
                Some(organization_row.uid),
                literal,
            )
            .await?;
            Ok(Some(ResolvedOrganization {
                uid: organization_row.uid,
                name: organization_row.name,
            }))
        }
    }
}

async fn resolve_subject_row(
    conn: &mut PgConnection,
    kind: &str,
    name: &str,
    parent_uid: Option<Uuid>,
    subject: &SubjectId,
) -> Result<PgResourceNode, StoreError> {
    let row = live_row_by_name(conn, API_GROUP, kind, name, parent_uid)
        .await?
        .ok_or_else(|| {
            StoreError::Validation(format!(
                "subject '{subject}' must identify a live {kind} named '{name}'"
            ))
        })?;
    Ok(PgResourceNode {
        uid: row.uid,
        name: row.name,
    })
}

struct PgResourceNode {
    uid: Uuid,
    name: String,
}

/// Require a resolved scope to hang under one specific Organization.
///
/// Both of ADR-0001 §4's containment rules reduce to this: an org
/// `RoleBinding` may not leave its parent org's subtree, and a
/// `PlatformRoleBinding` naming a static org-native subject may not reach
/// outside that subject's own org. A scope whose outermost node is not an
/// Organization at all — a root-scoped kind such as `rise.dev/User/alice` —
/// lies in no org subtree and fails the same way.
pub(crate) fn require_scope_within_organization(
    scope: &Scope,
    resolved: &ResolvedScope,
    organization_uid: Uuid,
    organization_name: &str,
    explanation: &str,
) -> Result<(), StoreError> {
    let root = resolved.root();
    if root.kind == ORGANIZATION_KIND && root.uid == organization_uid {
        return Ok(());
    }
    Err(StoreError::Validation(format!(
        "scope '{scope}' resolves under {} '{}', not Organization '{organization_name}'; {explanation}",
        root.kind, root.name
    )))
}
