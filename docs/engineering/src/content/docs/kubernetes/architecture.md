---
title: "Architecture"
---

## Metacontroller Integration

[Metacontroller](https://metacontroller.github.io/metacontroller/) is a Kubernetes operator that implements the composite-controller pattern on top of a simple webhook protocol — so Rise does not need to run a watch loop or write reconciliation logic from scratch.

**Sync webhook**

Metacontroller calls `POST /api/v1/metacontroller/sync` whenever a `RiseProject` resource changes or the configured resync interval elapses. The request body contains:

- `parent`: the `RiseProject` object (name equals the project slug; spec is intentionally empty — the database is the source of truth)
- `children`: a snapshot of every child resource Metacontroller currently owns, grouped by kind

Rise reads the project state from the database, inspects the observed children to update deployment health/status, then returns the fully-specified set of child resources that should exist. Metacontroller creates, updates, or deletes child resources to match — including garbage-collecting anything no longer returned.

**Finalize webhook**

When a `RiseProject` is deleted, Metacontroller calls `POST /api/v1/metacontroller/finalize` before removing child resources. Rise marks all deployments for the project as `Stopped`, then returns `finalized: true`, at which point Metacontroller deletes the owned children.

**Why Metacontroller**

Using Metacontroller lets Rise express desired cluster state as a stateless function (database state → JSON list of resources) without owning the watch loop, retry logic, or garbage collection. Metacontroller handles watch/cache/retry; Rise handles business logic.

For webhook authentication details, see [Webhook Security](operations#webhook-security).

## Naming Scheme

Resources follow consistent naming patterns:

| Resource | Pattern | Example |
|----------|---------|---------|
| Namespace | `rise-{project}` | `rise-my-app` |
| Deployment | `{project}-{deployment_id}` | `my-app-20251207-143022` |
| Service | `{escaped_group}` | `default`, `mr--26` |
| Ingress | `{escaped_group}` | `default`, `mr--26` |
| ServiceAccount | `env-{environment}` | `env-production`, `env-staging` |
| Secret | `rise-registry-creds` | `rise-registry-creds` |

**Character escaping**: Sequences of characters not in `[A-Za-z0-9-_.]` are replaced with `--`. For example, `mr/26` becomes `mr--26`. Consecutive hyphens (`--`) are disallowed in group names to prevent collisions, and the normalized result must be at most 63 characters (Kubernetes label value limit).

## Deployment Groups and URLs

Each deployment group gets its own Service and Ingress with a unique URL:

| Group | URL Pattern | Example (Subdomain) | Example (Sub-path) |
|-------|-------------|---------------------|-------------------|
| `default` | `production_ingress_url_template` | `my-app.apps.rise.local` | `rise.local/my-app` |
| Custom groups | `staging_ingress_url_template` | `my-app-mr--26.preview.rise.local` | `rise.local/my-app/mr--26` |

**Environments and URL routing**: [Environments](../../user-guide/environments) layer on top of deployment groups to control which URL template applies. The environment marked as **production** determines which deployments receive the production URL (via `production_ingress_url_template`). All other deployments — regardless of their group name — use the staging URL template. Custom domains also attach to production environment deployments only. See [user-guide/environments](../../user-guide/environments) for how to create and configure environments.

## Sub-path vs Subdomain Routing

Rise supports two Ingress routing modes configured globally via URL templates.

**Subdomain Routing** (recommended for production):
- Production: `{project_name}.apps.rise.local`
- Staging: `{project_name}-{deployment_group}.preview.rise.local`
- Each project gets a unique subdomain
- Ingress path: `/` (Prefix type)
- No path rewriting needed
- Requires a wildcard TLS certificate (e.g., `*.apps.rise.example.com` and `*.preview.rise.example.com`) — see [Custom Domains](custom-domains) for cert-manager setup

**Sub-path Routing** (development / single-domain setups only):
- Production: `rise.local/{project_name}`
- Staging: `rise.local/{project_name}/{deployment_group}`
- All projects share the same domain with different paths
- Ingress path: `/{project}(/|$)(.*)` (ImplementationSpecific type with regex)
- Nginx automatically rewrites paths
- Not recommended for production: applications must handle `X-Forwarded-Prefix` correctly, and path-based cookie isolation does not protect project cookies from sibling projects on the same host

For production deployments, subdomain routing with a wildcard certificate is strongly recommended.

### Path Rewriting

For sub-path routing, Nginx automatically rewrites paths so your application receives requests at `/` while preserving the original path prefix:

- **Client request**: `GET https://rise.local/myapp/api/users`
- **Application receives**: `GET /api/users`
- **Headers added**: `X-Forwarded-Prefix: /myapp`

The controller uses the built-in `nginx.ingress.kubernetes.io/x-forwarded-prefix` annotation to add this header. Configure your application to use the `X-Forwarded-Prefix` header when generating URLs to ensure links and assets work correctly.

**Example configuration**:
```yaml
kubernetes:
  production_ingress_url_template: "rise.local/{project_name}"
  staging_ingress_url_template: "rise.local/{project_name}/{deployment_group}"
  auth_backend_url: "http://rise-backend.default.svc.cluster.local:3000"
  auth_signin_url: "https://rise.local"
```

## Blue/Green Deployments

The controller implements blue/green deployments using Service selector updates:

1. **Deploy new Deployment**: Create new Deployment with deployment-specific labels
2. **Wait for health**: Wait until new Deployment pods are ready and pass health checks
3. **Switch traffic**: Update Service selector to point to new deployment labels
4. **Previous deployment**: Old Deployment remains but receives no traffic

This ensures zero-downtime deployments with instant rollback capability.

## Labels

All resources are labeled for management and selection:

```yaml
labels:
  app.kubernetes.io/managed-by: "rise"
  rise.dev/project: "my-app"
  rise.dev/environment: "production"        # present when deployment has an environment
  rise.dev/deployment-group: "default"
  rise.dev/deployment-id: "20251207-143022"
  rise.dev/deployment-uuid: "550e8400-e29b-41d4-a716-446655440000"
```
