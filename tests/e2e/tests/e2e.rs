//! E2E harness entrypoint — a plain binary (`harness = false`), not a libtest
//! test. Every scenario shares one expensive backend `bring_up` (a whole compose
//! stack or minikube cluster), so they run as one in-order suite under our own
//! reporter rather than independent libtest tests that would each re-provision.
//! Gated on `RISE_E2E_BACKEND`; skips (exit 0) when unset. Exits non-zero on any
//! scenario failure so `cargo test` / CI treats it as a failure.

use std::process::ExitCode;
use std::time::Instant;

use rise_e2e::{backend, report, scenario, BackendKind};

fn main() -> ExitCode {
    let Some(kind) = BackendKind::from_env() else {
        eprintln!(
            "[e2e] RISE_E2E_BACKEND unset — skipping the e2e harness \
             (set RISE_E2E_BACKEND=docker|minikube to run it)"
        );
        return ExitCode::SUCCESS;
    };
    let mode = std::env::var("RISE_E2E_REGISTRY_MODE").unwrap_or_else(|_| "oci-client-auth".into());
    report::section(&format!(
        "rise-e2e harness — backend={}, registry={mode}",
        kind.as_str()
    ));

    let total = Instant::now();
    let mut backend = match backend::create(kind) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[e2e] failed to construct backend driver: {e:#}");
            return ExitCode::FAILURE;
        }
    };
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
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(e)) => {
            eprintln!("\n[e2e] FAILED: {e:#}");
            ExitCode::FAILURE
        }
        // The panic message was already printed by the default hook; just fail.
        Err(_) => ExitCode::FAILURE,
    }
}
