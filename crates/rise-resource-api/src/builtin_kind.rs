/// Static API identity and placement of one Rise built-in resource kind.
///
/// The PostgreSQL built-in registry consumes this contract metadata directly
/// when installing its transaction-scoped normalization and admission path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltInKindDefinition {
    pub api_version: &'static str,
    pub kind: &'static str,
    pub collection: &'static str,
    pub parent: Option<BuiltInKindParent>,
}

/// Fixed parent identity for a Rise built-in resource kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltInKindParent {
    pub api_version: &'static str,
    pub kind: &'static str,
}

/// Shared by the Organization-owned identity and policy kinds.
pub(crate) const ORGANIZATION_PARENT: BuiltInKindParent = BuiltInKindParent {
    api_version: crate::API_VERSION_V1ALPHA1,
    kind: crate::ORGANIZATION_KIND,
};
