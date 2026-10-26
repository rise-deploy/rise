//! The shared session-ceiling cases (`testdata/session-ceilings.json`, ADR-0006)
//! against the typed API's check. `rise-authz` runs the same file against the
//! engine; both must agree with every case.

use rise_backend_auth::session_scope::{
    compile, AccessRequest, Kind, Operation, SessionCeiling, Subresource, Target, Verb,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

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
    request: Option<AccessRequest>,
    details: Vec<serde_json::Value>,
    #[serde(default)]
    refused_by: Vec<String>,
}

#[derive(Deserialize)]
struct Case {
    ceiling: String,
    verb: Verb,
    kind: Kind,
    subresource: Option<Subresource>,
    project: String,
    environment: Option<String>,
    allowed: bool,
}

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/session-ceilings.json")
}

/// The compiler still produces each request-backed ceiling's stored claim.
/// `RISE_BLESS_SESSION_CEILINGS=1` rewrites the stored claims instead.
#[test]
fn compiled_claims_match_the_stored_ones() {
    let raw = std::fs::read_to_string(path()).unwrap();
    let mut file: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let vectors: Vectors = serde_json::from_str(&raw).unwrap();
    let bless = std::env::var_os("RISE_BLESS_SESSION_CEILINGS").is_some();
    let mut stale = Vec::new();
    for (name, ceiling) in &vectors.ceilings {
        let Some(request) = &ceiling.request else {
            continue;
        };
        let compiled = compile(&vectors.organization, request).expect("a restricted request");
        if compiled != ceiling.details {
            stale.push(name.clone());
            file["ceilings"][name]["details"] = serde_json::Value::Array(compiled);
        }
    }
    if bless {
        let mut text = serde_json::to_string_pretty(&file).unwrap();
        text.push('\n');
        std::fs::write(path(), text).unwrap();
    } else {
        assert!(
            stale.is_empty(),
            "stored claims differ from what compile() produces for {stale:?}; \
             rerun with RISE_BLESS_SESSION_CEILINGS=1 if the change is intended"
        );
    }
}

#[test]
fn the_typed_check_agrees_with_every_case() {
    // Blessing rewrites the file this test reads.
    if std::env::var_os("RISE_BLESS_SESSION_CEILINGS").is_some() {
        return;
    }
    let vectors: Vectors = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    let mut wrong = Vec::new();
    for (name, ceiling) in &vectors.ceilings {
        let parsed = SessionCeiling::parse(&ceiling.details);
        if ceiling.refused_by.iter().any(|by| by == "typed") {
            assert!(parsed.is_err(), "ceiling {name} should be refused");
            continue;
        }
        let parsed = parsed.unwrap_or_else(|e| panic!("ceiling {name} refused: {e}"));
        for case in vectors.cases.iter().filter(|case| &case.ceiling == name) {
            let op = match case.subresource {
                Some(subresource) => Operation::sub(case.verb, case.kind, subresource),
                None => Operation::new(case.verb, case.kind),
            };
            let target = Target {
                organization: &vectors.organization,
                project: &case.project,
                environment: case.environment.as_deref(),
            };
            if parsed.allows(op, &target) != case.allowed {
                wrong.push(format!(
                    "{name}: {:?} {:?} {:?} in {}/{:?} should be {}",
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
