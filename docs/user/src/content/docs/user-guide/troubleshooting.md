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

## Getting Help

- Check deployment logs: `rise deployment logs <project> <deployment-id>`
- Verbose CLI output: `RUST_LOG=debug rise <command>`
- Use `rise --help` or `rise <command> --help` for flag details
