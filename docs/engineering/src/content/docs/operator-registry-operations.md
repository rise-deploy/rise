---
title: "Registry Backend Operations"
---

# Registry Backend Operations

This page is for platform operators maintaining Rise registry integrations.

## Overview

Rise supports multiple registry provider modes through backend configuration.  
Operators are responsible for provider selection, IAM/credentials setup, and production hardening.

## AWS ECR Production Setup

### Architecture: Two-Role Pattern

**Controller Role (`rise-backend`)**:
- Create/delete ECR repositories
- Tag repositories (managed, orphaned)
- Configure repository settings
- Assume the push role

**Push Role (`rise-backend-ecr-push`)**:
- Push/pull images to ECR (under configured prefix)
- Used by backend to generate scoped credentials for CLI workflows

### Terraform Module

Use `modules/rise-aws` to provision ECR access patterns:

```hcl
module "rise_ecr" {
  source = "../modules/rise-aws"

  name        = "rise-backend"
  repo_prefix = "rise/"
  auto_remove = false
}
```

### EKS + IRSA

For Kubernetes-based production installs, prefer IRSA over static credentials and wire the backend service account to an IAM role.

### Non-AWS Runtime

If Rise runs outside AWS, provision an IAM user/keys path and store credentials in a secure secret store.

## GitLab Container Registry

### Credential flow

Rise uses two distinct credential mechanisms for GitLab, depending on the context:

**CLI image push** — The backend mints a short-lived (~15 min) scoped JWT from GitLab's JWT auth endpoint for each push operation:

```
GET <gitlab_url>/jwt/auth?service=container_registry&scope=repository:<namespace>/<project>:push,pull
Authorization: Basic <base64(username:token)>
```

The JWT is injected directly into the container CLI's auth config file (using the `registrytoken` key), bypassing `docker login`. This keeps push credentials out of the host's persistent credential store and limits each token's scope to a single repository.

**Kubernetes image pull secrets** — When `mint_pull_secrets: true`, the controller writes a standard `kubernetes.io/dockerconfigjson` secret containing the PAT into each project's namespace. The container runtime (containerd/CRI-O) reads the PAT and handles its own JWT exchange with GitLab on each pull.

> **Note:** Pre-obtained JWTs cannot be used in Kubernetes pull secrets because containerd does not implement the `registrytoken` field in `dockerconfigjson` (its `ParseAuth` function has no support for pre-obtained bearer tokens). Providing a PAT instead allows the container runtime to perform the full token exchange itself. Follow https://github.com/containerd/containerd/pull/13032 for progress.

### IAM / token requirements

The GitLab token must have `read_registry` and `write_registry` scopes. A [Deploy Token](https://docs.gitlab.com/ee/user/project/deploy_tokens/) scoped to the group is recommended over a personal access token in production.

### Troubleshooting

**`ErrImagePull` / "access forbidden"**
- Verify `mint_pull_secrets: true` is set and the pull secret exists in the project namespace (`kubectl get secret -n rise-<project>`).
- Confirm the token has `read_registry` scope for the namespace.
- Check the token hasn't expired or been revoked.

**`"access": []` in the minted JWT**
- GitLab does not support wildcard repository scopes. The image path in the JWT scope must exactly match `<namespace>/<project>`.

**JWT auth returns non-2xx**
- Verify `gitlab_url` is reachable from the backend pod and that `username`/`token` are correct.

## JFrog Artifactory

### Architecture

Rise mints short-lived, project-scoped JFrog access tokens for each push and pull operation. Two token-issuing backends are supported:

- **Vault** — Uses Rise's [`vault-plugin-secrets-artifactory`](https://github.com/rise-deploy/vault-plugin-secrets-artifactory/releases/tag/v1.8.9-rise.2) fork to broker scoped tokens with scope override allowlists. Configure admin `allow_scope_override="opt-in"` and the Rise role with `allow_scope_override=true`.
- **Direct** — Uses JFrog's access token API (`POST /access/api/v1/tokens`) with an admin-scoped token.

### Credential flow

**CLI image push** — The backend requests a multi-scope token covering the three path groups needed for a Docker/OCI push:

```
artifact:{docker_repo_key}/{project}/{tag}/**:r,w    # manifest
artifact:{docker_repo_key}/{project}/_uploads/**:r,w  # blob staging
artifact:{docker_repo_key}/{project}/sha256*/*:r,w    # content-addressed manifests (BuildKit attestations)
```

The token is used to `docker login` before build+push. Both `docker push` and `docker buildx build --push` (including remote BuildKit with attestations) are supported.

**Kubernetes image pull secrets** — When `mint_pull_secrets: true`, the controller creates `kubernetes.io/dockerconfigjson` secrets with a read-only scoped token:

```
artifact:{docker_repo_key}/{project}/**:r
```

### Artifact scope details

JFrog artifact scopes control which repository paths a token can access. The scope syntax is `artifact:{path}:{permissions}` where permissions are `r` (read), `w` (write), `d` (delete).

Key behaviours:
- `*` matches a single path level; `**` matches recursively
- Multiple scopes can be space-separated in a single token request
- Docker/OCI push requires `r` permission in addition to `w` (the client must HEAD blobs to check existence before uploading)

Push credentials use a multi-scope token covering the three path groups shown above. This is the tightest scope that works for all push methods.

A simpler `{project}/**` scope also works but is less restrictive — it allows writing manifests for any tag, not just the deployment's tag.

Vault should still validate overrides at the repository boundary. With the Rise plugin fork, configure the role allowlist as:

```sh
vault write artifactory/config/admin \
  url="https://jfrog.example.com" \
  access_token="$JFROG_ADMIN_TOKEN" \
  allow_scope_override="opt-in" \
  use_expiring_tokens=true

vault write artifactory/roles/rise \
  scope="artifact:{docker_repo_key}/**:r" \
  default_ttl=600 \
  max_ttl=86400 \
  allow_scope_override=true \
  allowed_scopes='["artifact:{docker_repo_key}/**:r","artifact:{docker_repo_key}/**:r,w"]'
```

This allows Rise's narrower per-operation `artifact:{docker_repo_key}/{project}/...` requests but rejects unrelated Artifactory scopes.

Why `{tag}/**` alone is insufficient: remote BuildKit writes content-addressed manifests (attestations, multi-platform indexes) at `sha256:{digest}/` paths that are siblings of the tag directory, not children. The `sha256*/*` glob matches these paths (the `*` after `sha256` covers the colon and digest).

The following table summarizes scope patterns tested against both `docker push` and `docker buildx build --push` (remote BuildKit with attestations):

```
Scope pattern                                         docker push  buildx --push
----------------------------------------------------------------------------------
{project}/**                                                 PASS           PASS
{project}/*                                                  FAIL           FAIL
{tag}/** + _uploads/**                                       PASS           FAIL
{tag}/*.json + _uploads/**                                   PASS           FAIL
{tag}/* + _uploads/**                                        PASS           FAIL
{tag}/** + _uploads/** + sha256__*/**                        PASS           FAIL
{tag}/** + _uploads/** + sha256*/**                          PASS           PASS
{tag}/** + _uploads/** + sha256*/*                           PASS           PASS
{project}/**  (w only)                                       FAIL           FAIL
{tag}/** + _uploads/** + sha256*/*  (w only)                 FAIL           FAIL
{tag}/** + _uploads/** + sha256*/**  (w only)                FAIL           FAIL
```

Rise uses `{tag}/** + _uploads/** + sha256*/*` — the tightest scope that works for both push methods. The JFrog scope test script lives in the repository under `scripts/jfrog-scope-test.py`.

### JFrog JCR (free edition) limitations

JFrog Container Registry (JCR) does not support the `/api/repositories` REST API for repository management. Docker repositories must be created via the UI or the UI API (`/ui/api/v1/ui/admin/repositories`) with cookie-based session auth. The dev environment's Vault entrypoint (`dev/vault/entrypoint.sh`) demonstrates this approach.

### Token requirements

- **Vault mode**: Rise's Vault plugin fork with admin `allow_scope_override="opt-in"`, a Rise role with `allow_scope_override=true`, `allowed_scopes` for `artifact:{docker_repo_key}/**:r` and `artifact:{docker_repo_key}/**:r,w`, and an admin-scoped token configured on the Vault plugin.
- **Direct mode**: A JFrog access token with `applied-permissions/admin` scope. This token is used to mint short-lived scoped tokens for each operation.

### Troubleshooting

**`unauthorized: No permission to write manifest`**
- Verify the token scope includes `**` (recursive wildcard). Single `*` does not match nested paths.
- If using BuildKit with attestations, ensure the scope covers the full project path, not just the tag.

**`unauthorized: Not Permitted to upload blob`**
- Verify the scope includes read permissions (`r,w` not just `w`). The push protocol requires HEAD requests to check blob existence.

**`unknown blob` during push**
- The token lacks read permissions. Ensure scope includes `r`.

**BuildKit HTTPS errors against HTTP registry**
- In `buildkitd.toml`, use only `http = true` without `insecure = true`. BuildKit treats `insecure = true` as "use HTTPS with self-signed certs", which overrides `http = true`.

## Docker/OCI Registry Mode

For `oci-client-auth` mode, the backend returns target registry information while clients use standard registry auth behavior.

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

## Troubleshooting (Operator-Level)

### ECR access denied

- Verify controller role can assume push role.
- Verify push-role policy scope and repo prefix alignment.
- Verify target repository exists and naming conventions match.

### Docker registry connectivity failures

- Verify registry endpoint reachability from both backend and client environments.
- Verify auth state (`docker login`) and namespace/repo permissions.

## Extending Registry Providers

To add a provider:
1. Implement the registry provider trait in backend registry provider modules.
2. Add provider configuration to registry settings.
3. Register provider selection in provider factory/bootstrap logic.
