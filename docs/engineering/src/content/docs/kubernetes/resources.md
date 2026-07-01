---
title: "Resources Reference"
---

## Namespace

Created once per project:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: rise-my-app
  labels:
    app.kubernetes.io/managed-by: "rise"
    rise.dev/project: "my-app"
```

## Secret (Image Pull Credentials)

Created/refreshed automatically for private registries:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: rise-registry-creds
  namespace: rise-my-app
  annotations:
    rise.dev/last-refresh: "2025-12-07T14:30:22Z"
type: kubernetes.io/dockerconfigjson
data:
  .dockerconfigjson: <base64-encoded-docker-config>
```

**Auto-refresh**: Secrets are automatically refreshed every hour to handle short-lived credentials (e.g., ECR tokens expire after 12 hours).

### Configuring Image Pull Secrets

The Kubernetes controller supports three modes for managing image pull secrets:

**1. Automatic Management (with registry provider)**
- When a registry provider is configured (e.g., AWS ECR), the controller automatically creates and refreshes the `rise-registry-creds` secret in each project namespace
- Credentials are fetched from the registry provider on-demand
- Secrets are automatically refreshed every hour
- No additional configuration needed

**2. External Secret Reference**
- For static Docker registries where credentials are managed externally (e.g., manually created secrets, sealed-secrets, external-secrets operator)
- Configure the secret name in the deployment controller settings:

```yaml
deployment_controller:
  type: kubernetes
  # ... other settings ...
  external_pull_secret_name: "my-registry-secret"
```

- The controller will reference this secret name in all Deployments
- The secret must exist in each project namespace before deployments can succeed
- The controller will NOT create or manage this secret
- Useful when:
  - Using a static registry that doesn't support dynamic credential generation
  - Managing secrets through GitOps tools like sealed-secrets or external-secrets operator
  - Using a cluster-wide image pull secret that's pre-configured in all namespaces

**3. No Image Pull Secret**
- When no registry provider is configured and no `external_pull_secret_name` is set
- Deployments will not include any `imagePullSecrets` field
- Only works with public container images or when using Kubernetes cluster defaults

**Example configurations:**

Using AWS ECR (automatic):
```yaml
registry:
  type: ecr
  region: us-east-1
  account_id: "123456789012"
  # ... other ECR settings ...

deployment_controller:
  type: kubernetes
  # No external_pull_secret_name needed - automatically managed
```

Using external secret:
```yaml
registry:
  type: oci-client-auth
  registry_url: "registry.example.com"
  # ... other registry settings ...

deployment_controller:
  type: kubernetes
  # ... other settings ...
  external_pull_secret_name: "my-registry-secret"
```

For external secrets, ensure the secret exists in each namespace:
```bash
# Create secret in namespace
kubectl create secret docker-registry my-registry-secret \
  --docker-server=registry.example.com \
  --docker-username=myuser \
  --docker-password=mypassword \
  -n rise-my-app
```

## Deployment

One per deployment:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app-20251207-143022
  namespace: rise-my-app
  labels:
    app.kubernetes.io/managed-by: "rise"
    rise.dev/project: "my-app"
    rise.dev/environment: "production"
    rise.dev/deployment-group: "default"
    rise.dev/deployment-id: "20251207-143022"
    rise.dev/deployment-uuid: "550e8400-e29b-41d4-a716-446655440000"
spec:
  replicas: 1
  selector:
    matchLabels:
      rise.dev/project: "my-app"
      rise.dev/environment: "production"
      rise.dev/deployment-group: "default"
      rise.dev/deployment-id: "20251207-143022"
      rise.dev/deployment-uuid: "550e8400-e29b-41d4-a716-446655440000"
  template:
    metadata:
      labels:
        rise.dev/project: "my-app"
        rise.dev/environment: "production"
        rise.dev/deployment-group: "default"
        rise.dev/deployment-id: "20251207-143022"
        rise.dev/deployment-uuid: "550e8400-e29b-41d4-a716-446655440000"
    spec:
      serviceAccountName: env-production
      imagePullSecrets:
        - name: rise-registry-creds
      containers:
        - name: app
          image: registry.example.com/my-app@sha256:abc123...
          ports:
            - containerPort: 8080
```

## Service

One per deployment group (updated via server-side apply):

```yaml
apiVersion: v1
kind: Service
metadata:
  name: default
  namespace: rise-my-app
  labels:
    app.kubernetes.io/managed-by: "rise"
    rise.dev/project: "my-app"
    rise.dev/environment: "production"
spec:
  type: ClusterIP
  selector:
    rise.dev/project: "my-app"
    rise.dev/environment: "production"
    rise.dev/deployment-group: "default"
    rise.dev/deployment-id: "20251207-143022"  # Updated on traffic switch
    rise.dev/deployment-uuid: "550e8400-e29b-41d4-a716-446655440000"  # Updated on traffic switch
  ports:
    - port: 80
      targetPort: 8080
      protocol: TCP
```

## Ingress

One per deployment group:

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: default
  namespace: rise-my-app
  labels:
    app.kubernetes.io/managed-by: "rise"
    rise.dev/project: "my-app"
    rise.dev/environment: "production"
  annotations:
    kubernetes.io/ingress.class: "nginx"
spec:
  rules:
    - host: my-app.apps.rise.local
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: default
                port:
                  number: 80
```

## Pod Security Settings

Rise enforces secure-by-default Pod Security Standards for all deployed applications:

**Security context:**
- Containers must run as non-root (enforced, but image chooses UID)
- All Linux capabilities dropped
- Privilege escalation blocked
- Seccomp RuntimeDefault profile applied
- Writable root filesystem (for compatibility)

**Resource limits (configurable):**
- CPU request: 500m, CPU limit: 2, Memory request: 256Mi, Memory limit: 2Gi

**Health probes (configurable):**
- HTTP GET on application port at `/` path
- Initial delay: 10s, period: 10s, timeout: 5s, failure threshold: 3

### Configuration Examples

**Custom resource limits:**
```yaml
deployment_controller:
  type: "kubernetes"
  # ... other fields ...
  pod_resources:
    cpu_request: "50m"
    cpu_limit: "1"
    memory_request: "128Mi"
    memory_limit: "1Gi"
```

**Custom health probes:**
```yaml
deployment_controller:
  health_probes:
    path: "/health"
    initial_delay_seconds: 15
    liveness_enabled: true
    readiness_enabled: true
```

**Disable security context** (not recommended):
```yaml
deployment_controller:
  type: "kubernetes"
  pod_security_enabled: false
```

### Troubleshooting

**Error: "container has runAsNonRoot and image will run as root"**

Your image runs as root (UID 0). Add a USER directive to your Dockerfile:

```dockerfile
# Node.js
USER node

# Python
USER nobody

# Or specific UID
USER 1000:1000
```

Verify with: `docker run --rm <image> id` (should show uid != 0)

**Note:** Railpack doesn't currently support non-root images ([railpack#286](https://github.com/railwayapp/railpack/issues/286)). Use Docker or Pack build backends, or disable pod security.

**Permission denied errors:**
- Ensure files are owned by the non-root user: `COPY --chown=node:node . /app`
- Use `/tmp` for temporary files

**Health probe failures:**
- Check logs: `kubectl logs -n rise-{project} {pod-name}`
- Increase `initial_delay_seconds` if app starts slowly
- Verify app responds at the configured path

**OOMKilled pods:**
- Check events: `kubectl describe pod -n rise-{project} {pod-name}`
- Increase `memory_limit` in configuration
