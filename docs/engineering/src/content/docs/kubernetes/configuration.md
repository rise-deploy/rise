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
