---
title: "Troubleshooting"
---

Common issues and solutions when using Rise.

## Deployment Issues

### Deployment Stuck or Failed

**Check deployment logs:**

```bash
rise deployment logs my-app 20241205-1234
rise deployment logs my-app 20241205-1234 --follow
```

**Check deployment status:**

```bash
rise deployment show my-app:latest --follow
```

### "Image pull failed" or Registry Errors

- Verify the image exists and the tag is correct
- For pre-built images, ensure Rise has access to the registry
- For ECR, check IAM role permissions

### Deployment Reaches "Unhealthy"

- Check that your application listens on the port specified by the `PORT` environment variable (default: 8080)
- Review application logs: `rise deployment logs my-app 20241205-1234`
- Ensure health check endpoint responds

## Build Issues

### Buildpack: CA Certificate Verification Errors

**Symptom:**

```
ERROR: failed to initialize analyzer: validating registry read access
```

**Solution:**

```bash
export SSL_CERT_FILE=/path/to/your/ca-cert.crt
rise deploy
```

Rise automatically injects the certificate into the pack lifecycle container.

### Railpack: BuildKit Experimental Feature Error

**Symptom:**

```
ERROR: requested experimental feature mergeop has been disabled
```

**Solution:**

```bash
docker buildx create --use
```

### Build Fails with SSL Errors

See [SSL & Proxy Configuration](../ssl-proxy) for managed BuildKit daemon setup and certificate injection.

## Authentication Issues

### "Failed to start local callback server"

Ports 8765-8767 are all in use. Close applications using these ports and try `rise login` again.

### "Code exchange failed"

The backend or identity provider may not be running. Check backend logs.

### Token Expired

```bash
rise login
```

Tokens expire after 1 hour by default.

## Service Account Issues

### "The 'aud' claim is required"

Add `--claim aud=https://rise.example.net` when creating the service account:

```bash
rise sa create my-project \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/myrepo
```

### "No service account matched the token claims"

1. Check token claims match exactly (case-sensitive)
2. Verify issuer URL has no trailing slash
3. Ensure ALL service account claims are present in the token

### "Multiple service accounts matched this token"

Make claims more specific to avoid ambiguity (e.g., differentiate by `ref_protected` or `aud`).

### "403 Forbidden" (Service Account)

Service accounts can only deploy, not manage projects. Use a regular user account for project operations.

See [Authentication](../authentication) for full service account setup.

## Inspecting Token Claims

When a deploy is rejected with a permission or "claims do not match" error — most
often from CI, where the token comes from `RISE_TOKEN`, `RISE_TOKEN_COMMAND`, or
auto-detected GitHub Actions OIDC rather than an interactive login — the quickest
way to see *what identity the CLI is actually presenting* is to raise the log
level on the token-resolution path. The CLI logs the resolved token's decoded
header and claims at `debug` (the signature is never logged):

```bash
RUST_LOG=rise::cli::login::token_utils=debug rise deploy
```

```
DEBUG rise::cli::login::token_utils: Using token from RISE_TOKEN environment variable
DEBUG rise::cli::login::token_utils: Token header.claims is {"alg":"RS256","typ":"JWT","kid":"38826b17-..."}.{"iss":"https://token.actions.githubusercontent.com","aud":"https://rise.example.net","sub":"repo:my-org/my-service:environment:production","repository":"my-org/my-service","repository_owner":"my-org","environment":"production","ref":"refs/heads/main","ref_protected":"false","event_name":"push","workflow_ref":"my-org/my-service/.github/workflows/rise.yml@refs/heads/main","exp":1780386362,"iat":1780386062,...}
```

The source label names where the token came from — `RISE_TOKEN environment
variable`, `RISE_TOKEN_COMMAND`, `GitHub Actions OIDC`, or `stored login token`.

Compare the printed `iss`, `aud`, and the rest of the claims against your service
account configuration (`rise sa list -p <project>`). They must match exactly — a mismatched
`iss` (e.g. a trailing slash) or a missing claim is the usual cause of
[`No service account matched the token claims`](#no-service-account-matched-the-token-claims).

`RUST_LOG=debug rise <command>` works too but is much noisier; the
`rise::cli::login::token_utils=debug` target isolates just the token line.

This same line is emitted regardless of token source, so it also surfaces the
claims of tokens **minted through the token provider** — a `RISE_TOKEN_COMMAND`
that prints a JWT, or a GitHub Actions OIDC token minted on demand.

For the *server* side of the same exchange, raise the log level on the backend
instead. Keep the general output at `info` and turn up only the auth and
workload-token paths so the relevant lines aren't buried:

```bash
RUST_LOG=info,rise::server::auth=debug,rise::server::workload_tokens=debug \
  rise backend server
```

This surfaces:

- `rise::server::auth::handlers` — the OIDC ID-token claims during browser login
  (`ID token claims: {...}`)
- `rise::server::auth::middleware` — the issuer the backend peeked at and the
  JWKS-validation outcome for an incoming token
- `rise::server::auth::context` — per-service-account claim-mismatch reasons
  (`SA <id> claim mismatch: ...`); these log at `info`, so they show even without
  the `debug` targets above
- `rise::server::workload_tokens::handlers` — `Issued workload identity token`
  (project / environment / audience / ttl) when an app mints a token

`RUST_LOG=rise=debug` works as a catch-all but is far noisier.

See [Workload Identity Tokens](../workload-identity-tokens) for inspecting the
Rise-signed tokens an app mints for downstream systems.

## Getting Help

- Check deployment logs: `rise deployment logs <project> <deployment-id>`
- Verbose CLI output: `RUST_LOG=debug rise <command>`
- Use `rise --help` or `rise <command> --help` for flag details
