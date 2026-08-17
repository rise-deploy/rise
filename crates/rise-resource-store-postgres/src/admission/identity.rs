use rise_resource_api::{
    ControllerSpec, ControllerTrustPolicySpec, CreateResourceParams, GroupMembershipSpec,
    GroupSpec, OwnerReference, ServiceAccountSpec, ServiceAccountTrustPolicySpec, StoreError,
    UpdateResourceParams, UserIdentitySpec, UserSpec, API_VERSION_V1ALPHA1, CONTROLLER_KIND,
    CONTROLLER_TRUST_POLICY_KIND, GROUP_KIND, GROUP_MEMBERSHIP_KIND, SERVICE_ACCOUNT_KIND,
    SERVICE_ACCOUNT_TRUST_POLICY_KIND, USER_IDENTITY_KIND, USER_KIND,
};
use serde::de::DeserializeOwned;
use sqlx::PgConnection;

use crate::admission::{canonicalize, parse_existing};
use crate::models::PgResourceRow;

/// Identity-specific admission attached to an immutable built-in registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityAdmission {
    User,
    UserIdentity,
    Controller,
    ControllerTrustPolicy,
    Group,
    GroupMembership,
    ServiceAccount,
    ServiceAccountTrustPolicy,
}

impl IdentityAdmission {
    pub(crate) fn for_identity(api_version: &str, kind: &str) -> Option<Self> {
        if api_version != API_VERSION_V1ALPHA1 {
            return None;
        }
        Some(match kind {
            USER_KIND => Self::User,
            USER_IDENTITY_KIND => Self::UserIdentity,
            CONTROLLER_KIND => Self::Controller,
            CONTROLLER_TRUST_POLICY_KIND => Self::ControllerTrustPolicy,
            GROUP_KIND => Self::Group,
            GROUP_MEMBERSHIP_KIND => Self::GroupMembership,
            SERVICE_ACCOUNT_KIND => Self::ServiceAccount,
            SERVICE_ACCOUNT_TRUST_POLICY_KIND => Self::ServiceAccountTrustPolicy,
            _ => return None,
        })
    }

    pub(crate) fn canonicalize(
        self,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        match self {
            Self::User => canonicalize::<UserSpec>(value, USER_KIND),
            Self::UserIdentity => canonicalize::<UserIdentitySpec>(value, USER_IDENTITY_KIND),
            Self::Controller => canonicalize::<ControllerSpec>(value, CONTROLLER_KIND),
            Self::ControllerTrustPolicy => {
                canonicalize::<ControllerTrustPolicySpec>(value, CONTROLLER_TRUST_POLICY_KIND)
            }
            Self::Group => canonicalize::<GroupSpec>(value, GROUP_KIND),
            Self::GroupMembership => {
                canonicalize::<GroupMembershipSpec>(value, GROUP_MEMBERSHIP_KIND)
            }
            Self::ServiceAccount => canonicalize::<ServiceAccountSpec>(value, SERVICE_ACCOUNT_KIND),
            Self::ServiceAccountTrustPolicy => canonicalize::<ServiceAccountTrustPolicySpec>(
                value,
                SERVICE_ACCOUNT_TRUST_POLICY_KIND,
            ),
        }
    }

    pub(crate) async fn admit_create_context(
        self,
        conn: &mut PgConnection,
        params: &CreateResourceParams,
    ) -> Result<(), StoreError> {
        if self == Self::GroupMembership {
            let user = lock_live_user_by_name(conn, &params.name).await?;
            validate_membership_owner(&params.owner_references, &user)?;
        }
        Ok(())
    }

    pub(crate) async fn admit_update_before_dependent(
        self,
        conn: &mut PgConnection,
        current: &PgResourceRow,
        params: &UpdateResourceParams,
    ) -> Result<(), StoreError> {
        if self == Self::GroupMembership && !params.owner_references.is_empty() {
            // An intentionally unowned membership remains editable after its
            // named User is gone. Adding or retaining lifecycle ownership,
            // however, must still identify the current live User.
            let user = lock_live_user_by_name(conn, &current.name).await?;
            validate_membership_owner(&params.owner_references, &user)?;
        }
        Ok(())
    }

    pub(crate) fn validate_update_immutable(
        self,
        current: &PgResourceRow,
        params: &UpdateResourceParams,
    ) -> Result<(), StoreError> {
        match self {
            Self::UserIdentity => {
                let before: UserIdentitySpec = parse_existing(&current.spec, USER_IDENTITY_KIND)?;
                let after: UserIdentitySpec = parse_existing(&params.spec, USER_IDENTITY_KIND)?;
                if before.issuer != after.issuer || before.subject != after.subject {
                    return Err(StoreError::Validation(
                        "UserIdentity issuer and subject are immutable; delete and recreate the mapping"
                            .into(),
                    ));
                }
            }
            Self::ControllerTrustPolicy => {
                immutable_trust::<ControllerTrustPolicySpec>(current, &params.spec)?;
            }
            Self::ServiceAccountTrustPolicy => {
                immutable_trust::<ServiceAccountTrustPolicySpec>(current, &params.spec)?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn immutable_trust<T>(
    current: &PgResourceRow,
    updated: &serde_json::Value,
) -> Result<(), StoreError>
where
    T: DeserializeOwned + PartialEq,
{
    let before: T = parse_existing(&current.spec, &current.kind)?;
    let after: T = parse_existing(updated, &current.kind)?;
    if before != after {
        return Err(StoreError::Validation(format!(
            "{} issuer and claim matcher are immutable; delete and recreate the trust policy",
            current.kind
        )));
    }
    Ok(())
}

async fn lock_live_user_by_name(
    conn: &mut PgConnection,
    name: &str,
) -> Result<PgResourceRow, StoreError> {
    sqlx::query_as::<_, PgResourceRow>(
        r#"
        SELECT *
        FROM resource_store.resources
        WHERE api_version = 'rise.dev/v1alpha1'
          AND kind = 'User'
          AND parent_uid IS NULL
          AND name = $1
          AND deletion_timestamp IS NULL
        FOR SHARE
        "#,
    )
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    .map_err(StoreError::backend)?
    .ok_or_else(|| {
        StoreError::Validation(format!(
            "GroupMembership name '{name}' must identify a live {API_VERSION_V1ALPHA1} User"
        ))
    })
}

fn validate_membership_owner(
    owner_references: &[OwnerReference],
    user: &PgResourceRow,
) -> Result<(), StoreError> {
    if owner_references.is_empty() {
        return Ok(());
    }
    if owner_references.len() != 1 {
        return Err(StoreError::Validation(
            "GroupMembership must have zero owner references or exactly one matching User owner"
                .into(),
        ));
    }
    let owner = &owner_references[0];
    if owner
        .api_version()
        .split_once('/')
        .is_none_or(|(group, _)| group != "rise.dev")
        || owner.kind() != USER_KIND
        || owner.name() != user.name
        || owner.uid() != user.uid
    {
        return Err(StoreError::Validation(
            "GroupMembership owner reference must identify its matching live User".into(),
        ));
    }
    Ok(())
}
