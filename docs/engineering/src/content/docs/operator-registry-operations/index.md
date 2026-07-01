---
title: "Registry Backend Operations"
---

This page is for platform operators maintaining Rise registry integrations.

## Overview

Rise supports multiple registry provider modes through backend configuration.  
Operators are responsible for provider selection, IAM/credentials setup, and production hardening.

Supported providers:

- [AWS ECR](aws-ecr)
- [GitLab Container Registry](gitlab)
- [JFrog Artifactory](jfrog)
- [Docker/OCI Registry](docker-oci)

Cross-cutting:

- [Shared External Registry (external_registry_hosts)](shared-external-host) — when source repos opt into [client-controlled push](../../user-guide/client-push) to a shared external registry (e.g. JFrog), this policy + `external_pull_secret_name` wiring keeps the cross-project image-substitution check intact.

## Backend Configuration

Registry configuration is loaded from backend config files under `config/`.

Typical precedence:
1. `{RISE_CONFIG_RUN_MODE}.{toml,yaml,yml}` (required)
2. `local.{toml,yaml,yml}` (optional local overrides)

Use environment variable substitution for secrets and environment-specific values.

## Registry Credentials API

Operator reference endpoint:

```text
GET /api/v1/projects/<project-name>/deployments/<deployment-id>/registry-credentials
```

Credentials are scoped to a specific deployment and are only available while the deployment
is in a pre-push state (Pending, Building, or Pushing). The endpoint returns 409 Conflict
if the deployment has already progressed past the Pushing state.

Returned credentials are provider-specific and intended for authenticated clients.

## Security Recommendations

1. Use least-privilege IAM/policy scope per project.
2. Prefer short-lived credentials and role-based access.
3. Enforce TLS for registry traffic in production.
4. Monitor credential issuance and image push activity.
5. Rotate long-lived/static credentials on a regular cadence.

## Troubleshooting

### Docker registry connectivity failures

- Verify registry endpoint reachability from both backend and client environments.
- Verify auth state (`docker login`) and namespace/repo permissions.

## Extending Registry Providers

To add a provider:
1. Implement the registry provider trait in backend registry provider modules.
2. Add provider configuration to registry settings.
3. Register provider selection in provider factory/bootstrap logic.
