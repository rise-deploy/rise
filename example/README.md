# Rise Examples

This directory contains example applications demonstrating various Rise features and deployment patterns.

## Examples

### Hello World Examples

Simple static and dynamic applications to get started with Rise.

- **[hello-world](./hello-world/)** - Static HTML page served with nginx
- **[hello-world-js](./hello-world-js/)** - Node.js Express application
- **[hello-world-py](./hello-world-py/)** - Python Flask application

### Multi-Container Examples

- **[multi-container](./multi-container/)** - Redis-backed job queue: frontend submits text, API enqueues, worker analyzes. Shows `[routes]` ingress and auto-injected `RISE_CONTAINER_HOST__<NAME>` cross-container service discovery

### AWS Examples

- **[s3-file-browser](./s3-file-browser/)** - Web-based S3 file browser with upload/download/delete

### OAuth Examples

Demonstrate the OAuth 2.0 extension for end-user authentication.

- **[oauth-pkce-flow](./oauth-pkce-flow/)** - PKCE OAuth flow for SPAs
  - Uses PKCE (Proof Key for Code Exchange) for enhanced security
  - Best for: React, Vue, Angular, vanilla JavaScript
  - Security: No client secrets needed, prevents code interception
  - Stack: nginx serving static HTML/JS (port 8080)

- **[oauth-exchange-flow](./oauth-exchange-flow/)** - RFC 6749-compliant token endpoint flow for backend apps
  - Authorization code exchanged server-side via `/token` endpoint
  - Best for: Rails, Django, Express, server-rendered apps
  - Security: HttpOnly cookies, XSS-safe, tokens never exposed to browser
  - Stack: Node.js/Express (port 8080)

## Quick Start

Each example includes its own README with detailed setup instructions.

### General Steps

1. **Start local development environment**:
   ```bash
   docker-compose up -d
   ```

2. **Login to Rise**:
   ```bash
   rise login
   ```

3. **Create a project** (if needed):
   ```bash
   rise project create <project-name>
   ```

4. **Deploy the example**:
   ```bash
   cd example/<example-name>
   rise deployment create <project-name>
   ```

## OAuth Examples Setup

The OAuth examples require additional setup to create the OAuth extension:

1. **Create the OAuth extension**:
   ```bash
   # Encrypt the Dex client secret
   ENCRYPTED=$(rise encrypt "rise-backend-secret")

   # Create OAuth extension
   rise extension create oauth-demo oauth-dex \
     --type oauth \
     --spec '{
       "provider_name": "Dex (Local Dev)",
       "description": "Local Dex OIDC provider for development",
       "client_id": "rise-backend",
       "client_secret_encrypted": "'"$ENCRYPTED"'",
       "authorization_endpoint": "http://localhost:5556/dex/auth",
       "token_endpoint": "http://localhost:5556/dex/token",
       "scopes": ["openid", "email", "profile"]
     }'
   ```

2. **Restart Dex** (callback URLs already configured):
   ```bash
   docker-compose restart dex
   ```

3. **Deploy the example**:
   ```bash
   cd example/oauth-pkce-flow  # or oauth-exchange-flow
   rise deployment create oauth-demo
   ```

4. **Test locally** (optional):
   - PKCE flow: `rise deployment create oauth-demo` and visit the deployed URL
   - Exchange flow: Run `npm install && npm start` and visit `http://localhost:8080`

## Default Dex Credentials

For local development with Dex:

- **Email**: `admin@example.com` / `dev@example.com` / `user@example.com`
- **Password**: `password`

## Documentation

For more details on Rise features, see the main documentation:

- [OAuth Extension](../docs/oauth.md)
- [Build Backends](../docs/builds.md)
- [Deployments](../docs/deployments.md)
- [Configuration](../docs/configuration.md)

## Contributing

When adding new examples:

1. Create a new directory under `example/`
2. Include a README with setup instructions
3. Add entry to this README
4. Keep examples minimal and focused on one feature
