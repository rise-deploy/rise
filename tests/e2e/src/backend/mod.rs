//! The backend driver seam. Scenarios are written once against `dyn Backend`;
//! Docker and Kubernetes each implement the per-backend bits (bring-up, CLI
//! invocation, app reach, teardown). Shared behaviour (e.g. wait-for-healthy)
//! lives as default methods.

use anyhow::Result;
use std::time::Duration;

use crate::cli::CliOutput;
use crate::dex::DexEndpoint;
use crate::http::HttpResponse;

pub use crate::BackendKind;

mod docker;
mod minikube;

/// Auth override for a single CLI invocation. Passing `None` to
/// [`Backend::rise_cli`] uses the backend's admin CI bearer and sets no identity.
pub struct CliAuth<'a> {
    pub token: &'a str,
    /// `RISE_IDENTITY` — set to exchange the token for a service account.
    pub identity: Option<&'a str>,
}

/// A tiny prebuilt HTTP app for the public-deploy scenario. Differs per backend:
/// Kubernetes enforces `runAsNonRoot`, so it needs a non-root image bound to a
/// high port, whereas Docker can run `traefik/whoami` on :80.
pub struct SampleApp {
    pub image: &'static str,
    pub http_port: &'static str,
    /// A stable substring expected in the response body, if any.
    pub body_marker: Option<&'static str>,
}

pub trait Backend {
    fn kind(&self) -> BackendKind;
    fn name(&self) -> &'static str {
        self.kind().as_str()
    }

    /// Stand up (or connect to) the stack and make the CLI usable.
    fn bring_up(&mut self) -> Result<()>;
    /// Best-effort cleanup; safe to call more than once.
    fn tear_down(&mut self);

    /// Run `rise <args>`. With `auth = None`, uses the admin CI bearer.
    fn rise_cli(&self, args: &[&str], auth: Option<CliAuth<'_>>) -> Result<CliOutput>;

    /// Like [`Backend::rise_cli`] but for `deploy --backend docker:build` (source
    /// builds), which need access to a docker daemon. Defaults to `rise_cli`
    /// (the docker backend runs the CLI on the host, which already has docker).
    fn rise_cli_build(&self, args: &[&str], auth: Option<CliAuth<'_>>) -> Result<CliOutput> {
        self.rise_cli(args, auth)
    }

    /// GET an app path through this backend's ingress. `Ok(None)` means app-HTTP
    /// reach isn't wired for this backend yet — a *declared* gap the scenario
    /// logs, never silent drift.
    fn reach_app(&self, project: &str, path: &str) -> Result<Option<HttpResponse>>;

    /// The test Dex reachable from the harness, when this backend exposes one.
    fn dex(&self) -> Option<&DexEndpoint>;

    /// A small HTTP app to deploy for the public-deploy scenario.
    fn sample_app(&self) -> SampleApp;

    /// Whether this backend can build & deploy an app from source
    /// (`deploy --backend docker:build`) — i.e. it has a registry the runtime can
    /// pull from. Only minikube's jfrog-vault mode does, in the harness.
    fn supports_source_build(&self) -> bool {
        false
    }

    /// Base URL of the Rise control-plane API as reachable from the harness host.
    fn api_base(&self) -> &str;
    /// The admin CI bearer used for authenticated API calls.
    fn ci_bearer(&self) -> &str;

    /// Authenticated GET against the Rise API (`path` starts with `/`).
    fn api_get(&self, path: &str) -> Result<HttpResponse> {
        crate::http::get_auth(&format!("{}{}", self.api_base(), path), self.ci_bearer())
    }

    /// Poll `rise deployment list` until the project reports a Healthy
    /// deployment. Shared across backends (reuses `rise_cli`).
    fn wait_healthy(&self, project: &str) -> Result<()> {
        crate::http::poll(
            Duration::from_secs(300),
            Duration::from_secs(5),
            &format!("project '{project}' to report Healthy"),
            || {
                let out = self.rise_cli(
                    &["deployment", "list", "--project", project, "--limit", "5"],
                    None,
                )?;
                Ok(out.combined().contains("Healthy"))
            },
        )
    }

    /// The base URL of this backend's ingress (Docker/Traefik), for raw
    /// host-routed requests. `None` if the backend has no shared HTTP ingress.
    fn traefik_base(&self) -> Option<&str> {
        None
    }

    /// JSON of the app container's labels (Docker/Traefik label assertions).
    fn app_container_labels(&self, _project: &str) -> Result<String> {
        anyhow::bail!(
            "container-label inspection is not supported by the {} backend",
            self.name()
        )
    }

    /// GET the Traefik API (e.g. `/api/http/services/...`). Docker only.
    fn traefik_api(&self, _path: &str) -> Result<HttpResponse> {
        anyhow::bail!(
            "the Traefik API is not available on the {} backend",
            self.name()
        )
    }

    /// A single GET through the ingress with a `Host` header, NO retry (so a 5xx
    /// during a cutover is observed, not polled away). Docker only.
    fn ingress_get_once(&self, _project: &str, _path: &str) -> Result<HttpResponse> {
        anyhow::bail!(
            "ingress_get_once is not supported by the {} backend",
            self.name()
        )
    }

    /// Each running app container's `Config.Env` as a JSON-array string — to
    /// assert which revision is live after a cutover. Docker only.
    fn app_container_envs(&self, _project: &str) -> Result<Vec<String>> {
        anyhow::bail!(
            "app_container_envs is not supported by the {} backend",
            self.name()
        )
    }

    /// Backend-specific preparation before the workload-identity scenario (e.g.
    /// Docker recreates the backend with the identity compose overlay). No-op by default.
    fn prepare_workload_identity(&self) -> Result<()> {
        Ok(())
    }

    /// Re-apply the infrastructure (e.g. re-run `helm upgrade`) to assert it's
    /// idempotent. Only meaningful for backends that provision via a chart.
    fn reapply_chart(&self) -> Result<()> {
        anyhow::bail!(
            "chart reapply is not supported by the {} backend",
            self.name()
        )
    }

    /// Wait until a stopped deployment's workload is actually gone. The default
    /// polls `rise deployment list` until it no longer reports Healthy; backends
    /// can override with a stronger check (e.g. zero pods).
    fn wait_workload_removed(&self, project: &str) -> Result<()> {
        crate::http::poll(
            Duration::from_secs(120),
            Duration::from_secs(2),
            &format!("workload for '{project}' to be removed"),
            || {
                let out = self.rise_cli(
                    &["deployment", "list", "--project", project, "--limit", "5"],
                    None,
                )?;
                Ok(!out.combined().contains("Healthy"))
            },
        )
    }
}

/// Construct the driver for the selected backend (does not bring it up).
pub fn create(kind: BackendKind) -> Result<Box<dyn Backend>> {
    Ok(match kind {
        BackendKind::Docker => Box::new(docker::DockerBackend::new()?),
        BackendKind::Minikube => Box::new(minikube::MinikubeBackend::new()?),
    })
}
