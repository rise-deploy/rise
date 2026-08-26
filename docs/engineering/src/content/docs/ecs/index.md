---
title: "Amazon ECS"
description: "Running Rise's deployment backend on Amazon ECS with the Fargate launch type."
---

The ECS backend deploys apps as **Fargate tasks** routed by **Traefik's ECS
provider**. There is no cluster to operate and no host to own: Rise's control
plane talks to the ECS API, and AWS schedules the workloads.

The design, its rationale, and the two AWS behaviours it depends on (both
verified against a real account) are recorded in
[ADR-0005](/operator-docs/adr/0005-ecs-deployment-backend/).

## How it maps

| Rise concept | ECS / AWS |
|---|---|
| Install | one ECS cluster |
| Deployment × container spec | one ECS **service** + a task-definition revision |
| Replicas | the service's `desiredCount` |
| Routing | Traefik router labels in the container definition's `dockerLabels` |
| Secret env vars | SSM Parameter Store `SecureString`, injected by ECS at task start |
| Bookkeeping | resource **tags** (`rise.dev/project`, …) — the only discovery mechanism |

Two consequences are worth internalising before you operate it:

- **A deployment's drift is applied with `UpdateService`.** ECS performs the
  rolling replacement itself, so nothing is destroyed to change anything. A
  cutover between deployments still overlaps two services behind one Traefik
  service and drains via Traefik's health check, as on Docker.
- **Task definitions are registered only when their content hash changes.**
  `RegisterTaskDefinition` sustains one request per second, so a steady install
  costs zero registrations.

## Prerequisites

- An ECS cluster, and subnets for the tasks' `awsvpc` ENIs. Public subnets need
  `assign_public_ip: true` — a Fargate task must reach ECR and CloudWatch to
  start at all, and a public subnet has no NAT gateway.
- **A raised Fargate on-demand vCPU quota.** A fresh account gets **6**, and
  every task rounds up to at least 0.25 vCPU (Rise's defaults resolve to 0.5).
  This is the single most common cause of tasks that never leave `PROVISIONING`.
- A Traefik service in the cluster running the ECS provider with
  `--providers.ecs.exposedByDefault=false` (it defaults to **true**, which would
  give every task in the cluster a default router). If the cluster hosts more
  than one Rise install, each also needs a distinct `controller_class_name` and
  a `--providers.ecs.constraints` matching it — see
  [Terraform](/operator-docs/ecs/terraform/).
- `traefik_api_url` reachable from the control plane if any project uses a
  `health_check` — Traefik's `serverStatus` is the authoritative readiness
  signal with no fallback, so without it such a deployment never becomes
  Healthy.

The [Terraform modules](/operator-docs/ecs/terraform/) provision all of it —
against a cluster and VPC they create, or ones you already run — and refuse the
configurations the backend rejects at startup. Start there.

`tests/e2e/bootstrap` and `tests/e2e/run` stand up a deliberately smaller shape
for the end-to-end suite — public subnets, no load balancer, Postgres in a task,
and every component per run under its own scope so several runs share one
cluster. Useful to read, and the clearest worked example of the multi-install
scoping above, but a test environment rather than a template.

## IAM

Two roles, kept separate because they are trusted differently.

**Task execution role** — assumed by ECS itself, to pull images and resolve
secrets:

- `AmazonECSTaskExecutionRolePolicy` (ECR pull + CloudWatch Logs)
- `ssm:GetParameters` and `kms:Decrypt` on `/{ssm_parameter_prefix}/*`
- `secretsmanager:GetSecretValue` on `repository_credentials_secret_arn`, if set

See [Container registry](#container-registry) for what the pull path needs in
detail — it is the execution role that authenticates it, so a missing permission
there surfaces as a task that cannot start rather than as an API error.

**Control-plane role** — the identity Rise runs as:

- `ecs:CreateService` / `UpdateService` / `DeleteService` / `DescribeServices` /
  `DescribeTasks` / `TagResource`, on the cluster's services and tasks
- `ecs:DescribeClusters` (the startup connectivity check), `ecs:ListServices`
  and `ecs:ListTasks`
- `ecs:RegisterTaskDefinition` — the one action that supports neither
  resource-level permissions nor the `ecs:cluster` condition key
- `ssm:PutParameter` / `DeleteParameter(s)` / `GetParametersByPath` /
  `AddTagsToResource` on `/{ssm_parameter_prefix}/*`, plus `kms` on the CMK if
  the parameters use one
- `iam:PassRole` **scoped to exactly the execution and task roles above**, with
  an `iam:PassedToService` condition of `ecs-tasks.amazonaws.com`

That last scoping matters: an unscoped `iam:PassRole` would let anyone who can
create a Rise deployment run a task as any role in the account.

The control plane needs **no** `ec2`, `logs` or `servicediscovery` access. Task
addresses come from `DescribeTasks` attachment details rather than
`DescribeNetworkInterfaces`, workload logs are written by the ECS agent under the
execution role, and Cloud Map registration for workloads is not implemented.
`modules/rise-aws` grants none of them.

The control-plane role must also trust `ecs-tasks.amazonaws.com`: on ECS it *is*
the task role Rise runs as, so without that trust the task cannot start.

## Container registry

ECS authenticates every image pull **itself**, using the task execution role,
and it re-authenticates at every task start — scale-out, task replacement, AZ
rebalance. Rise never mints a pull credential for this backend and never hands
ECS one. That shapes which registries can work here:

| `registry.type` | Supported | What it needs |
|---|---|---|
| `ecr` | ✅ recommended | `execution_role_arn`, in the **same AWS account** as the cluster |
| `oci-client-auth`, anonymous | ✅ | the registry reachable from the task subnets |
| `oci-client-auth` with credentials | ✅ | `repository_credentials_secret_arn` |
| `gitlab`, `jfrog` | ❌ | nothing — see below |

GitLab and JFrog issue short-lived scoped pull tokens that Rise refreshes on the
puller's behalf; on Kubernetes it re-mints the pull Secret every six hours. ECS
gives it no equivalent hook, so a deploy would succeed and then fail hours later
when the token expired. Configuring one alongside the ECS controller is refused
at startup rather than left to fail at 3am.

### ECR

Nothing extra is stored: the execution role *is* the credential.

- **Execution role**: `AmazonECSTaskExecutionRolePolicy`, or explicitly
  `ecr:GetAuthorizationToken` on `*` plus `ecr:BatchGetImage`,
  `ecr:GetDownloadUrlForLayer` and `ecr:BatchCheckLayerAvailability` on
  `arn:aws:ecr:{region}:{account}:repository/{repo_prefix}*`.
- **Control-plane role**: `sts:AssumeRole` on `registry.push_role_arn` (the role
  Rise assumes to mint the CLI's scoped push credentials), plus
  `ecr:CreateRepository`, `ecr:DescribeRepositories`, `ecr:DeleteRepository` and
  `ecr:TagResource` — repository lifecycle uses the control plane's own identity,
  not the push role.
- **Network path**: a task must reach ECR before it can start. Either a public IP
  or a NAT gateway, or the `com.amazonaws.{region}.ecr.api` and
  `com.amazonaws.{region}.ecr.dkr` interface endpoints **plus the S3 gateway
  endpoint** — layer blobs come from S3, and a setup with only the two ECR
  endpoints fails on the pull rather than on the API call.

**The registry must live in the cluster's account.** Rise creates repositories
with tags and image scanning only and writes no repository policy, which
cross-account ECR requires; identity-based permissions on the execution role are
not sufficient on their own. A mismatch is refused at startup, because it is
otherwise invisible — `registry.account_id` only formats image references, so
repositories would be created in one account while every deployment pointed at
another.

Repositories are provisioned by a background controller on a 10-second poll, not
at project-create time. A create-then-deploy within that window mints
credentials against a repository that does not exist yet.

### A private registry that is not ECR

Store the username and password in a Secrets Manager secret, set
`repository_credentials_secret_arn` to its ARN, and grant the execution role
`secretsmanager:GetSecretValue` on it (plus `kms:Decrypt` if the secret uses a
customer-managed key). The ARN is stamped on every container definition as
`repositoryCredentials`; ECS reads the secret at task start, so rotating the
credentials inside it needs no redeploy.

## Configuration

`config/ecs.yaml` ships in the image and is selected with
`RISE_CONFIG_RUN_MODE=ecs`. The settings specific to this backend:

| Setting | Notes |
|---|---|
| `cluster`, `region` | which cluster to reconcile |
| `subnets`, `security_groups` | accept a YAML list **or a comma-separated string**, so they can come straight from a Terraform output via an env var |
| `assign_public_ip` | required on public subnets |
| `execution_role_arn`, `task_role_arn` | see IAM above; the execution role is also what pulls from ECR |
| `repository_credentials_secret_arn` | Secrets Manager secret for a private non-ECR registry |
| `log_group` | `awslogs` destination; omit for no container logging |
| `ssm_parameter_prefix`, `ssm_kms_key_id` | where secret env vars live |
| `cpu_architecture` | `X86_64` or `ARM64` — also the CLI's platform hint. Common spellings (`amd64`, `aarch64`, any case) are normalised; anything else is **refused at startup**, since defaulting would build images the tasks cannot execute |
| `auth_backend_url` | **must be reachable from inside the cluster** (a Cloud Map name or internal load balancer), never the public URL |
| `traefik_api_url` | required for projects using `health_check` |
| `reconcile_interval_secs` | defaults to 30; ECS is a throttled API |

:::caution[Set a customer-managed KMS key for secret isolation]
`ssm_kms_key_id` is optional, but on a multi-tenant install it should be set.
Without it, secret env vars are written as `SecureString`s under the
AWS-managed `alias/aws/ssm` key, whose default policy lets **any** account
principal permitted to call SSM decrypt them — so a broad `ReadOnlyAccess`-style
role can read every project's secrets. A customer-managed key gates decryption
behind an explicit `kms:Decrypt` grant (scoped by `kms:ViaService = ssm.*`),
which is what makes "reading ECS reveals only a parameter name" hold.
:::

## Not supported yet

Each of these **fails closed at deploy time** with an error naming the reason
and the alternatives, rather than deploying something that quietly does not
work:

- **Multi-container deployments** — cross-container discovery needs Cloud Map
  registration, so `RISE_CONTAINER_HOST__*` would be absent.
- **Workload identity tokens** (`[identity].audiences`) — there is no way to
  write files into a running Fargate task; a sidecar on a shared task volume is
  the intended mechanism.
- **Runtime logs in the Rise UI/API** — `deployment_logs: none` by default, so
  `rise deployment logs` and the UI's Logs tab return nothing for ECS
  deployments. Installs running Loki can point `type: loki` at it today (it is
  backend-agnostic). A native **CloudWatch log backend** is the intended
  replacement — it lets Rise surface a deployment's logs scoped to the project
  the caller is authorized for, using the control plane's own IAM, so operators
  need not hand out CloudWatch access. Until it lands, the only way to read ECS
  workload logs is the CloudWatch console: all workloads share one install-wide
  log group (separated only by a `{project}-{group}` stream prefix), so that
  access is not a per-project boundary — scope it accordingly.

## Troubleshooting

**Tasks never leave `PROVISIONING`.** Almost always the Fargate vCPU quota.
Check `aws ecs describe-services --services … --query 'services[].events'`.

**A deployment stays `Deploying` forever.** If the project sets a
`health_check`, confirm `traefik_api_url` is reachable — readiness comes from
Traefik's `serverStatus` with no fallback. The controller logs a warning at
startup when the URL is unset.

**The app is not routable.** Confirm Traefik's ECS provider is running against
the right cluster and that `exposedByDefault` is `false`. Rise stamps
`traefik.enable=true` itself; the routing configuration is in the container
definition's `dockerLabels`, which is the only place the provider reads it.

**`CannotPullContainerError` on every task.** The execution role is the whole
pull credential on this backend. Check it has the ECR read actions above, that
the task can reach ECR at all (public IP, NAT, or all three VPC endpoints), and
— for a private non-ECR registry — that `repository_credentials_secret_arn` is
set and readable by that role.

**Memory is higher than requested.** Expected — see the CPU/memory row of the
[feature matrix](/operator-docs/deployment-backends/). Fargate accepts only a
fixed table of sizes and Rise rounds up, never down. The resolved size is
logged at reconcile.
