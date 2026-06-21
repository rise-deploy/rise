//! Standalone `rise compose` e2e suite.
//!
//! This deliberately does not use the backend driver: `rise compose` is a local
//! Docker Compose workflow and should prove it can build and run a multi-container
//! app without a Rise control plane.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::{cli, http, report};

const APP_PATH: &str = "example/multi-container";

pub fn run() -> Result<()> {
    report::section("rise compose suite");
    report::note("RUN  compose-multi-container");

    let project = unique("e2e-compose");
    let port = next_router_port().to_string();

    let result = report::step("rise compose up", || {
        let up = rise_command(&[
            "compose",
            "up",
            APP_PATH,
            "--project",
            &project,
            "--router-port",
            &port,
            "--detach",
            "--container-cli",
            "docker",
        ])?;
        cli::run_checked(up).map(|_| ())
    })
    .and_then(|_| assert_compose_app(&port));

    let cleanup = report::step("rise compose down", || {
        let down = rise_command(&[
            "compose",
            "down",
            APP_PATH,
            "--project",
            &project,
            "--container-cli",
            "docker",
        ])?;
        cli::run_checked(down).map(|_| ())
    });

    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            report::note("PASS compose-multi-container");
            Ok(())
        }
        (Ok(()), Err(cleanup)) => Err(cleanup).context("rise compose down"),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(cleanup)) => {
            eprintln!("[e2e] compose-multi-container cleanup failed: {cleanup:#}");
            Err(e)
        }
    }
}

fn rise_command(args: &[&str]) -> Result<Command> {
    let spec = std::env::var("RISE_BIN").unwrap_or_else(|_| "cargo run --features cli --".into());
    let mut parts = spec.split_whitespace();
    let program = parts
        .next()
        .context("RISE_BIN is empty; expected a rise binary path or command")?;
    let mut command = Command::new(program);
    command
        .args(parts)
        .args(args)
        .current_dir(repo_root()?)
        // Keep the standalone suite from using env-provided token sources; with
        // no token source, `rise compose` exercises its offline env-var fallback.
        .env_remove("RISE_TOKEN")
        .env_remove("RISE_TOKEN_COMMAND")
        .env_remove("RISE_IDENTITY")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL");
    Ok(command)
}

fn assert_compose_app(port: &str) -> Result<()> {
    let base = format!("http://127.0.0.1:{port}");

    report::step("frontend route", || {
        http::poll(
            Duration::from_secs(90),
            Duration::from_secs(2),
            "compose frontend route",
            || {
                let resp = http::get(&format!("{base}/"), None)?;
                Ok(resp.status == 200 && resp.body.contains("Text analyzer queue"))
            },
        )
    })?;

    report::step("api route reaches redis", || {
        http::poll(
            Duration::from_secs(90),
            Duration::from_secs(2),
            "compose api health route",
            || {
                let resp = http::get(&format!("{base}/api/health"), None)?;
                Ok(resp.status == 200 && resp.body.contains("\"status\":\"ok\""))
            },
        )
    })?;

    let text = "rise compose compose redis worker worker";
    let submitted = http::client()
        .post(format!("{base}/api/jobs"))
        .json(&serde_json::json!({ "text": text }))
        .send()
        .context("POST /api/jobs")?;
    let submitted_status = submitted.status().as_u16();
    let submitted_body = submitted.text().unwrap_or_default();
    anyhow::ensure!(
        submitted_status == 201,
        "job submit returned {submitted_status}:\n{submitted_body}"
    );
    let submitted_json: serde_json::Value =
        serde_json::from_str(&submitted_body).context("parse submitted job")?;
    let job_id = submitted_json["id"]
        .as_str()
        .context("submitted job has no id")?
        .to_string();

    let mut last = String::new();
    report::step("worker completes redis-backed job", || {
        http::poll(
            Duration::from_secs(90),
            Duration::from_secs(2),
            "compose worker to complete a Redis-backed job",
            || {
                let resp = http::get(&format!("{base}/api/jobs/{job_id}"), None)?;
                last = resp.body.clone();
                if resp.status != 200 {
                    return Ok(false);
                }
                let job: serde_json::Value = serde_json::from_str(&resp.body)?;
                Ok(job["status"] == serde_json::json!("completed")
                    && job["worker_pid"]
                        .as_str()
                        .is_some_and(|pid| !pid.is_empty())
                    && job["result"]["words"] == serde_json::json!(6)
                    && job["result"]["top_words"].as_array().is_some_and(|words| {
                        words.iter().any(|w| {
                            w["word"] == serde_json::json!("compose")
                                && w["count"] == serde_json::json!(2)
                        })
                    }))
            },
        )
        .with_context(|| format!("last job response:\n{last}"))
    })
}

fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn next_router_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(18080);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn repo_root() -> Result<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .context("resolve repo root from CARGO_MANIFEST_DIR")
}
