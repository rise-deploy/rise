//! Running the `rise` CLI (and other external commands) and capturing output.

use anyhow::{Context, Result};
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Hard wall-clock cap for a single diagnostic command. The diagnostics path
/// runs precisely when the cluster is likely unhealthy, so a wedged subprocess
/// must not hang the whole run — it is killed past this budget.
const DUMP_TIMEOUT: Duration = Duration::from_secs(30);

/// Captured result of a CLI invocation. Non-zero exits are *not* errors — the
/// caller inspects [`CliOutput::success`] / `stderr` (negative-path scenarios
/// expect failures).
#[derive(Debug)]
pub struct CliOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// stdout + stderr joined — convenient for `contains` assertions.
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Run a command to completion, capturing stdout/stderr. Spawning failure (e.g.
/// the binary is missing) is an `Err`; a non-zero exit is reported in `CliOutput`.
pub fn run(mut cmd: Command) -> Result<CliOutput> {
    let out = cmd
        .output()
        .with_context(|| format!("spawning command: {cmd:?}"))?;
    Ok(CliOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Run a diagnostic command best-effort and print its output to stderr under a
/// header. Never returns an error — used on the failure path to surface cluster
/// / container state and logs, so a missing tool or a non-zero exit just prints
/// what it can and moves on.
pub fn dump(label: &str, mut cmd: Command) {
    eprintln!("\n--- {label} ---");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("(could not run {cmd:?}: {e})");
            return;
        }
    };
    // Drain both pipes on threads so a chatty child can't deadlock on a full
    // pipe buffer while we wait on the wall-clock budget.
    let mut out_pipe = child.stdout.take().expect("piped stdout");
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let out_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out_pipe.read_to_end(&mut b);
        b
    });
    let err_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err_pipe.read_to_end(&mut b);
        b
    });

    let deadline = Instant::now() + DUMP_TIMEOUT;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                eprintln!("(error waiting on {cmd:?}: {e})");
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
        }
    };

    let stdout = String::from_utf8_lossy(&out_h.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_h.join().unwrap_or_default()).into_owned();
    if !stdout.trim().is_empty() {
        eprint!("{stdout}");
        if !stdout.ends_with('\n') {
            eprintln!();
        }
    }
    if !stderr.trim().is_empty() {
        eprintln!("[stderr] {}", stderr.trim_end());
    }
    if timed_out {
        eprintln!(
            "[diagnostics command exceeded {}s — killed]",
            DUMP_TIMEOUT.as_secs()
        );
    }
}

/// Run a setup command (compose up, docker cp, …) and fail loudly on non-zero.
pub fn run_checked(cmd: Command) -> Result<CliOutput> {
    let label = format!("{cmd:?}");
    let out = run(cmd)?;
    if !out.success() {
        anyhow::bail!(
            "command failed (exit {:?}): {label}\nstdout: {}\nstderr: {}",
            out.status,
            out.stdout,
            out.stderr
        );
    }
    Ok(out)
}
