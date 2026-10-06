---
title: "Persistence"
---

Rise apps run as stateless containers and keep their state in backing
services. The setups here connect those services to Rise apps through
[workload identity](/docs/user-guide/workload-identity-tokens/): each
deployment holds a short-lived, Rise-signed token naming its project and
environment, so a service that trusts Rise as an OIDC issuer can scope each
app to its own data without long-lived secrets.

| Service | Setup |
|---|---|
| OpenBao / HashiCorp Vault (KV v2) | [OpenBao / Vault](./openbao-vault/) |
| PostgreSQL on AWS RDS | [AWS RDS Provisioner extension](../extensions/aws-rds-provisioner/) |
| S3 buckets | [AWS S3 Bucket extension](../extensions/aws-s3-bucket/) |

The user-facing side of each is in the user docs'
[Persistence](/docs/persistence/) section.
