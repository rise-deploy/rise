---
title: "Terraform"
description: "Standing up a Rise install on Amazon ECS with the rise-aws and rise-ecs Terraform modules."
---

Two modules under [`modules/`](https://github.com/rise-deploy/rise/tree/develop/modules)
stand up the topology [ADR-0005](/operator-docs/adr/0005-ecs-deployment-backend/)
describes.

| Module | Owns |
|---|---|
| `rise-aws` | IAM: the control-plane role and policy, the ECS task execution role, the ECR push role, KMS |
| `rise-ecs` | Everything that runs: VPC, cluster, Cloud Map, RDS, Secrets Manager, the NLB, Traefik, and the Rise service |

They are separate because they have different lifetimes. `rise-aws` is
account-global — its role and policy are named after `name`, and an install that
already uses it for ECR or the RDS extension keeps the one it has. `rise-ecs` is
per-environment, so staging and production in one account are two instances of
it against a single `rise-aws`.

`rise-ecs` consumes `rise-aws`'s outputs rather than nesting it. That direction
matters: `rise-aws` scopes every policy by interpolating ARNs from *names*, never
by referencing a resource, which is what keeps the edge one-directional. Both
modules must therefore be given the same `name`, `ecs_cluster_name` and
`ssm_parameter_prefix`.

## Minimal install

```hcl
module "rise_aws" {
  source = "./modules/rise-aws"

  name       = "rise"
  enable_ecr = true
  enable_ecs = true

  ecs_cluster_name     = "rise"
  ssm_parameter_prefix = "rise"
  ecs_secret_arns      = module.rise_ecs.secret_arns_for_execution_role
}

module "rise_ecs" {
  source = "./modules/rise-ecs"

  name           = "rise"
  ingress_domain = "rise.example.com"
  rise_image_tag = "0.23.0"
  admin_email    = "ops@example.com"
  acme_email     = "ops@example.com"

  controller_role_arn = module.rise_aws.role_arn
  execution_role_arn  = module.rise_aws.ecs_execution_role_arn
  ecr_push_role_arn   = module.rise_aws.push_role_arn

  oidc_issuer        = "https://id.example.com"
  oidc_client_id     = "rise"
  oidc_client_secret = var.oidc_client_secret
}
```

Point a wildcard DNS record at the load balancer — `dns_records_required` says
exactly what to create, or pass `route53_zone_id` and the module makes them.
`*.<domain>` is required, not optional: projects are served at
`<project>.<domain>` and groups and environments add another label.

Try it without an identity provider by setting `deploy_dex = true` and
`dex_admin_password_bcrypt`. That runs Dex as an ECS service — a demo, with
in-memory storage that loses sessions whenever the task is replaced.

## Into a cluster you already run

Pass `vpc` and `cluster` and the module creates neither:

```hcl
module "rise_ecs" {
  # …
  vpc = {
    id                 = "vpc-0123456789abcdef0"
    public_subnet_ids  = ["subnet-aaa", "subnet-bbb"]
    private_subnet_ids = ["subnet-ccc", "subnet-ddd"]
  }
  cluster = { name = "my-existing-cluster" }
}
```

Either can be brought independently. Brought subnets are checked against the VPC
you named and a brought cluster is checked for `ACTIVE`, so a mismatch surfaces
in `terraform plan` rather than several minutes into an apply.

## What it configures

The Rise container runs the `config/ecs.yaml` that ships in the image
(`RISE_CONFIG_RUN_MODE=ecs`), so the module's job is filling that file's
environment interpolations. Two of them are the difference between a working
install and a puzzling one, and the module derives both from Cloud Map:

- `RISE_AUTH_BACKEND_URL` — `http://rise.<namespace>:3000`. Traefik calls it for
  every forwardAuth subrequest, so it must be reachable *from inside the
  cluster* and must never be the public URL.
- `RISE_TRAEFIK_API_URL` — `http://traefik.<namespace>:8080`. Readiness comes
  from Traefik's `serverStatus` with no fallback, so without it a project with a
  `health_check` never becomes Healthy.

Subnets and security groups are passed as comma-separated strings. The settings
loader accepts a YAML list or a comma-separated string precisely so a Terraform
output can travel through one environment variable.

Running the control plane somewhere else — EKS, EC2, on-premises — is supported
by the `rise_task_environment` output, which is the same map the module sets on
its own task definition, so the two cannot drift.

## Configurations the modules will not produce

Each of these is rejected by the backend at startup. The modules fail in
`terraform plan` instead:

| Refused | Why |
|---|---|
| `registry_type` of `gitlab` or `jfrog` | Both issue short-lived scoped pull tokens that Rise refreshes on the puller's behalf. ECS re-authenticates at every task start with no refresh hook, so a deploy would succeed and break hours later. |
| An ECR account other than the caller's | Rise writes no ECR repository policy, so cross-account pulls cannot work. The account is taken from `aws_caller_identity` and is not a variable. |
| ECR without a push role | The CLI could not obtain push credentials. |
| A `cpu_architecture` other than `X86_64`/`ARM64` | Fargate has exactly two. |
| An `ecr_repo_prefix` with no trailing slash | It is concatenated onto the project name literally — `"rise"` yields `risemyapp`. |
| ACME with `nat_gateway_mode = "none"` | VPC endpoints reach AWS services only; an HTTP-01 challenge needs Let's Encrypt, so the certificate would silently never arrive. |
| No identity provider of either kind | `oidc_issuer` or `deploy_dex` is required. |

## Verifying without applying

```bash
mise run terraform:check
```

Format-checks, validates and runs the `terraform test` suites for both modules.
The tests plan against the real provider schema with only the identity data
sources overridden, so they catch wiring errors `terraform validate` cannot see;
`rise-aws`'s suite additionally asserts the control-plane policy stays scoped —
that `iam:PassRole` names exactly two roles and carries its service condition,
and that no permission the backend never calls creeps back in.

## Cost and caveats

See the [module README](https://github.com/rise-deploy/rise/tree/develop/modules/rise-ecs)
for the cost table, the single-replica Traefik constraint, and the note about the
generated database password living in Terraform state — and how to avoid it with
`database_url_secret_arn`.
