//! E2E harness entrypoint. Gated on `RISE_E2E_BACKEND`; skips instantly when unset.
//!
//! This is a single `cargo test` (`e2e_suite`) on purpose: every scenario shares
//! one expensive backend `bring_up` (a whole compose stack or minikube cluster),
//! so they run as one in-order suite rather than independent tests that would each
//! re-provision. The per-scenario results + timings are reported in the output.

use std::time::Instant;

use rise_e2e::{backend, report, scenario, BackendKind};

#[test]
fn e2e_suite() {
    let Some(kind) = BackendKind::from_env() else {
        eprintln!(
            "[e2e] RISE_E2E_BACKEND unset — skipping the e2e harness \
             (set RISE_E2E_BACKEND=docker|minikube to run it)"
        );
        return;
    };
    let mode = std::env::var("RISE_E2E_REGISTRY_MODE").unwrap_or_else(|_| "oci-client-auth".into());
    report::section(&format!(
        "rise-e2e harness — backend={}, registry={mode}",
        kind.as_str()
    ));

    let total = Instant::now();
    let mut backend = backend::create(kind).expect("construct backend driver");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        report::section(&format!("Bringing up {}", kind.as_str()));
        let up = Instant::now();
        backend.bring_up()?;
        report::note(&format!(
            "bring-up complete ({})",
            report::human(up.elapsed())
        ));
        scenario::run_all(backend.as_ref())
    }));

    report::section("Tearing down");
    let down = Instant::now();
    backend.tear_down();
    report::note(&format!(
        "teardown complete ({})",
        report::human(down.elapsed())
    ));
    report::note(&format!(
        "total wall-clock {}",
        report::human(total.elapsed())
    ));

    match outcome {
        Ok(result) => result.expect("e2e scenarios passed"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
