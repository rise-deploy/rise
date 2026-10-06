---
title: "Persistence"
---

Rise runs your app as a stateless container: anything written to the container's
filesystem is lost on the next deployment. Keep state in a backing service
instead.

The recommended way for an app to reach a backing service is its
[workload identity](../user-guide/workload-identity-tokens/). Every deployment
holds a short-lived, Rise-signed token naming its project and environment, so a
service that trusts Rise as an OIDC provider can hand each app the access that
fits it — `my-app` in `staging` sees staging data, never production's, and
never another project's — with no long-lived secret to store or rotate.

## Options

| Need | Option | How it is wired |
|---|---|---|
| Secrets and small key/value config | [OpenBao / HashiCorp Vault](./openbao-vault/) | Workload identity → KV v2, one path per project and environment |
| PostgreSQL | [AWS RDS Provisioner](../extensions/aws-rds-provisioner/) | Project extension; injects connection variables |
| Object storage | [AWS S3 Bucket](../extensions/aws-s3-bucket/) | Project extension |
| Anything that trusts OIDC (AWS IAM, GCP, …) | [Workload Identity Tokens](../user-guide/workload-identity-tokens/) | Federate the token directly |
