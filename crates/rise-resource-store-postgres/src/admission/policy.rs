use rise_resource_api::{
    PlatformRoleBindingSpec, PlatformRoleRefKind, RoleBindingSpec, RoleRefKind, RoleSpec,
    StoreError, API_VERSION_V1ALPHA1, ORGANIZATION_KIND, PLATFORM_ROLE_BINDING_KIND,
    PLATFORM_ROLE_KIND, ROLE_BINDING_KIND, ROLE_KIND,
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
        spec: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        match self {
            // A Role body references nothing and grants nothing until a binding
            // names it, so canonicalization already left it complete.
            Self::Role | Self::PlatformRole => Ok(spec.clone()),
            Self::RoleBinding => {
                self.admit_role_binding(conn, builtins, parent_uid, spec)
                    .await
            }
            Self::PlatformRoleBinding => {
                self.admit_platform_role_binding(conn, builtins, spec).await
            }
        }
    }

    async fn admit_role_binding(
        self,
        conn: &mut PgConnection,
        builtins: &BuiltInRegistry,
        parent_uid: Option<Uuid>,
        spec: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let organization = self.parent_organization(conn, parent_uid).await?;
        let parsed: RoleBindingSpec = parse_existing(spec, ROLE_BINDING_KIND)?;
        let normalized = parsed
            .normalize(&organization.name)
            .map_err(|error| StoreError::Validation(error.to_string()))?;

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
        spec: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let parsed: PlatformRoleBindingSpec = parse_existing(spec, PLATFORM_ROLE_BINDING_KIND)?;
        let normalized = parsed
            .normalize()
            .map_err(|error| StoreError::Validation(error.to_string()))?;

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
