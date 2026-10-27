---
title: Operate Rise
description: Configure Rise, operate platform services, and maintain production infrastructure.
---

These docs cover running Rise itself: platform configuration, production infrastructure, observability, and database maintenance. Application developers should use the [user docs](/docs/).

## Deployment infrastructure

Rise supports Kubernetes, Docker, and Amazon ECS. Start with [deployment backends](/operator-docs/deployment-backends/) to compare capabilities and find the installation instructions for your infrastructure.

## Platform operations

- [Operator guide](/operator-docs/operator-guide/) — platform responsibilities and operational workflows.
- [Configuration](/operator-docs/configuration/) — control-plane settings, identity providers, ingress, and deployment defaults.
- [Container registries](/operator-docs/operator-registry-operations/) — image storage and credentials for ECR, GitLab, Artifactory, and Docker/OCI registries.
- [Persistent logs](/operator-docs/persistent-logs/) — log storage, retention, and the Loki backend.
- [Database operations](/operator-docs/database/) — PostgreSQL administration and platform storage.

Read [production deployment](/operator-docs/production/) when preparing an installation for production use.

## Before upgrading

Read the [changelog](/operator-docs/changelog/) for breaking changes, configuration updates, and required operator actions. The [PostgreSQL upgrade guide](/operator-docs/upgrading-postgresql/) covers database version upgrades.
