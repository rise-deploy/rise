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
| Runtime | VPC with public subnets, ECS cluster, Cloud Map namespace, log group, security groups |
| Edge | the long-running **Traefik** service, and **Dex** |
| Addressing | a Route 53 zone; the harness UPSERTs records into it at run start |
| State | the S3 bucket both workspaces keep their state in |

Traefik and Dex are persistent because they are slow to start and carry no
per-run state. Postgres and the Rise control plane are not — they are per-run,
so every run gets a fresh database and the image under test.

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
terraform apply -target=aws_s3_bucket.state \
  -var "dns_zone_name=e2e.example.com" -var "region=$REGION"

# 2. Move that state into the bucket, and finish.
terraform init -migrate-state \
  -backend-config="bucket=${NAME}-tfstate-${ACCOUNT}" \
  -backend-config="region=$REGION"
terraform apply -var "dns_zone_name=e2e.example.com" -var "region=$REGION"
```

From then on:

```bash
terraform init -backend-config="bucket=${NAME}-tfstate-${ACCOUNT}" -backend-config="region=$REGION"
terraform apply -var "dns_zone_name=e2e.example.com"
```

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

## Two workspaces, one environment

| | |
|---|---|
| `tests/e2e/bootstrap` | this one — long-lived, applied rarely |
| `tests/e2e/run` | Postgres and the control plane, applied and destroyed by the harness around every suite |

Both keep state in the same bucket under different keys. `run` reads this
workspace's outputs through `terraform_remote_state`, which is why this one's
state has to be in S3 rather than on whoever applied it last.

The directory is not `aws/` because the environment is not necessarily only AWS:
if the suite later needs something hosted elsewhere, it belongs in this
workspace alongside the rest, not in a third place.

## Why a Route 53 zone

Fargate tasks cannot hold an Elastic IP, so Traefik's public address changes
whenever its task is replaced. A zone makes the *domain* stable instead: the
harness resolves Traefik's current IP at run start and UPSERTs the apex and
wildcard records. Without it the environment's identity would change under it,
and a persistent Dex would be stranded by the first task replacement — its
Traefik router encodes the domain.

`route53:ChangeResourceRecordSets` is not an IAM write, so the harness can do
this under the no-IAM-write role.

## Cost

Idle, this is a NAT-free public-subnet VPC plus two small Fargate tasks
(Traefik 0.5 vCPU, Dex 0.25) and a hosted zone: roughly **$20/month**. There is
no NAT gateway, no load balancer and no RDS instance — the per-run stack uses
public IPs and a Postgres task, which is why.
