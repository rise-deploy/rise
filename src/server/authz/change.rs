//! Turning a resource write into the changes ADR-0001 §5's grant gate diffs.
//!
//! The gate does not know what a resource write is; it knows Role bodies,
//! bindings, membership edges, identity mappings, and access-driving labels.
//! This module is the translation, and it is deliberately the only place that
//! decides whether a write is authorization-changing at all.
//!
//! Two rules govern the translation:
//!
//! * **The after-state must be the state that will be stored.** A binding's
//!   `scope` and `subject` are contextually normalized against the parent
//!   Organization before persistence, so the gate is handed
//!   `RoleBindingSpec::normalize`'s output — the same function admission calls
//!   inside the same transaction — rather than the caller's raw spec. Gating a
//!   different binding than the one written would be a hole, not a mismatch.
//! * **Undecidable means gated.** Where this module cannot tell whether a write
//!   is authorization-changing, it produces a change and lets the gate answer.
//!   The gate returns "no claims" cheaply for a write that delegates nothing.

use std::collections::BTreeMap;

use rise_authz::engine::{
    AuthorizationChange, BindingChange, BindingKind, BindingState, GroupMembershipChange,
    IdentityMappingChange, LabelChange, ResourceTree, RoleBodyChange, RoleReference,
};
use rise_resource_api::{
    LabelKey, PlatformRoleBindingSpec, ResourceRow, RoleBindingSpec, RoleRefKind, RoleSpec,
    SubjectId, SubjectMembership, UserIdentitySpec, UserSpec, API_VERSION_V1ALPHA1,
    CONTROLLER_KIND, CONTROLLER_TRUST_POLICY_KIND, GROUP_KIND, GROUP_MEMBERSHIP_KIND,
    ORGANIZATION_KIND, PLATFORM_ROLE_BINDING_KIND, PLATFORM_ROLE_KIND, ROLE_BINDING_KIND,
    ROLE_KIND, SERVICE_ACCOUNT_KIND, SERVICE_ACCOUNT_TRUST_POLICY_KIND, USER_IDENTITY_KIND,
    USER_KIND,
};
use uuid::Uuid;

use super::AuthorizationContext;
use crate::server::error::ServerError;

/// One gated change, with the phrase a refusal is worded around.
pub struct GatedChange {
    pub operation: String,
    pub change: AuthorizationChange,
}

/// Every gated change a single write produces.
///
/// A write can produce more than one: relabelling a resource changes as many
/// domains as it changes access-driving keys, and each is compared separately.
pub type AuthorizationChangeSet = Vec<GatedChange>;

/// Whether a kind's writes are ever authorization-changing by their content.
///
/// Labels are handled separately: any resource of any kind can carry an
/// access-driving label.
fn is_policy_kind(api_version: &str, kind: &str) -> bool {
    api_version == API_VERSION_V1ALPHA1
        && matches!(
            kind,
            ROLE_KIND
                | PLATFORM_ROLE_KIND
                | ROLE_BINDING_KIND
                | PLATFORM_ROLE_BINDING_KIND
                | GROUP_MEMBERSHIP_KIND
                | USER_KIND
                | USER_IDENTITY_KIND
                | CONTROLLER_TRUST_POLICY_KIND
                | SERVICE_ACCOUNT_TRUST_POLICY_KIND
        )
}

/// Whether a `UserIdentity` mapping would confer operator standing.
///
/// ADR-0001 §5 lets only an operator create such a mapping, because no ordinary
/// authority can delegate the one tier with nothing above it. The predicate is
/// configuration, not policy: an identity confers operator standing iff its
/// `(issuer, subject)` pair is in the restart-loaded `operatorIdentities` set.
///
/// That set does not exist yet — operator standing is derived from the
/// `auth.operator_users` / `auth.operator_idp_groups` configuration, which
/// selects Users by email and IdP group and never consults a `UserIdentity`
/// resource. So today no mapping can confer it, and this answers `false` for a
/// reason rather than by omission. When the selector set lands, this is the one
/// function that reads it.
fn confers_operator_standing(_identity: &UserIdentitySpec) -> bool {
    false
}

/// The gated changes a `create` produces, other than its labels.
pub async fn change_for_create(
    ctx: &AuthorizationContext,
    api_version: &str,
    kind: &str,
    name: &str,
    parent: Option<&ResourceRow>,
    spec: &serde_json::Value,
) -> Result<AuthorizationChangeSet, ServerError> {
    if !is_policy_kind(api_version, kind) {
        return Ok(Vec::new());
    }
    let organization = organization_of(ctx, parent).await?;
    Ok(match kind {
        ROLE_KIND | PLATFORM_ROLE_KIND => vec![GatedChange {
            operation: format!("creating {kind} '{name}'"),
            change: AuthorizationChange::RoleBody(RoleBodyChange {
                kind: role_ref_kind(kind),
                name: name.to_owned(),
                organization,
                before: Vec::new(),
                after: statements(spec)?,
            }),
        }],
        ROLE_BINDING_KIND | PLATFORM_ROLE_BINDING_KIND => vec![GatedChange {
            operation: format!("creating {kind} '{name}'"),
            change: AuthorizationChange::Binding(BindingChange {
                before: None,
                // A create has no row yet, and the gate uses the UID only to
                // decide which stored binding the after-universe replaces.
                after: Some(Box::new(binding_state(
                    Uuid::new_v4(),
                    kind,
                    organization,
                    spec,
                )?)),
            }),
        }],
        GROUP_MEMBERSHIP_KIND => {
            let (user, group) = membership_edge(ctx, name, parent).await?;
            vec![GatedChange {
                operation: format!("adding {user} to {group}"),
                change: AuthorizationChange::GroupMembership(GroupMembershipChange { user, group }),
            }]
        }
        USER_IDENTITY_KIND => {
            let identity = parent_identity(ctx, kind, parent).await?;
            let parsed: UserIdentitySpec = parse_spec(spec, kind)?;
            // An inactive mapping authenticates nobody, so it reaches nothing of
            // the parent identity's policy until it is activated — which is
            // itself gated below.
            if !parsed.active {
                Vec::new()
            } else {
                vec![GatedChange {
                    operation: format!("mapping an external identity onto {identity}"),
                    change: AuthorizationChange::IdentityMapping(IdentityMappingChange {
                        identity,
                        confers_operator: confers_operator_standing(&parsed),
                    }),
                }]
            }
        }
        CONTROLLER_TRUST_POLICY_KIND | SERVICE_ACCOUNT_TRUST_POLICY_KIND => {
            let identity = parent_identity(ctx, kind, parent).await?;
            vec![GatedChange {
                operation: format!("trusting an external issuer for {identity}"),
                change: AuthorizationChange::IdentityMapping(IdentityMappingChange {
                    identity,
                    confers_operator: false,
                }),
            }]
        }
        // A `User` create brings an identity with no policy into being; it
        // delegates nothing until something binds or maps to it.
        _ => Vec::new(),
    })
}

/// The gated changes an `update` produces, other than its labels.
pub async fn change_for_update(
    ctx: &AuthorizationContext,
    row: &ResourceRow,
    spec: &serde_json::Value,
) -> Result<AuthorizationChangeSet, ServerError> {
    if !is_policy_kind(&row.api_version, &row.kind) {
        return Ok(Vec::new());
    }
    let parent = parent_row(ctx, row).await?;
    let organization = organization_of(ctx, parent.as_ref()).await?;
    let kind = row.kind.as_str();
    Ok(match kind {
        ROLE_KIND | PLATFORM_ROLE_KIND => vec![GatedChange {
            operation: format!("editing {kind} '{}'", row.name),
            change: AuthorizationChange::RoleBody(RoleBodyChange {
                kind: role_ref_kind(kind),
                name: row.name.clone(),
                organization,
                before: statements(&row.spec)?,
                after: statements(spec)?,
            }),
        }],
        ROLE_BINDING_KIND | PLATFORM_ROLE_BINDING_KIND => vec![GatedChange {
            operation: format!("editing {kind} '{}'", row.name),
            change: AuthorizationChange::Binding(BindingChange {
                before: Some(Box::new(binding_state(
                    row.uid,
                    kind,
                    organization.clone(),
                    &row.spec,
                )?)),
                after: Some(Box::new(binding_state(row.uid, kind, organization, spec)?)),
            }),
        }],
        // Activation is a grant: an inactive identity reaches none of its
        // policy, so turning it on makes the whole of it reachable through that
        // credential path (ADR-0001 §5). Deactivation introduces nothing.
        USER_KIND => {
            let before: UserSpec = parse_spec(&row.spec, kind)?;
            let after: UserSpec = parse_spec(spec, kind)?;
            if after.active && !before.active {
                let identity: SubjectId = subject("user", None, &row.name)?;
                vec![GatedChange {
                    operation: format!("activating {identity}"),
                    change: AuthorizationChange::IdentityMapping(IdentityMappingChange {
                        identity,
                        confers_operator: false,
                    }),
                }]
            } else {
                Vec::new()
            }
        }
        USER_IDENTITY_KIND => {
            let before: UserIdentitySpec = parse_spec(&row.spec, kind)?;
            let after: UserIdentitySpec = parse_spec(spec, kind)?;
            if after.active && !before.active {
                let identity = parent_identity(ctx, kind, parent.as_ref()).await?;
                vec![GatedChange {
                    operation: format!("activating an external identity mapping onto {identity}"),
                    change: AuthorizationChange::IdentityMapping(IdentityMappingChange {
                        identity,
                        confers_operator: confers_operator_standing(&after),
                    }),
                }]
            } else {
                Vec::new()
            }
        }
        // A trust policy's issuer and parent are immutable, so an update can
        // only narrow the claims it accepts — which introduces no authority.
        // A GroupMembership is an empty marker whose member is its immutable
        // name; there is nothing an update can change about the edge.
        _ => Vec::new(),
    })
}

/// The gated changes a `delete` produces.
///
/// Deleting a Deny is a grant (ADR-0001 §5, scenario 31), so a Role body and a
/// binding are both diffed against emptiness. Removing a membership, an identity
/// mapping, or a trust policy only ever takes authority away.
pub async fn change_for_delete(
    ctx: &AuthorizationContext,
    row: &ResourceRow,
) -> Result<AuthorizationChangeSet, ServerError> {
    if !is_policy_kind(&row.api_version, &row.kind) {
        return Ok(Vec::new());
    }
    let parent = parent_row(ctx, row).await?;
    let organization = organization_of(ctx, parent.as_ref()).await?;
    let kind = row.kind.as_str();
    Ok(match kind {
        ROLE_KIND | PLATFORM_ROLE_KIND => vec![GatedChange {
            operation: format!("deleting {kind} '{}'", row.name),
            change: AuthorizationChange::RoleBody(RoleBodyChange {
                kind: role_ref_kind(kind),
                name: row.name.clone(),
                organization,
                before: statements(&row.spec)?,
                after: Vec::new(),
            }),
        }],
        ROLE_BINDING_KIND | PLATFORM_ROLE_BINDING_KIND => vec![GatedChange {
            operation: format!("deleting {kind} '{}'", row.name),
            change: AuthorizationChange::Binding(BindingChange {
                before: Some(Box::new(binding_state(
                    row.uid,
                    kind,
                    organization,
                    &row.spec,
                )?)),
                after: None,
            }),
        }],
        _ => Vec::new(),
    })
}

/// The gated changes an access-driving label write produces (ADR-0001 §6.6).
///
/// Every key whose *own* value changes is handed to the gate, which decides
/// whether any binding selects on it and, if so, diffs the authority over the
/// resource and its K-inheriting subtree. Keys are compared on the resource's
/// own labels: an unchanged own value cannot move the effective value on
/// anything below it.
pub fn label_changes(
    target: &ResourceTree,
    target_uid: Option<Uuid>,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    creation: bool,
) -> Result<AuthorizationChangeSet, ServerError> {
    let mut changes = Vec::new();
    for key in before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let after_value = after.get(key);
        if before.get(key) == after_value {
            continue;
        }
        let parsed: LabelKey = key.parse().map_err(|error| {
            ServerError::bad_request(format!("invalid label key '{key}': {error}"))
        })?;
        changes.push(GatedChange {
            operation: match after_value {
                Some(value) => format!("setting label '{key}' to '{value}'"),
                None => format!("removing label '{key}'"),
            },
            change: AuthorizationChange::Label(LabelChange {
                target: target.clone(),
                target_uid,
                key: parsed,
                after_value: after_value.cloned(),
                creation,
            }),
        });
    }
    Ok(changes)
}

// -----------------------------------------------------------------------------
// Resolution helpers
// -----------------------------------------------------------------------------

fn role_ref_kind(kind: &str) -> RoleRefKind {
    if kind == PLATFORM_ROLE_KIND {
        RoleRefKind::PlatformRole
    } else {
        RoleRefKind::Role
    }
}

fn statements(
    spec: &serde_json::Value,
) -> Result<Vec<rise_resource_api::PolicyStatement>, ServerError> {
    let parsed: RoleSpec = parse_spec(spec, ROLE_KIND)?;
    Ok(parsed.statements)
}

fn parse_spec<T: serde::de::DeserializeOwned>(
    spec: &serde_json::Value,
    kind: &str,
) -> Result<T, ServerError> {
    serde_json::from_value(spec.clone())
        .map_err(|error| ServerError::bad_request(format!("invalid {kind} spec: {error}")))
}

/// The binding as it will be stored, with contextual normalization applied.
fn binding_state(
    uid: Uuid,
    kind: &str,
    organization: Option<String>,
    spec: &serde_json::Value,
) -> Result<BindingState, ServerError> {
    if kind == PLATFORM_ROLE_BINDING_KIND {
        let parsed: PlatformRoleBindingSpec = parse_spec(spec, kind)?;
        let normalized = parsed
            .normalize()
            .map_err(|error| ServerError::bad_request(error.to_string()))?;
        return Ok(BindingState {
            uid,
            kind: BindingKind::PlatformRoleBinding,
            organization: None,
            subject: normalized.subject().clone(),
            subject_membership: normalized.subject_membership(),
            scope: normalized.scope().clone(),
            selector: normalized.label_selector().cloned(),
            role: RoleReference {
                kind: RoleRefKind::PlatformRole,
                name: normalized.role_ref().name.clone(),
            },
        });
    }
    let organization = organization.ok_or_else(|| {
        ServerError::bad_request("a RoleBinding must be created under an Organization")
    })?;
    let parsed: RoleBindingSpec = parse_spec(spec, kind)?;
    let normalized = parsed
        .normalize(&organization)
        .map_err(|error| ServerError::bad_request(error.to_string()))?;
    Ok(BindingState {
        uid,
        kind: BindingKind::RoleBinding,
        organization: Some(organization),
        subject: normalized.subject().clone(),
        // Org bindings reject the field: their parent-org recipient boundary is
        // structural, which `Any` is the faithful rendering of (ADR-0001 §4).
        subject_membership: SubjectMembership::Any,
        scope: normalized.scope().clone(),
        selector: normalized.label_selector().cloned(),
        role: RoleReference {
            kind: normalized.role_ref().kind,
            name: normalized.role_ref().name.clone(),
        },
    })
}

/// The Organization a policy resource is parented under, or `None` at the root.
async fn organization_of(
    ctx: &AuthorizationContext,
    parent: Option<&ResourceRow>,
) -> Result<Option<String>, ServerError> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    if parent.api_version == API_VERSION_V1ALPHA1 && parent.kind == ORGANIZATION_KIND {
        return Ok(Some(parent.name.clone()));
    }
    // A policy resource's parent is an Organization or nothing; anything else is
    // rejected by placement validation before this runs.
    let _ = ctx;
    Ok(None)
}

async fn parent_row(
    ctx: &AuthorizationContext,
    row: &ResourceRow,
) -> Result<Option<ResourceRow>, ServerError> {
    let Some(parent_uid) = row.parent_uid else {
        return Ok(None);
    };
    ctx.store()
        .get(parent_uid)
        .await
        .map_err(crate::server::resources::error_map::store_error_to_server_error)
}

/// The `(user, group)` edge a `GroupMembership` creates.
///
/// The membership's own name is the User's canonical name (ADR-0001 §1); its
/// parent Group and that Group's Organization supply the other half.
async fn membership_edge(
    ctx: &AuthorizationContext,
    name: &str,
    parent: Option<&ResourceRow>,
) -> Result<(SubjectId, SubjectId), ServerError> {
    let parent = parent.ok_or_else(|| {
        ServerError::bad_request("a GroupMembership must be created under a Group")
    })?;
    if parent.kind != GROUP_KIND {
        return Err(ServerError::bad_request(
            "a GroupMembership must be created under a Group",
        ));
    }
    let organization = ctx
        .tree(parent.uid)
        .await?
        .organization()
        .map(str::to_owned)
        .ok_or_else(|| ServerError::internal("Group is not contained by an Organization"))?;
    Ok((
        subject("user", None, name)?,
        subject("group", Some(&organization), &parent.name)?,
    ))
}

/// The identity a mapping or trust policy attaches to: its parent resource.
async fn parent_identity(
    ctx: &AuthorizationContext,
    kind: &str,
    parent: Option<&ResourceRow>,
) -> Result<SubjectId, ServerError> {
    let parent = parent.ok_or_else(|| {
        ServerError::bad_request(format!("a {kind} must be created under its identity"))
    })?;
    match parent.kind.as_str() {
        USER_KIND => subject("user", None, &parent.name),
        CONTROLLER_KIND => subject("controller", None, &parent.name),
        SERVICE_ACCOUNT_KIND => {
            let organization = ctx
                .tree(parent.uid)
                .await?
                .organization()
                .map(str::to_owned)
                .ok_or_else(|| {
                    ServerError::internal("ServiceAccount is not contained by an Organization")
                })?;
            subject("serviceaccount", Some(&organization), &parent.name)
        }
        other => Err(ServerError::bad_request(format!(
            "a {kind} cannot be parented under {other}"
        ))),
    }
}

fn subject(kind: &str, organization: Option<&str>, name: &str) -> Result<SubjectId, ServerError> {
    let rendered = match organization {
        Some(organization) => format!("{kind}:{organization}/{name}"),
        None => format!("{kind}:{name}"),
    };
    rendered
        .parse()
        .map_err(|error| ServerError::bad_request(format!("invalid subject '{rendered}': {error}")))
}
