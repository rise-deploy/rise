---
title: "Upgrading PostgreSQL (Helm Chart)"
---

This guide covers upgrading the PostgreSQL major version for the Helm chart's built-in PostgreSQL (`postgresql.enabled: true`). If you use a managed database (RDS, Cloud SQL, etc.), use your provider's upgrade tooling instead.

PostgreSQL major version upgrades (e.g., 16 to 18) are **not** in-place compatible. A newer PostgreSQL binary cannot read a data directory created by an older major version. The Helm chart includes init containers that detect version mismatches and can perform the upgrade automatically using [pgautoupgrade](https://github.com/pgautoupgrade/pgautoupgrade).

## Prerequisites

- `kubectl` access to the cluster
- Helm 3

## Procedure

### 1. Back up your database

```bash
kubectl exec -it <postgresql-pod> -- pg_dump -U rise rise > rise-backup.sql
```

Replace `<postgresql-pod>` with your actual pod name (e.g., `rise-postgresql-0`).

### 2. Upgrade with pgautoupgrade enabled

Ensure `postgresql.image.tag` is set to the target major version and that `postgresql.upgrade.image.tag` matches the same major version (e.g., both targeting PG 18). Then enable the upgrade:

```bash
helm upgrade <release> <chart> \
  --set postgresql.upgrade.enabled=true \
  --set postgresql.image.tag="18-trixie" \
  --set postgresql.upgrade.image.tag="18-trixie"
```

With `postgresql.upgrade.enabled=true`, the pod runs a single **pg-upgrade** init container (using the pgautoupgrade image) before starting PostgreSQL. This container detects the version mismatch and migrates the data directory automatically.

Watch the pod logs to confirm the upgrade completes:

```bash
kubectl logs <postgresql-pod> -c pg-upgrade -f
```

### 3. Verify

```bash
kubectl exec -it <postgresql-pod> -- psql -U rise -c "SELECT version();"
```

### 4. Disable the upgrade flag

Once verified, disable the upgrade flag so the pgautoupgrade init container no longer runs:

```bash
helm upgrade <release> <chart> \
  --set postgresql.upgrade.enabled=false
```

### 5. Clean up (optional)

After a successful upgrade, pgautoupgrade may leave the old data directory on the PVC. To reclaim space:

```bash
kubectl exec -it <postgresql-pod> -- rm -rf /var/lib/postgresql/data/pgdata_old
```

## What happens if you skip the upgrade flag?

If the chart's PostgreSQL image is a newer major version than the existing data directory, the **pg-version-check** init container will fail with a clear error message and instructions. The PostgreSQL pod will not start until you either:

- Set `postgresql.upgrade.enabled=true` and run `helm upgrade`, or
- Pin `postgresql.image.tag` back to the previous major version

## Troubleshooting

**pg-upgrade init container fails**: Check the logs with `kubectl logs <pod> -c pg-upgrade`. The most common cause is insufficient disk space on the PVC -- `pg_upgrade --link` needs minimal extra space, but some temporary space is still required.

**PostgreSQL won't start after upgrade**: Check `kubectl logs <pod> -c postgresql`. If the data directory is corrupted, restore from the backup taken in step 1.
