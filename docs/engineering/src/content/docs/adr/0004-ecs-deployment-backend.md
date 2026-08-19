---
title: "ADR-0004: ECS Deployment Backend"
---

## Status

**Draft** (design direction; open questions listed below are not yet closed).
Date: 2026-08-18.

This ADR records how Rise's public feature surface maps onto Amazon ECS and the
surrounding AWS services, so that the implementation PRs argue about code rather
than about topology. It flips to **Proposed** once the open questions in
[Open questions](#open-questions) are answered — several of them are quota and
provider-capability facts that must be verified against AWS, not decided by us.

## Context

Rise deploys container apps through a pluggable **deployment backend**. Two ship
today:

- **Kubernetes** — Deployments/Services/Ingresses, reconciled out-of-process by
  Metacontroller calling Rise's sync webhook.
- **Docker** — one container per container-spec replica on a single host, routed
  by Traefik's Docker provider, reconciled by an in-process leader-elected loop
  (`rise-backend-docker`'s `DockerReconciler`).

The seam between them is `rise-backend-core`: the `DeploymentBackend` trait (URL
computation + environment cleanup for HTTP handlers), the `DeploymentStore`
trait (the only database boundary a controller may use), the provider traits
(`RegistryProvider`, `EncryptionProvider`), and the backend-agnostic runtime
helpers (`resolve_runtime_containers`, `resolve_deployment_env_vars`,
`effective_access_requirement`, `DeploymentUrlBuilder`, the deployment state
machine, quantity parsing). `rise-backend-docker` was extracted onto that seam
first and is the template a third backend follows.

Rise is already AWS-aware in the pieces around the runtime: an ECR
`RegistryProvider`, an AWS KMS `EncryptionProvider`, RDS provisioning as a
project extension, and a `modules/rise-aws` Terraform module for the required
IAM. What is missing is a runtime that does not require the operator to run
Kubernetes or hand-manage a Docker host. ECS on Fargate is the smallest AWS
runtime that can carry Rise's model: it schedules containers, has a first-class
IAM identity per task, native secret injection, and a managed log path.

The hard part is not "start a container on ECS". It is preserving the semantics
Rise's public API already promises — access classes enforced at the edge,
per-route auth, blue/green cutover without a request gap, workload identity
files at a fixed in-container path, cross-container discovery, per-deployment
observability — on a control plane that is API-polled, quota-bounded, and has no
equivalent of a Docker socket.

## Decision

### D1. A new crate, `rise-backend-ecs`, on the existing seam

The backend ships as `crates/rise-backend-ecs`, depending on `rise-backend-core`
and `rise-deployment-spec` only — never on `rise-deploy`. It contributes:

- `EcsBackend: DeploymentBackend` — URL computation via `DeploymentUrlBuilder`
  and a no-op `cleanup_environment` (the reconcile loop GCs on its next tick),
  structurally identical to `DockerBackend`.
- `EcsReconciler` — an in-process reconcile loop, leader-elected through
  `rise-runtime-sync`'s `with_leader_election`, gated on a `controller_class`
  drawn from the project's owning Organization exactly as the Docker reconciler
  gates today.
- A `DeploymentControllerSettings::Ecs` variant in `src/server/settings.rs`,
  selected in `AppState` construction alongside `Kubernetes` and `Docker`.

Rationale: this is the shape the Docker extraction already validated, it keeps
the ECS backend out of `rise-deploy`'s dependency graph, and it is compatible
with the later move to out-of-process controllers — an in-process loop behind
`DeploymentStore` is the same code an external controller runs once the resource
client exists.

**Not decided here:** whether ECS reconciliation eventually moves out of process.
It follows whatever the external-controller work decides for Docker.

### D2. Fargate first; `awsvpc` networking

The backend targets **Fargate** launch type on `awsvpc` networking. EC2 capacity
providers are a later, additive option (a `capacity_provider` setting), not a
day-one variant.

Consequences accepted: per-task ENIs (subnet IP consumption scales with replica
count), task start latency measured in tens of seconds rather than the Docker
backend's seconds, no privileged containers, no host bind mounts, and CPU/memory
restricted to Fargate's discrete size table (see D9).

### D3. Object mapping

| Rise concept | ECS / AWS object | Lifetime |
|---|---|---|
| Install | one ECS **cluster** (`cluster_name` setting) | operator-managed |
| Project | no ECS object — a naming prefix + resource tags | — |
| Deployment × container spec | one ECS **service** + one **task-definition revision** | created on deploy, deleted when the deployment is retired |
| Replica | `desiredCount` on that service | — |
| Container spec | one container definition in the task | — |
| Workload identity delivery | a sidecar container + task-scoped volume | per task |
| Secret env vars | SSM Parameter Store `SecureString` parameters (D7) | per deployment |
| Route / access class | Traefik router + middleware labels in `dockerLabels` (D5) | per deployment |
| Cross-container discovery | AWS Cloud Map service (D10) | per project/group/container |
| Logs | CloudWatch Logs group + stream (D11) | per project |

The critical choice here is **one ECS service per (deployment, container spec)**,
not per (project, group, container) updated in place. It mirrors both existing
backends — K8s creates a Deployment per Rise deployment, Docker creates
containers per Rise deployment — and it is what makes Rise's blue/green model
expressible: the old and new deployments must be able to run simultaneously
while the router drains one into the other, and a rollback must be able to
re-create a prior deployment's exact workload rather than roll a mutable service
back through a task-definition revision.

Cost: service churn per deploy, service-per-deployment pressure on the
per-cluster service quota (soft, raisable — exact figure to confirm, see
[Open questions](#open-questions)), and `DeleteService` draining time on GC.
An in-place service update would avoid all three but would give up overlap,
per-deployment observability, and clean rollback. We take the churn.

### D4. Naming and tagging

Task definition family: `{prefix}-{project}-{group}-{container}`, revision per
deployment. Service name: `{prefix}-{project}-{group}-{deployment_id}-{container}`.
Both truncated with a stable hash suffix when they exceed ECS's 255-character,
`[a-zA-Z0-9_-]` limit — a project name is user-controlled and Rise's DNS-label
constraint is looser than what a concatenation can survive.

Discovery is by **tag**, not by name parsing. Every service and task carries the
same bookkeeping set the Docker backend stamps as labels — `{ns}/managed-by`,
`/controller-class`, `/project`, `/deployment-group`, `/deployment-id`,
`/deployment-uuid`, `/container`, `/environment`, `/env-hash`, `/image`,
`/route-hash` — under the configured `label_namespace` (AWS tag keys permit
`/` and `.`, so `rise.dev/project` is valid verbatim). `propagateTags` carries
the service's tags onto its tasks. The reconciler's drift detection and orphan GC
read these tags, so the diff logic stays conceptually identical to Docker's.

### D5. Ingress: Traefik on ECS, with a load balancer as the edge

**Traefik is the router.** It runs as an ECS service in the same cluster, using
Traefik's **ECS provider** to discover tasks and read routing configuration from
each container definition's `dockerLabels`. The label vocabulary is the same one
`rise-backend-docker`'s `labels.rs` already generates — router rule, entrypoint,
service port, forwardAuth middleware, per-service health check — so the label
builder is shared code, not a second implementation.

**An AWS load balancer terminates the edge.** An ALB (or NLB) fronts the Traefik
service: ACM certificates, AWS-native health checks and access logs, and a
stable public DNS name for the install's wildcard record. Traefik performs no
ACME — its file-based ACME store has no good multi-replica story on Fargate
(see [Alternatives](#alternatives-considered)).

**Access classes keep full parity.** `Authenticated` and `Member` are enforced by
Traefik forwardAuth against the same `/api/v1/auth/ingress` handler both existing
backends use, including the per-route `&access=<req>` the handler enforces. This
is the decisive reason Traefik comes first.

**ALB-native routing is deferred, deliberately.** Routing each container spec to
its own ALB target group is attractive: weighted target groups would give a
genuinely atomic blue/green flip (better than either shipping backend), and ALB
health checks would replace Traefik's. It cannot carry access classes today:

- ALB has no forwardAuth equivalent — no "call this URL, use its status, copy
  its response headers upstream" primitive.
- ALB's `authenticate-oidc` action covers *authentication* only. It could
  conceivably serve `Authenticated` if Rise exposes conformant OIDC discovery,
  but it cannot express `Member` (per-project authorization), and it changes the
  session cookie model out from under `/.rise` and the `rise_jwt` contract.

So ALB-native routing is recorded as a **future, opt-in ingress mode** for
installs that use only the `None` access class, not as the default. Adding it
must not fork the access-class enforcement path: if it lands, projects with a
non-`None` requirement continue to route through Traefik.

### D6. Registry: ECR, pulled by the task execution role

ECR is the recommended and default registry for this backend. The existing ECR
`RegistryProvider` is unchanged — the CLI still pushes with the scoped
credentials the backend mints via the push role.

Pulls need no Rise-managed credential at all: the **task execution role** carries
`ecr:GetAuthorizationToken` / `BatchGetImage` / `GetDownloadUrlForLayer`, so ECS
authenticates the pull itself. `RegistryProvider::requires_pull_secret()` is a
Kubernetes-only concept and is simply not consulted by this backend.

Non-ECR registries remain usable: private ones need a Secrets Manager secret
referenced as `repositoryCredentials` on the container definition. That is
supported but not the recommended path, and an ECS-specific setting names the
secret ARN.

### D7. Secret environment variables: injected by ECS, not flattened into env

Plain env vars go into the container definition's `environment`. **Secret env
vars are written to SSM Parameter Store as `SecureString` parameters** (path
`/{prefix}/{project}/{group}/{deployment_id}/{KEY}`, KMS key configurable) and
referenced from the container definition's `secrets` block; ECS resolves them at
task start using the execution role.

This closes on ECS the gap the Docker backend documents as a limitation: secret
values are not visible in the task definition, and `DescribeTaskDefinition`
returns only the parameter ARN. It matches the Kubernetes backend's per-project
Secret semantically.

Parameter Store (Standard tier) is the default because it is free at Rise's value
sizes; Secrets Manager is a configurable alternative for operators who want its
rotation and cross-account features. Parameters are written per deployment and
deleted when the deployment's services are GC'd, so a rollback never resurrects a
stale value.

The 4 KB Standard-tier parameter value limit and the per-task limit on `secrets`
entries are real bounds — see [Open questions](#open-questions).

### D8. Workload identity: a sidecar writing the same file contract

Rise's workload identity contract is a set of files at fixed in-container paths:

```text
/var/run/secrets/rise/identity/credential
/var/run/secrets/rise/identity/tokens/<audience-filename>
```

Kubernetes mounts a Secret to produce them; Docker uploads them through the
daemon's archive API and re-mints in place. ECS has neither a Secret volume nor a
"write a file into a running container" API, so the files are produced **inside
the task**:

- A task-scoped volume is mounted at `/var/run/secrets/rise/identity` in both
  the app container and a small **identity sidecar** (the Rise image, running an
  identity-agent subcommand), with the app container declaring `dependsOn` the
  sidecar's `HEALTHY`/`START` condition so the files exist before the app runs.
- The sidecar receives the deployment's **bootstrap credential** through the same
  ECS `secrets` mechanism as D7, exchanges it at Rise's token-exchange endpoint
  for the per-audience tokens, writes the files, and re-mints before expiry using
  the shared `refresh_due_after_secs`/`remint_after_secs` helpers.

Consequences: the credential is generated by the control plane (not recovered
from a running container as on Docker, which has no ECS analogue), the token TTL
policy stays in `rise-backend-core`, and the in-container contract a workload
reads is byte-identical across all three backends. Cost: one extra container per
task (memory overhead against the task's Fargate size) and a new
`rise` subcommand that must be tested standalone.

### D9. CPU and memory: rounded up to the Fargate size table

Fargate accepts only discrete CPU values and, per CPU value, a bounded memory
range in fixed increments. A `rise.toml` request of `cpu = "300m", memory =
"700Mi"` has no exact Fargate equivalent.

The rule: parse with `rise-backend-core`'s existing `quantity` helpers, take the
**limit** half of any request-limit range (matching what the Docker backend feeds
`nano_cpus`/`memory`), then **round up to the smallest valid Fargate combination**
that satisfies both. A request that exceeds the largest Fargate size, or that
violates the install's `deployment_constraints`, is rejected at deploy time with
an error naming the nearest valid size — not silently clamped.

Rounding up is billing-visible, so the resolved size is surfaced on the
deployment (and in the CLI's deploy output) rather than hidden.

### D10. Cross-container discovery via Cloud Map

`RISE_CONTAINER_HOST__{CONTAINER}` must resolve to a name that reaches every
replica of a sibling container spec, shared by the old and new deployments during
a cutover (the Docker backend achieves this with one replica-free network alias).

Decision: a private DNS namespace per install; a Cloud Map service per
**(project, group, container)** — deliberately *not* per deployment — registered
by the ECS service via `serviceRegistries`, with `RISE_CONTAINER_HOST__*` pointing
at its DNS name.

This is the decision with the largest unresolved risk: whether two ECS services
(the outgoing and incoming deployments) may register into a single Cloud Map
service is exactly the property the overlap model needs, and it must be verified
before implementation. Fallbacks, in order of preference, are recorded in
[Open questions](#open-questions).

### D11. Logs: CloudWatch by default

App containers use the `awslogs` driver into a log group per project
(`/{prefix}/{project}`), stream prefixed by deployment and container, retention
configurable. A new `DeploymentLogsSettings::CloudWatch` variant implements the
existing `RuntimeLogBackend` seam — tail, follow, level classification (reusing
the Kubernetes backend's regex classifier), search, and the volume histogram over
CloudWatch Logs' `FilterLogEvents`/`StartLiveTail`.

Installs already running Loki keep it: FireLens (Fluent Bit) as the log driver
plus the existing `Loki` logs backend, with the standard `rise_project` /
`rise_deployment_id` stream labels. That path needs no new Rise code.

### D12. Cutover and health: overlap-and-drain, Traefik-authoritative readiness

Cutover works as it does on Docker: the new deployment's services register into
the **same** Traefik router/service as the outgoing one, Traefik load-balances
across both, and the outgoing deployment is retired only once the new servers are
`UP` in Traefik's `serverStatus` (read from the Traefik API, authoritative, no
fallback — the same rule and the same client the Docker backend uses).

That makes ECS a **rolling overlap, not an atomic switch**, the same documented
difference the Docker backend carries against Kubernetes' Service selector flip.
ECS's own `deploymentConfiguration` (`minimumHealthyPercent` /
`maximumPercent`) governs replica-level rolling *within* a service; the
deployment-level cutover is Rise's, not ECS's.

Optionally, a container spec's `health_check` also becomes an ECS container
`healthCheck` (curl-based command) so ECS itself restarts a wedged container.
Traefik's verdict remains the one that gates the Rise state machine, so readiness
semantics do not fork.

`controller_metadata.pod_status` is built from `DescribeTasks` — one task maps to
one "pod", `lastStatus`/`healthStatus`/`stoppedReason` map onto the existing JSON
shape — so the frontend's Pods tab renders unchanged, exactly as the Docker
backend does it.

### D13. Reconcile loop economics: ECS is a throttled, polled API

Unlike a Docker socket, every observation costs a rate-limited API call, and ECS
throttles per-account. The loop therefore:

- discovers Rise-managed services by **tag** (Resource Groups Tagging API) rather
  than enumerating the cluster,
- batches `DescribeServices` (10 services per call) and `DescribeTasks` (100 per
  call), and reuses one describe pass per tick across drift detection, readiness,
  and `pod_status`,
- defaults to a **longer tick than Docker's 5 s** (30 s proposed) with jitter,
- treats `ThrottlingException` as retryable with exponential backoff and never
  as deployment failure,
- keeps `DEPLOYING_TIMEOUT_MINUTES` under review: Fargate task start plus image
  pull can approach the current 5-minute budget for large images, and this
  backend may need its own configurable ceiling rather than the shared constant.

### D14. AWS credentials and Terraform

The backend uses the standard AWS credential chain, so the Rise task's own **task
role** is the production path (no static keys), with explicit key settings for
non-AWS-hosted control planes, matching how the ECR provider is configured today.

`modules/rise-aws` gains an optional ECS section: cluster, execution role,
per-workload task role, the Traefik service's role and security groups, Cloud Map
namespace, SSM parameter path + KMS grants, CloudWatch log group policy, and the
control-plane role statements the reconciler needs (`ecs:*` scoped to the
cluster, `iam:PassRole` scoped to the execution/task roles, `ssm:PutParameter` on
the Rise path, `servicediscovery:*` on the namespace). `iam:PassRole` scoping is
the security-sensitive one: the reconciler must be able to pass only the roles
Rise created, never an arbitrary role ARN, and the task-role ARN must not be
operator-overridable per project without a corresponding policy condition.

### D15. Reference AWS topology (operator guidance, not backend logic)

For completeness, the recommended install shape this backend assumes:

| Concern | Service |
|---|---|
| Rise control plane | ECS service in the same cluster (or anywhere with API reach) |
| Rise database | **RDS for PostgreSQL** (Multi-AZ), private subnets |
| Container registry | **ECR** (D6) |
| Secret encryption | AWS **KMS** provider (already implemented) + SSM for injection (D7) |
| Edge | ALB/NLB + **ACM**, fronting Traefik (D5) |
| App logs | **CloudWatch Logs** (D11) |
| Workload networking | private subnets, NAT or VPC endpoints for ECR/SSM/Logs |

None of this is enforced by the backend — Rise's database configuration is
runtime-agnostic — but it is what the Terraform module and operator docs will
describe.

## Consequences

**Positive**

- A managed AWS runtime with no Kubernetes and no operator-owned Docker host.
- Per-task IAM identity, native secret injection (a strict improvement over the
  Docker backend's flattened env), and a managed log pipeline.
- Access classes, per-route auth, and custom domains keep full parity because the
  edge is the same Traefik + `ingress_auth` pair both other backends use.
- Most of `rise-backend-docker`'s pure logic — Traefik label construction,
  `pod_status` shaping, drift hashing, rolling cutover policy — is reusable, and
  the parts that prove genuinely shared should be promoted into
  `rise-backend-core` rather than copied.

**Negative / accepted**

- Slower feedback: Fargate start latency and a 30 s reconcile tick make a deploy
  visibly less immediate than on Docker.
- Rounded-up CPU/memory is billing-visible (D9).
- Service-per-deployment churn against soft ECS quotas, and slow `DeleteService`
  draining on GC.
- An extra sidecar container per task for identity (D8).
- Traefik on ECS is another operator-run component; the backend is not "ECS only".
- ECS API throttling becomes a real failure mode the reconciler must absorb.

**Operator impact:** new install topology, new IAM, new settings variant, new logs
backend variant. This ADR's implementation requires an
[Upgrade Notes](../upgrade-notes.md) entry in the release it lands in, and the
workstream is tracked on the Rise Rollout Tracker.

## Deployment-backend parity

Per the parity policy in `CLAUDE.md` and
[Deployment Backends](../deployment-backends.md), the explicit call-outs:

- **ECS→existing backends:** nothing in this ADR proposes changing Kubernetes or
  Docker behavior. Where shared logic is promoted into `rise-backend-core`, it
  must be behavior-preserving for both.
- **Fundamental (documented) limitations:** none identified yet. The rolling-vs-
  atomic cutover difference (D12) matches the Docker backend's existing note.
- **Deliberate gaps to track, not accept:** per-group network isolation is
  *expressible* on ECS (security groups per service), unlike on single-host
  Docker — so leaving it out of the first implementation is a parity bug to
  track, not a limitation to document.
- The feature matrix gains an **ECS column in the PR that first makes ECS
  selectable**, not in this ADR — the matrix documents what ships.

## Alternatives considered

**ALB-native routing instead of Traefik.** Rejected as the default: no
forwardAuth equivalent, so `Authenticated`/`Member` access classes cannot be
enforced, and `authenticate-oidc` covers only authentication while replacing
Rise's session model. Retained as a future opt-in mode for `None`-only installs
(D5), where weighted target groups would beat every current backend's cutover.

**Traefik doing ACME on ECS.** Rejected: Traefik's ACME store is a file. Multiple
Traefik tasks need either a single-replica constraint or an EFS mount, both of
which trade availability or complexity for something ACM does natively behind the
load balancer.

**ECS Service Connect instead of Cloud Map** for cross-container discovery.
Attractive (client aliases, built-in retries/telemetry) but it injects an Envoy
sidecar per task, and its behavior when two services advertise the same alias —
precisely the overlap case — is the same unknown as Cloud Map's, with more moving
parts. Cloud Map first; Service Connect reconsidered if D10's fallback is needed.

**Blue/green via CodeDeploy.** ECS's native blue/green requires an ALB with two
target groups and puts the cutover decision in CodeDeploy. That duplicates the
state machine Rise already owns, only works with ALB-native routing, and makes
rollback CodeDeploy's concept rather than Rise's.

**One long-lived ECS service per (project, group, container), updated in place.**
Fewer services, native rolling updates, no quota pressure — but no old/new
overlap, no per-deployment observability, and rollback becomes task-definition
archaeology. Rejected (D3).

**"Just run EKS."** A legitimate answer for operators who want Kubernetes, and
the existing backend already serves it. It does not serve operators who chose AWS
specifically to avoid running a cluster, which is this backend's audience.

**AWS App Runner / Lambda.** App Runner hides exactly the routing and identity
control Rise needs; Lambda is a different execution model (no long-lived server
process, no Traefik-visible servers). Both remain plausible *separate* backends,
not implementations of this one.

## Delivery outline

Roughly one workstream per phase, each independently reviewable:

1. **Skeleton** — `rise-backend-ecs` crate, `EcsBackend: DeploymentBackend`,
   `DeploymentControllerSettings::Ecs`, wiring in `AppState`, config schema
   regenerated. No reconciler: URLs compute, nothing deploys.
2. **Single-container happy path** — task definition + service create/update/GC,
   tags, plain env, ECR pull, reconcile loop under leader election, deployment
   state machine transitions, `pod_status` from `DescribeTasks`.
3. **Ingress** — Traefik ECS provider labels shared with `rise-backend-docker`,
   access classes + forwardAuth, `/.rise` routes, custom domains, cutover and
   drain via Traefik `serverStatus`.
4. **Secrets and identity** — SSM `SecureString` injection (D7), identity sidecar
   and the `rise` identity-agent subcommand (D8).
5. **Multi-container** — per-spec services, Cloud Map discovery, routes.
6. **Logs** — the `CloudWatch` logs backend variant.
7. **Operator surface** — Terraform ECS section, operator docs page, feature
   matrix column, upgrade notes.

Testing is not a phase. Tier-1 contract tests land with the phase whose calls
they cover; the tier-3 sandbox account and its `Backend` driver are stood up
during phase 2, because phase 2 is where the design's AWS assumptions are first
falsifiable. See [Testing](#testing).

## Testing

The e2e harness (`tests/e2e`) already has the right shape for this: scenarios are
written once against the `Backend` driver seam, each backend self-provisions its
own stack, and a capability a backend lacks surfaces as a **declared skip with a
reason** rather than silent drift. An ECS driver is an additive
`tests/e2e/src/backend/ecs.rs`, not a second suite.

What to run it against is three tiers, because no single environment covers both
"the reconciler emits the right AWS objects" and "the app actually serves
traffic":

| Tier | Environment | Runs | Proves | Cost |
|---|---|---|---|---|
| 1. Contract | Fake ECS/SSM/Cloud Map API (`moto` server or an in-crate stub) | Every PR | The reconciler emits the intended task definitions, services, tags, Traefik labels, SSM parameters; drift detection and GC converge | Free, seconds |
| 2. Emulated | LocalStack ECS (runs tasks as local Docker containers) | Every PR, if viable | The above **plus** Traefik discovering real containers and routing to them — most existing scenarios run unchanged | Paid license; minutes |
| 3. Real | AWS sandbox account, GitHub OIDC → IAM role | Nightly / pre-release | Fargate sizing and cold start, IAM and `PassRole` scoping, ECS `secrets` injection, Cloud Map DNS, ALB/ACM, API throttling | Real AWS spend; slow |

Tier 1 is an ordinary workspace integration test in `rise-backend-ecs`, not an e2e
scenario: `moto` implements the ECS surface this backend uses
(`RegisterTaskDefinition`, `Create`/`Update`/`DeleteService`,
`Describe`/`ListTasks`, `Describe`/`ListServices`, resource tagging) but **runs no
containers**, so it can only assert API-call shape. That is still the highest
value-per-second test available: most reconciler bugs are "we sent the wrong
thing", and this catches them on every PR for free.

Tier 3 is gated exactly like the existing `E2E / Docker` and `E2E / Minikube`
jobs — `push` to `develop` or a trusted PR — which is also what makes federated
AWS credentials safe to expose. Cost is managed by keeping the expensive standing
pieces (VPC, NAT, ALB, Traefik service) long-lived in the sandbox account and
churning only ECS services, task definitions and SSM parameters per run, with a
tag-scoped sweeper that deletes anything the harness left behind.

**The load-bearing caveat:** tiers 1 and 2 do not de-risk this design. Every
question in [Open questions](#open-questions) — Cloud Map multi-registration,
quota ceilings, `PassRole` scoping, Fargate cold-start against
`DEPLOYING_TIMEOUT_MINUTES` — is a fact about real AWS, and an emulator that
answers them "yes" is not evidence. Tier 3 must therefore exist **before** the
design is settled, not after: the phase-2 spike runs against a real account, and
tiers 1 and 2 are what keep it regression-tested afterwards.

## Open questions

Each must be resolved before this ADR moves to **Proposed**; the first three are
verification against AWS, not choices.

1. **Cloud Map multi-registration (D10).** Can two ECS services register into one
   Cloud Map service simultaneously? If not, the fallbacks are, in order:
   (a) per-deployment Cloud Map services plus a stable alias repointed at
   cutover; (b) route internal traffic through Traefik on an internal
   entrypoint (uniform with external routing, at a latency cost);
   (c) accept a discovery-name switch at cutover rather than an overlap, and
   document it as a parity note.
2. **Traefik ECS provider fidelity.** Confirm the ECS provider consumes the full
   label set the Docker provider does — specifically per-service health-check
   labels and forwardAuth middleware — and confirm its refresh interval and IAM
   requirements.
3. **Quotas and limits.** Services per cluster, task-definition revisions,
   `secrets` entries per container, SSM Standard-tier value size, ALB listener
   rules and certificates per listener — the exact current figures, and which are
   raisable.
4. **Reconcile interval and `DEPLOYING_TIMEOUT_MINUTES`.** Is the shared
   5-minute constant adequate for Fargate cold start + large image pull, or does
   this backend need its own configurable ceiling in `rise-backend-core`?
5. **Task role granularity.** One shared task role per install, per project, or
   per deployment? Per-project is the useful unit for app-level AWS access, but
   it multiplies IAM objects and complicates `iam:PassRole` scoping (D14).
6. **LocalStack viability for tier 2 (see [Testing](#testing)).** Two blockers,
   both cheap to spike: (a) does Traefik's ECS provider work against a LocalStack
   endpoint (`AWS_ENDPOINT_URL`), and are the task IPs LocalStack reports in
   `DescribeTasks` reachable from the Traefik container? (b) ECS is in
   LocalStack's paid plans — the free Hobby plan is non-commercial — so tier 2
   needs a licensing answer before it can run in CI.
7. **Shared-logic promotion.** Which of `rise-backend-docker`'s modules
   (`labels`, `pod_status`, `diff`, `rolling`) move to `rise-backend-core` versus
   staying Docker-specific — decided per module during phase 3, not up front.

## References

- [Deployment Backends](../deployment-backends.md) — the parity policy and
  feature matrix this backend must eventually appear in.
- [ADR-0001: Unified Permission Model](./0001-unified-permission-model.md) —
  controller identity and the token model an out-of-process ECS controller would
  use.
- `crates/rise-backend-core/` — the seam (`DeploymentBackend`, `DeploymentStore`,
  provider traits, runtime helpers).
- `crates/rise-backend-docker/` — the reference implementation this backend
  mirrors (reconcile loop, Traefik labels, cutover, identity delivery).
- `modules/rise-aws/` — the Terraform module extended by D14.
- `tests/e2e/README.md` — the `Backend` driver seam, the scenario matrix, and the
  declared-skip convention an ECS driver plugs into.
