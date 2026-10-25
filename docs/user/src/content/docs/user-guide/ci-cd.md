---
title: "CI/CD Setup"
---

The recommended pattern for Rise CI/CD is two service accounts with different environment restrictions: one for production (restricted to protected refs) and one for preview deployments (restricted to the staging environment). This is enforced server-side — a misconfigured pipeline cannot deploy to production using the preview service account.

See [Environments](../environments) for how environments, deployment groups, and service account restrictions work.

## Recommended Setup

| Service Account | Allowed environments | CI trigger |
|-----------------|---------------------|------------|
| `sa-production` | `production` only | Protected branches / tags |
| `sa-preview` | `staging` only | All branches (MR pipelines) |

**Step 1 — Create the environments:**

```bash
rise environment create staging -p my-app --group staging --color blue
```

The default `production` environment (mapped to the `default` group) already exists.

**Step 2 — Create the service accounts** (with environment restrictions set via the web UI or API after creation):

```bash
# Production SA: only protected refs
rise sa create -p my-app \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/my-app \
  --claim ref_protected=true

# Preview SA: any ref, restricted to staging in the web UI
rise sa create -p my-app \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/my-app
```

Then, in the web UI, edit the preview service account and restrict it to the `staging` environment.

**Step 3 — Deploy from CI:**

```yaml
# GitLab CI example
deploy-production:
  only: [tags]
  id_tokens:
    RISE_TOKEN:
      aud: https://rise.example.net
  script:
    - rise deploy -E production --image $CI_REGISTRY_IMAGE:$CI_COMMIT_TAG

deploy-preview:
  except: [tags]
  id_tokens:
    RISE_TOKEN:
      aud: https://rise.example.net
  script:
    - rise deploy -E staging --group mr/$CI_MERGE_REQUEST_IID --expire 7d
```

With this setup:
- Production deployments are served at the project's main URL and custom domain.
- Preview deployments get a staging-environment URL (e.g. `staging--my-app.preview.rise.example.com`) and share staging-scoped variables (e.g. a staging database URL).
- The preview SA is physically unable to deploy to production even if a pipeline is misconfigured — the environment restriction enforces it server-side.

## Promotion-Based Pipeline

Rather than building production from a tag, you can ship to production exactly the image that already ran in staging. Build and deploy once, then [promote](../deployments#promoting-between-environments) that deployment:

```yaml
# GitLab CI example
deploy-staging:
  only: [main]
  id_tokens:
    RISE_TOKEN:
      aud: https://rise.example.net
  script:
    - rise deploy -E staging --status-file deployment.json
  artifacts:
    paths: [deployment.json]

promote-production:
  only: [main]
  needs: [deploy-staging]
  when: manual            # or run it from a tag pipeline after a soak period
  id_tokens:
    RISE_TOKEN:
      aud: https://rise.example.net
  script:
    - rise promote "$(jq -r .deployment_id deployment.json)" -E production --yes
```

`--yes` is required in CI: without a terminal, `rise promote` refuses to wait for confirmation. It still prints the plan (pinned image, resource changes, variable changes) to the job log. The promoting job runs as a service account that may deploy to `production`.

Once every production deployment goes through promotion, an administrator can enforce it:

```bash
rise environment update production --promote-from staging
```

Afterwards, a direct `rise deploy -E production` (a fresh build or `--image`) fails:

```json
HTTP 403
{
  "error": "Environment 'production' only accepts deployments promoted from 'staging'. Promote a staging deployment: rise promote <deployment-id> -E production",
  "error_code": "promotion_source_required",
  "details": { "environment": "production", "promotion_source_environment": "staging" }
}
```

The CLI exits with status 1 and prints the hint. Redeploys and rollbacks of what's already in production keep working.

If the project records [migration barriers](../deployments#migration-compatibility-barriers), a promotion or rollback that would move production back across one fails with `409` (`error_code: "migration_barrier"`). Only add `--force` to a pipeline step deliberately — usually after restoring a matching database snapshot.

For GitHub Actions and other providers, see [Service Accounts](../service-accounts).
