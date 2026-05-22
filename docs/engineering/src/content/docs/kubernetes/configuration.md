---
title: "Configuration"
---

## YAML Configuration

```yaml
kubernetes:
  # Optional: path to kubeconfig (defaults to in-cluster if not set)
  kubeconfig: "/path/to/kubeconfig"

  # Ingress class to use
  ingress_class: "nginx"

  # Ingress URL template for production (default) deployment group
  # Supports both subdomain and sub-path routing (must contain {project_name})
  production_ingress_url_template: "{project_name}.apps.rise.local"

  # Optional: Ingress URL template for staging (non-default) deployment groups
  # Must contain both {project_name} and {deployment_group} placeholders
  staging_ingress_url_template: "{project_name}-{deployment_group}.preview.rise.local"

  # Or for sub-path routing:
  # production_ingress_url_template: "rise.local/{project_name}"
  # staging_ingress_url_template: "rise.local/{project_name}/{deployment_group}"

  # Namespace format (must contain {project_name})
  namespace_format: "rise-{project_name}"

  # Custom domain TLS mode
  # - "per-domain": Each custom domain gets its own tls-{domain} secret (for cert-manager)
  # - "shared": All custom domains share ingress_tls_secret_name
  custom_domain_tls_mode: "per-domain"  # Default

  # Annotations for custom domain ingresses (e.g., cert-manager integration)
  custom_domain_ingress_annotations:
    cert-manager.io/cluster-issuer: "letsencrypt-prod"
```

## Kubeconfig Options

The controller supports two authentication modes:

**In-cluster mode** (recommended for production):
- Omit `kubeconfig` setting
- Uses service account mounted at `/var/run/secrets/kubernetes.io/serviceaccount/`
- Requires RBAC permissions for the controller's service account

**External kubeconfig**:
- Set `kubeconfig` path explicitly
- Useful for development or external cluster access
- Falls back to `~/.kube/config` if path not specified

## Extra Projected Service Account Tokens

The `extra_service_token_audiences` deployment controller option mounts
additional **Kubernetes-issued** ServiceAccount tokens into every deployed app
pod. This is useful for systems like Vault that expect a Kubernetes service
account token minted for a custom audience.

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
- Each map key becomes a filename in that directory (validated as a safe path
  segment — letters, numbers, `.`, `_`, `-`)
- Each file contains a Kubernetes service account token minted for the
  configured audience

Examples:
- `/var/run/secrets/rise/tokens/vault`
- `/var/run/secrets/rise/tokens/metrics`

The kubelet mints and rotates these tokens; lifetime uses Kubernetes defaults
(Rise does not set `expirationSeconds`). Because they are issued by the
cluster, their `iss` and claims are Kubernetes-shaped.

This is platform-wide configuration — it applies to **every** deployment and
is not controllable per project.

:::note[Not the same as workload identity tokens]
These are *Kubernetes-issued* SA tokens. They are distinct from **Rise workload
identity tokens** (`/var/run/secrets/rise/identity/`), which are issued and
signed by Rise itself, describe the Rise project/environment, and are
configured per project in `.rise.toml`. See the
[Workload Identity Tokens](../../user-guide/workload-identity-tokens) user guide.
:::
