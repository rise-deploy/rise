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
mod ecs;
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

    /// Absolute path to `rel` under the repo root, as the CLI sees it. Both
    /// backends run the CLI with the repo root as cwd — the Minikube harness runs
    /// it in a container that bind-mounts the repo root at the same path — so a
    /// path under the repo root is readable by the CLI on either backend, unlike
    /// `/tmp` (which the Minikube CLI container does not mount). Use this for
    /// dir-based deploys (a generated `rise.toml`) that must run on both backends.
    fn cli_visible_path(&self, rel: &str) -> String;

    /// GET an app path through this backend's ingress. `Ok(None)` means app-HTTP
    /// reach isn't wired for this backend yet — a *declared* gap the scenario
    /// logs, never silent drift.
    fn reach_app(&self, project: &str, path: &str) -> Result<Option<HttpResponse>>;

    /// Reach the app repeatedly through the ingress, holding ONE route open for the
    /// whole call (no per-sample port-forward churn), invoking `check(body)` on each
    /// 200 every `interval` until it passes or `duration` elapses. Returns whether
    /// it passed. Used for the workload-identity jti-refresh observation.
    fn poll_app(
        &self,
        project: &str,
        path: &str,
        duration: Duration,
        interval: Duration,
        check: &mut dyn FnMut(&str) -> bool,
    ) -> Result<bool>;

    /// The test Dex reachable from the harness, when this backend exposes one.
    fn dex(&self) -> Option<&DexEndpoint>;

    /// A small HTTP app to deploy for the public-deploy scenario.
    fn sample_app(&self) -> SampleApp;

    /// Block until the registry has a repository ready for `project`, for
    /// backends whose registry provisions them asynchronously.
    ///
    /// Default: nothing to wait for. ECS with an ECR registry overrides it —
    /// repositories are created by a leader-elected controller on a 10-second
    /// poll, not at project-create time, so a create-then-deploy inside that
    /// window pushes against a repository that does not exist yet.
    fn wait_registry_ready(&self, project: &str) -> Result<()> {
        let _ = project;
        Ok(())
    }

    /// The image reference the runtime was actually told to run for `project`.
    ///
    /// `Ok(None)` means the backend does not expose it — a declared gap, never a
    /// silent pass. ECS reads it back off the running service's task definition,
    /// which is the only evidence that the pull went through the configured
    /// registry rather than somewhere else.
    fn deployed_image(&self, project: &str) -> Result<Option<String>> {
        let _ = project;
        Ok(None)
    }

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

    /// Poll the deployments API until the project's latest deployment reports
    /// `status == "Healthy"`. Uses the machine-readable API (not the CLI table) so
    /// the oracle is a typed status check, not a substring match.
    fn wait_healthy(&self, project: &str) -> Result<()> {
        crate::http::poll(
            Duration::from_secs(300),
            Duration::from_secs(5),
            &format!("project '{project}' to report a Healthy deployment"),
            || Ok(latest_deployment_status(self, project)?.as_deref() == Some("Healthy")),
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

    /// GET the Traefik API (e.g. `/api/http/services/...`). Traefik-fronted
    /// backends only (Docker, ECS).
    fn traefik_api(&self, _path: &str) -> Result<HttpResponse> {
        anyhow::bail!(
            "the Traefik API is not available on the {} backend",
            self.name()
        )
    }

    /// Traefik provider name a service is registered under, for the `@<provider>`
    /// suffix the API keys services by. `docker` for the Docker daemon provider,
    /// `ecs` for the ECS provider.
    fn traefik_provider(&self) -> &'static str {
        "docker"
    }

    /// The app's ingress hostname for this backend (Traefik host on docker, the
    /// chart's ingress URL on minikube).
    fn app_host(&self, project: &str) -> String;

    /// One GET through the backend's real HTTP ingress (Traefik on docker, the
    /// nginx ingress controller on minikube), with optional `Cookie` and a toggle
    /// for following redirects. Unlike `reach_app` (which may port-forward straight
    /// to the app Service), this traverses the ingress so ingress-level auth runs.
    fn ingress_get(
        &self,
        project: &str,
        path: &str,
        follow_redirects: bool,
        cookie: Option<&str>,
    ) -> Result<crate::http::Resp>;

    /// Assert the backend wired ingress-level auth for a private (Member) project:
    /// Traefik forwardAuth labels on docker, nginx auth annotations on minikube.
    fn assert_ingress_auth_configured(&self, project: &str) -> Result<()>;

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

    /// Upgrade the running stack in place to the target version (`RISE_IMAGE_TAG`)
    /// and make the CLI usable again, then wait for the control plane to be
    /// healthy. Only meaningful in the upgrade flow, where `bring_up` first stood
    /// the stack up at the older `RISE_E2E_UPGRADE_FROM` version: Docker recreates
    /// the `rise` service on the new image (the DB volume — and its data — is kept,
    /// so the new server runs its migrations against it); Kubernetes `helm
    /// upgrade`s from the old released chart to the in-repo chart on the new image.
    /// Default: unsupported.
    fn upgrade(&mut self) -> Result<()> {
        anyhow::bail!(
            "in-place upgrade is not supported by the {} backend",
            self.name()
        )
    }

    /// Wait until a stopped deployment's workload is actually gone — every pod /
    /// container removed — so a subsequent logs query can only be served by the
    /// log store, not a live workload. Each backend checks its own runtime;
    /// there is deliberately no status-string default, since a not-`Healthy`
    /// deployment status does not prove the workload has been torn down (it may
    /// still be terminating).
    fn wait_workload_removed(&self, project: &str) -> Result<()>;

    /// The `jti` of the workload-identity token the controller has minted for
    /// `filename`, read at the *source* it writes (the K8s Secret on minikube) —
    /// before in-pod mount propagation. Lets a scenario assert the controller
    /// re-mints separately from delivery to the pod. `Ok(None)` when the backend
    /// has no source distinct from the in-pod file (Docker bind-mounts the
    /// controller's file directly), or when it isn't present yet.
    fn minted_token_jti(&self, _project: &str, _filename: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Dump backend diagnostics (cluster / container state + logs) to stderr.
    /// Called on the failure path *before* `tear_down` tears the stack down, so a
    /// bring-up or scenario failure leaves the root cause inline in the run log
    /// (the failing command's own output rarely explains, e.g., why a pod won't
    /// start). Best-effort and silent on success; no-op by default.
    fn dump_diagnostics(&self) {}
}

/// The latest deployment's `status` string for a project, via the deployments API.
/// `Ok(None)` if the call didn't return a usable status (keep polling).
fn latest_deployment_status(b: &(impl Backend + ?Sized), project: &str) -> Result<Option<String>> {
    let resp = b.api_get(&format!("/api/v1/projects/{project}/deployments"))?;
    if resp.status != 200 {
        return Ok(None);
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&resp.body).unwrap_or(serde_json::Value::Null);
    // The API returns deployments ordered created_at DESC, so [0] is the latest.
    Ok(parsed
        .get(0)
        .and_then(|d| d.get("status"))
        .and_then(|s| s.as_str())
        .map(str::to_string))
}

/// Construct the driver for the selected backend (does not bring it up).
pub fn create(kind: BackendKind) -> Result<Box<dyn Backend>> {
    Ok(match kind {
        BackendKind::Docker => Box::new(docker::DockerBackend::new()?),
        BackendKind::Minikube => Box::new(minikube::MinikubeBackend::new()?),
        BackendKind::Ecs => Box::new(ecs::EcsBackend::new()?),
    })
}
