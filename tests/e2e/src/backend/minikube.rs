//! Kubernetes-backend driver: a *thin connector*. The CI job keeps the existing
//! `scripts/ci/e2e-minikube.sh` bring-up (minikube + helm + `kubectl port-forward
//! svc/rise-ci-chart 3000:3000`); the harness connects via `RISE_URL` and runs the
//! CLI from the image. App-HTTP reach and the SA-exchange scenario are declared
//! gaps here in this increment (see `applies_to`), to be ported later.

use anyhow::{Context, Result};
use std::process::Command;

use super::{Backend, BackendKind, CliAuth};
use crate::cli::{self, CliOutput};
use crate::dex::DexEndpoint;
use crate::http::{self, HttpResponse};
use crate::token;

// Matches helm/rise/values-ci.yaml.
const SECRET_B64: &str = "dGVzdC1qd3Qtc2VjcmV0LWtleS1mb3ItY2ktdGVzdGluZy1vbmx5LW5vdC1zZWN1cmU=";
const DEFAULT_PUBLIC_URL: &str = "http://rise.local";
const DEFAULT_RISE_URL: &str = "http://127.0.0.1:3000";

pub struct MinikubeBackend {
    rise_url: String,
    image: String,
    ci_token: String,
}

impl MinikubeBackend {
    pub fn new() -> Result<Self> {
        let rise_url = std::env::var("RISE_URL").unwrap_or_else(|_| DEFAULT_RISE_URL.to_string());
        let public_url =
            std::env::var("RISE_PUBLIC_URL").unwrap_or_else(|_| DEFAULT_PUBLIC_URL.to_string());
        let repo = std::env::var("RISE_IMAGE_REPOSITORY")
            .unwrap_or_else(|_| "ghcr.io/rise-deploy/rise".to_string());
        let tag = std::env::var("RISE_IMAGE_TAG")
            .context("RISE_IMAGE_TAG must be set for the minikube backend")?;
        let ci_token = token::mint_ci_token(SECRET_B64, &public_url)?;
        Ok(Self {
            rise_url,
            image: format!("{repo}:{tag}"),
            ci_token,
        })
    }
}

impl Backend for MinikubeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Minikube
    }

    fn bring_up(&mut self) -> Result<()> {
        // The cluster + port-forward are established by the CI script; just
        // confirm the control plane is reachable.
        http::poll(
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(2),
            "rise backend /health (port-forward)",
            || Ok(http::get(&format!("{}/health", self.rise_url), None)?.status == 200),
        )
    }

    fn tear_down(&mut self) {
        // Cluster teardown is owned by the CI script.
    }

    fn rise_cli(&self, args: &[&str], auth: Option<CliAuth<'_>>) -> Result<CliOutput> {
        // Run the CLI from the image on the host network (reaches the port-forward).
        let pwd = std::env::current_dir().context("cwd")?;
        let pwd_str = pwd.to_string_lossy().to_string();
        let mut c = Command::new("docker");
        c.args(["run", "--rm", "--network", "host"])
            .args(["-e", &format!("RISE_URL={}", self.rise_url)])
            .args(["-e", &format!("MISE_TRUSTED_CONFIG_PATHS={pwd_str}")])
            .args(["-v", &format!("{pwd_str}:{pwd_str}")])
            .args(["-w", &pwd_str]);
        match &auth {
            Some(a) => {
                c.args(["-e", &format!("RISE_TOKEN={}", a.token)]);
                if let Some(id) = a.identity {
                    c.args(["-e", &format!("RISE_IDENTITY={id}")]);
                }
            }
            None => {
                c.args(["-e", &format!("RISE_TOKEN={}", self.ci_token)]);
            }
        }
        c.args(["--entrypoint", "/usr/local/bin/rise", &self.image]);
        c.args(args);
        cli::run(c)
    }

    fn reach_app(&self, _project: &str, _path: &str) -> Result<Option<HttpResponse>> {
        // App-HTTP reach (per-project `kubectl port-forward` to the app svc) is
        // not yet ported; scenarios assert via wait_healthy here.
        Ok(None)
    }

    fn dex(&self) -> Option<&DexEndpoint> {
        // The in-cluster Dex isn't reachable from the harness host in this
        // increment, so the SA-exchange scenario is skipped on minikube.
        None
    }
}
