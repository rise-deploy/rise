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
| State | the S3 bucket the per-run apply keeps its state in |

Traefik and Dex are persistent because they are slow to start and carry no
per-run state. Postgres and the Rise control plane are not — they are per-run,
so every run gets a fresh database and the image under test.

## Applying

```bash
cd tests/e2e/aws/bootstrap
terraform init
terraform apply -var 'dns_zone_name=e2e.example.com' -var 'github_repository=rise-deploy/rise'
```

Then delegate `dns_zone_name` to the name servers in the `dns_name_servers`
output, once. Everything after that is automatic.

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
