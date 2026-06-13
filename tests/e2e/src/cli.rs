//! Running the `rise` CLI (and other external commands) and capturing output.

use anyhow::{Context, Result};
use std::process::Command;

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
