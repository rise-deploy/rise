# Rise Helm Chart

This Helm chart deploys the Rise application on Kubernetes, including the backend API server and optionally Dex for OIDC authentication.

## Prerequisites

- Kubernetes 1.19+
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
    client_secret: ""  # Provided via envFrom
    admin_users:
      - "admin@example.com"

  registry:
    type: "oci-client-auth"
    registry_url: "registry.example.com"
    namespace: "rise-apps"

  kubernetes:
    ingress_class: "nginx"
    production_ingress_url_template: "{project_name}.apps.example.com"
    staging_ingress_url_template: "{project_name}-{deployment_group}.preview.example.com"
    auth_backend_url: "http://rise.default.svc.cluster.local:3000"
    auth_signin_url: "https://rise.example.com"
    namespace_format: "rise-{project_name}"

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

# Inject sensitive configuration via envFrom
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
dex:
  enabled: true
  config: |
    issuer: https://dex.example.com/dex

    storage:
      type: kubernetes
      config:
        inCluster: true

    web:
      http: 0.0.0.0:5556

    staticClients:
    - id: rise-backend
      redirectURIs:
      - 'https://rise.example.com/api/v1/auth/callback'
      name: 'Rise Backend'
      secret: your-client-secret-here

    connectors:
    - type: github
      id: github
      name: GitHub
      config:
        clientID: $GITHUB_CLIENT_ID
        clientSecret: $GITHUB_CLIENT_SECRET
        redirectURI: https://dex.example.com/dex/callback
```

Create the secret with required environment variables:

```bash
kubectl create secret generic rise-secrets \
  --from-literal=DATABASE_URL=postgres://rise:YOUR_DB_PASSWORD@postgresql:5432/rise \
  --from-literal=AUTH_CLIENT_SECRET=YOUR_CLIENT_SECRET
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
| `logs.loki.retentionHint` | Display-only retention hint for empty log states | `""` |
| `logs.alloy.enabled` | Install bundled Grafana Alloy subchart for Pod log shipping | `false` |
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
| `dex.image.tag` | Dex image tag | `v2.40.0` |
| `dex.service.port` | Dex HTTP port | `5556` |
| `dex.service.grpcPort` | Dex gRPC port | `5557` |
| `dex.issuerUrl` | OIDC issuer URL injected into Dex config | `http://dex:5556/dex` |
| `dex.config` | Dex configuration in YAML format (empty = use default) | `""` |

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
   - Configure `namespace_format` with a consistent prefix (e.g., `rise-{project_name}`)
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

4. **Namespace Format Configuration**: Always align your `namespace_format` configuration in the Rise config with your organization's namespace policies.

## Configuration Format

### Rise Configuration

The `config` parameter is converted to YAML and mounted as `/etc/rise/local.yaml`. The backend loads required `production` (from `RISE_CONFIG_RUN_MODE=production`) and then applies optional `local` overrides, without requiring Helm chart updates for new options.

**Sensitive Values:** For sensitive configuration like secrets and passwords, leave them empty in the config and provide them via `envFrom` instead. See [Environment Variables](#environment-variables) below.

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
    client_secret: ""  # Provided via envFrom

  registry:
    type: "oci-client-auth"
    registry_url: "registry.example.com"
    namespace: "rise-apps"
```

### Dex Configuration

When Dex is enabled (`dex.enabled: true`), the chart provides sensible defaults for development and testing.

#### Using the Default Configuration

By default, `dex.config` is empty and the chart uses a pre-configured development setup that includes:
- In-memory storage (data is lost on pod restart)
- Static password authentication with test users (`admin@example.com`, `dev@example.com` and `user@example.com`, all with password "password")
- Placeholder redirect URIs (`http://rise.example.com/` and `http://rise.example.com/auth/callback`)

**Important:** The default configuration is for testing only. You must customize the redirect URIs to match your actual `public_url` for production deployments.

To use the default config, simply set the issuer URL:

```yaml
dex:
  enabled: true
  issuerUrl: "https://dex.example.com/dex"
```

The `issuerUrl` will be automatically injected into the Dex configuration, replacing the default `http://localhost:5556/dex`.

#### Custom Configuration

For production deployments, you can provide a complete custom configuration via `dex.config`:

```yaml
dex:
  enabled: true
  issuerUrl: "https://dex.example.com/dex"
  config: |
    storage:
      type: kubernetes
      config:
        inCluster: true

    web:
      http: 0.0.0.0:5556

    staticClients:
    - id: rise-backend
      redirectURIs:
      - 'https://rise.example.com/api/v1/auth/callback'
      name: 'Rise Backend'
      secret: your-client-secret-here

    connectors:
    - type: github
      id: github
      name: GitHub
      config:
        clientID: $GITHUB_CLIENT_ID
        clientSecret: $GITHUB_CLIENT_SECRET
        redirectURI: https://dex.example.com/dex/callback
```

**Note:** The `issuerUrl` is automatically injected as the `issuer:` field in the Dex config, so you don't need to specify it in the config YAML.

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

**Example:**

```yaml
# values.yaml
config:
  auth:
    issuer: "http://dex:5556/dex"
    client_id: "rise-backend"
    client_secret: ""  # Provided via environment variable

envFrom:
  - secretRef:
      name: rise-secrets
```

```bash
# Create secret with actual value
kubectl create secret generic rise-secrets \
  --from-literal=AUTH_CLIENT_SECRET=your-actual-secret
```

The backend reads `AUTH_CLIENT_SECRET` from the environment and uses it for the auth configuration.

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

**Note:** The Rise backend automatically reads certain environment variables for sensitive configuration. Common examples include `DATABASE_URL`, `AUTH_CLIENT_SECRET`, `AWS_ACCESS_KEY_ID`, and `AWS_SECRET_ACCESS_KEY`.

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
