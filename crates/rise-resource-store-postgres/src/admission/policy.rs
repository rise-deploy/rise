use rise_resource_api::{
    is_immutable_policy_seed, system_admin_binding_spec, system_admin_role_spec,
    PlatformRoleBindingSpec, PlatformRoleRefKind, RoleBindingSpec, RoleRefKind, RoleSpec,
    StoreError, API_VERSION_V1ALPHA1, OPERATORS_SUBJECT, ORGANIZATION_KIND,
    PLATFORM_ROLE_BINDING_KIND, PLATFORM_ROLE_KIND, ROLE_BINDING_KIND, ROLE_KIND,
    SYSTEM_ADMIN_PLATFORM_ROLE,
};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::admission::reference::{
    require_scope_within_organization, resolve_binding_subject, resolve_policy_ref, resolve_scope,
};
use crate::admission::{canonicalize, parse_existing};
use crate::builtin::BuiltInRegistry;
use crate::models::PgResourceRow;

/// Policy-specific admission attached to an immutable built-in registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyAdmission {
    Role,
    RoleBinding,
    PlatformRole,
    PlatformRoleBinding,
}

impl PolicyAdmission {
    pub(crate) fn for_policy(api_version: &str, kind: &str) -> Option<Self> {
        if api_version != API_VERSION_V1ALPHA1 {
            return None;
        }
        Some(match kind {
            ROLE_KIND => Self::Role,
            ROLE_BINDING_KIND => Self::RoleBinding,
            PLATFORM_ROLE_KIND => Self::PlatformRole,
            PLATFORM_ROLE_BINDING_KIND => Self::PlatformRoleBinding,
            _ => return None,
        })
    }

    /// Context-free canonicalization.
    ///
    /// A Role body is fully canonical after this: its statements are pure
    /// policy data. A binding is only *syntactically* canonical — its `scope`
    /// may still be absent, because the default depends on the parent
    /// Organization or on the subject, neither of which is knowable here.
    /// [`Self::admit_context`] completes it inside the transaction.
    ///
    /// `subjectMembership` is the exception that does normalize here: it is
    /// context-free, so an omitted value becomes the persisted PascalCase
    /// `Any` at this point, while an explicit `null` or unknown value fails.
    pub(crate) fn canonicalize(
        self,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        match self {
            Self::Role => canonicalize::<RoleSpec>(value, ROLE_KIND),
            Self::PlatformRole => canonicalize::<RoleSpec>(value, PLATFORM_ROLE_KIND),
            Self::RoleBinding => canonicalize::<RoleBindingSpec>(value, ROLE_BINDING_KIND),
            Self::PlatformRoleBinding => {
                canonicalize::<PlatformRoleBindingSpec>(value, PLATFORM_ROLE_BINDING_KIND)
            }
        }
    }

    /// Contextual normalization and reference validation, returning the spec to
    /// persist.
    ///
    /// Shared by create and update so a binding that was normalized once keeps
    /// being re-validated against current rows: an update carries the stored
    /// normalized spec back through the same path, which is idempotent because
    /// an explicit `scope` normalizes to itself.
    pub(crate) async fn admit_context(
        self,
        conn: &mut PgConnection,
        builtins: &BuiltInRegistry,
        parent_uid: Option<Uuid>,
        name: &str,
        spec: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        match self {
            // A Role body references nothing and grants nothing until a binding
            // names it, so canonicalization already left it complete.
            Self::Role => Ok(spec.clone()),
            Self::PlatformRole => {
                require_shipped_seed(PLATFORM_ROLE_KIND, name, parent_uid, spec, || {
                    serde_json::to_value(system_admin_role_spec()).map_err(StoreError::backend)
                })?;
                Ok(spec.clone())
            }
            Self::RoleBinding => {
                self.admit_role_binding(conn, builtins, parent_uid, name, spec)
                    .await
            }
            Self::PlatformRoleBinding => {
                self.admit_platform_role_binding(conn, builtins, parent_uid, name, spec)
                    .await
            }
        }
    }

    async fn admit_role_binding(
        self,
        conn: &mut PgConnection,
        builtins: &BuiltInRegistry,
        parent_uid: Option<Uuid>,
        name: &str,
        spec: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let organization = self.parent_organization(conn, parent_uid).await?;
        let parsed: RoleBindingSpec = parse_existing(spec, ROLE_BINDING_KIND)?;
        let normalized = parsed
            .normalize(&organization.name)
            .map_err(|error| StoreError::Validation(error.to_string()))?;

        require_authorable_subject(normalized.subject(), parent_uid, name)?;
        resolve_binding_subject(conn, normalized.subject()).await?;

        // Reference direction (ADR-0001 §4): an org binding reaches its own
        // org's Roles or any PlatformRole, never another org's Role.
        let (kind, parent) = match normalized.role_ref().kind {
            RoleRefKind::Role => (ROLE_KIND, Some(organization.uid)),
            RoleRefKind::PlatformRole => (PLATFORM_ROLE_KIND, None),
        };
        resolve_policy_ref(
            conn,
            API_VERSION_V1ALPHA1,
            kind,
            &normalized.role_ref().name,
            parent,
        )
        .await?;

        // `normalize` already rejected a wildcard scope for an org binding.
        let scope = normalized.scope();
        let resolved = resolve_scope(conn, builtins, scope).await?;
        require_scope_within_organization(
            scope,
            &resolved,
            organization.uid,
            &organization.name,
            "an org RoleBinding's scope must lie within its own Organization's subtree",
        )?;

        serde_json::to_value(normalized).map_err(StoreError::backend)
    }

    async fn admit_platform_role_binding(
        self,
        conn: &mut PgConnection,
        builtins: &BuiltInRegistry,
        parent_uid: Option<Uuid>,
        name: &str,
        spec: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let parsed: PlatformRoleBindingSpec = parse_existing(spec, PLATFORM_ROLE_BINDING_KIND)?;
        let normalized = parsed
            .normalize()
            .map_err(|error| StoreError::Validation(error.to_string()))?;

        require_authorable_subject(normalized.subject(), parent_uid, name)?;
        // Compare the *normalized* forms: the spec that reaches the row has its
        // contextual defaults applied, so comparing the caller's pre-normalized
        // input against a normalized default would never match.
        let normalized_value = serde_json::to_value(&normalized).map_err(StoreError::backend)?;
        require_shipped_seed(
            PLATFORM_ROLE_BINDING_KIND,
            name,
            parent_uid,
            &normalized_value,
            || {
                let shipped: rise_resource_api::LocallyNormalizedPlatformRoleBindingSpec =
                    system_admin_binding_spec()
                        .normalize()
                        .map_err(|error| StoreError::Validation(error.to_string()))?;
                serde_json::to_value(shipped).map_err(StoreError::backend)
            },
        )?;

        let subject_organization = resolve_binding_subject(conn, normalized.subject()).await?;

        let PlatformRoleRefKind::PlatformRole = normalized.role_ref().kind;
        resolve_policy_ref(
            conn,
            API_VERSION_V1ALPHA1,
            PLATFORM_ROLE_KIND,
            &normalized.role_ref().name,
            None,
        )
        .await?;

        // A platform binding's scope is otherwise unrestricted: `*`, any org's
        // subtree, or a root-scoped instance are all legitimate platform policy.
        let scope = normalized.scope();
        if !scope.is_wildcard() {
            let resolved = resolve_scope(conn, builtins, scope).await?;
            // A Group or ServiceAccount subject carries its organization in its
            // own identifier, so a binding for one cannot reach outside it —
            // `normalize` already rejected the explicit-wildcard spelling of
            // the same escape.
            if let Some(organization) = subject_organization {
                require_scope_within_organization(
                    scope,
                    &resolved,
                    organization.uid,
                    &organization.name,
                    "a static org-native subject's binding cannot reach outside that subject's own Organization",
                )?;
            }
        }

        serde_json::to_value(normalized).map_err(StoreError::backend)
    }

    /// Read the parent Organization the binding hangs under.
    ///
    /// `validate_builtin_placement` has already proven this parent is a live
    /// Organization and left it `FOR SHARE`-locked; this read is for its
    /// *name*, which is what scope defaulting and the containment message need.
    async fn parent_organization(
        self,
        conn: &mut PgConnection,
        parent_uid: Option<Uuid>,
    ) -> Result<PgResourceRow, StoreError> {
        let parent_uid = parent_uid.ok_or_else(|| {
            StoreError::Validation(format!(
                "{ROLE_BINDING_KIND} requires an Organization parent"
            ))
        })?;
        sqlx::query_as::<_, PgResourceRow>(
            r#"
            SELECT *
            FROM resource_store.resources
            WHERE uid = $1
              AND api_version = $2
              AND kind = $3
              AND deletion_timestamp IS NULL
            FOR SHARE
            "#,
        )
        .bind(parent_uid)
        .bind(API_VERSION_V1ALPHA1)
        .bind(ORGANIZATION_KIND)
        .fetch_optional(&mut *conn)
        .await
        .map_err(StoreError::backend)?
        .ok_or_else(|| {
            StoreError::Validation(format!(
                "{ROLE_BINDING_KIND} parent must identify a live {API_VERSION_V1ALPHA1} {ORGANIZATION_KIND}"
            ))
        })
    }
}

/// Hold an immutable seed to its shipped body on the way in.
///
/// ADR-0001 §5 ships `PlatformRole/system-admin` and its binding as fixed data.
/// Admission is the only writer that can enforce that, and it enforces it as
/// equality against the shipped default rather than as "reserved name, any
/// content": a reserved row whose body drifted would be the most misleading
/// possible artifact, since the guarantee it appears to express lives in the
/// evaluator instead.
///
/// Non-seed names pass straight through.
fn require_shipped_seed<F>(
    kind: &str,
    name: &str,
    parent_uid: Option<Uuid>,
    spec: &serde_json::Value,
    shipped: F,
) -> Result<(), StoreError>
where
    F: FnOnce() -> Result<serde_json::Value, StoreError>,
{
    // Placement is part of the identity: only the root rows are reserved.
    if parent_uid.is_some() || !is_immutable_policy_seed(kind, name) {
        return Ok(());
    }
    let shipped = shipped()?;
    if spec == &shipped {
        return Ok(());
    }
    Err(StoreError::Validation(format!(
        "{kind} '{name}' is seeded and immutable; its spec must be the shipped default"
    )))
}

/// Reject the one subject nobody may author.
///
/// The recovery tier is hardcoded in the evaluator, so a binding naming
/// `system:operators` would look like the source of that authority without being
/// it. Exactly one row may carry it — the seeded root `PlatformRoleBinding` named
/// `system-admin` — and this applies to *both* binding kinds: an org
/// `RoleBinding` naming it is inert under §1's recipient boundary today, and
/// admitting misleading policy on the strength of it being currently inert is how
/// it stops being inert later.
///
/// This lives here rather than in context-free `normalize()` because it needs the
/// resource's name and placement, and because admission is authoritative for
/// direct store calls that supply no validator.
fn require_authorable_subject(
    subject: &rise_resource_api::BindingSubject,
    parent_uid: Option<Uuid>,
    name: &str,
) -> Result<(), StoreError> {
    let names_operators = subject
        .literal()
        .is_some_and(|subject| subject.as_ref() == OPERATORS_SUBJECT);
    let is_reserved_seed = parent_uid.is_none() && name == SYSTEM_ADMIN_PLATFORM_ROLE;
    if names_operators && !is_reserved_seed {
        return Err(StoreError::Validation(format!(
            "{OPERATORS_SUBJECT} is reserved for the seeded root \
             {PLATFORM_ROLE_BINDING_KIND} '{SYSTEM_ADMIN_PLATFORM_ROLE}'"
        )));
    }
    Ok(())
}
