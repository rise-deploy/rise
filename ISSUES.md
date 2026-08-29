# ECS deployment backend — review findings

Working scratchpad from a six-lane review panel over the ECS backend
(`crates/rise-backend-ecs`, the shared `rise-backend-core`/`rise-backend-traefik`
machinery it uses, `modules/rise-aws` + `modules/rise-ecs`, the `tests/e2e`
driver, and the CI workflow) on branch `claude/ecs-deployment-backend-5xdgqa`.

**Status:** A–E, G, H, I, J, L, O, Q, R, S fixed (see the ✅ notes). CloudWatch
log backend split out as a Follow-up (still unimplemented — no `CloudWatch`
variant in `src/server/deployment/logs.rs` as of 2026-08-27). Remaining open:
F, K, M, P. Re-verified against current `develop` (post #460, #468): task #33
(lifecycle duplication) and #35 (project rename) from "Excluded as known" are
now fixed; the Docker/K8s plaintext-env-hash item is half-fixed (K8s only —
see that entry). #468 closed the reconcile/status state-machine trio (O, Q,
R) as one PR.

This is a triage list, not a deliverable. Every item below
was verified against the code, not inferred from names. Severity is this
reviewer's grading, sometimes lower than the finding lane's.

Legend — **Scope**: `ECS` = owned by this work; `harness` = e2e/CI only;
`infra` = Terraform/IAM; `shared` = pre-existing, cross-backend (Docker/K8s
carry the same defect), flagged for completeness, not a regression here.

---

## Verified clean (no action)

- **Ingress routing / access-class enforcement** — fail-closed at every layer.
  Users cannot control `dockerLabels` (`exposedByDefault=false`, labels are
  server-assembled), backtick injection into Traefik rules is blocked by
  upstream charset validation on paths/domains/names, forwardAuth `access`/
  `project` params are server-stamped, the unqualified `{r}-auth` middleware
  reference is correct for the `@ecs` provider, and readiness fails closed via
  the same `routes_withheld`. No path to expose a private project, hijack a
  host/path, inject Traefik config, or bypass forwardAuth.
- **Secret code-path** — no plaintext reaches the task definition `environment`,
  `dockerLabels`, log config, or `content_hash`; none reaches logs, error
  messages, the `env-hash` tag, controller metadata, or the Pods tab. No SSM
  path traversal or cross-project collision (segments can't escape their
  subtree; `/`-only separator is excluded from every component except the depth
  issue in item D). `secret_fingerprint` is unpredictable without the encryption
  key for both AES-GCM and KMS providers.
- **Server wiring & config validation** — every `config/ecs.yaml` key has a
  matching settings field; account/role/registry coupling validated at startup;
  `access_classes_missing_auth_backend_url` fail-closed for ECS; cpu arch
  canonicalized-or-rejected at load.
- **CI triggers / concurrency / image-gate** — `labeled`/`synchronize` logic,
  the per-scope concurrency group, and the image-gate poll are all correct.
- **Fargate size table** matches AWS exactly; **`content_hash`** framing is
  length-prefixed and collision-safe; **leadership** is confirmed before every
  ECS mutation; steady-state API budget is as documented.

---

## Fix-worthy — ECS-specific, owned by this work

### ✅ A. Secret parameters leak on every retirement path except supersession — Med — Scope: ECS
> **FIXED.** `delete_secrets_for` now also runs from the diff's `Delete` action
> (via new `project`/`deployment_group` fields on `ActualService`), after a
> successful `delete_service`, best-effort. Refactored to take the path
> components rather than a synthetic `ServiceTags`.
`delete_service` (`crates/rise-backend-ecs/src/reconciler.rs`) scales to zero and
force-deletes; it never touches SSM. `delete_secrets_for` has only two call
sites: the orphan sweep (~:564) and the superseded-predecessor path (~:2233).
Every other retirement — user stop (`Terminating`→`Stopped`), expiry,
cancellation, the Deploying-timeout/permanent-rejection `Failed` paths, and the
straggler siblings retired at ~:2256 — deletes the service through the diff's
`Delete` action and leaves the `SecureString` parameters behind. They are
unreachable by any future GC (the project stays reconciled/`sweep_exempt`, and
no service remains for the sweep to revisit), so live secret plaintext lingers
until full project deletion — a hygiene issue and slow erosion of the
10k-parameter/region quota.
**Fix:** call `delete_secrets_for` from the diff's `Delete` action (tags are on
`ManagedService`), or from `complete_termination`.
**Verified:** `delete_service` body + the two call sites.

### ✅ B. Rollout-hold flaps Healthy→Unhealthy in the terminal phase of an in-place roll — Med — Scope: ECS
> **FIXED.** `describe_service_tasks` now lists both `desiredStatus=RUNNING`
> and `=STOPPED` tasks and merges them, so a draining-but-serving outgoing
> task is visible to the hold. The `lastStatus == RUNNING` check keeps
> `still_serving` accurate.
`describe_service_tasks` (`reconciler.rs`, ~:2048) calls `list_tasks()` with only
cluster + service, so it defaults to `desiredStatus=RUNNING`. When ECS (no LB, no
container health check) reaches the new task's RUNNING state it immediately flips
the old task's `desiredStatus` to STOPPED — at which point the still-serving,
still-draining old task drops out of the list. During the window before Traefik's
ECS provider (~15s poll) reports the new task UP, `current` is not-ready and
`outgoing` is empty, so `hold_status_during_rollout` sees `still_serving=false`,
declines the hold, and marks a routine, correctly-progressing roll (e.g. adding a
custom domain) Unhealthy, then flaps back next tick. This is a gap in the hold
added in commit `90132da`.
**Fix:** include draining tasks in the read (list both `RUNNING` and `STOPPED`
desired-status tasks); the existing `lastStatus == RUNNING` check keeps
`still_serving` accurate for a task that is draining vs. already gone. (Better
than trusting the PRIMARY-rollout timer alone, which would mask a genuinely
failing roll.)
**Verified:** `list_tasks()` builder has no `desired_status`; the partition +
hold at ~:1951 / ~:2020.

### ✅ C. Persistent `TagResource` failure → perpetual re-roll, and the hold never expires — Med — Scope: ECS
> **FIXED (option 2).** The converged task-definition hash is now persisted in
> the deployment's `controller_metadata` (no new column) in the same DB write
> that records health. The diff treats a service as converged if the tag **or**
> the persisted hash matches desired, and the `TagResource` write is downgraded
> to best-effort — so a role missing `ecs:TagResource` no longer drives an
> unbounded re-roll. Note on the "reads Healthy while churning" line: with fix B
> the hold only holds because an outgoing task is genuinely serving, so traffic
> does flow throughout; the defect was the silent re-roll, not a false Healthy.
> A distinct "reconciling/progressing" status is deferred as a future nicety.
The convergence marker (`task-definition-hash` tag) is written in a separate,
failable call *after* the `UpdateService` it records (`update_service`, ~:1745).
If tagging fails persistently — realistic case: the controller role missing
`ecs:TagResource` — each tick: diff sees the stale tag → `UpdateTaskDefinition` →
`prepare_task_definition` rewrites all SSM params and registers a new (identical)
revision, burning the 1/s budget → `UpdateService` starts a *new* ECS rolling
deployment. Fargate tasks start and SIGTERM-kill in a permanent loop. Worse:
each roll resets the PRIMARY deployment's `createdAt`, so `rollout_started_at`
stays ~one tick old and the hold's expiry window never elapses — the deployment
reads Healthy forever while churning, with only an error-level log. The shipped
Terraform grants `ecs:TagResource`, so the trigger is a hand-rolled/mis-scoped
role, but the failure mode is silent live-service degradation.
**Fix:** write the hash tag *before* `UpdateService` (record intent, then
mutate), or persist the hash on the DB row where it can't fail.
**Verified:** tag-after-update ordering; `primary_rollout_start` reset each roll;
hold expiry keyed on it.

### ✅ D. SSM path built from the raw `deployment_group` → breaks AWS's 15-level hierarchy cap — Med — Scope: ECS
> **FIXED.** `ssm::parameter_name` and `deployment_path_prefix` now escape the
> group with `normalize_deployment_group` (parity with K8s/Docker), collapsing
> `/` to a single segment. Added a `MAX_KEY_SEGMENT_CHARS` bound so the composed
> name cannot exceed SSM's 2048-char limit.
`ssm::parameter_name` composes `/{prefix}/{project}/{group}/{deployment_id}/{KEY}`
from `deployment.deployment_group` passed **raw** (`reconciler.rs:1124`). The DB
stores the group raw, and `is_valid_group_name` legally permits `/` inside it
(regex `^[a-z0-9][a-z0-9/-]*[a-z0-9]$`, up to 100 raw / 63 normalized chars).
K8s and Docker normalize the group for every naming use (`escaped_group_name`);
ECS does not. A group like `a/b/c/.../t` (11+ slash segments — a plausible CI
convention) pushes the composed name past AWS SSM's 15-level cap, so every
secret `PutParameter` for that deployment fails with an opaque
`ValidationException` at reconcile time — exactly the failure class this crate
otherwise catches locally. Related: no check on the composed name length (2048)
or the env-var key length anywhere.
**Fix:** normalize/escape the group on the ECS path for parity, or validate the
composed name's length + depth in `ssm::validate`.
**Verified:** raw pass-through at :1124; `normalize_deployment_group` applied
only at use sites; K8s `escaped_group_name` at `resource_builder.rs:224+`.

### ✅ E. Fargate CPU sizing truncates an oversized request to a small valid size — Med — Scope: ECS
> **FIXED.** The two `as u32` casts are now `u32::try_from(...)` that bail with
> the "exceeds the largest Fargate task size" error on overflow.
`sizing.rs` `let want_cpu_units = (want_millicores * 1024).div_ceil(1000) as u32;`
truncates a `u64` (from unbounded `parse_cpu_millicores`) rather than
saturating/erroring. A CPU request whose true `want_cpu_units` exceeds 2^32 wraps
to a small in-table value and returns `Ok(FargateSize { rounded_up: false })` —
silently under-provisioning a request that should be rejected, with no warning
(the reconciler gates its warn on `rounded_up`). Violates the module's stated
"never receive less than asked" contract. Reachable only past the operator's
`max_cpu` validation, but this module is documented as the last line.
(`want_bytes ... as u32` for memory has the same shape.)
**Fix:** `u32::try_from(...)` and bail on overflow.
**Verified:** `want_millicores: u64`; `resolve()` has no self-contained bound
before the cast.

### F. Image ref can exceed AWS's 256-char tag-value limit — Low — Scope: ECS
`tags.rs` `render()` inserts `self.image` verbatim as a tag value. A
digest-pinned ref on a long registry path (nested GitLab groups, JFrog, a
user-supplied `--image`) can exceed 256 chars → `TagResource`/`CreateService`
fails at reconcile. No test covers per-value length (only the 50-tag count).
**Fix:** length-guard/skip the image tag value.
**Verified plausible:** verbatim insert; no length check on the path.

---

## Fix-worthy — e2e harness (scratch account only, but real)

### ✅ G. `reap_dead_scopes` never reaps a dead scope's ECS workload services — Med — Scope: harness
> **FIXED.** New `all_managed_workloads` lists every managed workload service
> with its owning scope; `reap_dead_scopes` now deletes those whose scope is
> dead, and `workload_services`/`sweep` filter the same list to the current
> scope. Reap summary counts services too.
`tests/e2e/src/backend/ecs.rs` `reap_dead_scopes` (~:681) collects a dead scope's
orphaned ECR repositories and SSM parameters but **not** its Rise-created ECS
workload services — even though its own doc comment claims "repositories, secrets
and workloads." The GitHub Actions `if: always()` backstop runs `terraform
destroy` directly on runner death, which tears down only the Terraform-managed
stack (Traefik/Dex/Postgres/Rise); the scope's deployed sample-app services are
Rise's own resources and are left running, pinning Fargate tasks/ENIs
indefinitely. Gap in task #32.
**Fix:** enumerate + delete the dead scope's workload services (the same
`list-services` output already drives control-plane detection; classify workload
services by controller-class tag or name and delete them).
**Verified:** function body collects repos + params only; `sweep()` (self-scope
workloads) never runs against another scope.

### ✅ H. DNS A-record leaked when `bring_up` fails after the UPSERT — Low — Scope: harness
> **FIXED.** `self.stack` (which carries `traefik_ip`) is now recorded before
> the DNS UPSERT/verify steps, so a bring-up that fails on them still lets
> `tear_down` delete the record.
`bring_up` UPSERTs `<scope>.<zone>` + `*.<scope>.<zone>` (~:1132) before setting
`self.stack` (~:1145). If any step in between fails (`verify_dns_visible`,
propagation timeout), `bring_up` returns `Err` with `self.stack == None`, so
`tear_down`'s DNS delete (guarded on `self.stack.is_some()`, ~:1221) is skipped —
the record dangles at a dead/reassignable IP until a later same-scope run
UPSERTs over it.
**Fix:** capture `traefik_ip` (or set `self.stack`) before/independent of the DNS
+ verify steps.

### ✅ I. `tear_down` panics and skips all cleanup if `bring_up` fails on its first line — Low — Scope: harness
> **FIXED.** `tear_down` returns early when `env` is unset (nothing was
> created) and gates the API-based steps (`delete_projects`,
> `wait_workloads_removed`) on `stack` being up; sweep + destroy + DNS-delete
> still run when only `env` is present.
`bring_up` sets `self.env` (~:1074) after `read_bootstrap_env()?` (~:1069). If
that first call fails, `self.env == None`; `main.rs` then calls `tear_down`
(outside the `catch_unwind`), which reaches `workload_services()` →
`self.env().expect(...)` and panics — unwinding out of `main` and skipping
`sweep()`, `terraform destroy`, and the DNS delete. Low impact (nothing new was
created) but any pre-existing stale state for that scope also goes uncleaned, and
it's an ugly panic instead of the graceful `ExitCode::FAILURE` path.
**Fix:** set `self.env` before the first fallible call, or make `tear_down`
tolerate `env == None`.

---

## Infra / IAM hardening (production)

### ✅ J. SecureStrings readable by any broad-SSM-read principal without a customer CMK — Med — Scope: infra
> **FIXED (docs).** ECS operator docs now carry a caution: set a
> customer-managed `ssm_kms_key_id` on multi-tenant installs, or broad SSM-read
> principals can decrypt every project's secrets under `alias/aws/ssm`.
`put_secrets` sets `key_id` only when `ssm_kms_key_id` is `Some`
(`reconciler.rs:1451`); `modules/rise-aws` makes `ssm_kms_key_arn` optional
(null → AWS-managed `alias/aws/ssm`). Under the AWS-managed key, decryption is
authorized for any account principal permitted to call SSM (no explicit key
grant required, unlike a CMK), so a `ReadOnlyAccess`-class principal can
`get-parameters --with-decryption` and read plaintext. With a customer CMK the
`kms:ViaService` + resource-scoped grant gates it properly.
**Fix:** require a customer-managed `ssm_kms_key_id` for multi-tenant installs,
or document the requirement prominently.

### K. Backend/controller role trusts account-root superfluously on ECS installs — Low — Scope: infra
`modules/rise-aws/main.tf:495-502` unconditionally trusts
`arn:aws:iam::<account>:root`. On ECS with `create_iam_user = false`, the
`ecs-tasks.amazonaws.com` service principal is what hands the role to the task;
the root trust is unnecessary and lets any account principal with
`sts:AssumeRole` assume the full control-plane role. Low risk in the intended
dedicated account.
**Fix:** gate the root-trust statement off when a service/IRSA trust is present
and no IAM user is created, or document "ECS installs run in a dedicated
account."
**Verified:** statement is unconditional; service-principal statement is the
`dynamic` block below it.

### ✅ L. `oidc_client_secret` silently defaults to a repo-published constant — Low — Scope: infra
> **FIXED.** `modules/rise-ecs` now has a precondition requiring
> `oidc_client_secret` when `deploy_dex = false`, instead of coalescing to the
> published `rise-backend-secret` default.
`modules/rise-ecs/secrets.tf:76` + `dex.tf:14`: `coalesce(var.oidc_client_secret,
"rise-backend-secret")`, default `null`. Intentional for the Dex demo; for a
real-IdP install where the operator forgets it, the module writes a well-known
constant into Secrets Manager as the client secret instead of failing.
**Fix:** require `oidc_client_secret` (or add a precondition) when
`deploy_dex = false`.

### M. OIDC trust subject wildcard is broader than best practice — Low (downgraded) — Scope: infra/harness
`tests/e2e/bootstrap/oidc.tf` trusts `repo:${var.github_repository}:*`. The IAM
lane graded this High on a fork-PR credential-theft theory; **downgraded** —
GitHub gives fork `pull_request` runs a read-only token and no `id-token`, so the
wildcard is not mintable from a fork today, and the workflow's `if:` head-repo
guard + GitHub's fork-approval gate stand in front regardless. Worth tightening
(exclude `:pull_request`, or pin to `:ref:`/`:environment:`) as defense-in-depth
so it never depends on that GitHub behaviour. Scratch account only.

### ✅ N. One shared CloudWatch log group across all projects — Low — Scope: infra
> **ADDRESSED (interim doc) → proper fix deferred.** The real resolution is a
> native CloudWatch log backend (see the Follow-up section) so users read logs
> through Rise's authz and never need direct CloudWatch access. Until then the
> ECS docs note that the single install-wide log group is not a per-project
> boundary. Rise never logs secret values, so this is app-output visibility.
All projects' container stdout/stderr land in one log group, separated only by
stream prefix (`task_definition.rs:270`). App-controlled output, so an app that
logs its own secrets makes them cross-project visible to a logs-read principal.
**Fix:** operator note; optionally per-project log groups.

---

## Pre-existing / cross-backend (not introduced here)

### ✅ O. Supersession is non-atomic → two active deployments serving mixed versions — High (shared) — Scope: shared
> **FIXED.** `mark_deployment_healthy_and_supersede` (new `DeploymentStore`
> method, `src/db/deployments.rs`) marks the new deployment `Healthy` and the
> group's previous active deployment `Terminating(Superseded)` in one Postgres
> transaction, modeled on the existing `mark_as_active` precedent — a
> crash/restart/write-error between the two writes can no longer happen, since
> there's only one write. `handle_deployment_became_healthy`
> (`rise_backend_core::lifecycle`, shared by K8s/Docker/ECS) now calls this
> instead of the old two-step `mark_deployment_healthy` +
> `mark_deployment_terminating` sequence; the reconvergence loop for stragglers
> (added in #460) is unchanged and stays as a secondary self-healing pass. The
> `#460` note below is superseded by this fix.
`handle_deployment_became_healthy` (`reconciler.rs:2200-2274`):
`mark_deployment_healthy(new)` commits, then `mark_deployment_terminating(old)`.
A crash/restart/write-error between them (caught as a warn at ~:730) leaves the
new deployment `Healthy` — after which the only arm that calls this function
(`Deploying if all_ready`) can never fire again, no other loop looks for "two
non-terminal deployments in one group," and the predecessor stays
`Healthy`/`is_active` indefinitely. Because Traefik router/service names are
deployment-id-free, both deployments' tasks share one Traefik service and the
one-tick overlap becomes permanent load-balancing across app versions until the
*next* deploy's straggler loop retires both. Same defect in the Docker reconciler
and the K8s webhook it was ported from.
**Fix:** a per-tick invariant — "one group, >1 non-terminal, one Healthy →
supersede the older" — makes it self-healing.
**Verified:** the two-write sequence; no reconvergence arm.

### P. Migrated-project sweep deletes the successor controller's live SSM params — Med (shared/future) — Scope: shared
Two ECS controllers in one account/region sharing the default
`ssm_parameter_prefix` write and delete identical parameter names for the same
deployment row (the path has nothing controller-specific). On a controller-class
migration, controller B creates its service and writes the params; controller A's
sweep then `delete_secrets_for`s the migrated project by that same prefix,
removing the params B's task definition references. B never rewrites them (its
tag hash matches desired, so no Create/Update runs), and the deployment fails on
the next task restart with "unable to pull secrets." Tied to the not-yet-shipped
multi-controller workstream.
**Fix:** fold the controller class into the SSM path, or skip `delete_secrets_for`
on the `Migrated` branch (safe to delete only when the deployment row is
terminal/absent).

### ✅ Q. Transient `ListTasks`/`DescribeTasks` error masquerades as "no tasks" → one-tick Unhealthy flap — Low-Med (shared) — Scope: shared
> **FIXED.** `describe_service_tasks` now returns `Result<Vec<TaskView>>`:
> any `ListTasks`/`DescribeTasks` error along the way (tracked via `had_error`,
> now logged at `warn!` instead of `debug!`) collapses the whole pass to `Err`
> via the extracted `tasks_or_indeterminate` helper, rather than returning
> whatever partial/empty data was collected. `reconcile_health` propagates the
> error with `?` before touching `all_ready`/`pods`/status, so an
> indeterminate tick leaves the deployment's status untouched instead of
> flapping it to Unhealthy.
`reconciler.rs:~2121`: both errors `debug!` and collapse to an empty list, so a
routine `ThrottlingException` (these are per-deployment, per-tick calls against a
throttled cluster-read budget) flips a healthy deployment to Unhealthy for a tick
(status churn, error message overwritten, project status recomputed).
**Fix:** protect the current status on an API error (skip the transition), the
way compute-desired errors protect services from GC.

### ✅ R. `mark_*` store functions skip transition validation → status races — Low (shared) — Scope: shared
> **FIXED.** Every `mark_*` function in `src/db/deployments.rs`
> (`mark_healthy`, `mark_unhealthy`, `mark_terminating`, `mark_failed`,
> `mark_cancelled`, `mark_stopped`, `mark_superseded`, `mark_expired`,
> `mark_cancelling`) now guards its `UPDATE` with a new
> `is_valid_transition(status, '<target>')` SQL function (mirroring
> `state_machine::is_valid_transition`, kept in sync by an exhaustive
> build-time parity test) and returns `Option<Deployment>` — `None` when the
> guard rejects a stale write. `update_status` gained the same DB-side guard
> alongside its existing Rust-side check. Reconcile-tick callers treat `None`
> as a benign no-op; the HTTP stop endpoints surface it as a 409 Conflict
> instead of silently losing the user's request.
`src/db/deployments.rs:709-825`: `mark_healthy`/`mark_unhealthy`/`mark_terminating`
are unconditional `UPDATE ... SET status` with no `WHERE status IN (...)` and no
`validate_transition`. The health pass can overwrite a user's concurrent stop
(`Terminating` → `Healthy`, an invalid transition), silently losing it; and a
leadership-lost replica's step-4 (which runs no `confirm_leadership`, unlike
apply/sweep) can write stale statuses. Shared with Docker.
**Fix:** status-guarded `UPDATE ... WHERE status = $expected`.

### ✅ S. Out-of-band `desiredCount` drift flaps Unhealthy for a tick — Low (shared) — Scope: shared
> **FIXED.** The health pass now sizes the expected replica count from
> `clamp_replicas(spec.replicas)` (what Rise wants) rather than the observed
> `desiredCount`, so a console scale-up no longer flaps the deployment.
`reconciler.rs:~1992` uses the snapshot's `desired_count` as the expected replica
count (Docker uses the spec's). A console scale-up makes the intervening tick
judge expected against tasks that legitimately don't exist yet → Unhealthy flap,
no outage. Narrow (deployment `replicas` is immutable after insert).
**Fix:** `clamp_replicas(spec.replicas)` like Docker, or `min(snapshot, desired)`.

---

## Follow-up — after this PR

### CloudWatch runtime-log backend — ECS log parity + resolves N
ECS runs on the `None` log backend (`src/server/deployment/logs.rs`), so ECS
deployments surface **no** logs in the Rise UI/API — a parity gap with
Kubernetes/Docker/Loki. Add a `DeploymentLogsSettings::CloudWatch` variant and a
`CloudWatchLogBackend` implementing `RuntimeLogBackend::stream_logs` (FilterLogEvents
against the install log group, scoped by the `{project}-{deployment_group}/` stream
prefix — trailing slash matters, it disambiguates `proj-a`+`b` from `proj`+`a-b`)
and `query_volume` (likely `supports_volume=false`, as Kubernetes). Plus config
wiring in `config/ecs.yaml`, the `logs:FilterLogEvents`/`GetLogEvents` grant, a
feature-matrix row, and docs. This dissolves **N**: logs are served through Rise's
own project authz, so operators never delegate CloudWatch access.

## Excluded as known / accepted (briefed out of the review)

- Retirement routing window — needs a workload-side drain (task #31).
- ✅ ~~~400 lines of lifecycle logic duplicated from the Docker reconciler
  (task #33)~~ — **FIXED** in #460: `perform_status_transition`,
  `complete_termination`, and `handle_deployment_became_healthy` now live once
  in `rise_backend_core::lifecycle`, shared by K8s/Docker/ECS.
- ✅ ~~Project rename duplicates workloads until the old deployment goes
  terminal (task #35)~~ — **FIXED** in #460 for Docker/ECS: workloads now carry
  the project's immutable UUID and reconcilers match on it first. Kubernetes is
  a documented different trade-off, not silently broken (see #460's summary):
  a rename still orphans the name-keyed `RiseProject` CRD, but the existing
  "project not found" path deletes it and lets owner-reference GC tear down
  cleanly — full teardown+recreate rather than a harmless leftover duplicate.
- `ecs_task_role_arn` defaults to the controller role — deployed workloads
  inherit the controller's account-wide `ssm:*`/`ecs:*`/`iam:PassRole` (filed
  separately; the single highest-impact cross-tenant exposure, but out of scope
  here by prior decision). Still unimplemented as of 2026-08-27
  (`modules/rise-ecs/variables.tf:38` still documents no per-project task roles).
- Docker/Kubernetes hash secret plaintext into their env-hash label/annotation
  (deferred by the user). **Partially fixed in #460**: Kubernetes' pod
  annotation hash now goes through `rise_backend_core::env::hash_env` over
  secret *fingerprints* (`src/server/deployment/webhook.rs`), same approach ECS
  already used (`redact_secrets_for_hash` at `reconciler.rs:1034`). **Docker is
  still open** — `crates/rise-backend-docker/src/reconciler.rs:847` hashes the
  full merged env including plaintext secret values, with no redaction step.
