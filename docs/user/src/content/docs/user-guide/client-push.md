---
title: "Client-Controlled Push (rise.toml [registry])"
---

By default, Rise mints push credentials for its operator-configured registry (ECR, GitLab, JFrog, etc.) and you `rise deploy` — the backend hands the CLI scoped creds, the CLI builds and pushes, Rise records the deployment.

The **client-controlled push** path inverts the credential ownership: your source repo owns the registry path, your CLI handles auth ambiently (Vault token, docker login, whatever), and Rise stores the result as a pre-built image. Use this when:

- Your source repo pushes to a registry Rise wasn't configured against (e.g. your team's JFrog while the backend's primary registry is ECR).
- You want CI to manage its own registry creds (a Vault role, a GitLab JWT exchange) instead of having Rise broker tokens for it.
- You're consolidating many source repos onto a shared registry path and want each repo to declare its own image base.

## Opt in: `rise.toml [registry]`

Add a `[registry]` block to your project's `rise.toml`:

```toml
[project]
name = "my-app"

[registry]
image_base = "jfrog.example.com/my-team-playground/my-team-apps"
```

When `[registry]` is present, `rise deploy`:

1. Tags `{image_base}/{project}:{deployment_id}` — `jfrog.example.com/my-team-playground/my-team-apps/my-app:<uuidv7>`.
2. Builds and pushes ambiently (see [Auth](#auth) below).
3. Reads the `sha256:...` digest back from `docker push`.
4. POSTs `{image, image_digest}` to Rise as a pre-built deploy.

Rise validates the image ref against the operator's policy (see [Cross-Project Validation](#cross-project-validation)) and creates the deployment with `image_digest` pinned — the kubelet pulls by digest, so the deployment is reproducible even if the tag is reused.

## Workspace inheritance: `rise.workspace.toml`

For source repos with many apps under one umbrella registry path, declare the `[registry]` once at the repo root:

```
my-repo/
├── .git/
├── rise.workspace.toml      # workspace defaults
└── apps/
    ├── frontend/rise.toml   # inherits [registry] from above
    ├── api/rise.toml
    └── worker/rise.toml
```

```toml
# rise.workspace.toml
[registry]
image_base = "jfrog.example.com/my-team-playground/my-team-apps"
```

When `rise deploy` runs in any subdirectory, the CLI walks up looking for `rise.workspace.toml` (stops at the first `.git` boundary), and merges its `[registry]` (and other defaults) into the leaf's effective config. Leaf-level `[registry]` in `apps/<x>/rise.toml` overrides the workspace.

## Per-environment override

Often you want different image bases per environment — e.g. MR pipelines push to a playground repo (anyone can push) and `develop` pushes to a snapshot repo (write-token gated to the protected branch). Use `[environments.<name>.registry]`:

```toml
# rise.workspace.toml
[registry]
image_base = "jfrog.example.com/my-team-playground/my-team-apps"

[environments.production.registry]
image_base = "jfrog.example.com/my-team-snapshot/my-team-apps"
```

When `rise deploy --environment production` resolves to the `production` env, the CLI picks `[environments.production.registry]`. Other environments (including the workspace default for MR/staging deploys) use the top-level `[registry]`.

## Auth

The CLI doesn't require any Rise-mediated auth for the push — your container CLI authenticates to the registry however it normally would. Common patterns:

### Vault-minted JFrog token (laptop)

One-off CLI config (writes `~/.config/rise/config.json`):

```bash
rise vault configure \
  --address https://vault.example.com:8200 \
  --auth-path <your-oidc-mount-path> \
  --auth-role <your-oidc-role> \
  --artifactory-token-path '<vault/kv/path/user_token/{email}>'
```

All four flags are required — Rise ships no defaults for these (they're
operator-specific). Ask your platform team what to put where, or check
your organization's rise-deployment docs.

Then `rise deploy` against any `[registry]` project:

1. Refreshes the Vault OIDC session if needed (browser, ~once a day).
2. Mints a fresh JFrog access token.
3. `docker login <host>` with `username=email, password=token`.
4. Builds and pushes.

No JFrog creds in source repos, no `docker login` to remember.

### Vault JWT (GitLab CI)

GitLab issues an ID token (`id_tokens` in `.gitlab-ci.yml`) that Vault accepts via the `jwt` auth method:

```yaml
default:
  id_tokens:
    VAULT_ID_TOKEN:
      aud: $VAULT_SERVER_URL

job:
  secrets:
    DOCKER_AUTH_CONFIG:
      token: $VAULT_ID_TOKEN
      vault:
        path: <vault/kv/path/publish-...>
        field: docker_auth_config
```

GitLab Runner writes `$DOCKER_AUTH_CONFIG` to the job container's `~/.docker/config.json`, so `docker push` finds the creds ambiently. The new CLI's `ensure_jfrog_docker_login_if_configured` is a no-op when Vault CLI config isn't set, so the build/push proceeds with whatever auth is already in `~/.docker/config.json`.

### Anything else

Whatever puts valid creds in `~/.docker/config.json` works — `docker login` you ran by hand, a credential helper, mounted secrets, etc. The CLI doesn't care.

## Cross-Project Validation

To prevent project `evil` from claiming project `compass`'s JFrog image ref, the operator's `validate_image_for_project` policy runs against every `{image, image_digest}` POST. Default behavior:

- Images under the configured backend registry prefix (`<registry_url>/...`) must have project name as the first path segment after the prefix.
- Other external images are allowed (so `nginx:latest` etc. work for the pre-built-image flow).

When the backend has `registry.external_registry_hosts: [jfrog.example.com]` set (operator-side config), images on those hosts get the **strict policy**: the last path segment before `:`/`@` must equal the project name. Source repos pushing to a shared external registry can't substitute each other's refs.

If your push lands on a strict host but the path doesn't end in `<project>`, the create-deployment API rejects the call with a clear error. Fix: re-tag with the correct project name segment (the CLI does this automatically when `[registry]` is used — `{image_base}/{project}:{deployment_id}`).

## Local Dev

`rise deploy` against a personal backend works identically. If the backend is configured without `external_registry_hosts`, any image ref under the JFrog host is accepted; if `external_registry_hosts` is set, the same policy applies. See [Local Development](./local-development) for the personal-backend setup.

## When NOT to use this

The default backend-vended push path is simpler and still recommended when:

- Your source repo uses Rise's primary registry (typically ECR) — no opt-in needed.
- You don't have a Vault/JWT auth chain already wired for the registry you want to push to.
- You want Rise to manage credential lifetime / scoping (the legacy path mints short-lived scoped creds per deploy; the client-push path leaves it to you).
