---
title: "Amazon ECS"
description: "Running Rise's deployment backend on Amazon ECS with the Fargate launch type."
---

The ECS backend deploys apps as **Fargate tasks** routed by **Traefik's ECS
provider**. There is no cluster to operate and no host to own: Rise's control
plane talks to the ECS API, and AWS schedules the workloads.

The design, its rationale, and the two AWS behaviours it depends on (both
verified against a real account) are recorded in
[ADR-0004](/operator-docs/adr/0004-ecs-deployment-backend/).

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
  give every task in the cluster a default router).
- `traefik_api_url` reachable from the control plane if any project uses a
  `health_check` — Traefik's `serverStatus` is the authoritative readiness
  signal with no fallback, so without it such a deployment never becomes
  Healthy.

`tests/e2e/aws/ecs-stack.sh` stands up a complete working example of all of
this — cluster, security group, IAM roles, Cloud Map namespace, Traefik,
Postgres, Dex and Rise — and is the fastest way to see the wiring end to end.

## IAM

Two roles, kept separate because they are trusted differently.

**Task execution role** — assumed by ECS itself, to pull images and resolve
secrets:

- `AmazonECSTaskExecutionRolePolicy` (ECR pull + CloudWatch Logs)
- `ssm:GetParameters` and `kms:Decrypt` on `/{ssm_parameter_prefix}/*`

**Control-plane role** — the identity Rise runs as:

- `ecs:*` on the cluster
- `ec2:DescribeNetworkInterfaces` (task IPs, for readiness)
- `logs:*` on the configured log group
- `ssm:PutParameter` / `DeleteParameter(s)` / `GetParametersByPath` on
  `/{ssm_parameter_prefix}/*`
- `iam:PassRole` **scoped to exactly the execution and task roles above**

That last scoping matters: an unscoped `iam:PassRole` would let anyone who can
create a Rise deployment run a task as any role in the account.

## Configuration

`config/ecs.yaml` ships in the image and is selected with
`RISE_CONFIG_RUN_MODE=ecs`. The settings specific to this backend:

| Setting | Notes |
|---|---|
| `cluster`, `region` | which cluster to reconcile |
| `subnets`, `security_groups` | accept a YAML list **or a comma-separated string**, so they can come straight from a Terraform output via an env var |
| `assign_public_ip` | required on public subnets |
| `execution_role_arn`, `task_role_arn` | see IAM above |
| `log_group` | `awslogs` destination; omit for no container logging |
| `ssm_parameter_prefix`, `ssm_kms_key_id` | where secret env vars live |
| `cpu_architecture` | `X86_64` or `ARM64` — also the CLI's platform hint |
| `auth_backend_url` | **must be reachable from inside the cluster** (a Cloud Map name or internal load balancer), never the public URL |
| `traefik_api_url` | required for projects using `health_check` |
| `reconcile_interval_secs` | defaults to 30; ECS is a throttled API |

## Not supported yet

Each of these **fails closed at deploy time** with an error naming the reason
and the alternatives, rather than deploying something that quietly does not
work:

- **Multi-container deployments** — cross-container discovery needs Cloud Map
  registration, so `RISE_CONTAINER_HOST__*` would be absent.
- **Workload identity tokens** (`[identity].audiences`) — there is no way to
  write files into a running Fargate task; a sidecar on a shared task volume is
  the intended mechanism.
- **Runtime logs** — `deployment_logs: none` by default. Installs running Loki
  can use `type: loki` today (it is backend-agnostic); a CloudWatch backend is
  the intended replacement.

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

**Memory is higher than requested.** Expected — see the CPU/memory row of the
[feature matrix](/operator-docs/deployment-backends/). Fargate accepts only a
fixed table of sizes and Rise rounds up, never down. The resolved size is
logged at reconcile.
