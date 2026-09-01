//! The Organization read seam.
//!
//! A deployment backend needs a few per-Organization facts on every reconcile:
//! which controller class owns the Org, and what namespace prefix its projects
//! live under. Those come from the resource store, which backends do not depend
//! on. This trait is the narrow read boundary between them, implemented in
//! `rise-deploy` over the resource API and typically cached behind a short TTL.

use std::collections::BTreeMap;

use async_trait::async_trait;
use uuid::Uuid;

/// Annotation on an Organization naming the namespace prefix its projects
/// deploy into. Absent, the `org-{discriminator}-` fallback applies.
pub const NAMESPACE_PREFIX_ANNOTATION: &str = "kubernetes.rise.dev/namespace-prefix";

/// The `org-{discriminator}-` fallback used when no namespace-prefix annotation
/// is set. Shared so bootstrap and the per-request path agree by construction.
pub fn resolve_namespace_prefix_fallback(discriminator: &str) -> String {
    format!("org-{discriminator}-")
}

/// Resolve an Organization's namespace prefix from its annotations and
/// discriminator: the annotation wins, otherwise the fallback.
pub fn resolve_namespace_prefix(
    annotations: &BTreeMap<String, String>,
    discriminator: &str,
) -> String {
    annotations
        .get(NAMESPACE_PREFIX_ANNOTATION)
        .cloned()
        .unwrap_or_else(|| resolve_namespace_prefix_fallback(discriminator))
}

/// Per-Organization facts a deployment backend reads while reconciling.
#[async_trait]
pub trait OrganizationView: Send + Sync {
    /// The Organization a project belongs to.
    ///
    /// Bootstrap validation refuses to start the server with unlinked projects,
    /// so an unlinked project here is a broken invariant and surfaces as `Err`
    /// rather than `Ok(None)`.
    async fn organization_uid_for_project(&self, project_id: Uuid) -> anyhow::Result<Uuid>;

    /// `spec.deploymentControllerClass`, or `None` when the Organization exists
    /// but does not set it.
    async fn controller_class(&self, org_uid: Uuid) -> anyhow::Result<Option<String>>;

    /// The namespace prefix the Organization's projects deploy into.
    async fn namespace_prefix(&self, org_uid: Uuid) -> anyhow::Result<String>;
}

/// Decide whether a controller may reconcile an Organization, comparing the
/// controller's configured class against the Organization's.
///
/// A controller with no configured class reconciles everything.
pub fn controller_class_matches(configured: Option<&str>, org_class: Option<&str>) -> bool {
    match configured {
        None => true,
        Some(configured) => org_class == Some(configured),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotation_wins_over_fallback() {
        let mut annotations = BTreeMap::new();
        annotations.insert(
            NAMESPACE_PREFIX_ANNOTATION.to_string(),
            "custom-".to_string(),
        );
        assert_eq!(resolve_namespace_prefix(&annotations, "abc"), "custom-");
    }

    #[test]
    fn falls_back_to_discriminator() {
        assert_eq!(
            resolve_namespace_prefix(&BTreeMap::new(), "abc"),
            "org-abc-"
        );
    }

    #[test]
    fn unconfigured_controller_reconciles_every_org() {
        // Legacy installs (no controller class configured) reconcile every
        // project regardless of what the Organization carries.
        assert!(controller_class_matches(None, None));
        assert!(controller_class_matches(
            None,
            Some("kubernetes.rise.dev/default")
        ));
        assert!(controller_class_matches(None, Some("something-else")));
    }

    #[test]
    fn matching_class_reconciles() {
        assert!(controller_class_matches(
            Some("kubernetes.rise.dev/default"),
            Some("kubernetes.rise.dev/default"),
        ));
    }

    #[test]
    fn mismatched_class_is_refused() {
        assert!(!controller_class_matches(
            Some("kubernetes.rise.dev/default"),
            Some("kubernetes.rise.dev/other"),
        ));
    }

    #[test]
    fn org_without_a_class_is_refused() {
        // An Org that has no `spec.deploymentControllerClass` is not owned by
        // this (or any) controller, even when the controller has a class
        // configured. Refuse to reconcile.
        assert!(!controller_class_matches(
            Some("kubernetes.rise.dev/default"),
            None
        ));
    }
}
