//! Minimal progress reporter for the harness: titled sections, timed steps, and a
//! scenario summary — so a run shows what infrastructure came up (and that it's
//! online) and how each scenario fared. Plain text (no ANSI) so the output stays
//! readable in CI logs and terminals alike.

use anyhow::Result;
use std::io::Write as _;
use std::time::{Duration, Instant};

/// Width the step label is dot-padded to, for aligned `ok (Xs)` columns.
const LABEL_WIDTH: usize = 38;

/// A titled section header (e.g. bring-up, scenarios, teardown).
pub fn section(title: &str) {
    eprintln!("\n==> {title}");
}

/// A free-form indented note.
pub fn note(msg: &str) {
    eprintln!("    {msg}");
}

/// Run `f` as a timed, labeled step. Prints `label ......... ok (Xs)` (or
/// `FAILED (Xs)`) and returns f's result. The label is flushed before running so
/// an in-progress step is visible on a TTY; subprocess output is captured by the
/// caller (not streamed), so the line never interleaves.
pub fn step<T>(label: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    eprint!("    {label:.<LABEL_WIDTH$} ");
    let _ = std::io::stderr().flush();
    let start = Instant::now();
    let out = f();
    let outcome = if out.is_ok() { "ok" } else { "FAILED" };
    eprintln!("{outcome} ({})", human(start.elapsed()));
    out
}

/// A labeled step that reports a value (e.g. an HTTP status) instead of `ok`.
pub fn step_value<T: std::fmt::Display>(label: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    eprint!("    {label:.<LABEL_WIDTH$} ");
    let _ = std::io::stderr().flush();
    let start = Instant::now();
    let out = f();
    match &out {
        Ok(v) => eprintln!("{v} ({})", human(start.elapsed())),
        Err(_) => eprintln!("FAILED ({})", human(start.elapsed())),
    }
    out
}

/// Format a duration compactly (e.g. `1.5s`, `2m03s`).
pub fn human(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 60.0 {
        format!("{}m{:02}s", (s / 60.0) as u64, (s % 60.0) as u64)
    } else {
        format!("{s:.1}s")
    }
}
