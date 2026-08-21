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

use super::{AuthorizationContext, Disclosure};
use crate::server::error::ServerError;

/// One gated change, with the phrase a refusal is worded around and how much of
/// a refusal may be shown to the caller.
pub struct GatedChange {
    pub operation: String,
    pub change: AuthorizationChange,
    /// Whether this change's recipients and domains were reconstructed from the
    /// request or read out of stored policy. See [`Disclosure`].
    pub disclosure: Disclosure,
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
        // A Role body's claims name every binding that references it — stored
        // policy, in any organization.
        ROLE_KIND | PLATFORM_ROLE_KIND => vec![GatedChange {
            operation: format!("creating {kind} '{name}'"),
            disclosure: Disclosure::NONE,
            change: AuthorizationChange::RoleBody(RoleBodyChange {
                kind: role_ref_kind(kind),
                name: name.to_owned(),
                organization,
                before: Vec::new(),
                after: statements(spec)?,
            }),
        }],
        // A create has no before-state, so every claim is reconstructed from the
        // spec the caller just sent.
        ROLE_BINDING_KIND | PLATFORM_ROLE_BINDING_KIND => vec![GatedChange {
            operation: format!("creating {kind} '{name}'"),
            disclosure: Disclosure::FROM_REQUEST,
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
                // The recipient is the User the request named, but the domains
                // are every stored binding that names the Group — their scopes
                // and selectors are policy the caller may hold no read on.
                disclosure: Disclosure::RECIPIENT_ONLY,
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
                    // A User's claims are expanded over its live Group ties, so
                    // even the recipients are read out of the store.
                    disclosure: Disclosure::NONE,
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
                // A Controller or ServiceAccount holds no Group ties, so the
                // recipient is the identity the request addressed — but the
                // domains still come from stored bindings.
                disclosure: Disclosure::RECIPIENT_ONLY,
                change: AuthorizationChange::IdentityMapping(IdentityMappingChange {
                    identity,
                    confers_operator: false,
                }),
            }]
        }
        // A `User` create is an activation, not a blank identity. ADR-0001 §1
        // binds a `GroupMembership` and a `user:` subject to the *name*, and
        // says so deliberately: deleting a User leaves the markers in place and
        // recreating the name reactivates them. So a create under a name that
        // stale policy still refers to makes that policy reachable again — the
        // same event the update path gates when `active` goes false → true, and
        // reachable without holding `update` at all. `active` also defaults to
        // true, so an empty spec is the interesting case.
        USER_KIND => {
            let parsed: UserSpec = parse_spec(spec, kind)?;
            if parsed.active {
                let identity: SubjectId = subject("user", None, name)?;
                vec![GatedChange {
                    operation: format!("creating {identity}"),
                    disclosure: Disclosure::NONE,
                    change: AuthorizationChange::IdentityMapping(IdentityMappingChange {
                        identity,
                        confers_operator: false,
                    }),
                }]
            } else {
                Vec::new()
            }
        }
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
            disclosure: Disclosure::NONE,
            change: AuthorizationChange::RoleBody(RoleBodyChange {
                kind: role_ref_kind(kind),
                name: row.name.clone(),
                organization,
                before: statements(&row.spec)?,
                after: statements(spec)?,
            }),
        }],
        // The before-state is a stored row the caller may hold no `get` on, and
        // a rejection cannot say which universe it came from.
        ROLE_BINDING_KIND | PLATFORM_ROLE_BINDING_KIND => vec![GatedChange {
            operation: format!("editing {kind} '{}'", row.name),
            disclosure: Disclosure::NONE,
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
                    disclosure: Disclosure::NONE,
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
                    disclosure: Disclosure::NONE,
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
    // Deleting an Organization tombstones every Role and RoleBinding beneath
    // it, and a tombstoned binding stops applying at once — the engine filters
    // on liveness, not on collection. So an Organization delete is a delete of
    // its whole policy tier, Denies included, and has to be diffed as one.
    //
    // Only this parent needs it. The other structural cascades over policy
    // rows — a Group taking its GroupMemberships, a User its UserIdentities, a
    // ServiceAccount its trust policies — only ever remove authority, which
    // ADR-0001 §4 does not gate.
    if row.api_version == API_VERSION_V1ALPHA1 && row.kind == ORGANIZATION_KIND {
        return cascaded_policy_deletions(ctx, row).await;
    }
    policy_row_deletion(ctx, row).await
}

/// The gated change deleting one policy row produces, or nothing for a row that
/// carries no policy. Split out from [`change_for_delete`] so the Organization
/// cascade can reach it without recursing back through the cascade arm.
async fn policy_row_deletion(
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
            disclosure: Disclosure::NONE,
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
            disclosure: Disclosure::NONE,
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

/// The gated changes deleting an Organization produces: one per policy row the
/// cascade would take with it.
///
/// The rows are diffed individually rather than as one aggregate change so the
/// gate compares each against the writer's authority the same way a direct
/// delete of that row would. A caller who may delete every one of them
/// individually may delete the Organization; a caller who may not is refused
/// here rather than through the back door.
async fn cascaded_policy_deletions(
    ctx: &AuthorizationContext,
    organization: &ResourceRow,
) -> Result<AuthorizationChangeSet, ServerError> {
    let mut changes = Vec::new();
    for kind in [ROLE_KIND, ROLE_BINDING_KIND] {
        let rows = ctx
            .store()
            .list(API_VERSION_V1ALPHA1, kind, Some(organization.uid))
            .await
            .map_err(crate::server::resources::error_map::store_error_to_server_error)?;
        for row in rows {
            changes.extend(policy_row_deletion(ctx, &row).await?);
        }
    }
    Ok(changes)
}

/// The gated change a write that *schedules* a policy row's deletion produces.
///
/// Attaching an owner reference is not a spec edit, so nothing in
/// `change_for_update` sees it — but the edge is a scheduled delete: when the
/// owner goes, the store tombstones the dependent, and a tombstoned Deny stops
/// applying. Without this, a caller refused a direct delete of a Deny that caps
/// them could attach it to a resource they own, delete that, and have the cap
/// removed with no gate anywhere in the sequence.
///
/// Only *introduced* references produce a change; the caller has already been
/// held to `delete` on the dependent and `use` on the owner by
/// `authorize_owner_references`, which is ordinary authority rather than the
/// delegation question this answers.
pub async fn change_for_scheduled_deletion(
    ctx: &AuthorizationContext,
    row: &ResourceRow,
    before: &[rise_resource_api::OwnerReference],
    after: &[rise_resource_api::OwnerReference],
) -> Result<AuthorizationChangeSet, ServerError> {
    if after.iter().all(|reference| before.contains(reference)) {
        return Ok(Vec::new());
    }
    Ok(change_for_delete(ctx, row)
        .await?
        .into_iter()
        .map(|gated| GatedChange {
            operation: format!(
                "making {} '{}' a dependent of another resource, which schedules its deletion",
                row.kind, row.name
            ),
            ..gated
        })
        .collect())
}

/// The gated changes an access-driving label write produces (ADR-0001 §6.6).
///
/// Every key whose *own* value changes is handed to the gate, which decides
/// whether any binding selects on it and, if so, diffs the authority over the
/// resource and its K-inheriting subtree. Keys are compared on the resource's
/// own labels: an unchanged own value cannot move the effective value on
/// anything below it.
/// What a refused label write may say about itself.
///
/// Nothing here comes from the request. The recipient is whatever subject the
/// *stored* binding resolves to under the new value — always a literal, and for
/// a literal-subject binding read straight out of policy, so it can name another
/// organization's Group or ServiceAccount. The domains are the written resource
/// *and every descendant inheriting the key*, which would enumerate a subtree
/// the caller may see none of. So: neither.
pub fn label_disclosure() -> Disclosure {
    Disclosure::NONE
}

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
            // A set names the key and value the caller just sent. A removal
            // does not: the key comes from the stored label map, so naming it
            // would let a caller holding `update` without `get` read back a
            // resource's access-driving keys one refused write at a time. The
            // full change is in the `resource.grant_gate` audit record.
            operation: match after_value {
                Some(value) => format!("setting label '{key}' to '{value}'"),
                None => "removing an access-driving label".to_owned(),
            },
            disclosure: label_disclosure(),
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
