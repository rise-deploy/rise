# ECS e2e — bootstrap

The persistent half of the ECS end-to-end environment. **Applied once, by hand,**
into the Scratch account. The harness never touches it.

It exists because the harness runs under a GitHub OIDC role with **no IAM-write**:
everything requiring an IAM write is created here, and the per-run apply
(`../run`) creates only non-IAM resources.

## What it creates

| | |
|---|---|
| GitHub OIDC | provider + the role CI assumes (`repo:rise-deploy/rise:*`) |
| IAM for Rise | `modules/rise-aws` with `enable_ecs` — controller role, execution role, ECR push role — plus Traefik's task role |
| Runtime | VPC with public subnets, ECS cluster, Cloud Map namespace, log group |
| Addressing | a Route 53 zone; each run UPSERTs its own subtree into it |
| State | the S3 bucket both workspaces keep their state in |

**Nothing runs here.** This is an empty cluster and the IAM the per-run identity
cannot create for itself. Traefik, Dex, Postgres and the Rise control plane are
all stood up per run and destroyed again, so every run exercises the whole thing
coming up — including the routing layer, which is what an operator actually
does — and an idle environment costs a hosted zone rather than two Fargate
tasks.

## Applying, the first time

This workspace creates the S3 bucket its own state lives in, so the very first
apply is two-phase. Once. Afterwards it is an ordinary `init` + `apply`.

```bash
cd tests/e2e/bootstrap
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
NAME=rise-e2e
REGION=eu-central-1

# 1. Local state, just far enough to create the bucket.
terraform init -backend=false
terraform apply -target=aws_s3_bucket.state -var "region=$REGION"

# 2. Move that state into the bucket, and finish.
terraform init -migrate-state \
  -backend-config="bucket=${NAME}-tfstate-${ACCOUNT}" \
  -backend-config="region=$REGION"
terraform apply -var "region=$REGION"
```

From then on:

```bash
terraform init -backend-config="bucket=${NAME}-tfstate-${ACCOUNT}" -backend-config="region=$REGION"
terraform apply
```

`dns_zone_name` defaults to `rise-deploy.click`. Delegate it from the registrar
once, using the `dns_name_servers` output.

The `backend_config` output prints those arguments if you forget them.

The bucket carries `prevent_destroy`. A `terraform destroy` would otherwise
delete the state file it is reading from, halfway through, orphaning everything
else. Tearing the environment down for real means destroying everything else
first, then emptying and removing the bucket by hand.

## Applying it from CI

`enable_ci_bootstrap_role = true` creates a second OIDC role that can apply this
workspace, so changes to it land through CI rather than from someone's laptop.

**It can write IAM, and a principal that can create roles and attach policies
can grant itself whatever the account allows.** That is a reasonable trade in an
account holding nothing else, and a bad one anywhere else. It is off by default
for that reason, and two things narrow it when on:

- IAM writes are scoped to `<name>` and `<name>-*` role and policy names, so it
  cannot touch anything this workspace does not own, and it cannot replace the
  account-global OIDC provider that every other workflow trusts.
- Its trust names specific refs — `ci_bootstrap_subjects`, defaulting to the
  develop branch — rather than the run role's `repo:<repo>:*`. A pull request,
  or a branch anyone can push, cannot assume it. Point it at
  `repo:<repo>:environment:<name>` for a GitHub Environment with required
  reviewers if you want a human in the loop.

## Wiring the suite into CI

Applying this workspace is not enough on its own; the workflow
(`.github/workflows/e2e-ecs.yml`) needs three repository **variables** and one
label.

Under *Settings → Secrets and variables → Actions → Variables*:

| Variable | Value |
|---|---|
| `AWS_E2E_ROLE_ARN` | `terraform output ci_role_arn` |
| `AWS_E2E_REGION` | the region this was applied to |
| `AWS_E2E_ENV_NAME` | only if `name` is not `rise-e2e` |

Variables rather than secrets on purpose: a role ARN and a region are not
sensitive, and a masked value that is simply *unset* fails as an empty string
with no hint of why.

Then create the **`ecs-e2e` label** once. The pull-request trigger fires on
`labeled` and matches that exact name, so without the label existing there is no
way to opt a pull request in.

Three ways to run it:

- **Label a pull request `ecs-e2e`.** Runs against that PR's image, under scope
  `pr-<number>`.
- **Nightly**, 03:17 UTC, against `develop`.
- **`workflow_dispatch`.** Pass `image_tag` explicitly — the default is
  `develop-<short-sha>`, which only exists for a commit that was pushed to
  `develop`.

The workflow does **not** build the image. It deploys one `ci.yml` already
published to GHCR for the same commit, so the run is only possible once that
commit's `Build / Image manifest` job has passed.

## Two workspaces, one environment

| | |
|---|---|
| `tests/e2e/bootstrap` | this one — long-lived, applied rarely |
| `tests/e2e/run` | Traefik, Dex, Postgres and the control plane — applied and destroyed by the harness around every suite |

Both keep state in the same bucket under different keys. `run` reads this
workspace's outputs through `terraform_remote_state`, which is why this one's
state has to be in S3 rather than on whoever applied it last.

The directory is not `aws/` because the environment is not necessarily only AWS:
if the suite later needs something hosted elsewhere, it belongs in this
workspace alongside the rest, not in a third place.

## Why a Route 53 zone, and why runs are scoped

Fargate tasks cannot hold an Elastic IP, and Traefik is per-run, so its address
is new every run. The zone makes the *name* stable instead.

Each run owns a scope — `pr-457`, `nightly`, `dev-<user>` — and everything it
creates is named after it: the Terraform state key, the DNS subtree, Rise's
controller class, the security groups, its Cloud Map entries. At bring-up it
UPSERTs `<scope>.<zone>` and `*.<scope>.<zone>` at the Traefik it just created,
and deletes both at teardown. So `rise.pr-457.rise-deploy.click` is one run's
control plane and `myapp.pr-457.rise-deploy.click` an app deployed into it.

The wildcard is required, and one label is enough: projects are served at
`<project>.<scope>.<zone>`, and groups and environments join the project name
with a dash rather than a dot.

Scoping is what lets runs overlap. A shared apex would have each run repointing
the other's traffic; a shared controller class would have each run's orphan
collector deleting the other's services; a shared Traefik would answer for both.
The scope closes all three — the last via
`--providers.ecs.constraints`, matched against the controller class Rise stamps
into `dockerLabels`.

`route53:ChangeResourceRecordSets` is not an IAM write, so the harness can do
this under the no-IAM-write role.

## Cost

Idle, this is a NAT-free public-subnet VPC, an empty ECS cluster and a hosted
zone — roughly **$1/month**, the zone being nearly all of it. An empty cluster
and a VPC without a NAT gateway are free.

A run adds four small Fargate tasks (Traefik 0.5 vCPU, Dex, Postgres and Rise at
0.25–0.5 each) for as long as it lasts. Overlapping runs multiply that, and
share the account's Fargate quota.
