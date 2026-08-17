use std::collections::BTreeMap;

use rise_resource_api::{ResourceKind, ResourceRow, Scope, API_GROUP, ORGANIZATION_KIND};

use crate::engine::AuthorizationError;

/// One node of a resource's structural ancestry.
///
/// Only the facts authorization reads: the node's version-independent kind, its
/// canonical name, and its own stored labels. Inheritance is resolved over the
/// chain, never stored per node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceNode {
    pub kind: ResourceKind,
    pub name: String,
    pub labels: BTreeMap<String, String>,
}

impl ResourceNode {
    pub fn new(kind: ResourceKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            labels: BTreeMap::new(),
        }
    }

    pub fn with_labels(
        mut self,
        labels: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.labels = labels
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        self
    }

    fn from_row(row: &ResourceRow) -> Result<Self, AuthorizationError> {
        Ok(Self {
            kind: row.resource_kind()?,
            name: row.name.clone(),
            labels: row.labels.clone(),
        })
    }
}

/// The evaluation target: a resource together with its root-first ancestry.
///
/// A create request has no stored row yet, so the chain is the parent ancestry
/// plus the proposed leaf. That keeps one target shape for every verb: nothing
/// in evaluation distinguishes a resource that exists from one being written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceTree {
    nodes: Vec<ResourceNode>,
}

impl ResourceTree {
    /// Build from a root-first chain whose last node is the resource itself.
    pub fn new(nodes: Vec<ResourceNode>) -> Result<Self, AuthorizationError> {
        if nodes.is_empty() {
            return Err(AuthorizationError::invalid_target(
                "a resource tree must contain at least the resource itself",
            ));
        }
        Ok(Self { nodes })
    }

    /// Build from a store ancestor chain, which already includes the leaf.
    pub fn from_rows(rows: &[ResourceRow]) -> Result<Self, AuthorizationError> {
        Self::new(
            rows.iter()
                .map(ResourceNode::from_row)
                .collect::<Result<Vec<_>, _>>()?,
        )
    }

    /// Build from an already-resolved ancestry plus a leaf that is not stored
    /// yet, or that was fetched separately (list items, create requests).
    pub fn with_leaf(ancestors: &[ResourceNode], leaf: ResourceNode) -> Self {
        let mut nodes = ancestors.to_vec();
        nodes.push(leaf);
        Self { nodes }
    }

    pub fn nodes(&self) -> &[ResourceNode] {
        &self.nodes
    }

    pub fn leaf(&self) -> &ResourceNode {
        self.nodes.last().expect("non-empty resource tree")
    }

    /// The Organization this resource is contained by, if any.
    ///
    /// Organization is a root kind, so an org-contained resource always roots at
    /// its own Organization and a root-scoped kind never does. An Organization
    /// is contained by itself.
    pub fn organization(&self) -> Option<&str> {
        let root = self.nodes.first().expect("non-empty resource tree");
        (root.kind.group() == API_GROUP && root.kind.kind() == ORGANIZATION_KIND)
            .then_some(root.name.as_str())
    }

    /// Nearest-wins label resolution down the chain (ADR-0001 §6.1).
    ///
    /// A descendant's own value shadows an ancestor's for that key; it never
    /// unions with it. Always computed, never stored: the read-path display
    /// value and the authorization-path match are the same walk.
    pub fn effective_labels(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        for node in &self.nodes {
            labels.extend(
                node.labels
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        labels
    }

    /// Whether a binding placed at `scope` applies to this resource.
    ///
    /// A scope names a node of the tree by its own kind and its root-first
    /// ancestor names; the grant reaches that node and everything below it.
    pub fn covered_by(&self, scope: &Scope) -> bool {
        let Some(kind) = scope.resource_kind() else {
            return true;
        };
        let names = scope.names();
        if names.len() > self.nodes.len() {
            return false;
        }
        let scoped = &self.nodes[..names.len()];
        scoped
            .iter()
            .zip(names)
            .all(|(node, name)| node.name == name)
            && scoped.last().is_some_and(|node| &node.kind == kind)
    }
}
