//! E2E harness entrypoint. Gated on `RISE_E2E_BACKEND`; skips instantly when unset.

use rise_e2e::{backend, scenario, BackendKind};

#[test]
fn e2e_suite() {
    let Some(kind) = BackendKind::from_env() else {
        eprintln!("[e2e] RISE_E2E_BACKEND unset — skipping E2E harness");
        return;
    };
    eprintln!("[e2e] backend = {}", kind.as_str());

    let mut backend = backend::create(kind).expect("construct backend driver");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        backend.bring_up()?;
        scenario::run_all(backend.as_ref())
    }));
    backend.tear_down();
    match outcome {
        Ok(result) => result.expect("e2e scenarios passed"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
