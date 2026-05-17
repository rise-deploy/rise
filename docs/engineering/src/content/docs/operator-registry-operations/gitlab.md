---
title: "GitLab Container Registry"
---

## Credential Flow

Rise uses two distinct credential mechanisms for GitLab, depending on the context:

**CLI image push** — The backend mints a short-lived (~15 min) scoped JWT from GitLab's JWT auth endpoint for each push operation:

```
GET <gitlab_url>/jwt/auth?service=container_registry&scope=repository:<namespace>/<project>:push,pull
Authorization: Basic <base64(username:token)>
```

The JWT is injected directly into the container CLI's auth config file (using the `registrytoken` key), bypassing `docker login`. This keeps push credentials out of the host's persistent credential store and limits each token's scope to a single repository.

**Kubernetes image pull secrets** — When `mint_pull_secrets: true`, the controller writes a standard `kubernetes.io/dockerconfigjson` secret containing the PAT into each project's namespace. The container runtime (containerd/CRI-O) reads the PAT and handles its own JWT exchange with GitLab on each pull.

> **Note:** Pre-obtained JWTs cannot be used in Kubernetes pull secrets because containerd does not implement the `registrytoken` field in `dockerconfigjson` (its `ParseAuth` function has no support for pre-obtained bearer tokens). Providing a PAT instead allows the container runtime to perform the full token exchange itself. Follow https://github.com/containerd/containerd/pull/13032 for progress.

## IAM / Token Requirements

The GitLab token must have `read_registry` and `write_registry` scopes. A [Deploy Token](https://docs.gitlab.com/ee/user/project/deploy_tokens/) scoped to the group is recommended over a personal access token in production.

## Troubleshooting

**`ErrImagePull` / "access forbidden"**
- Verify `mint_pull_secrets: true` is set and the pull secret exists in the project namespace (`kubectl get secret -n rise-<project>`).
- Confirm the token has `read_registry` scope for the namespace.
- Check the token hasn't expired or been revoked.

**`"access": []` in the minted JWT**
- GitLab does not support wildcard repository scopes. The image path in the JWT scope must exactly match `<namespace>/<project>`.

**JWT auth returns non-2xx**
- Verify `gitlab_url` is reachable from the backend pod and that `username`/`token` are correct.
