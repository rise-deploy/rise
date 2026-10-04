//! The shared session-ceiling cases (ADR-0006), kept with the typed API's check
//! in `rise-backend-auth/testdata/session-ceilings.json`, against the engine.
//! Both evaluators must agree with every case, so a CLI session means the same
//! thing before and after its kinds migrate to the resource API.
//!
//! Targets are placed as ADR-0001 places these kinds: the Environment, the
//! environment-bound kinds and every other project resource sit directly under
//! their Project, the bound ones carrying the governed `rise.dev/environment`
//! label. A migration that places a kind elsewhere must change [`tree_for`].

use super::principal::AuthorizationCap;
use super::tree::{ResourceNode, ResourceTree};
use crate::policy::{statement_matches, PermissionTuple};
use rise_resource_api::{ResourceKind, SubresourceName, Verb};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The governed label binding a resource to its environment (ADR-0006 §4).
const ENVIRONMENT_LABEL: &str = "rise.dev/environment";
/// Kinds whose instances carry [`ENVIRONMENT_LABEL`].
const ENVIRONMENT_BOUND: [&str; 2] = ["rise.dev/Deployment", "rise.dev/EnvironmentVariable"];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vectors {
    organization: String,
    ceilings: BTreeMap<String, Ceiling>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ceiling {
    details: Vec<serde_json::Value>,
    #[serde(default)]
    refused_by: Vec<String>,
}

#[derive(Deserialize)]
struct Case {
    ceiling: String,
    verb: Verb,
    kind: String,
    subresource: Option<String>,
    project: String,
    environment: Option<String>,
    allowed: bool,
}

fn kind(name: &str) -> ResourceKind {
    name.parse().unwrap()
}

/// The target's ancestry: Organization, Project, and the resource itself.
fn tree_for(organization: &str, case: &Case) -> ResourceTree {
    let mut nodes = vec![
        ResourceNode::new(kind("rise.dev/Organization"), organization),
        ResourceNode::new(kind("rise.dev/Project"), case.project.as_str()),
    ];
    match case.kind.as_str() {
        "rise.dev/Project" => {}
        "rise.dev/Environment" => nodes.push(ResourceNode::new(
            kind(&case.kind),
            case.environment
                .as_deref()
                .expect("an Environment case names it"),
        )),
        other => {
            let leaf = ResourceNode::new(kind(other), "subject");
            nodes.push(match &case.environment {
                Some(environment) if ENVIRONMENT_BOUND.contains(&other) => {
                    leaf.with_labels([(ENVIRONMENT_LABEL, environment.as_str())])
                }
                _ => leaf,
            });
        }
    }
    ResourceTree::new(nodes).unwrap()
}

#[test]
fn the_engine_agrees_with_every_case() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../rise-backend-auth/testdata/session-ceilings.json");
    let vectors: Vectors = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut wrong = Vec::new();
    for (name, ceiling) in &vectors.ceilings {
        let cap = AuthorizationCap::from_details(Some(&ceiling.details));
        if ceiling.refused_by.iter().any(|by| by == "engine") {
            assert!(cap.is_err(), "ceiling {name} should be refused");
            continue;
        }
        let cap = cap.unwrap_or_else(|e| panic!("ceiling {name} refused: {e:?}"));
        for case in vectors.cases.iter().filter(|case| &case.ceiling == name) {
            let statements = cap
                .ceiling_for(&tree_for(&vectors.organization, case))
                .expect("a ceiling is restricted");
            let request = PermissionTuple {
                verb: case.verb,
                kind: kind(&case.kind),
                subresource: case
                    .subresource
                    .as_deref()
                    .map(|s| s.parse::<SubresourceName>().unwrap()),
            };
            let allowed = statements
                .iter()
                .any(|statement| statement_matches(statement, &request));
            if allowed != case.allowed {
                wrong.push(format!(
                    "{name}: {:?} {} {:?} in {}/{:?} should be {}",
                    case.verb,
                    case.kind,
                    case.subresource,
                    case.project,
                    case.environment,
                    if case.allowed { "allowed" } else { "denied" }
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} case(s) disagree:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}
