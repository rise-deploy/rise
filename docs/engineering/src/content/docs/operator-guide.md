---
title: "Operator Guide"
---

The Operator Guide covers configuring and running Rise as a platform.

Use this section if you are:

- Operating Rise in shared environments
- Managing backend settings and infrastructure integrations
- Running Rise on Kubernetes or in production environments

For application developer workflows, start with the User Guide.

## Upgrade notes

### Unified container model (one-time deployment re-roll)

Rise models every deployment as one or more containers — a single-container app
is the one-container case (an implicit container named `app`). The on-cluster
resource names reflect this: the Deployment is `<project>-<deployment_id>-app`,
its Service is `<group>-app` (on the app's own port), and the ingress backend
points at that Service.

Upgrading from a release that predates this model recreates the K8s resources
for every running deployment on the first reconcile: each app's Deployment and
Service are renamed and the ingress backend moves, so **every app restarts once**.
Existing container images are reused (the synthesized `app` pulls the
deployment's existing image tag — nothing is rebuilt), and both subdomain and
sub-path routing are preserved. Schedule a maintenance window for the upgrade.
