# Rise Helm Chart

This Helm chart deploys the Rise application on Kubernetes, including the backend API server and optionally Dex for OIDC authentication.

## Prerequisites

- Kubernetes 1.32+
- Helm 3.0+
- PostgreSQL database (can be deployed as a dependency or use external database)

## Installation

### Using default values (for testing only)

```bash
helm install rise ./helm/rise
```

### Production installation with custom values

Create a `values-prod.yaml` file:

```yaml
image:
  repository: ghcr.io/rise-deploy/rise
  tag: "0.4.0"
  pullPolicy: IfNotPresent

# Rise configuration in YAML format
config:
  server:
    host: "0.0.0.0"
    port: 3000
    public_url: "https://rise.example.com"

  auth:
    issuer: "https://dex.example.com/dex"
    client_id: "rise-backend"
    # client_secret comes from OIDC_CLIENT_SECRET via envFrom below.
    admin_users:
      - "admin@example.com"
    # Alternatively (or additionally), grant the admin role by IdP group:
    # admin_idp_groups:
    #   - "platform-admins"

  registry:
    type: "oci-client-auth"
    registry_url: "registry.example.com"
    namespace: "rise-apps"

  kubernetes:
    ingress_class: "nginx"
    production_ingress_url_template: "{project_name}.apps.example.com"
    staging_ingress_url_template: "{project_name}--{deployment_group}.preview.example.com"
    auth_backend_url: "http://rise.default.svc.cluster.local:3000"
    auth_signin_url: "https://rise.example.com"

  # Optional: preserve historic namespace naming (otherwise namespaces are
  # synthesized as "org-{discriminator}-{project_name}" from the bootstrapped
  # default Organization).
  default_organization:
    kubernetes_namespace_prefix: "rise-"

ingress:
  enabled: true
  className: "nginx"
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt-prod
  hosts:
    - host: rise.example.com
      paths:
        - path: /
          pathType: Prefix
  tls:
    - secretName: rise-tls
      hosts:
        - rise.example.com

# Sensitive configuration, including OIDC_CLIENT_SECRET
envFrom:
  - secretRef:
      name: rise-secrets

# Server container resources
server:
  resources:
    limits:
      cpu: 2000m
      memory: 1Gi
    requests:
      cpu: 500m
      memory: 256Mi

# Optional: Enable Dex for OIDC authentication
# Optional: Enable Dex for OIDC authentication (see Dex Configuration below)
dex:
  enabled: true
  config:
    issuer: "https://dex.example.com/dex"
    storage:
      type: kubernetes
      config:
        inCluster: true
    web:
      http: "0.0.0.0:5556"
    staticClients:
      - id: rise-backend
        redirectURIs:
          - "https://rise.example.com/api/v1/auth/callback"
        name: "Rise Backend"
        secret: "<client secret>"
    connectors:
      - type: github
        id: github
        name: GitHub
        config:
          clientID: $GITHUB_CLIENT_ID
          clientSecret: $GITHUB_CLIENT_SECRET
          redirectURI: https://dex.example.com/dex/callback
```

Create the secrets:

```bash
kubectl create secret generic rise-secrets \
  --from-literal=OIDC_CLIENT_SECRET=YOUR_CLIENT_SECRET \
  --from-literal=DATABASE_URL=postgres://rise:YOUR_DB_PASSWORD@postgresql:5432/rise \
  --from-literal=RISE_JWT_SIGNING_SECRET="$(openssl rand -base64 32)"
```

Install the chart:

```bash
helm install rise ./helm/rise -f values-prod.yaml
```

## Configuration

The following table lists the configurable parameters of the Rise chart and their default values.

### Core Parameters

| Parameter | Description | Default |
|-----------|-------------|---------|
| `replicaCount` | Number of server replicas | `1` |
| `image.repository` | Image repository | `ghcr.io/rise-deploy/rise` |
| `image.tag` | Image tag | `""` (uses Chart appVersion) |
| `image.pullPolicy` | Image pull policy | `IfNotPresent` |
| `config` | Rise configuration in YAML format | See values.yaml |
| `logs.backend` | Runtime log backend (`kubernetes` or `loki`) | `kubernetes` |
| `logs.loki.enabled` | Install bundled Grafana Loki subchart | `false` |
| `logs.loki.externalUrl` | External Loki URL when not using bundled Loki | `""` |
| `logs.loki.tenantId` | Loki tenant id sent as `X-Scope-OrgID` (multi-tenant Loki only) | `""` |
| `logs.loki.timeoutSeconds` | Backend HTTP timeout for Loki queries | `10` |
| `logs.loki.retentionHint` | Display-only retention hint for empty log states | `""` |
| `logs.loki.bearerTokenSecret.name` | Secret holding a bearer token for the backend's Loki client (also requires matching Alloy `extraEnv`, see below) | `""` |
| `logs.loki.bearerTokenSecret.key` | Key inside the secret holding the token | `token` |
| `logs.loki.labels.project` | Loki stream label that carries the Rise project name | `rise_project` |
| `logs.loki.labels.deploymentId` | Loki stream label that carries the Rise deployment id | `rise_deployment_id` |
| `logs.loki.labels.container` | Loki stream label that carries the deployment's container name | `container` |
| `logs.alloy.enabled` | Install bundled Grafana Alloy subchart for Pod log shipping | `false` |

When pointing Rise at an operator-managed Loki whose existing labels differ
(e.g. `app` / `instance`), override `logs.loki.labels.project` and
`logs.loki.labels.deploymentId` to match. When the bundled Alloy subchart is
also enabled, those names flow through to its relabel rules so backend
queries and shipped logs stay in sync.

The bundled Alloy uses a cluster-wide Pod watch (filtered to
`app.kubernetes.io/managed-by=rise`); see the [Loki backend doc](../../docs/engineering/src/content/docs/persistent-logs/loki.md)
for the RBAC trade-off and how to swap in your own shipper if you need
tighter scoping.

#### Loki bearer token

When `logs.loki.bearerTokenSecret.name` is set, the Rise backend reads the token
from `RISE_LOKI_BEARER_TOKEN`. Alloy must receive the *same* token to push to
the external Loki — wire it via the subchart's `extraEnv`:

```yaml
logs:
  backend: loki
  loki:
    enabled: false
    externalUrl: "https://loki.example.internal"
    bearerTokenSecret:
      name: loki-token
      key: token
  alloy:
    enabled: true

alloy:
  alloy:
    extraEnv:
      - name: RISE_LOKI_BEARER_TOKEN
        valueFrom:
          secretKeyRef:
            name: loki-token
            key: token
```
| `env` | Additional environment variables as key-value pairs | `[]` |
| `envFrom` | List of sources to populate environment variables (ConfigMaps/Secrets) | `[]` |
| `ingress.enabled` | Enable ingress | `false` |

### Server

| Parameter | Description | Default |
|-----------|-------------|---------|
| `server.resources` | Server container resource requests/limits | See values.yaml |

### Dex (Optional OIDC Provider)

| Parameter | Description | Default |
|-----------|-------------|---------|
| `dex.enabled` | Enable Dex deployment | `false` |
| `dex.replicaCount` | Number of Dex replicas | `1` |
| `dex.image.repository` | Dex image repository | `ghcr.io/dexidp/dex` |
| `dex.image.tag` | Dex image tag | `v2.45.1` |
| `dex.service.port` | Dex HTTP port | `5556` |
| `dex.service.grpcPort` | Dex gRPC port | `5557` |
| `dex.config` | Dex configuration file, as YAML; ships no users | See values.yaml |

### PostgreSQL

The chart can deploy a PostgreSQL database using a simple StatefulSet.

| Parameter | Description | Default |
|-----------|-------------|---------|
| `postgresql.enabled` | Enable PostgreSQL deployment | `false` |
| `postgresql.image.repository` | PostgreSQL image repository | `postgres` |
| `postgresql.image.tag` | PostgreSQL image tag | `18-trixie` |
| `postgresql.image.pullPolicy` | Image pull policy | `IfNotPresent` |
| `postgresql.auth.username` | PostgreSQL username | `rise` |
| `postgresql.auth.password` | PostgreSQL password | `rise123` |
| `postgresql.auth.database` | PostgreSQL database | `rise` |
| `postgresql.persistence.enabled` | Enable persistent storage | `true` |
| `postgresql.persistence.size` | Storage size for PostgreSQL data | `8Gi` |
| `postgresql.persistence.storageClass` | Storage class (empty = cluster default) | `""` |
| `postgresql.resources.requests.cpu` | CPU request | `100m` |
| `postgresql.resources.requests.memory` | Memory request | `256Mi` |
| `postgresql.resources.limits.cpu` | CPU limit | `500m` |
| `postgresql.resources.limits.memory` | Memory limit | `512Mi` |

**Automatic DATABASE_URL Injection:**
When `postgresql.enabled: true`, the chart automatically injects the `DATABASE_URL` environment variable into all containers using the configured credentials. You don't need to manually configure it via `envFrom`.

**Security Warning:**
The default password (`rise123`) is for development only. In production, override it using a Kubernetes Secret:
```yaml
postgresql:
  auth:
    password: ""  # Leave empty in values.yaml
# Create secret manually:
# kubectl create secret generic rise-postgresql-password --from-literal=password=YOUR_SECURE_PASSWORD
# Then reference it in envFrom or mount as volume
```

**External PostgreSQL:**
To use an external PostgreSQL database instead, keep `postgresql.enabled: false` and provide `DATABASE_URL` via a Secret referenced in `envFrom`.

## Architecture

The Rise backend runs as a single process that includes:

1. **API Server**: Handles HTTP requests and user interactions
2. **Background Controllers**: Manage project lifecycle, deployments, and registry credentials

The backend can optionally be deployed alongside:

- **Dex**: OIDC provider for authentication (optional, disabled by default, runs as a separate deployment)

## Security Considerations

### RBAC and Namespace Management

When Kubernetes deployment is configured (via `config.kubernetes`), Rise requires cluster-wide permissions via ClusterRole and ClusterRoleBinding to:

- Create, manage, and delete namespaces
- Deploy applications (Deployments, Services, Ingresses, NetworkPolicies) within those namespaces

**Important Security Notes:**

1. **Namespace Isolation**: Kubernetes RBAC does not support wildcard patterns for namespace names in ClusterRoles. Rise requires cluster-wide permissions to manage namespaces dynamically.

2. **Recommended Practices**:
   - Configure `default_organization.kubernetes_namespace_prefix` (e.g., `rise-`) so namespaces follow a consistent prefix
   - Use admission controllers (OPA/Gatekeeper) to enforce namespace naming policies
   - Implement network policies to isolate Rise-managed namespaces
   - Enable audit logging to monitor namespace creation and resource access
   - Consider using namespace quotas to limit resource consumption

3. **Example Enforcement with OPA Gatekeeper**:
   ```yaml
   # Ensure all Rise-created namespaces follow the naming pattern
   apiVersion: constraints.gatekeeper.sh/v1beta1
   kind: K8sRequiredLabels
   metadata:
     name: rise-namespace-naming
   spec:
     match:
       kinds:
       - apiGroups: [""]
         kinds: ["Namespace"]
     parameters:
       labels:
         - key: "app.kubernetes.io/managed-by"
           allowedRegex: "^rise$"
   ```

4. **Namespace Format Configuration**: Always align `default_organization.kubernetes_namespace_prefix` in the Rise config with your organization's namespace policies.

## Configuration Format

### Rise Configuration

The `config` parameter is converted to YAML and mounted as `/etc/rise/local.yaml`. The backend loads required `production` (from `RISE_CONFIG_RUN_MODE=production`) and then applies optional `local` overrides, without requiring Helm chart updates for new options.

**Sensitive Values:** `config` is rendered into a ConfigMap. Supply secrets through `env` or `envFrom`. Leave secret fields unset to use the variables read by `production.yaml`, or set them to `${VARIABLE_NAME}` to reference another environment variable. An empty string overrides the default. See [Environment Variables](#environment-variables) below.

Example:

```yaml
config:
  server:
    host: "0.0.0.0"
    port: 3000
    public_url: "https://rise.example.com"

  auth:
    issuer: "https://dex.example.com/dex"
    client_id: "rise-backend"

  registry:
    type: "oci-client-auth"
    registry_url: "registry.example.com"
    namespace: "rise-apps"
```

### Dex Configuration

With `dex.enabled: true` the chart runs Dex with `dex.config` as its
configuration file. The default has in-memory storage, a `rise-backend` client
with placeholder redirect URIs, and **no users**: add `connectors` for a real
identity provider, or enable the password DB with users whose hashes you
generate yourself.

```yaml
dex:
  enabled: true
  config:
    issuer: "https://dex.example.com/dex"
    storage:
      type: kubernetes
      config:
        inCluster: true
    web:
      http: "0.0.0.0:5556"
    staticClients:
      - id: rise-backend
        redirectURIs:
          - "https://rise.example.com/api/v1/auth/callback"
          - "http://127.0.0.1:8765/callback"
          - "http://127.0.0.1:8766/callback"
          - "http://127.0.0.1:8767/callback"
        name: "Rise Backend"
        secret: "<client secret>"
    connectors:
      - type: github
        id: github
        name: GitHub
        config:
          clientID: $GITHUB_CLIENT_ID
          clientSecret: $GITHUB_CLIENT_SECRET
          redirectURI: https://dex.example.com/dex/callback
    oauth2:
      skipApprovalScreen: true
```

For static users, set `enablePasswordDB: true` and list them under
`staticPasswords`, each with a bcrypt hash of a password you chose:

```bash
htpasswd -bnBC 10 "" 'your-password' | tr -d ':\n'
```

Helm replaces lists rather than merging them, so `staticClients`,
`staticPasswords` and `connectors` must each be given in full.

### Environment Variables

Rise supports two ways to inject environment variables:

1. **`env`**: Direct key-value pairs for non-sensitive configuration
2. **`envFrom`**: Reference Secrets and ConfigMaps for sensitive values

#### Direct Environment Variables (`env`)

Use the `env` parameter for simple key-value environment variables:

```yaml
env:
  - name: RUST_LOG
    value: "info"
  - name: AWS_REGION
    value: "us-east-1"
```

#### Environment Variables from Secrets/ConfigMaps (`envFrom`)

The `envFrom` parameter allows you to inject environment variables from Secrets and ConfigMaps. This is the recommended approach for:

- Sensitive configuration (secrets, passwords, API keys)
- Database connection strings
- Cloud provider credentials

The image's `production.yaml` reads secrets from these variables:
`DATABASE_URL`, `OIDC_CLIENT_SECRET` and `RISE_JWT_SIGNING_SECRET`. Leave those
keys unset under `config` to use these defaults. To reference a different
variable, set a field such as `config.auth.client_secret` to `${CLIENT_SECRET}`
and supply `CLIENT_SECRET` through `env` or `envFrom`.

**Example:**

```yaml
# values.yaml
envFrom:
  - secretRef:
      name: rise-secrets
```

```bash
kubectl create secret generic rise-secrets \
  --from-literal=OIDC_CLIENT_SECRET=your-actual-secret \
  --from-literal=RISE_JWT_SIGNING_SECRET="$(openssl rand -base64 32)"
```

**Example: Using a Secret for AWS credentials**

Create a secret with your AWS credentials:

```bash
kubectl create secret generic rise-aws-creds \
  --from-literal=AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE \
  --from-literal=AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY \
  --from-literal=AWS_REGION=us-east-1
```

Configure the chart to use this secret:

```yaml
envFrom:
  - secretRef:
      name: rise-aws-creds
```

**Example: Using multiple sources**

You can combine multiple Secrets and ConfigMaps:

```yaml
envFrom:
  # AWS credentials from secret
  - secretRef:
      name: rise-aws-creds
  # Additional environment-specific config
  - configMapRef:
      name: rise-env-config
  # Override specific settings
  - secretRef:
      name: rise-overrides
```

**Note:** Besides the variables `production.yaml` names, the AWS SDK reads its usual variables, such as `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.

## Upgrading

To upgrade an existing release:

```bash
helm upgrade rise ./helm/rise -f values-prod.yaml
```

## Uninstalling

To uninstall/delete the `rise` deployment:

```bash
helm uninstall rise
```
