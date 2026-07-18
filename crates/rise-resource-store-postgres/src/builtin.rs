//! Built-in resource registry.
//!
//! `Organization` and `ResourceDefinition` are first-class resource kinds owned
//! by Rise itself: they have typed Rust validators, their identity (group,
//! version, kind, plural) is fixed at compile time, and they have no row in
//! the `resource_definitions` projection table. Routing for these kinds is
//! resolved through this registry instead of through the database.
//!
//! The registry is the single source of truth for "what built-ins does this
//! store recognise?" The pre-registry implementation matched on hardcoded
//! collection strings in `PgResourceStore`; adding a new built-in (e.g.
//! `Project`, `Environment`, `Deployment`, `ServiceAccount` in the upcoming
//! migration PRs) meant copying two `match` arms in two methods. The registry
//! collapses that to a single [`BuiltInRegistration`].
//!
//! See [`ROADMAP.md`](../../../ROADMAP.md) § "PR A1".

use std::collections::HashMap;
use std::sync::Arc;

use rise_resource_api::{
    CollectionInfo, ResourceParentRef, SpecValidator, API_VERSION_V1ALPHA1,
    ORGANIZATION_COLLECTION, ORGANIZATION_KIND, RESOURCE_DEFINITION_COLLECTION,
    RESOURCE_DEFINITION_KIND,
};

use crate::validation::{OrganizationValidator, ResourceDefinitionValidator};

/// Static description of one built-in resource kind.
///
/// Equivalent to a `ResourceDefinition` row, but the identity fields live in
/// Rust (compile-time constants) and the spec/status validators are typed
/// implementations rather than a JSON Schema. Built-in registrations are
/// served at exactly one `api_version` — version evolution for built-ins
/// happens through code, not through `versions[]` entries on a database row.
#[derive(Clone)]
pub struct BuiltInRegistration {
    /// REST collection name (plural). Routing key for the HTTP API.
    pub collection: &'static str,
    /// `<group>/<version>` string. Stored verbatim on every row of this kind.
    pub api_version: &'static str,
    /// Resource kind, used in URLs, request bodies, and on every row.
    pub kind: &'static str,
    /// Parent resource reference. `None` for root-scoped kinds.
    pub parent: Option<ResourceParentRef>,
    /// Typed Rust validator for `spec` and `status`.
    pub spec_validator: Arc<dyn SpecValidator>,
}

impl BuiltInRegistration {
    /// Project the registration into the `CollectionInfo` shape returned by
    /// `ResourceStore::resolve_collection*`.
    ///
    /// Built-ins serve exactly one api_version, so `storage`, `served`, and
    /// `declared` collapse to the same single-element list. `allowed_status_controller_ids`
    /// is empty by default; controller ownership for built-ins lands in a
    /// later PR (see the `ROADMAP.md` Phase B work).
    pub fn collection_info(&self) -> CollectionInfo {
        CollectionInfo {
            api_version: self.api_version.to_string(),
            storage_api_version: self.api_version.to_string(),
            served_api_versions: vec![self.api_version.to_string()],
            declared_api_versions: vec![self.api_version.to_string()],
            kind: self.kind.to_string(),
            parent: self.parent.clone(),
            spec_validator: self.spec_validator.clone(),
            allowed_status_controller_ids: Vec::new(),
        }
    }

    /// The API group portion of `api_version` (`"rise.dev/v1alpha1"` → `"rise.dev"`).
    ///
    /// Used by [`BuiltInRegistry::by_group_kind`] to index registrations for
    /// `resolve_collection_by_kind` lookups, which carry a `(group, kind)`
    /// tuple from the parent-chain walk rather than a full `api_version`.
    ///
    /// `register()` validates `<group>/<version>` shape on insertion, so on a
    /// successfully registered entry the split here always succeeds; the
    /// fallback is only reachable from the constructor of an unregistered
    /// `BuiltInRegistration`.
    pub fn group(&self) -> &'static str {
        match self.api_version.split_once('/') {
            Some((g, _)) => g,
            None => self.api_version,
        }
    }
}

/// Strongly-typed registry of every built-in resource kind a store recognises.
///
/// Build one via [`Self::defaults`] (or programmatically, when a future PR
/// needs to inject a built-in only in tests or behind a feature flag) and pass
/// it to `PgResourceStore::with_builtin_registry`. The registry is immutable
/// once built — there is no runtime mutation API, by design: routing tables
/// changing under live traffic would create race windows for both routing and
/// validator selection.
pub struct BuiltInRegistry {
    by_collection: HashMap<&'static str, BuiltInRegistration>,
    /// `(group, kind)` → collection, populated when entries are inserted. Used
    /// by `resolve_collection_by_kind` so the parent-chain walk does not need
    /// to scan `by_collection`.
    by_group_kind: HashMap<(&'static str, &'static str), &'static str>,
}

impl BuiltInRegistry {
    /// Construct an empty registry. Prefer [`Self::defaults`] unless you have
    /// a specific reason to omit the standard built-ins (e.g. an integration
    /// test that wants to assert routing fails closed).
    pub fn empty() -> Self {
        Self {
            by_collection: HashMap::new(),
            by_group_kind: HashMap::new(),
        }
    }

    /// Construct the registry with every Rise built-in. The canonical set.
    ///
    /// New built-in kinds (Project, Environment, Deployment, ServiceAccount —
    /// see roadmap PR B2/B3/B4) get added here in one place.
    pub fn defaults() -> Self {
        let mut r = Self::empty();
        r.register(BuiltInRegistration {
            collection: ORGANIZATION_COLLECTION,
            api_version: API_VERSION_V1ALPHA1,
            kind: ORGANIZATION_KIND,
            parent: None,
            spec_validator: Arc::new(OrganizationValidator),
        });
        r.register(BuiltInRegistration {
            collection: RESOURCE_DEFINITION_COLLECTION,
            api_version: API_VERSION_V1ALPHA1,
            kind: RESOURCE_DEFINITION_KIND,
            parent: None,
            spec_validator: Arc::new(ResourceDefinitionValidator),
        });
        r
    }

    /// Insert a registration. Panics on:
    /// - a malformed `api_version` (must be `<group>/<version>`),
    /// - a duplicate `collection`, or
    /// - a duplicate `(group, kind)` tuple.
    ///
    /// Built-ins are statically declared, so each of these is a programmer
    /// error caught at startup rather than a runtime condition. Surfacing
    /// them as panics avoids silently shadowing one built-in with another
    /// during routing, and keeps the [`Self::group`] fallback unreachable for
    /// any registered entry.
    pub fn register(&mut self, reg: BuiltInRegistration) {
        if reg.api_version.split_once('/').is_none() {
            panic!(
                "built-in '{}' has malformed api_version '{}'; expected '<group>/<version>'",
                reg.collection, reg.api_version
            );
        }
        let key = reg.collection;
        if self.by_collection.contains_key(key) {
            panic!("duplicate built-in collection '{key}'");
        }
        let group_kind = (reg.group(), reg.kind);
        if self.by_group_kind.contains_key(&group_kind) {
            panic!(
                "duplicate built-in (group, kind) ('{}', '{}')",
                group_kind.0, group_kind.1
            );
        }
        self.by_group_kind.insert(group_kind, key);
        self.by_collection.insert(key, reg);
    }

    /// Look up a built-in by its plural collection name. Returns `None` for
    /// unknown collections; the caller falls through to the
    /// `resource_definitions` projection table.
    pub fn lookup_collection(&self, collection: &str) -> Option<&BuiltInRegistration> {
        self.by_collection.get(collection)
    }

    /// Look up a built-in by its `(group, kind)` tuple. Used by the
    /// parent-chain walk in `resolve_collection_by_kind`: the parent ref on a
    /// `ResourceDefinition` carries `{api_version, kind}` rather than a
    /// plural, so resolving an ancestor that happens to be a built-in goes
    /// through this index.
    pub fn lookup_by_group_kind(&self, group: &str, kind: &str) -> Option<&BuiltInRegistration> {
        self.by_group_kind
            .get(&(group, kind))
            .and_then(|c| self.by_collection.get(c))
    }

    /// Iterate every registered built-in. Order is unspecified; callers that
    /// need a stable order should sort by `collection` themselves.
    pub fn iter(&self) -> impl Iterator<Item = &BuiltInRegistration> {
        self.by_collection.values()
    }

    /// Number of registered built-ins. Useful for assertions in tests.
    pub fn len(&self) -> usize {
        self.by_collection.len()
    }

    /// `true` if no built-ins are registered. Mirrors [`Vec::is_empty`] for
    /// clippy consistency.
    pub fn is_empty(&self) -> bool {
        self.by_collection.is_empty()
    }
}

// Intentionally **no** `impl Default for BuiltInRegistry`. Rust convention is
// that `Default::default()` produces an empty value for collection-like types
// (`HashMap::default()`, `Vec::default()`); mapping it to the populated
// canonical set would invite a subtle test bug. Callers must pick between the
// two explicit constructors [`Self::empty`] and [`Self::defaults`].

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_register_organization_and_resource_definition() {
        let r = BuiltInRegistry::defaults();
        assert_eq!(r.len(), 2);
        assert!(r.lookup_collection(ORGANIZATION_COLLECTION).is_some());
        assert!(r
            .lookup_collection(RESOURCE_DEFINITION_COLLECTION)
            .is_some());
        assert!(r.lookup_collection("unknown").is_none());
    }

    #[test]
    fn lookup_by_group_kind_resolves_known_pairs() {
        let r = BuiltInRegistry::defaults();
        let org = r
            .lookup_by_group_kind("rise.dev", ORGANIZATION_KIND)
            .expect("Organization is built-in");
        assert_eq!(org.collection, ORGANIZATION_COLLECTION);

        let rd = r
            .lookup_by_group_kind("rise.dev", RESOURCE_DEFINITION_KIND)
            .expect("ResourceDefinition is built-in");
        assert_eq!(rd.collection, RESOURCE_DEFINITION_COLLECTION);

        assert!(r.lookup_by_group_kind("rise.dev", "DoesNotExist").is_none());
        assert!(r
            .lookup_by_group_kind("other.example", ORGANIZATION_KIND)
            .is_none());
    }

    #[test]
    fn collection_info_carries_built_in_identity() {
        let r = BuiltInRegistry::defaults();
        let info = r
            .lookup_collection(ORGANIZATION_COLLECTION)
            .expect("Organization is built-in")
            .collection_info();
        assert_eq!(info.api_version, API_VERSION_V1ALPHA1);
        assert_eq!(info.kind, ORGANIZATION_KIND);
        assert!(info.parent.is_none());
        // Built-ins serve exactly one api_version; storage/served/declared all
        // collapse to it.
        assert_eq!(info.served_api_versions, vec![API_VERSION_V1ALPHA1]);
        assert_eq!(info.declared_api_versions, vec![API_VERSION_V1ALPHA1]);
        assert_eq!(info.storage_api_version, API_VERSION_V1ALPHA1);
        // Controller ownership for built-ins is deferred (see roadmap PR B2+).
        assert!(info.allowed_status_controller_ids.is_empty());
    }

    #[test]
    #[should_panic(expected = "malformed api_version")]
    fn malformed_api_version_panics() {
        let mut r = BuiltInRegistry::empty();
        r.register(BuiltInRegistration {
            collection: "examples",
            // Missing the `/<version>` half; `register` must catch this
            // before it lands in the by_group_kind index with a degenerate
            // group string.
            api_version: "example.dev",
            kind: "Example",
            parent: None,
            spec_validator: Arc::new(OrganizationValidator),
        });
    }

    #[test]
    #[should_panic(expected = "duplicate built-in collection")]
    fn duplicate_collection_panics() {
        let mut r = BuiltInRegistry::defaults();
        r.register(BuiltInRegistration {
            collection: ORGANIZATION_COLLECTION,
            api_version: API_VERSION_V1ALPHA1,
            kind: "OtherKind",
            parent: None,
            spec_validator: Arc::new(OrganizationValidator),
        });
    }

    #[test]
    #[should_panic(expected = "duplicate built-in (group, kind)")]
    fn duplicate_group_kind_panics() {
        let mut r = BuiltInRegistry::defaults();
        r.register(BuiltInRegistration {
            collection: "otherplural",
            api_version: API_VERSION_V1ALPHA1,
            kind: ORGANIZATION_KIND,
            parent: None,
            spec_validator: Arc::new(OrganizationValidator),
        });
    }

    #[test]
    fn empty_registry_resolves_nothing() {
        let r = BuiltInRegistry::empty();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.lookup_collection(ORGANIZATION_COLLECTION).is_none());
        assert!(r
            .lookup_by_group_kind("rise.dev", ORGANIZATION_KIND)
            .is_none());
    }

    #[test]
    fn group_derived_from_api_version() {
        let reg = BuiltInRegistration {
            collection: "examples",
            api_version: "example.dev/v1",
            kind: "Example",
            parent: None,
            spec_validator: Arc::new(OrganizationValidator),
        };
        assert_eq!(reg.group(), "example.dev");
    }

    #[test]
    fn iter_visits_every_registration() {
        let r = BuiltInRegistry::defaults();
        let mut collections: Vec<&str> = r.iter().map(|reg| reg.collection).collect();
        collections.sort_unstable();
        assert_eq!(
            collections,
            vec![ORGANIZATION_COLLECTION, RESOURCE_DEFINITION_COLLECTION]
        );
    }
}
