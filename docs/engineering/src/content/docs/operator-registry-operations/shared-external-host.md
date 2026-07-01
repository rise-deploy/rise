---
title: "Shared External Registry (external_registry_hosts)"
---

When multiple source repos push to a single external registry — typically a JFrog instance shared across teams — Rise's default `validate_image_for_project` policy is too permissive: any external image (not under the backend's primary `registry_url` prefix) is allowed. That lets project `evil` POST `{image: "jfrog.example.com/team/compass:tag", image_digest: "..."}` and deploy another project's image into its own namespace.

The fix is a small policy applied via `registry.external_registry_hosts` on the existing ECR provider.

## How it works

```yaml
registry:
  type: ecr
  # ... existing ECR fields ...
  external_registry_hosts:
    - jfrog.example.com
```

When validation runs against a `{image, image_digest}` create-deployment call:

| Image host | Policy applied |
|---|---|
| Under backend's `registry_url` (e.g. ECR) | Default — first path segment after the prefix must equal `project_name`. |
| Listed in `external_registry_hosts` | **Strict** — last path segment before `:` or `@` must equal `project_name`. Rejects everything else. |
| Other external (`nginx:latest`, third-party Helm charts) | Allowed unchecked — preserves the pre-built-image workflow for legitimate third-party images. |

The strict path enforces the invariant **regardless of repo prefix layout**: source repos can structure their JFrog paths however they like (`my-team-playground/my-team-apps/<project>`, `another-team/foo/<project>`), and the last-segment check still gates cross-project substitution.

## Why this lives on the ECR provider

Earlier iterations of this feature shipped a separate `jfrog-static` registry provider, which forced a backend-wide cutover from ECR to JFrog. The current design keeps the backend on ECR (or whatever its primary provider is) and adds the strict policy as an additive operator setting. Source repos opt into the JFrog push path independently via [`rise.toml [registry]`](../../user-guide/client-push); the backend doesn't need to flip modes.

## Pairing with `external_pull_secret_name`

`external_registry_hosts` only governs validation of the **incoming image ref**. To actually pull from the external registry, the operator also sets:

```yaml
deployment_controller:
  external_pull_secret_name: jfrog-shared-pull-secret
```

The controller adds the named secret to every pod's `imagePullSecrets` alongside any registry-provider-minted scoped creds. Typically this secret is materialized cluster-wide by an [External Secrets Operator](https://external-secrets.io/) `ClusterExternalSecret`.

## Startup WARN — the misconfig signal

Rise emits a warning at backend startup when `external_pull_secret_name` is set but `external_registry_hosts` is empty (or the primary registry provider doesn't yet support the field — currently ECR-only). Grep the backend logs for `external_registry_hosts is empty` or `does not support external_registry_hosts` to spot it.

The warn isn't fatal — `external_pull_secret_name` is a pre-existing field used for unrelated cases too (private mirrors that don't share with other projects), so the backend can't decide unilaterally. The operator is expected to read the warning and either add the relevant host to `external_registry_hosts` or confirm the pull secret's scope is project-safe.

## Rollout shape

1. Add `external_registry_hosts` and `external_pull_secret_name` to the backend config. Sync.
2. The backend now accepts JFrog refs only when project name matches the last segment, and pods get the pull secret automatically.
3. Source repos opt in to the JFrog push path at their own pace via [`rise.toml [registry]`](../../user-guide/client-push). No coordinated cutover.
