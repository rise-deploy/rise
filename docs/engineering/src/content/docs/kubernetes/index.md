---
title: "Kubernetes Deployment Backend"
---

The Kubernetes deployment backend deploys applications to Kubernetes clusters using Deployments, Services, and Ingresses.

## Overview

The Kubernetes controller manages application deployments on Kubernetes by:
- Creating namespace-scoped resources for each project
- Deploying applications as Deployments with rolling updates
- Managing traffic routing with Services and Ingresses
- Implementing blue/green deployments via Service selector updates
- Automatically refreshing image pull secrets for private registries

## Resources Managed

Rise creates a `RiseProject` custom resource per project. Metacontroller watches these CRs and manages the following child resources based on the desired state returned by Rise's sync webhook:

| Resource | Scope | Purpose |
|----------|-------|---------|
| Namespace | One per project | Isolates project resources |
| Deployment | One per deployment | Runs application pods |
| Service | One per deployment group | Routes traffic to active deployment |
| Ingress | One per deployment group | Exposes HTTP/HTTPS endpoints |
| Endpoints | One per project (if backend configured) | Backend endpoints for the `rise-backend` Service (applied directly, not via Metacontroller) |
| NetworkPolicy | One per active deployment group | Restricts network access per deployment group |
| ServiceAccount | One per environment | Per-environment workload identity |
| Secret | One per project | Stores image pull credentials |
