---
title: "JFrog Artifactory"
---

## Architecture

Rise mints short-lived, project-scoped JFrog access tokens for each push and pull operation. Two token-issuing backends are supported:

- **Vault** — Uses Rise's [`vault-plugin-secrets-artifactory`](https://github.com/rise-deploy/vault-plugin-secrets-artifactory/releases/tag/v1.8.9-rise.2) fork to broker scoped tokens with scope override allowlists. Configure admin `allow_scope_override="opt-in"` and the Rise role with `allow_scope_override=true`.
- **Direct** — Uses JFrog's access token API (`POST /access/api/v1/tokens`) with an admin-scoped token.

## Credential Flow

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

## Artifact Scope Details

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

## JFrog JCR (Free Edition) Limitations

JFrog Container Registry (JCR) does not support the `/api/repositories` REST API for repository management. Docker repositories must be created via the UI or the UI API (`/ui/api/v1/ui/admin/repositories`) with cookie-based session auth. The dev environment's Vault entrypoint (`dev/vault/entrypoint.sh`) demonstrates this approach.

## Token Requirements

- **Vault mode**: Rise's Vault plugin fork with admin `allow_scope_override="opt-in"`, a Rise role with `allow_scope_override=true`, `allowed_scopes` for `artifact:{docker_repo_key}/**:r` and `artifact:{docker_repo_key}/**:r,w`, and an admin-scoped token configured on the Vault plugin.
- **Direct mode**: A JFrog access token with `applied-permissions/admin` scope. This token is used to mint short-lived scoped tokens for each operation.

## Troubleshooting

**`unauthorized: No permission to write manifest`**
- Verify the token scope includes `**` (recursive wildcard). Single `*` does not match nested paths.
- If using BuildKit with attestations, ensure the scope covers the full project path, not just the tag.

**`unauthorized: Not Permitted to upload blob`**
- Verify the scope includes read permissions (`r,w` not just `w`). The push protocol requires HEAD requests to check blob existence.

**`unknown blob` during push**
- The token lacks read permissions. Ensure scope includes `r`.

**BuildKit HTTPS errors against HTTP registry**
- In `buildkitd.toml`, use only `http = true` without `insecure = true`. BuildKit treats `insecure = true` as "use HTTPS with self-signed certs", which overrides `http = true`.
