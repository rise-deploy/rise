# Configuration Guide

Rise backend uses YAML configuration files with environment variable substitution support. TOML is also supported for backward compatibility.

## Configuration Files

Configuration files are located in `config/` and loaded in this order:

1. `{RISE_CONFIG_RUN_MODE}.{toml,yaml,yml}` - Environment-specific config (**required**)
   - `development.toml` or `development.yaml` when `RISE_CONFIG_RUN_MODE=development`
   - `production.toml` or `production.yaml` when `RISE_CONFIG_RUN_MODE=production`
2. `local.{toml,yaml,yml}` - Local overrides (not checked into git, optional)

Later files override earlier ones.

In container deployments, `RISE_CONFIG_DIR` is typically `/etc/rise`.

**File Format**: The backend supports both YAML and TOML formats. When multiple formats exist for the same config file name (for example `development.yaml` and `development.toml`), TOML takes precedence. YAML is the recommended format as it integrates seamlessly with Kubernetes/Helm deployments.

## Environment Variable Substitution

Configuration values can reference environment variables using the syntax:

```toml
# TOML example
client_secret = "${RISE_AUTH_CLIENT_SECRET:-rise-backend-secret}"
account_id = "${AWS_ACCOUNT_ID}"
public_url = "https://${DOMAIN_NAME}:${PORT}"
```

```yaml
# YAML example
auth:
  client_secret: "${RISE_AUTH_CLIENT_SECRET:-rise-backend-secret}"
registry:
  account_id: "${AWS_ACCOUNT_ID}"
server:
  public_url: "https://${DOMAIN_NAME}:${PORT}"
```

### Syntax

- `${VAR_NAME}` - Use environment variable `VAR_NAME`, error if not set
- `${VAR_NAME:-default}` - Use `VAR_NAME` if set, otherwise use `default`

### How It Works

1. Configuration files are parsed as TOML or YAML
2. String values are scanned for `${...}` patterns
3. Patterns are replaced with environment variable values
4. Resulting configuration is deserialized into Settings struct

This happens **after** TOML/YAML parsing but **before** deserialization, so:
- ✅ Works in all string values (including nested tables/maps and arrays)
- ✅ Preserves structure and types
- ✅ Clear error messages if required variables are missing

## Configuration Precedence

Configuration is loaded in this order (later values override earlier ones):

1. `{RISE_CONFIG_RUN_MODE}.{toml,yaml,yml}` - Active environment config (required)
2. `local.{toml,yaml,yml}` - Local overrides (not in git, optional)
3. Environment variable substitution - `${VAR}` patterns are replaced
4. DATABASE_URL special case - Overrides `[database] url` if set

**Note**: When multiple file formats exist for the same config file, TOML takes precedence over YAML.

Example (TOML):
```toml
# In production.toml
client_secret = "${AUTH_SECRET}" # Required in production

# In local.toml
client_secret = "my-local-secret"  # Override: hardcoded value
```

Example (YAML):
```yaml
# In production.yaml
auth:
  client_secret: "${AUTH_SECRET}"  # Required
```

### Special Cases

**DATABASE_URL**: For convenience, the DATABASE_URL environment variable is checked after config loading and will override any `[database] url` setting. This is optional - you can use `${DATABASE_URL}` in TOML instead:

```toml
# Option 1: Direct environment variable (checked after config loads)
[database]
url = ""  # Empty, DATABASE_URL env var will be used

# Option 2: Explicit substitution (recommended for consistency)
[database]
url = "${DATABASE_URL}"
```

**Note**: DATABASE_URL is only required at compile time for SQLX query verification. At runtime, you can set it via either method above.

## Examples

### Development (development.toml)

```toml
[server]
host = "0.0.0.0"
port = 3000
public_url = "http://localhost:3000"

[auth]
issuer = "http://localhost:5556/dex"
client_id = "rise-backend"
client_secret = "${RISE_AUTH_CLIENT_SECRET:-rise-backend-secret}"
```

### Production with Environment Variables (TOML)

```toml
# production.toml
[server]
host = "0.0.0.0"
port = "${PORT:-3000}"
public_url = "${PUBLIC_URL}"  # Required, no default
cookie_secure = true

[auth]
issuer = "${DEX_ISSUER}"
client_id = "${OIDC_CLIENT_ID}"
client_secret = "${OIDC_CLIENT_SECRET}"  # Required
admin_users = ["${ADMIN_EMAIL}"]

[registry]
type = "ecr"
region = "${AWS_REGION:-us-east-1}"
account_id = "${AWS_ACCOUNT_ID}"
role_arn = "${ECR_CONTROLLER_ROLE_ARN}"
push_role_arn = "${ECR_PUSH_ROLE_ARN}"
```

### Production with Environment Variables (YAML)

```yaml
# production.yaml - ideal for Kubernetes/Helm deployments
server:
  host: "0.0.0.0"
  port: "${PORT:-3000}"
  public_url: "${PUBLIC_URL}"  # Required, no default
  cookie_secure: true

auth:
  issuer: "${DEX_ISSUER}"
  client_id: "${OIDC_CLIENT_ID}"
  client_secret: "${OIDC_CLIENT_SECRET}"
  admin_users:
    - "${ADMIN_EMAIL}"

database:
  url: "${DATABASE_URL}"

registry:
  type: "ecr"
  region: "${AWS_REGION:-us-east-1}"
  account_id: "${AWS_ACCOUNT_ID}"
  role_arn: "${ECR_CONTROLLER_ROLE_ARN}"
  push_role_arn: "${ECR_PUSH_ROLE_ARN}"
```

Environment file:
```bash
# .env
PUBLIC_URL=https://rise.example.com
DEX_ISSUER=https://dex.example.com
OIDC_CLIENT_ID=rise-production
OIDC_CLIENT_SECRET=very-secret-value
ADMIN_EMAIL=admin@example.com
AWS_ACCOUNT_ID=123456789012
ECR_CONTROLLER_ROLE_ARN=arn:aws:iam::123456789012:policy/rise-backend
ECR_PUSH_ROLE_ARN=arn:aws:iam::123456789012:role/rise-backend-ecr-push
DATABASE_URL=postgres://rise:${DB_PASSWORD}@db.example.com/rise
```

### Local Overrides (local.toml)

For local development, create `local.toml` (not checked into git):

```toml
# Override just what you need
[auth]
client_secret = "my-local-secret"

[registry]
type = "oci-client-auth"
registry_url = "localhost:5000"
```

## Configuration Reference

### Server Settings

```toml
[server]
host = "0.0.0.0"              # Bind address
port = 3000                    # HTTP port
public_url = "http://..."      # Public URL (for OAuth redirects)
cookie_domain = ""             # Cookie domain ("" = current host only)
cookie_secure = false          # Set true for HTTPS
jwt_signing_secret = "..."     # JWT signing secret (base64-encoded, min 32 bytes)
jwt_expiry_seconds = 86400     # JWT expiry duration in seconds (default: 24 hours)
jwt_claims = ["sub", "email", "name"]  # Claims to include from IdP
rs256_private_key_pem = "..."  # Optional: RS256 private key (persists JWTs across restarts)
rs256_public_key_pem = "..."   # Optional: RS256 public key (derived if not provided)
docs_dir = "/var/rise/docs"    # Optional: directory to serve documentation from
```

**Documentation Serving (`docs_dir`):**
- When set, the backend serves markdown files from the specified directory at `/static/docs/*`
- In the container image, docs are copied to `/var/rise/docs`
- In development, set to `"docs"` to serve from the repository's `docs/` directory
- If not set, documentation endpoints return 404

**JWT Configuration:**
- `jwt_signing_secret`: Base64-encoded secret for HS256 JWT signing (generate with `openssl rand -base64 32`)
- `jwt_expiry_seconds`: Duration in seconds before JWTs expire (default: 86400 = 24 hours)
- `jwt_claims`: Claims to include from IdP token in Rise JWTs
- `rs256_private_key_pem`: Optional pre-configured RS256 private key (prevents JWT invalidation on restart)
- `rs256_public_key_pem`: Optional RS256 public key (automatically derived from private key if omitted)

### Auth Settings

```toml
[auth]
issuer = "http://..."          # OIDC issuer URL
client_id = "rise-backend"     # OAuth2 client ID
client_secret = "..."          # OAuth2 client secret
admin_users = ["email@..."]    # Admin user emails (array)
allow_team_creation = true     # Allow regular users to create teams (default: true)
                              # When false, only admins can create teams
```

**Team Creation Control:**
- `allow_team_creation = true` (default): All authenticated users can create teams
- `allow_team_creation = false`: Only admin users can create teams (suitable for centrally-managed organizations)

### Database Settings

```toml
[database]
url = "postgres://..."         # PostgreSQL connection string
                              # Or use DATABASE_URL env var
```

### Registry Settings

#### AWS ECR

```toml
[registry]
type = "ecr"
region = "us-east-1"
account_id = "123456789012"
repo_prefix = "rise/"
role_arn = "arn:aws:iam::..."
push_role_arn = "arn:aws:iam::..."
auto_remove = true
```

#### OCI Registry (Docker, Harbor, Quay)

```toml
[registry]
type = "oci-client-auth"
registry_url = "registry.example.com"
namespace = "rise-apps"
```

#### GitLab Container Registry

```yaml
registry:
  type: gitlab
  gitlab_url: "https://gitlab.com"         # GitLab instance URL
  registry_url: "registry.gitlab.com"      # Registry host
  namespace: "my-org/my-group/rise-apps"   # Image path prefix in the registry
  username: "${GITLAB_USERNAME}"
  token: "${GITLAB_TOKEN}"                 # Personal Access Token or Deploy Token
  mint_pull_secrets: true                  # Create K8s image pull secrets per project namespace
  # client_registry_url: ~                 # Optional: override URL returned to CLI clients
```

**How it works:**
- **CLI pushes**: the backend mints a short-lived (~15 min) scoped JWT from GitLab's JWT auth endpoint (`GET /jwt/auth?service=container_registry&scope=repository:<path>:push,pull`). The JWT is injected directly into the container CLI's auth config file rather than via `docker login`.
- **Kubernetes pull secrets**: when `mint_pull_secrets: true`, the controller creates a standard `kubernetes.io/dockerconfigjson` secret with the PAT in each project namespace. The container runtime uses the PAT to obtain its own JWT from GitLab on each pull.
- **`mint_pull_secrets: false`**: disable pull secret management when the cluster has its own image pull mechanism (e.g., pre-configured service account or node credentials).

**Token requirements:**
The GitLab token must have `read_registry` and `write_registry` scopes (or equivalent deploy token permissions).

#### JFrog Artifactory

JFrog supports two token-issuing backends: Vault (via `vault-plugin-secrets-artifactory`) and Direct (via JFrog's access token API).

**Vault mode:**

```yaml
registry:
  type: jfrog
  registry_host: "jfrog.example.com"
  docker_repo_key: "rise-docker-local"
  token_provider:
    type: vault
    # vault_addr: ~             # defaults to VAULT_ADDR env
    # vault_token: ~            # defaults to VAULT_TOKEN env
    # vault_token_file: ~       # alternative: read token from file (supports rotation)
    vault_mount_path: "artifactory"
    vault_role: "rise"          # must have allow_scope_override=true
  # push_permissions: "r,w"    # default
  # pull_permissions: "r"      # default
  # push_token_ttl: 600        # push token lifetime in seconds (default: 600)
  # pull_token_ttl: 86400      # pull token lifetime in seconds (default: 86400 = 24h)
  # mint_pull_secrets: true    # default
```

> **Note (Vault mode):** The Vault role's `max_ttl` must be >= `pull_token_ttl`. If the role's `max_ttl` is lower, Vault silently clamps the token TTL and the cached credentials may expire earlier than expected.

**Direct mode:**

```yaml
registry:
  type: jfrog
  registry_host: "jfrog.example.com"
  docker_repo_key: "rise-docker-local"
  token_provider:
    type: direct
    jfrog_url: "https://jfrog.example.com"
    admin_token: "${JFROG_ADMIN_TOKEN}"   # applied-permissions/admin scoped token
  # client_registry_url: ~     # optional: override registry URL returned to CLI clients
  # push_token_ttl: 600        # push token lifetime in seconds (default: 600)
  # pull_token_ttl: 86400      # pull token lifetime in seconds (default: 86400 = 24h)
```

**How it works:**
- **CLI pushes**: the backend mints a short-lived multi-scope token with `r,w` permissions scoped to the deployment tag, blob uploads, and content-addressed manifests. The token is used for `docker login` before push.
- **Kubernetes pull secrets**: when `mint_pull_secrets: true`, the controller creates image pull secrets with a long-lived read-only scoped token (`artifact:{docker_repo_key}/{project}/**:r`). Pull tokens are cached in memory and refreshed after 2/3 of their TTL to avoid minting a new token on every deploy.

See [Registry Backend Operations](operator-registry-operations.md#jfrog-artifactory) for scope details and troubleshooting.

### Controller Settings (Optional)

```toml
[controller]
reconcile_interval_secs = 5
health_check_interval_secs = 5
termination_interval_secs = 5
cancellation_interval_secs = 5
expiration_interval_secs = 60
secret_refresh_interval_secs = 3600
```

## Validation

The backend validates configuration on startup:
- Required fields must be set
- Invalid values cause startup failure with clear error messages
- Environment variable substitution errors are reported
- **Unknown configuration fields generate warnings** (as of v0.9.0)

### Checking Configuration

Use the `rise backend check-config` command to validate backend configuration:

```bash
rise backend check-config
```

This command:
- Loads and validates backend configuration files
- Reports any unknown/unused configuration fields as warnings
- Exits with an error if configuration is invalid
- Useful for CI/CD pipelines and deployment validation

Example output:
```
Checking backend configuration...
⚠️  WARN: Unknown configuration field in backend config: server.typo_field
⚠️  WARN: Unknown configuration field in backend config: unknown_section
✓ Configuration is valid
```

### JSON Schema

Rise provides a JSON Schema for backend configuration at:

- [`docs/schemas/backend-settings.schema.json`](schemas/backend-settings.schema.json)

Generate it with:

```bash
cargo run --features cli,backend -- backend config-schema > docs/schemas/backend-settings.schema.json
```

CI verifies this file is up to date on every PR and push.

### Unknown Field Warnings

Starting in v0.9.0, Rise warns about unrecognized configuration fields to help catch typos and outdated options:

**Backend Configuration (YAML/TOML):**
```bash
# Warnings appear in logs when starting server or using check-config
WARN rise::server::settings: Unknown configuration field in backend config: server.unknown_field
```

**Project Configuration (rise.toml):**
```bash
# Warnings appear when loading rise.toml (during build, deploy, etc.)
WARN rise::build::config: Unknown configuration field in ./rise.toml: build.?.typo_field
```

These are warnings, not errors - your configuration will still load and work. The warnings help you:
- Catch typos in field names
- Identify outdated configuration options after upgrades
- Ensure your configuration is being used as intended

Run with `RUST_LOG=debug` to see configuration loading details:

```bash
RUST_LOG=debug cargo run --bin rise -- backend server
```

## Custom Domains

Rise supports custom domains for projects, allowing you to serve your applications from your own domain names instead of (or in addition to) the default project URL.

### Primary Custom Domains

Each project can designate one custom domain as **primary**. The primary domain is used as the canonical URL for the application and is exposed via the `RISE_APP_URL` environment variable.

### RISE_APP_URL Environment Variable

Rise automatically creates a `RISE_APP_URL` deployment environment variable containing the canonical URL for the application. This variable is determined at deployment creation time and persisted in the database:

- **If a primary custom domain is set**: `RISE_APP_URL` contains the primary custom domain URL (e.g., `https://example.com`)
- **If no primary domain is set**: `RISE_APP_URL` contains the default project URL (e.g., `https://my-app.rise.dev`)

Since this is a deployment environment variable, you can view it via the API or CLI along with your other environment variables.

This environment variable is useful for:
- Generating absolute URLs in your application (e.g., for email links, OAuth redirects)
- Implementing canonical URL redirects (redirect all traffic to the primary domain)
- Setting the correct domain for cookies and CORS headers

**Example usage in your application:**

```javascript
// Node.js
const canonicalUrl = process.env.RISE_APP_URL;

// Redirect to canonical domain
app.use((req, res, next) => {
  const requestUrl = `${req.protocol}://${req.get('host')}`;
  if (requestUrl !== canonicalUrl) {
    return res.redirect(301, `${canonicalUrl}${req.url}`);
  }
  next();
});
```

```python
# Python
import os

canonical_url = os.environ.get('RISE_APP_URL')

# Flask: Set SERVER_NAME
app.config['SERVER_NAME'] = canonical_url.replace('https://', '').replace('http://', '')
```

### Managing Custom Domains

**Via Frontend:**
1. Navigate to your project's Domains tab
2. Add custom domains using the "Add Domain" button
3. Click the star icon next to a domain to set it as primary
4. The primary domain will show a filled yellow star and a "Primary" badge

**Via API:**

```bash
# List custom domains
curl https://rise.dev/api/projects/my-app/domains

# Add a custom domain
curl -X POST https://rise.dev/api/projects/my-app/domains \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"domain": "example.com"}'

# Set domain as primary
curl -X PUT https://rise.dev/api/projects/my-app/domains/example.com/primary \
  -H "Authorization: Bearer $TOKEN"

# Unset primary status
curl -X DELETE https://rise.dev/api/projects/my-app/domains/example.com/primary \
  -H "Authorization: Bearer $TOKEN"
```

### DNS Configuration

Before adding a custom domain, you must configure your DNS to point to your Rise deployment:

```
# A record for root domain
example.com.  IN  A  <rise-ingress-ip>

# CNAME for subdomain
www.example.com.  IN  CNAME  <rise-ingress-hostname>
```

Custom domains are added to the ingress for the default deployment group only.

### TLS/SSL

Custom domains use the same TLS configuration as the default project URL:
- If your Rise deployment uses a wildcard certificate, custom domains will use HTTP unless configured with per-domain TLS
- Configure `custom_domain_tls_mode` in the Kubernetes controller settings for automatic HTTPS on custom domains

### Behavior

- **Automatic reconciliation**: Setting or unsetting a primary domain triggers reconciliation of the active deployment to update the `RISE_APP_URL` environment variable
- **Deletion protection**: You can delete a primary domain; `RISE_APP_URL` will fall back to the default project URL
- **Multiple domains**: You can add multiple custom domains to a project, but only one can be primary
- **Environment variable list**: All custom domains (primary and non-primary) are also available in the `RISE_APP_URLS` environment variable as a JSON array
