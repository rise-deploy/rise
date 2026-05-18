---
title: "AWS RDS Provisioner Extension"
---

The `aws-rds-provisioner` extension provisions and manages a PostgreSQL instance on AWS RDS for a project.

## What It Does

- Creates and manages an RDS PostgreSQL instance.
- Supports shared or deployment-group-isolated database layouts.
- Injects connection variables into deployments.
- Handles database credential lifecycle.

## Configuration

```json
{
  "engine": "postgres",
  "engine_version": "16.2",
  "database_isolation": "shared",
  "database_url_env_var": "DATABASE_URL",
  "inject_pg_vars": true
}
```

## Fields

- `engine` (optional): currently `postgres`.
- `engine_version` (optional): specific PostgreSQL version.
- `database_isolation` (optional): `shared` or `isolated`.
- `database_url_env_var` (optional): name of injected DB URL variable.
- `inject_pg_vars` (optional): inject `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`, `PGPASSWORD`.

## Notes

- Initial provisioning may take several minutes.
- In `shared` mode, all deployment groups share one database.
- In `isolated` mode, each deployment group gets its own database.
