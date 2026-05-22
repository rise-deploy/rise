---
title: "Workload Identity Tokens"
---

Workload identity tokens let a *deployed app* federate its identity to external
systems — AWS STS, GCP Workload Identity Federation, HashiCorp Vault, Snowflake,
and any OIDC-trusting service — without storing long-lived secrets.

Rise issues each app a short-lived, **Rise-signed OIDC JWT** whose claims
describe the *Rise* identity (project + environment), not the underlying
runtime. External systems trust `https://<your-rise>` as an OIDC identity
provider and key their trust policies on `project` / `environment`. Because the
token is Rise-shaped, the downstream trust configuration stays the same even if
the app later moves to a different runtime.

**How it works:** Rise already publishes OIDC discovery
(`/.well-known/openid-configuration`) and a JWKS endpoint (`/api/v1/auth/jwks`).
A workload token is a regular RS256 JWT signed with the same key, so any OIDC
verifier can validate it.

## Token claims

| Claim | Value |
|---|---|
| `iss` | Your Rise backend URL |
| `sub` | `rise:proj:<project>:env:<environment>` (`_none` if the deployment has no environment) |
| `aud` | The audience you requested |
| `exp` / `iat` / `nbf` | Lifetime bounds |
| `jti` | Unique token ID |
| `project`, `environment`, `deployment_group`, `deployment_id` | Informational |

The subject is fixed and not user-configurable — it cannot be used to
impersonate another project or environment.

## Consuming tokens

Every Rise deployment gets a `rise-identity` Secret mounted at a standard,
read-only path. Everything is exposed as files — no environment variables are
injected:

| Path | Contents |
|---|---|
| `/var/run/secrets/rise/identity/credential` | The bootstrap credential |
| `/var/run/secrets/rise/identity/tokens/<name>` | An auto-minted token, one file per configured audience |

There are two ways to obtain a token.

:::note[Not to be confused with `/var/run/secrets/rise/tokens/`]
Your pod may also contain files under `/var/run/secrets/rise/tokens/` — a
**separate** feature. Those are *Kubernetes-issued* ServiceAccount tokens,
configured platform-wide by the Rise operator (not in `.rise.toml`). They carry
Kubernetes-shaped claims and a cluster issuer.

Workload identity tokens, described on this page, live under
`/var/run/secrets/rise/identity/`, are issued and signed by **Rise**, and carry
the Rise `project`/`environment` identity. Use these when you want trust policies
keyed on the Rise identity rather than on Kubernetes.
:::

### 1. Auto-mounted token files (Kubernetes)

List the audiences you need in `.rise.toml`:

```toml
[identity.audiences]
aws = "sts.amazonaws.com"
vault = "https://vault.example.com"
```

The map key is the in-pod filename; the value is the audience. On Kubernetes
the controller mints a token per audience, writes them into the `rise-identity`
Secret, and re-mints them before they expire. Your app just reads the files:

```
/var/run/secrets/rise/identity/tokens/aws     → JWT with aud=sts.amazonaws.com
/var/run/secrets/rise/identity/tokens/vault   → JWT with aud=https://vault.example.com
```

The kubelet keeps the mounted files up to date as Rise refreshes them, so
**always re-read the file** rather than caching the first read.

### 2. The token-exchange endpoint

For runtime-agnostic use, or audiences not known ahead of time, exchange the
bootstrap credential for a token:

```
POST /api/v1/identity/token
Authorization: Bearer <bootstrap credential>
Content-Type: application/json

{ "audience": "sts.amazonaws.com" }
```

Response:

```json
{ "token": "<JWT>", "token_type": "Bearer", "expires_in": 900 }
```

The bootstrap credential stops working once the deployment is torn down or
superseded.

The `rise` CLI wraps this endpoint — useful from a shell inside the pod:

```bash
rise identity token --audience sts.amazonaws.com
```

It reads the credential from `--credential`, falling back to the standard
credential file (`/var/run/secrets/rise/identity/credential`), and prints the
token.

## Example: federating to AWS STS

Register Rise as an OIDC provider in AWS IAM (provider URL = your Rise backend
URL), then trust the project in a role:

```json
{
  "Effect": "Allow",
  "Principal": { "Federated": "arn:aws:iam::<acct>:oidc-provider/<your-rise-host>" },
  "Action": "sts:AssumeRoleWithWebIdentity",
  "Condition": {
    "StringEquals": {
      "<your-rise-host>:sub": "rise:proj:my-app:env:production"
    }
  }
}
```

The app then assumes the role with the Rise token:

```bash
TOKEN=$(cat /var/run/secrets/rise/identity/tokens/aws)
aws sts assume-role-with-web-identity \
  --role-arn arn:aws:iam::<acct>:role/my-app \
  --role-session-name my-app \
  --web-identity-token "$TOKEN"
```
