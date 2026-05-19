---
title: "AWS RDS Provisioner Extension"
---

The `aws-rds-provisioner` extension provisions and manages a PostgreSQL instance on AWS RDS for a project.

## What It Does

- Creates and manages an RDS PostgreSQL instance.
- Supports shared or deployment-group-isolated database layouts.
- Injects connection variables into deployments.
- Handles database credential lifecycle.

## Terraform Setup

The `modules/rise-aws` Terraform module includes built-in support for the RDS extension. Enable it with `enable_rds = true` alongside your VPC configuration:

```hcl
module "rise_aws" {
  source = "path/to/modules/rise-aws"
  name   = "rise-backend"

  # ... other options (ECR, IRSA, etc.) ...

  enable_rds              = true
  rds_vpc_id              = "vpc-0abc123"
  rds_subnet_ids          = ["subnet-aaa", "subnet-bbb"]
  rds_allowed_security_groups = ["sg-eks-nodes"]  # allow EKS nodes to reach RDS
}
```

The module creates:
- IAM permissions scoped to RDS instances and subnet groups prefixed with `var.name`
- An RDS DB subnet group for VPC placement
- An RDS security group with ingress rules from the specified security groups or CIDR blocks

The `rise_config` output includes the values needed for the backend extension config:

```hcl
output "rise_config" {
  value = module.rise_aws.rise_config
  # rds.vpc_security_group_ids → backend config vpc_security_group_ids
  # rds.db_subnet_group_name   → backend config db_subnet_group_name
}
```

| Variable | Description |
|----------|-------------|
| `rds_vpc_id` | VPC where RDS instances are placed |
| `rds_subnet_ids` | Subnets for the DB subnet group (across multiple AZs) |
| `rds_allowed_security_groups` | Security groups allowed to reach RDS on port 5432 (e.g., EKS node group SG) |
| `rds_allowed_cidr_blocks` | CIDR blocks allowed to reach RDS on port 5432 (alternative to SGs) |

## Backend Configuration

Configure the extension provider in your Rise backend config:

```yaml
extensions:
  providers:
    - type: aws-rds-provisioner
      region: "eu-west-1"
      instance_size: "db.t4g.micro"
      disk_size: 20
      instance_id_prefix: "rise"        # must match IAM policy prefix in Terraform
      default_engine_version: "16.4"
      vpc_security_group_ids:           # from Terraform rise_config output
        - "sg-0abc123"
      db_subnet_group_name: "rise-backend-rds-xxxx"  # from Terraform rise_config output
      backup_retention_days: 7
      # backup_window: "03:00-04:00"
      # maintenance_window: "sun:04:00-sun:05:00"
```

For non-AWS deployments (Rise running outside EKS), also provide static credentials:

```yaml
      access_key_id: "AKIA..."
      secret_access_key: "${RDS_SECRET_ACCESS_KEY}"
```

| Field | Required | Description |
|-------|----------|-------------|
| `region` | yes | AWS region for RDS instances |
| `instance_size` | yes | RDS instance class (e.g., `db.t4g.micro`) |
| `disk_size` | yes | Allocated storage in GiB |
| `instance_id_prefix` | no | Prefix for RDS identifiers; must match IAM policy (default: `rise`) |
| `instance_id_template` | no | Template for instance IDs (default: `{prefix}-{project_name}-{extension_name}`) |
| `default_engine_version` | no | Default PostgreSQL version if not specified per-project |
| `vpc_security_group_ids` | no | VPC security group IDs for the RDS instance |
| `db_subnet_group_name` | no | DB subnet group for VPC placement |
| `backup_retention_days` | no | Backup retention in days, 1–35 (default: 7) |
| `backup_window` | no | Preferred backup window in UTC (e.g., `03:00-04:00`) |
| `maintenance_window` | no | Preferred maintenance window (e.g., `sun:04:00-sun:05:00`) |

## Project Extension Spec

Users configure the extension per-project through the API or web UI:

```json
{
  "engine": "postgres",
  "engine_version": "16.2",
  "database_isolation": "shared",
  "database_url_env_var": "DATABASE_URL",
  "inject_pg_vars": true
}
```

| Field | Description |
|-------|-------------|
| `engine` | Currently `postgres` |
| `engine_version` | Override the default PostgreSQL version |
| `database_isolation` | `shared` (one DB for all groups) or `isolated` (one DB per deployment group) |
| `database_url_env_var` | Name of the injected connection URL variable (default: `DATABASE_URL`) |
| `inject_pg_vars` | Also inject `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`, `PGPASSWORD` |

## Notes

- Initial provisioning may take several minutes.
- In `shared` mode, all deployment groups share one database.
- In `isolated` mode, each deployment group gets its own database.
