//! An in-memory resource graph and membership resolver.
//!
//! The engine's payoff is that its whole algorithm runs against fakes: these
//! tests exercise ADR-0001 §4 end to end with no Postgres and no HTTP.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use rise_authz::engine::{
    AuthenticatedPrincipal, AuthorizationError, MembershipResolver, PrincipalMembership,
    ResourceNode,
};
use rise_resource_api::{
    CollectionInfo, CreateResourceParams, DeleteOutcome, DeletionBlockerReport, PathSegment,
    ResourceApi, ResourceRow, ResourceStore, StoreError, UpdateResourceParams,
    API_VERSION_V1ALPHA1,
};
use uuid::Uuid;

pub const V1ALPHA1: &str = API_VERSION_V1ALPHA1;

#[derive(Default)]
pub struct StoreBuilder {
    rows: Vec<ResourceRow>,
    ancestor_depth: Option<usize>,
}

impl StoreBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resource(&mut self, kind: &str, name: &str, parent: Option<Uuid>) -> Uuid {
        self.row(kind, name, parent, BTreeMap::new(), serde_json::json!({}))
    }

    pub fn labeled(
        &mut self,
        kind: &str,
        name: &str,
        parent: Option<Uuid>,
        labels: &[(&str, &str)],
    ) -> Uuid {
        self.row(
            kind,
            name,
            parent,
            labels
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            serde_json::json!({}),
        )
    }

    /// A `Role` or `PlatformRole` body.
    pub fn role(
        &mut self,
        kind: &str,
        name: &str,
        parent: Option<Uuid>,
        statements: serde_json::Value,
    ) -> Uuid {
        self.row(
            kind,
            name,
            parent,
            BTreeMap::new(),
            serde_json::json!({ "statements": statements }),
        )
    }

    /// A binding, written in the normalized shape admission persists.
    pub fn binding(
        &mut self,
        kind: &str,
        name: &str,
        parent: Option<Uuid>,
        spec: serde_json::Value,
    ) -> Uuid {
        self.row(kind, name, parent, BTreeMap::new(), spec)
    }

    pub fn row(
        &mut self,
        kind: &str,
        name: &str,
        parent: Option<Uuid>,
        labels: BTreeMap<String, String>,
        spec: serde_json::Value,
    ) -> Uuid {
        let uid = Uuid::new_v4();
        let now = Utc::now();
        self.rows.push(ResourceRow {
            uid,
            api_version: V1ALPHA1.to_owned(),
            kind: kind.to_owned(),
            parent_uid: parent,
            name: name.to_owned(),
            discriminator: String::new(),
            labels,
            metadata: serde_json::json!({}),
            spec,
            status: serde_json::json!({}),
            revision: 1,
            finalizers: Vec::new(),
            owner_references: Vec::new(),
            deletion_timestamp: None,
            created_at: now,
            updated_at: now,
        });
        uid
    }

    /// Mark a row tombstoned, as `delete` does before collection.
    pub fn tombstone(&mut self, uid: Uuid) {
        for row in &mut self.rows {
            if row.uid == uid {
                row.deletion_timestamp = Some(Utc::now());
            }
        }
    }

    /// Cap `ancestors` at this many rows, counted from the leaf, the way the
    /// Postgres walk truncates a chain deeper than its bound.
    pub fn truncating_at(mut self, depth: usize) -> Self {
        self.ancestor_depth = Some(depth);
        self
    }

    pub fn build(self) -> Arc<FakeStore> {
        Arc::new(FakeStore {
            rows: self.rows,
            ancestor_depth: self.ancestor_depth,
        })
    }
}

pub struct FakeStore {
    rows: Vec<ResourceRow>,
    ancestor_depth: Option<usize>,
}

impl FakeStore {
    fn find(&self, uid: Uuid) -> Option<&ResourceRow> {
        self.rows.iter().find(|row| row.uid == uid)
    }

    pub fn node(&self, uid: Uuid) -> ResourceNode {
        let row = self.find(uid).expect("resource exists");
        ResourceNode {
            kind: row.resource_kind().expect("valid kind"),
            name: row.name.clone(),
            labels: row.labels.clone(),
        }
    }
}

#[async_trait::async_trait]
impl ResourceApi for FakeStore {
    async fn get(&self, uid: Uuid) -> Result<Option<ResourceRow>, StoreError> {
        Ok(self.find(uid).cloned())
    }

    async fn get_by_name(
        &self,
        _api_version: &str,
        kind: &str,
        name: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Option<ResourceRow>, StoreError> {
        Ok(self
            .rows
            .iter()
            .find(|row| row.kind == kind && row.name == name && row.parent_uid == parent_uid)
            .cloned())
    }

    async fn list(
        &self,
        _api_version: &str,
        kind: &str,
        parent_uid: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        // Deliberately returns tombstoned rows, exactly as the Postgres store
        // does: filtering them is the engine's job.
        Ok(self
            .rows
            .iter()
            .filter(|row| row.kind == kind && row.parent_uid == parent_uid)
            .cloned()
            .collect())
    }

    async fn create(&self, _: CreateResourceParams) -> Result<ResourceRow, StoreError> {
        unimplemented!("authorization never writes")
    }
    async fn list_versions(
        &self,
        _: &[String],
        _: &str,
        _: Option<Uuid>,
    ) -> Result<Vec<ResourceRow>, StoreError> {
        unimplemented!("unused by the engine")
    }
    async fn update(&self, _: Uuid, _: UpdateResourceParams) -> Result<ResourceRow, StoreError> {
        unimplemented!("authorization never writes")
    }
    async fn delete(&self, _: Uuid) -> Result<DeleteOutcome, StoreError> {
        unimplemented!("authorization never writes")
    }
    async fn update_controller_status(
        &self,
        _: Uuid,
        _: &str,
        _: serde_json::Value,
    ) -> Result<ResourceRow, StoreError> {
        unimplemented!("authorization never writes")
    }
    async fn update_controller_finalizers(
        &self,
        _: Uuid,
        _: &str,
        _: &[String],
        _: &[String],
    ) -> Result<ResourceRow, StoreError> {
        unimplemented!("authorization never writes")
    }
}

#[async_trait::async_trait]
impl ResourceStore for FakeStore {
    async fn ancestors(&self, uid: Uuid) -> Result<Vec<ResourceRow>, StoreError> {
        let mut chain = Vec::new();
        let mut cursor = Some(uid);
        while let Some(current) = cursor {
            let Some(row) = self.find(current) else {
                return Ok(Vec::new());
            };
            cursor = row.parent_uid;
            chain.push(row.clone());
            if self.ancestor_depth == Some(chain.len()) {
                break;
            }
        }
        chain.reverse();
        Ok(chain)
    }
    async fn try_collect(&self, _: Uuid) -> Result<DeleteOutcome, StoreError> {
        unimplemented!("authorization never writes")
    }
    async fn list_pending_collection(&self, _: i64) -> Result<Vec<ResourceRow>, StoreError> {
        unimplemented!("unused by the engine")
    }
    async fn list_deletion_blockers(&self, _: Uuid) -> Result<DeletionBlockerReport, StoreError> {
        unimplemented!("unused by the engine")
    }
    async fn resolve_path(&self, _: &[PathSegment]) -> Result<Vec<ResourceRow>, StoreError> {
        unimplemented!("unused by the engine")
    }
    async fn operator_update_status(
        &self,
        _: Uuid,
        _: &str,
        _: serde_json::Value,
    ) -> Result<ResourceRow, StoreError> {
        unimplemented!("authorization never writes")
    }
    async fn operator_update_finalizers(
        &self,
        _: Uuid,
        _: &str,
        _: &[String],
        _: &[String],
    ) -> Result<ResourceRow, StoreError> {
        unimplemented!("authorization never writes")
    }
    async fn resolve_collection(&self, _: &str) -> Result<Option<CollectionInfo>, StoreError> {
        unimplemented!("unused by the engine")
    }
    async fn resolve_collection_version(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        unimplemented!("unused by the engine")
    }
    async fn resolve_collection_by_kind(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        unimplemented!("unused by the engine")
    }
    async fn register_resource_definition(
        &self,
        _: CreateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        unimplemented!("authorization never writes")
    }
    async fn update_resource_definition(
        &self,
        _: Uuid,
        _: UpdateResourceParams,
    ) -> Result<ResourceRow, StoreError> {
        unimplemented!("authorization never writes")
    }
}

/// A membership resolver whose facts the test states directly, standing in for
/// `GroupMembership` reads and the configured operator selector set.
pub struct FakeMemberships {
    membership: PrincipalMembership,
}

impl FakeMemberships {
    pub fn none() -> Arc<Self> {
        Arc::new(Self {
            membership: PrincipalMembership::default(),
        })
    }

    pub fn groups(groups: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            membership: PrincipalMembership {
                groups: groups
                    .iter()
                    .map(|group| group.parse().expect("valid group subject"))
                    .collect::<BTreeSet<_>>(),
                is_operator: false,
            },
        })
    }

    pub fn operator() -> Arc<Self> {
        Arc::new(Self {
            membership: PrincipalMembership {
                groups: BTreeSet::new(),
                is_operator: true,
            },
        })
    }
}

#[async_trait::async_trait]
impl MembershipResolver for FakeMemberships {
    async fn resolve(
        &self,
        _principal: &AuthenticatedPrincipal,
    ) -> Result<PrincipalMembership, AuthorizationError> {
        Ok(self.membership.clone())
    }
}
