---
title: "Extend Rise"
description: Configure the project extension providers for databases, object storage, and OAuth integrations.
---

Project extensions add managed capabilities to a project, such as databases, object storage, or OAuth providers. Operators configure provider credentials and platform defaults; application teams configure the integrations available to their projects.

## Provider configuration

- [AWS RDS Provisioner](aws-rds-provisioner.md) — managed PostgreSQL on AWS RDS
- [AWS S3 Bucket](aws-s3-bucket.md) — managed S3 bucket with injected credentials
- [OAuth](oauth.md) — end-user OAuth 2.0 / OIDC integration
- [Snowflake OAuth Provisioner](snowflake-oauth-provisioner.md) — automated Snowflake security integration

## Resource-based extensions

The [Resource API](/operator-docs/resources/) describes the generic resource model and the intended direction for extending Rise with custom kinds and controllers. That API is a work in progress and is not yet in general production use.
