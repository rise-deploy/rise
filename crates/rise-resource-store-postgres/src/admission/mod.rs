//! Transaction-scoped admission for Rise's built-in resource kinds.
//!
//! Built-ins are activated through this seam rather than through a JSON Schema
//! on a `ResourceDefinition` row, because their contracts need facts a schema
//! cannot see: the identity kinds bind a resource name to a live `User`, and
//! the policy kinds normalize a binding against its parent Organization and
//! resolve `roleRef` and `scope` against rows in the same transaction.
//!
//! Two phases, deliberately separated:
//!
//! - [`BuiltInAdmission::canonicalize`] is pure and runs before the write
//!   transaction opens. It parses the caller's spec into the typed contract
//!   and re-serializes it, so unknown fields, malformed values, and
//!   non-canonical spellings fail before any lock is taken.
//! - The `admit_*` methods run inside the write transaction, own every
//!   database read and lock they need, and may rewrite `params.spec` with a
//!   contextually normalized value. They are authoritative: a caller that
//!   omits or forges the optional [`SpecValidator`] cannot bypass them.

mod identity;
mod policy;
mod reference;

use rise_resource_api::{
    CreateResourceParams, SpecValidator, StoreError, UpdateResourceParams, ValidationError,
};
use serde::{de::DeserializeOwned, Serialize};
use sqlx::PgConnection;

use crate::builtin::BuiltInRegistry;
use crate::models::PgResourceRow;

pub(crate) use identity::IdentityAdmission;
pub(crate) use policy::PolicyAdmission;

/// Kind-specific admission attached to an immutable built-in registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltInAdmission {
    Identity(IdentityAdmission),
    Policy(PolicyAdmission),
}

impl BuiltInAdmission {
    /// Resolve the admission for an exact built-in storage identity, or `None`
    /// for a built-in that needs no typed admission beyond its own validator.
    pub(crate) fn for_kind(api_version: &str, kind: &str) -> Option<Self> {
        IdentityAdmission::for_identity(api_version, kind)
            .map(Self::Identity)
            .or_else(|| PolicyAdmission::for_policy(api_version, kind).map(Self::Policy))
    }

    /// Context-free canonicalization. Runs before the write transaction opens.
    pub(crate) fn canonicalize(
        self,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        match self {
            Self::Identity(admission) => admission.canonicalize(value),
            Self::Policy(admission) => admission.canonicalize(value),
        }
    }

    /// Contextual create admission. Runs inside the write transaction, after
    /// structural placement is verified and before the row is inserted, and
    /// may replace `params.spec` with its normalized form.
    pub(crate) async fn admit_create_context(
        self,
        conn: &mut PgConnection,
        builtins: &BuiltInRegistry,
        params: &mut CreateResourceParams,
    ) -> Result<(), StoreError> {
        match self {
            Self::Identity(admission) => admission.admit_create_context(conn, params).await,
            Self::Policy(admission) => {
                params.spec = admission
                    .admit_context(conn, builtins, params.parent_uid, &params.spec)
                    .await?;
                Ok(())
            }
        }
    }

    /// Contextual update admission. Runs on the pre-lock snapshot, before the
    /// dependent row lock, so it shares the lock order creates use.
    pub(crate) async fn admit_update_before_dependent(
        self,
        conn: &mut PgConnection,
        builtins: &BuiltInRegistry,
        current: &PgResourceRow,
        params: &mut UpdateResourceParams,
    ) -> Result<(), StoreError> {
        match self {
            Self::Identity(admission) => {
                admission
                    .admit_update_before_dependent(conn, current, params)
                    .await
            }
            Self::Policy(admission) => {
                params.spec = admission
                    .admit_context(conn, builtins, current.parent_uid, &params.spec)
                    .await?;
                Ok(())
            }
        }
    }

    /// Re-checked against the locked row, so a writer that committed between
    /// the snapshot and the lock cannot slip an immutable-field change through.
    pub(crate) fn validate_update_immutable(
        self,
        current: &PgResourceRow,
        params: &UpdateResourceParams,
    ) -> Result<(), StoreError> {
        match self {
            Self::Identity(admission) => admission.validate_update_immutable(current, params),
            // Policy specs carry no immutable fields: ADR-0001 governs binding
            // and Role edits through the write-time grant gate, which evaluates
            // the effective before/after delta rather than freezing fields.
            Self::Policy(_) => Ok(()),
        }
    }
}

/// Pure/local validator projected into `CollectionInfo`. Contextual admission
/// above remains authoritative for every direct store call.
pub(crate) struct BuiltInSpecValidator(pub(crate) BuiltInAdmission);

impl SpecValidator for BuiltInSpecValidator {
    fn validate_spec(&self, spec: &serde_json::Value) -> Result<(), ValidationError> {
        self.0
            .canonicalize(spec)
            .map(|_| ())
            .map_err(|error| ValidationError::new(error.to_string()))
    }

    fn validate_status(&self, _status: &serde_json::Value) -> Result<(), ValidationError> {
        Ok(())
    }
}

pub(crate) fn canonicalize<T>(
    value: &serde_json::Value,
    kind: &str,
) -> Result<serde_json::Value, StoreError>
where
    T: DeserializeOwned + Serialize,
{
    let typed: T = serde_json::from_value(value.clone())
        .map_err(|error| StoreError::Validation(format!("invalid {kind} spec: {error}")))?;
    serde_json::to_value(typed).map_err(StoreError::backend)
}

pub(crate) fn parse_existing<T: DeserializeOwned>(
    value: &serde_json::Value,
    kind: &str,
) -> Result<T, StoreError> {
    serde_json::from_value(value.clone())
        .map_err(|error| StoreError::Validation(format!("stored {kind} spec is invalid: {error}")))
}
