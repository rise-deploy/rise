# `rise-ecs`

Terraform for a working Rise install on Amazon ECS (Fargate), in the shape
[ADR-0005](../../docs/engineering/src/content/docs/adr/0005-ecs-deployment-backend.md)
describes: a public load balancer fronting Traefik, the Rise
control plane and every deployed workload in private subnets, RDS for the
control-plane store, and ECR for images.

It pairs with [`rise-aws`](../rise-aws), which owns the IAM. Apply that first.

## Usage

```hcl
module "rise_aws" {
  source = "../rise-aws"

  name       = "rise"
  enable_ecr = true
  enable_ecs = true

  # Both modules must agree on these. rise-aws scopes its policies by
  # interpolating ARNs from names, which is what keeps the two free of a
  # dependency cycle.
  ecs_cluster_name     = "rise"
  ecs_log_group_name   = "/rise"
  ssm_parameter_prefix = "rise"

  # Names are deterministic, which avoids a module dependency cycle while
  # still confining task-secret reads to this install.
  ecs_secret_arns = ["arn:aws:secretsmanager:eu-west-1:123456789012:secret:rise/*"]
}

module "rise_ecs" {
  source = "../rise-ecs"

  name           = "rise"
  ingress_domain = "rise.example.com"
  log_group_name = "/rise"
  rise_image_tag = "0.23.0"
  admin_email    = "ops@example.com"
  acme_email     = "ops@example.com"

  controller_role_arn      = module.rise_aws.role_arn
  execution_role_arn       = module.rise_aws.ecs_execution_role_arn
  workload_task_role_arn   = module.rise_aws.ecs_task_role_arn
  traefik_task_role_arn    = module.rise_aws.ecs_traefik_role_arn
  create_traefik_task_role = false
  ecr_push_role_arn        = module.rise_aws.push_role_arn

  oidc_issuer        = "https://id.example.com"
  oidc_client_id     = "rise"
  oidc_client_secret = var.oidc_client_secret
}
```

`rise_image_ref` accepts a digest-pinned public image directly, for example
`ghcr.io/rise-deploy/rise@sha256:…`. Set exactly one of `rise_image_ref` and
`rise_image_tag`.

Then create the DNS records from the `dns_records_required` output, or pass
`route53_zone_id` and let the module make them. **The wildcard is required**:
projects are served at `<project>.<domain>`, and groups and environments add
another label.

## New cluster, or one you already run

Both the VPC and the cluster are create-or-bring. Omit them and the module
builds them; pass them and it deploys into what you have.

```hcl
vpc = {
  id                 = "vpc-0123456789abcdef0"
  public_subnet_ids  = ["subnet-aaa", "subnet-bbb"]   # for the load balancer
  private_subnet_ids = ["subnet-ccc", "subnet-ddd"]   # tasks land here
  database_subnet_ids = []                            # falls back to private
}

cluster = { name = "my-existing-cluster" }
```

Brought subnets are read back and checked against the VPC you named, and a
brought cluster is checked for `ACTIVE`, so a mismatch fails in `terraform plan`
rather than several minutes into an apply.

`enable_container_insights` applies only to a cluster the module creates — it
does not mutate a cluster it does not own.

## What it costs

Roughly, in `eu-central-1`, before traffic:

| | |
|---|---|
| NAT gateway (`single`) | ~$32/mo + $0.045/GB |
| Network load balancer | ~$16/mo + LCU |
| RDS `db.t4g.micro`, single-AZ | ~$12/mo |
| EFS (ACME storage) | cents |
| ECS Fargate | ~1.5 vCPU for the control plane, 0.5 per app task |

`enable_vpc_endpoints` is **not** a saving: eight interface endpoints across two
AZs runs well above a single NAT gateway. Choose it for a no-egress posture, not
for cost. The S3 *gateway* endpoint is created either way — it is free, ECR
layer blobs come from S3, and its absence fails the image pull rather than the
API call.

`nat_gateway_mode = "none"` requires `enable_vpc_endpoints`, and rules out ACME:
endpoints reach AWS services only, and an HTTP-01 challenge needs Let's Encrypt.
The module refuses that combination rather than letting the certificate silently
never arrive.

## TLS and custom domains

`edge_mode = "nlb-traefik-acme"` (default) terminates TLS at Traefik with ACME
HTTP-01, so a user adds a custom domain by CNAME and TLS follows with no further
step.

`edge_mode = "alb-acm"` terminates at an ALB instead, buying L7 access logs and
WAF. It costs custom domains their automatic certificates: ACM has no HTTP-01,
so each one gains a DNS-validated certificate an operator has to create, against
a default limit of 25 per listener. Port 80 redirects permanently to HTTPS;
Traefik still routes and still enforces access classes in both modes.

**Traefik runs one replica, and that is not configurable.** Its ACME file store
is not multi-writer safe. In ACME mode the deployment configuration also stops
the old task before starting the new one, since ECS's default would briefly run
two.

## Identity

Point `oidc_issuer` at a real provider. `deploy_dex = true` runs Dex as an ECS
service instead, so the install is usable end to end without one — **a demo**:
storage is in-memory, so sessions and refresh tokens are lost whenever the task
is replaced, and there is no HA.

For an IdP that publishes groups, set `oidc_group_claim`, `admin_idp_group`,
`platform_access_policy = "restrictive"`, and `platform_allowed_idp_group`.
Set `idp_group_sync_prefixes` to limit which canonical group names are mirrored
into Rise teams; an empty list accepts every canonical name. These values are
rendered through the ECS environment into the shipped configuration; no mounted
override file is required.

Dex needs a bcrypt hash, since Terraform has no bcrypt function:

```console
$ htpasswd -bnBC 10 "" 'your-password' | tr -d ':\n'
```

## Secrets and Terraform state

`DATABASE_URL`, the JWT signing key, the encryption key and the OIDC client
secret live in Secrets Manager and reach the task through the task definition's
`secrets` block, so they never appear in a `DescribeTaskDefinition` response.
The secret-version resources use write-only arguments: planning and refresh do
not call `GetSecretValue`, and the provider does not retain those values on the
secret-version resources in state. This requires Terraform 1.11 or newer and
AWS provider 6.50 or newer.

Set `control_plane_local_config_secret_arn` when the control plane needs an
operator-specific YAML overlay, for example extension-provider configuration.
ECS injects the secret value as `RISE_LOCAL_CONFIG_YAML`, which supplies the
local configuration layer that Rise loads after its shipped `default.yaml` and
`ecs.yaml`. The ECS module owns the control-plane address `0.0.0.0:3000`; the
overlay must not set `server.host` or `server.port` because task networking,
security groups, internal URLs and Traefik use that address. Include the secret
ARN in `modules/rise-aws`'s `ecs_secret_arns` so the task execution role can
resolve it:

```hcl
module "rise_aws" {
  # ...
  ecs_secret_arns = [aws_secretsmanager_secret.rise_local_config.arn]
}

module "rise_ecs" {
  # ...
  control_plane_local_config_secret_arn = aws_secretsmanager_secret.rise_local_config.arn
}
```

**Generated source values remain in Terraform state.** The database password,
JWT signing key and encryption key are outputs of persistent `random` resources;
an OIDC client secret supplied as an ordinary Terraform input can also reach the
calling configuration's state. Write-only secret versions prevent provider
reads and duplicate plaintext storage, but do not make their source values
ephemeral. Use an encrypted remote backend with restricted access. To keep the
database URL out entirely, create its secret yourself and pass
`database_url_secret_arn` — the module then creates no database at all.

Rotating the database password out of band will not read as drift (the instance
ignores changes to it), but you must update the secret to match.

## What this module does not do

Not gaps in the module — capabilities the ECS backend does not have yet, each
failing closed at deploy time rather than half-working:

- **Multi-container deployments.** Cross-container discovery needs Cloud Map
  registration for workloads, which is unimplemented. The namespace here exists
  for the control plane's own services. Their names are scoped to the module
  name: `<name>-control-plane`, `<name>-traefik` and, when enabled,
  `<name>-dex`. This keeps multiple installs distinct when they bring the same
  Cloud Map namespace.
- **Workload identity tokens.** There is no way to write files into a running
  Fargate task; a sidecar on a shared volume is the intended mechanism.

## Teardown

`terraform destroy` may need a retry. A Cloud Map service will not delete until
its instances are deregistered, and ECS deregisters them only as its own tasks
drain — that is AWS's ordering, not a fault here. `deletion_protection` defaults
to on for the database and load balancer; set it false for a throwaway install,
along with `secret_recovery_window_days = 0` (otherwise a destroy followed by an
apply collides on the still-scheduled secret names).

## Layout

`rise-aws` is a single `main.tf`; this module has roughly six times the resource
count, so it is split by concern: `network.tf`, `security-groups.tf`,
`cluster.tf`, `secrets.tf`, `database.tf`, `edge.tf`, `traefik.tf`, `rise.tf`,
`dex.tf`. Every create-or-bring decision resolves in `locals.tf` and nowhere
else — no resource refers to a counted resource directly, which is what keeps
the two topologies from forking the module.

`tests/` holds `terraform test` suites covering both topologies and each
guardrail. Run them with `mise run terraform:check`.
