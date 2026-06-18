//! E2E harness entrypoint — a binary you run with `cargo run`. Backend scenarios
//! share one expensive backend `bring_up` (a whole compose stack or minikube
//! cluster), so they run as one in-order suite under our own reporter rather than
//! as independent tests that would each re-provision. Standalone suites, such as
//! local `rise compose`, run without a backend. Gated on `RISE_E2E_BACKEND` or
//! `RISE_E2E_SUITE`; skips (exit 0) when neither is set.

use std::process::ExitCode;
use std::time::Instant;

use rise_e2e::{backend, compose, report, scenario, BackendKind};

fn main() -> ExitCode {
    match std::env::var("RISE_E2E_SUITE").ok().as_deref() {
        Some("compose") => return run_standalone_compose(),
        Some(other) if !other.is_empty() => {
            eprintln!("[e2e] RISE_E2E_SUITE={other:?} is not known (expected: compose)");
            return ExitCode::FAILURE;
        }
        _ => {}
    }

    let Some(kind) = BackendKind::from_env() else {
        eprintln!(
            "[e2e] RISE_E2E_BACKEND and RISE_E2E_SUITE unset — skipping the e2e harness \
             (set RISE_E2E_BACKEND=docker|minikube or RISE_E2E_SUITE=compose to run it)"
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

    // On any failure (scenario error or panic), capture diagnostics while the
    // stack is still up — teardown below deletes the cluster / compose stack.
    if !matches!(outcome, Ok(Ok(()))) {
        report::section("Capturing diagnostics (run failed)");
        backend.dump_diagnostics();
    }

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

fn run_standalone_compose() -> ExitCode {
    let total = Instant::now();
    let outcome = std::panic::catch_unwind(compose::run);
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
        Err(_) => ExitCode::FAILURE,
    }
}
