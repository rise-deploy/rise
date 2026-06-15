---
title: "Production Setup"
---

Guidelines for deploying Rise in production environments.

## Overview

This guide covers security, configuration, database setup, monitoring, and operational considerations for running Rise in production.

## Deployment Backend

Rise deploys applications to Kubernetes clusters. See [Kubernetes](./kubernetes/) for configuration and operational details.

## Security Best Practices

### Registry Credentials

- Use IAM roles (IRSA on EKS, instance profiles on EC2)
- Avoid long-lived IAM user credentials
- Rise generates scoped push credentials (single repo, 12-hour max)

### Network Isolation

- Deploy backend in private subnets with ALB in public subnets
- Enable TLS (HTTPS), terminate at load balancer
- Restrict database to backend security group only

### Secrets Management

Use AWS Secrets Manager or HashiCorp Vault for: `DATABASE_URL`, OAuth2 client secrets, registry credentials, JWT signing keys.

### Authentication

- Use trusted OIDC providers (Dex, Auth0, Okta)
- Configure redirect URLs, enable PKCE
- Dex production: external storage backend (PostgreSQL/etcd), configure SSO connectors, enable TLS. See [Dex docs](https://dexidp.io/docs/kubernetes/)

## Environment Variables

Key environment variables for production:

```bash
# Database (explicitly supported)
DATABASE_URL="postgres://rise:password@rds-endpoint:5432/rise"

# Configuration system
RISE_CONFIG_DIR="/etc/rise"                # Path to config directory
RISE_CONFIG_RUN_MODE="production"          # Which config file to load (production.yaml)
```

**Note**: Additional configuration should be placed in YAML config files rather than environment variables. See the configuration files in `config/` directory for all available options (registry, Kubernetes, auth, etc.).

## Database Setup

### PostgreSQL Configuration

Use managed database (AWS RDS, Cloud SQL, Azure Database).

**Recommended settings**: Multi-AZ, automated storage autoscaling, 7-30 day backup retention, encryption at rest, PostgreSQL 16+

### Database Backups

Enable automated backups (7+ days), take manual snapshots before major changes, enable point-in-time recovery.

### Major Version Upgrades

If you use the Helm chart's built-in PostgreSQL, see [Upgrading PostgreSQL](./upgrading-postgresql.md) for the major version upgrade procedure.

### Connection Pooling

Rise uses SQLx with connection pooling. Configure pool size based on load in `config/production.yaml` if needed.

## High Availability

### Health Checks

- `GET /health` - Overall health
- `GET /ready` - Readiness (database connectivity)

**LB config**: `/health`, 30s interval, 2/3 thresholds, 5s timeout

### Database Failover

RDS Multi-AZ: automatic failover (1-2 min), backend reconnects automatically.

## Monitoring

### Key Metrics

- Request rate/latency (P50, P95, P99), error rate (4xx/5xx)
- Active deployments, build/push times
- CPU/memory, DB connection pool, disk I/O
- Projects created, deployments/day, active users

### Logging

Rise writes logs to stdout. Aggregate with CloudWatch, Cloud Logging, ELK, or Loki+Grafana.

### Alerting

**Critical**: DB connection failures, >5% 5xx rate, controller not reconciling, low disk space
**Warning**: Slow queries (>1s), high CPU (>80%), memory leaks, old deployments

## Disaster Recovery

### Backup Strategy

**Backup**: Database (RDS snapshots — includes all Rise-managed secrets), git-tracked config
**Don't backup**: Container images (in ECR), credentials, binaries

### Recovery

1. Restore database from snapshot
2. Redeploy backend via Helm chart
3. Verify health (migrations run automatically on startup)
4. Restore any externally-managed configuration

## Operational Tasks

### Updating Rise

Update the image tag and upgrade the Helm release:

```bash
helm upgrade rise ./helm/rise --namespace rise --reuse-values --set image.tag=<new-tag>
```

Migrations run automatically on container startup.

### Cleanup

Deployments with `--expire` auto-delete. Manual: `rise deployment stop my-app:20241105-1234`

### Monitoring Database Size

```sql
SELECT pg_size_pretty(pg_database_size('rise'));
SELECT tablename, pg_size_pretty(pg_total_relation_size(schemaname||'.'||tablename))
FROM pg_tables WHERE schemaname = 'public' ORDER BY pg_total_relation_size(schemaname||'.'||tablename) DESC;
```

## Cost Optimization

- **Database**: Right-size instances (start `db.t3.medium`), auto-scale storage, use Reserved Instances
- **ECR**: Lifecycle policies, image compression, cleanup unused repos
- **Compute**: Right-size instances/nodes, spot instances for non-critical, auto-scaling

## Next Steps

- **Configure authentication**: Review the authentication settings in the bundled Rise user docs.
- **Set up CI/CD**: Review the service account workflow in the bundled Rise user docs.
