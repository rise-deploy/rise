//! Rise bookkeeping as ECS resource tags.
//!
//! The Docker backend stamps bookkeeping *labels* on containers and finds its
//! own workloads by filtering on them. ECS has no labels on services, so the
//! same vocabulary rides on **tags** — and because ECS tag keys permit `/` and
//! `.`, the keys are byte-identical to the Docker ones (`rise.dev/project`, …).
//! One vocabulary across backends means the operator docs, the debugging
//! muscle memory, and the drift logic all transfer.
//!
//! Tags are the *only* discovery mechanism: the reconciler never parses service
//! names to decide ownership, because a name is lossy (see
//! `rise_backend_core::naming::group_service_base`) while tags are exact.

use std::collections::HashMap;

use rise_backend_core::labels::{
    ns_key, SUFFIX_CONTAINER, SUFFIX_CONTROLLER_CLASS, SUFFIX_DEPLOYMENT_GROUP,
    SUFFIX_DEPLOYMENT_ID, SUFFIX_DEPLOYMENT_UUID, SUFFIX_ENVIRONMENT, SUFFIX_ENV_HASH,
    SUFFIX_IMAGE, SUFFIX_MANAGED_BY, SUFFIX_PROJECT, SUFFIX_ROUTE_HASH,
};

/// ECS allows 50 tags per resource. We stamp ~11, so the headroom is ample — but
/// assert it rather than trust it, since a future field could quietly cross the
/// line and `CreateService` would start failing for every project at once.
pub const MAX_TAGS_PER_RESOURCE: usize = 50;

/// The bookkeeping identity of one Rise-managed ECS service, as carried in tags.
///
/// This is the ECS analogue of `rise_backend_core::labels::BookkeepingLabels`,
/// minus the fields that only make sense per-container-instance (`generation`,
/// `replica`): on ECS a service owns its replicas via `desiredCount`, so there
/// is no per-replica identity to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceTags {
    pub project: String,
    pub deployment_group: String,
    pub deployment_id: String,
    pub deployment_uuid: String,
    pub container: String,
    pub environment: Option<String>,
    /// Digest over the container's whole environment, for operators comparing
    /// two services. ECS tags are widely readable, so secret values enter it as
    /// fingerprints of their stored form, never as plaintext — see
    /// `redact_secrets_for_hash`.
    pub env_hash: String,
    pub image: String,
    pub route_hash: String,
}

impl ServiceTags {
    /// Render to a `key -> value` map under the configured namespace, including
    /// the `managed-by` / `controller-class` markers the reconciler filters on.
    pub fn render(&self, label_namespace: &str, controller_class: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        out.insert(
            ns_key(label_namespace, SUFFIX_MANAGED_BY),
            "rise".to_string(),
        );
        out.insert(
            ns_key(label_namespace, SUFFIX_CONTROLLER_CLASS),
            controller_class.to_string(),
        );
        out.insert(
            ns_key(label_namespace, SUFFIX_PROJECT),
            self.project.clone(),
        );
        out.insert(
            ns_key(label_namespace, SUFFIX_DEPLOYMENT_GROUP),
            self.deployment_group.clone(),
        );
        out.insert(
            ns_key(label_namespace, SUFFIX_DEPLOYMENT_ID),
            self.deployment_id.clone(),
        );
        out.insert(
            ns_key(label_namespace, SUFFIX_DEPLOYMENT_UUID),
            self.deployment_uuid.clone(),
        );
        out.insert(
            ns_key(label_namespace, SUFFIX_CONTAINER),
            self.container.clone(),
        );
        out.insert(
            ns_key(label_namespace, SUFFIX_ENV_HASH),
            self.env_hash.clone(),
        );
        out.insert(ns_key(label_namespace, SUFFIX_IMAGE), self.image.clone());
        out.insert(
            ns_key(label_namespace, SUFFIX_ROUTE_HASH),
            self.route_hash.clone(),
        );
        // Only stamped when set, mirroring the Docker backend: an absent
        // environment must not become the literal string "None".
        if let Some(environment) = &self.environment {
            out.insert(
                ns_key(label_namespace, SUFFIX_ENVIRONMENT),
                environment.clone(),
            );
        }
        out
    }

    /// Recover the bookkeeping identity from an observed service's tags.
    ///
    /// `None` when a required tag is missing — such a service is not one we can
    /// attribute, and the reconciler must treat it as unknown rather than guess
    /// (guessing here would mean deleting someone else's workload).
    ///
    /// **The required set is frozen.** A service that fails to parse but carries
    /// our markers is retired unconditionally by the orphan sweep, so adding a
    /// sixth `?` here would make every service the previous version created
    /// unparseable on the first tick after an upgrade — a fleet-wide
    /// simultaneous retirement, on an install that changed nothing. New tags
    /// must be lenient (`unwrap_or_default`, as the drift-only fields below
    /// are) or backfilled onto existing services before the field is required.
    pub fn parse(tags: &HashMap<String, String>, label_namespace: &str) -> Option<Self> {
        let get = |suffix: &str| tags.get(&ns_key(label_namespace, suffix)).cloned();
        Some(Self {
            project: get(SUFFIX_PROJECT)?,
            deployment_group: get(SUFFIX_DEPLOYMENT_GROUP)?,
            deployment_id: get(SUFFIX_DEPLOYMENT_ID)?,
            deployment_uuid: get(SUFFIX_DEPLOYMENT_UUID)?,
            container: get(SUFFIX_CONTAINER)?,
            environment: get(SUFFIX_ENVIRONMENT),
            env_hash: get(SUFFIX_ENV_HASH).unwrap_or_default(),
            image: get(SUFFIX_IMAGE).unwrap_or_default(),
            route_hash: get(SUFFIX_ROUTE_HASH).unwrap_or_default(),
        })
    }

    /// Whether an observed tag set is a Rise-managed service of *this*
    /// controller class. Both markers must match: `managed-by` keeps us off
    /// unrelated services, `controller-class` keeps two Rise controllers sharing
    /// a cluster from garbage-collecting each other's workloads.
    pub fn is_managed(
        tags: &HashMap<String, String>,
        label_namespace: &str,
        controller_class: &str,
    ) -> bool {
        tags.get(&ns_key(label_namespace, SUFFIX_MANAGED_BY))
            .map(String::as_str)
            == Some("rise")
            && tags
                .get(&ns_key(label_namespace, SUFFIX_CONTROLLER_CLASS))
                .map(String::as_str)
                == Some(controller_class)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ServiceTags {
        ServiceTags {
            project: "myapp".to_string(),
            deployment_group: "default".to_string(),
            deployment_id: "20260101-120000".to_string(),
            deployment_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            container: "app".to_string(),
            environment: Some("production".to_string()),
            env_hash: "abc123".to_string(),
            image: "registry/myapp:20260101-120000".to_string(),
            route_hash: "rh-active".to_string(),
        }
    }

    /// Pins the frozen set from `parse`'s doc comment. This test failing means
    /// the upgrade contract is being changed: every service created by the
    /// previous version becomes unattributable, and unattributable services are
    /// retired unconditionally.
    #[test]
    fn only_the_five_identity_tags_are_required_to_attribute_a_service() {
        let rendered = sample().render("rise.dev", "default");
        let required = [
            "rise.dev/project",
            "rise.dev/deployment-group",
            "rise.dev/deployment-id",
            "rise.dev/deployment-uuid",
            "rise.dev/container",
        ];

        for key in required {
            let mut without = rendered.clone();
            without.remove(key);
            assert!(
                ServiceTags::parse(&without, "rise.dev").is_none(),
                "{key} must be required to attribute a service"
            );
        }

        for key in rendered.keys() {
            if required.contains(&key.as_str()) {
                continue;
            }
            let mut without = rendered.clone();
            without.remove(key);
            assert!(
                ServiceTags::parse(&without, "rise.dev").is_some(),
                "{key} must not be required -- making it so retires every \
                 service the previous version created"
            );
        }
    }

    #[test]
    fn tags_round_trip() {
        // Drift detection compares parsed tags against freshly computed desired
        // state. If render and parse disagree on even one key, every service
        // looks permanently drifted and the reconciler churns forever.
        let tags = sample();
        let rendered = tags.render("rise.dev", "default");
        let parsed = ServiceTags::parse(&rendered, "rise.dev").expect("round-trips");
        assert_eq!(parsed, tags);
    }

    #[test]
    fn absent_environment_is_not_stamped() {
        // A missing environment must be absent, not the string "None" — which
        // would round-trip into Some("None") and drift against desired forever.
        let mut tags = sample();
        tags.environment = None;
        let rendered = tags.render("rise.dev", "default");
        assert!(!rendered.contains_key("rise.dev/environment"));
        let parsed = ServiceTags::parse(&rendered, "rise.dev").expect("round-trips");
        assert_eq!(parsed.environment, None);
    }

    #[test]
    fn tag_count_stays_well_inside_the_ecs_limit() {
        // Crossing 50 tags would fail CreateService for every project at once.
        let rendered = sample().render("rise.dev", "default");
        assert!(
            rendered.len() <= MAX_TAGS_PER_RESOURCE,
            "{} tags exceeds the ECS limit of {MAX_TAGS_PER_RESOURCE}",
            rendered.len()
        );
    }

    #[test]
    fn is_managed_requires_both_markers() {
        // A service belonging to another Rise controller on the same cluster
        // must not be claimed — claiming it means deleting it as an orphan.
        let rendered = sample().render("rise.dev", "default");
        assert!(ServiceTags::is_managed(&rendered, "rise.dev", "default"));
        assert!(
            !ServiceTags::is_managed(&rendered, "rise.dev", "other-class"),
            "a different controller class must not be claimed"
        );

        let mut foreign = rendered.clone();
        foreign.remove("rise.dev/managed-by");
        assert!(
            !ServiceTags::is_managed(&foreign, "rise.dev", "default"),
            "an untagged service must never be claimed"
        );
    }

    #[test]
    fn parse_rejects_a_service_missing_identity_tags() {
        // Partial tags mean we cannot attribute the service. Returning a
        // half-filled identity would let the GC compare it against desired state
        // and delete a workload we do not actually understand.
        let mut rendered = sample().render("rise.dev", "default");
        rendered.remove("rise.dev/deployment-uuid");
        assert!(ServiceTags::parse(&rendered, "rise.dev").is_none());
    }
}
