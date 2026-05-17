---
title: "Custom Domains & TLS"
---

Rise supports custom domains for projects, allowing you to serve your application at your own domain names (e.g., `app.example.com`) instead of or in addition to the default project URL pattern.

## Overview

When custom domains are configured for a project:
- Rise creates a separate Ingress resource specifically for custom domains
- Custom domains always route to the root path (`/`) regardless of the default ingress URL pattern
- TLS certificates can be automatically provisioned using cert-manager integration

## TLS Certificate Management

Rise provides two modes for TLS certificate management on custom domains:

**Per-Domain Mode (Recommended for cert-manager)**

When `custom_domain_tls_mode` is set to `per-domain` (the default), each custom domain gets its own TLS secret named `tls-{domain}`. This mode is designed to work with cert-manager for automatic certificate provisioning:

```yaml
deployment_controller:
  type: kubernetes
  # ... other settings ...
  
  # TLS mode - per-domain creates separate secrets for each custom domain
  custom_domain_tls_mode: "per-domain"  # Default
  
  # Annotations to apply to custom domain ingresses (for cert-manager)
  custom_domain_ingress_annotations:
    cert-manager.io/cluster-issuer: "letsencrypt-prod"
    # Or use a specific issuer per namespace:
    # cert-manager.io/issuer: "letsencrypt-prod"
```

With this configuration:
- Each custom domain (e.g., `app.example.com`) will have its own TLS secret (`tls-app.example.com`)
- cert-manager will automatically provision Let's Encrypt certificates
- Certificates are automatically renewed by cert-manager
- No manual TLS secret management required

**Shared Mode**

When `custom_domain_tls_mode` is set to `shared`, all custom domains share the same TLS secret specified by `ingress_tls_secret_name`:

```yaml
deployment_controller:
  type: kubernetes
  # ... other settings ...
  
  # Shared TLS secret for all hosts (primary + custom domains)
  ingress_tls_secret_name: "my-wildcard-cert"
  
  # Use shared mode
  custom_domain_tls_mode: "shared"
```

This mode is useful when you have a wildcard certificate or want to manage certificates externally.

## Cert-Manager Setup

1. **Install cert-manager in your cluster:**

```bash
kubectl apply -f https://github.com/cert-manager/cert-manager/releases/download/v1.13.0/cert-manager.yaml
```

2. **Create a ClusterIssuer for Let's Encrypt:**

```yaml
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt-prod
spec:
  acme:
    # Let's Encrypt production server
    server: https://acme-v02.api.letsencrypt.org/directory
    email: your-email@example.com
    privateKeySecretRef:
      name: letsencrypt-prod-key
    solvers:
      - http01:
          ingress:
            class: nginx
```

3. **Configure Rise to use cert-manager:**

```yaml
deployment_controller:
  type: kubernetes
  # ... other settings ...
  
  custom_domain_tls_mode: "per-domain"
  custom_domain_ingress_annotations:
    cert-manager.io/cluster-issuer: "letsencrypt-prod"
```

4. **Add a custom domain to your project:**

```bash
rise domain add my-project custom-domain.example.com
```

cert-manager will automatically:
- Create an ACME challenge
- Validate domain ownership
- Issue a Let's Encrypt certificate
- Store it in the `tls-custom-domain.example.com` secret
- Automatically renew certificates before expiration

## DNS Configuration

For custom domains to work, you must configure DNS records to point to your Kubernetes ingress:

```
custom-domain.example.com.  A  <ingress-ip-address>
```

Or for CNAMEs:

```
custom-domain.example.com.  CNAME  <ingress-hostname>
```

## Extra Projected Service Account Tokens

You can configure additional projected service account tokens that Rise mounts into every deployed app pod. This is useful for systems like Vault that expect a Kubernetes service account token with a custom audience.

```yaml
deployment_controller:
  type: kubernetes
  # ... other settings ...
  extra_service_token_audiences:
    vault: "https://vault.example.com"
    metrics: "metrics-service"
```

With this configuration:
- Rise adds a single projected volume to each app pod
- The volume is mounted at `/var/run/secrets/rise/tokens`
- Each map key becomes a filename in that directory
- Each file contains a Kubernetes service account token minted for the configured audience

Examples:
- `/var/run/secrets/rise/tokens/vault`
- `/var/run/secrets/rise/tokens/metrics`

Token rotation and lifetime use Kubernetes defaults; Rise does not currently set `expirationSeconds`.

## Per-Environment ServiceAccounts

Each environment gets its own Kubernetes ServiceAccount named `env-{environment}` (e.g., `env-production`, `env-staging`). The ServiceAccount is created or updated via server-side apply on each deployment reconcile, and pods are configured to use it instead of the namespace's `default` SA.

This is useful for cloud IAM integrations such as AWS IRSA or GCP Workload Identity, where IAM roles are bound to specific ServiceAccounts. By giving each environment its own SA, you can grant different permissions per environment (e.g., production accesses a production database, staging accesses a staging database).

**Example: Annotating the production SA for AWS IRSA**

```bash
kubectl annotate serviceaccount env-production \
  -n rise-my-app \
  eks.amazonaws.com/role-arn=arn:aws:iam::123456789012:role/my-app-production
```

Deployments without an associated environment (legacy deployments) continue to use the namespace's `default` ServiceAccount.

**Backwards compatibility**: By default, deployments in the production environment use the namespace's `default` ServiceAccount instead of creating a dedicated one (`use_default_service_account_for_production` defaults to `true`). This preserves existing IAM bindings (e.g., IRSA annotations) on the `default` SA. Non-production environments still get their own `env-{name}` SAs. To opt out and create a dedicated SA for production as well, set it to `false`:

```yaml
deployment_controller:
  type: "kubernetes"
  # ... other settings ...
  use_default_service_account_for_production: false
```

## Troubleshooting

**Certificate not being issued:**
- Check cert-manager logs: `kubectl logs -n cert-manager deployment/cert-manager`
- Check certificate status: `kubectl get certificate -n rise-<project>`
- Verify DNS is correctly configured and resolves to your ingress
- Check ClusterIssuer/Issuer status: `kubectl describe clusterissuer letsencrypt-prod`

**"Certificate not ready" error:**
- cert-manager is still working on the challenge - wait a few minutes
- Check challenge status: `kubectl get challenges -n rise-<project>`
- Verify ingress controller can handle ACME challenges

**Multiple certificate requests:**
- Check that `custom_domain_ingress_annotations` are correctly configured
- Verify you're not mixing cert-manager annotations in `ingress_annotations` and `custom_domain_ingress_annotations`
