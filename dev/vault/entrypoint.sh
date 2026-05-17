#!/bin/sh
set -e

JFROG_URL="http://rise-jfrog:8082"
JFROG_USER="admin"
JFROG_PASS="password"

PLUGIN_VERSION="v1.8.9-rise.2"
PLUGIN_REPOSITORY="rise-deploy/vault-plugin-secrets-artifactory"
PLUGIN_DIR="/vault/plugins"
PLUGIN_PATH="$PLUGIN_DIR/artifactory-secrets-plugin"
PLUGIN_MARKER="$PLUGIN_PATH.version"

# --- 1. Install curl (needed for cookie-based UI API auth) ---
apk add --no-cache curl > /dev/null 2>&1
echo "curl installed."

# --- 2. Cache-aware plugin download ---
if [ ! -f "$PLUGIN_PATH" ] || [ "$(cat "$PLUGIN_MARKER" 2>/dev/null || true)" != "${PLUGIN_REPOSITORY}@${PLUGIN_VERSION}" ]; then
  echo "Downloading vault-plugin-secrets-artifactory $PLUGIN_VERSION ..."
  ARCH=$(uname -m)
  case "$ARCH" in
    x86_64)  ARCH="amd64" ;;
    aarch64) ARCH="arm64" ;;
    *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
  esac
  wget -q -O "$PLUGIN_PATH" \
    "https://github.com/${PLUGIN_REPOSITORY}/releases/download/${PLUGIN_VERSION}/artifactory-secrets-plugin_${PLUGIN_VERSION#v}_linux_${ARCH}"
  chmod +x "$PLUGIN_PATH"
  echo "${PLUGIN_REPOSITORY}@${PLUGIN_VERSION}" > "$PLUGIN_MARKER"
  echo "Plugin downloaded and installed."
else
  echo "Plugin already cached, skipping download."
fi

# --- 3. Start Vault dev server in background ---
vault server \
  -dev \
  -dev-root-token-id=root \
  -dev-listen-address=0.0.0.0:8200 \
  -dev-plugin-dir="$PLUGIN_DIR" &
VAULT_PID=$!

export VAULT_ADDR="http://127.0.0.1:8200"
export VAULT_TOKEN="root"

# --- 4. Wait for Vault readiness ---
echo "Waiting for Vault to start ..."
until vault status > /dev/null 2>&1; do
  sleep 1
done
echo "Vault is ready."

# --- 5. Register and enable the secrets engine ---
PLUGIN_SHA256=$(sha256sum "$PLUGIN_PATH" | awk '{print $1}')
vault plugin register \
  -sha256="$PLUGIN_SHA256" \
  -command="$(basename "$PLUGIN_PATH")" \
  secret artifactory
echo "Artifactory secrets plugin registered."

vault secrets enable -path=artifactory artifactory
echo "Artifactory secrets engine enabled at artifactory/."

# --- 6. Wait for JFrog health ---
echo "Waiting for JFrog to become healthy ..."
until wget -q --spider "${JFROG_URL}/router/api/v1/system/health" 2>/dev/null; do
  sleep 5
done
echo "JFrog is healthy."

# --- 7. Accept JFrog EULA ---
echo "Accepting JFrog EULA ..."
curl -sf -o /dev/null -u "${JFROG_USER}:${JFROG_PASS}" \
  -X POST "${JFROG_URL}/artifactory/ui/jcr/eula/accept" || true
echo "JFrog EULA accepted."

# --- 8. Create Docker repository for local dev registry usage ---
# JFrog JCR (free edition) does not support the /api/repositories REST API.
# We use the UI API with cookie-based session auth instead.
echo "Creating 'rise-docker-local' Docker repository ..."
REPO_COOKIE_JAR=$(mktemp)
curl -sf -c "$REPO_COOKIE_JAR" \
  -H "Content-Type: application/json" \
  -H "X-Requested-With: XMLHttpRequest" \
  -d "{\"user\":\"${JFROG_USER}\",\"password\":\"${JFROG_PASS}\",\"type\":\"login\"}" \
  "${JFROG_URL}/ui/api/v1/ui/auth/login" > /dev/null 2>&1

REPO_ACCESS=$(sed -n 's/.*ACCESSTOKEN[[:space:]]*//p' "$REPO_COOKIE_JAR" | tr -d '[:space:]')
REPO_REFRESH=$(sed -n 's/.*REFRESHTOKEN[[:space:]]*//p' "$REPO_COOKIE_JAR" | tr -d '[:space:]')

curl -sf -o /dev/null \
  -H "Content-Type: application/json" \
  -H "X-Requested-With: XMLHttpRequest" \
  -H "cookie: ACCESSTOKEN=${REPO_ACCESS}; REFRESHTOKEN=${REPO_REFRESH}" \
  -X POST \
  -d '{
    "type":"localRepoConfig",
    "typeSpecific":{"repoType":"Docker","dockerApiVersion":"V2","maxUniqueTags":0,"dockerTagRetention":0,"blockPushingSchema1":false},
    "general":{"repoKey":"rise-docker-local"},
    "basic":{"layout":"simple-default","includesPattern":"**/*","excludesPattern":""},
    "advanced":{"cache":{"keepUnusedArtifactsHours":""},"distribution":{"gpgSign":false},"replication":{"blockPullReplications":false,"blockPushReplications":false}}
  }' \
  "${JFROG_URL}/ui/api/v1/ui/admin/repositories" || true

rm -f "$REPO_COOKIE_JAR"
echo "Docker repository 'rise-docker-local' ready."

# --- 9. Create a JFrog admin access token ---
# Uses the UI API (the only way to get an applied-permissions/admin scoped token).
# Ref: https://github.com/rise-deploy/vault-plugin-secrets-artifactory/blob/master/scripts/getArtifactoryAdminToken.sh
echo "Creating JFrog admin access token ..."
COOKIE_JAR=$(mktemp)
MAX_RETRIES=30
RETRY=0
TOKEN=""

while [ -z "$TOKEN" ] && [ "$RETRY" -lt "$MAX_RETRIES" ]; do
  # Step 1: Login to get session cookies
  LOGIN_RESP=$(curl -sf -c "$COOKIE_JAR" \
    -H "Content-Type: application/json" \
    -H "X-Requested-With: XMLHttpRequest" \
    -d "{\"user\":\"${JFROG_USER}\",\"password\":\"${JFROG_PASS}\",\"type\":\"login\"}" \
    "${JFROG_URL}/ui/api/v1/ui/auth/login" 2>&1) || LOGIN_RESP=""

  if [ -n "$LOGIN_RESP" ]; then
    # Extract cookie values
    ACCESSTOKEN=$(sed -n 's/.*ACCESSTOKEN[[:space:]]*//p' "$COOKIE_JAR" | tr -d '[:space:]')
    REFRESHTOKEN=$(sed -n 's/.*REFRESHTOKEN[[:space:]]*//p' "$COOKIE_JAR" | tr -d '[:space:]')

    if [ -n "$ACCESSTOKEN" ] && [ -n "$REFRESHTOKEN" ]; then
      # Step 2: Request admin-scoped token via UI API
      TOKEN_RESP=$(curl -sf \
        -H "X-Requested-With: XMLHttpRequest" \
        -H "cookie: ACCESSTOKEN=${ACCESSTOKEN}; REFRESHTOKEN=${REFRESHTOKEN}" \
        -G \
        --data-urlencode "expiry=0" \
        --data-urlencode "services[]=all" \
        --data-urlencode "scope=applied-permissions/admin" \
        --data-urlencode "username=vault-admin" \
        --data-urlencode "description=Vault plugin admin token" \
        "${JFROG_URL}/ui/api/v1/access/token/scoped" 2>&1) || TOKEN_RESP=""

      TOKEN=$(echo "$TOKEN_RESP" | sed -n 's/.*"access_token" *: *"\([^"]*\)".*/\1/p')
    fi
  fi

  if [ -z "$TOKEN" ]; then
    RETRY=$((RETRY + 1))
    echo "  Token creation attempt $RETRY/$MAX_RETRIES failed, retrying in 5s ..."
    sleep 5
  fi
done
rm -f "$COOKIE_JAR"

if [ -z "$TOKEN" ]; then
  echo "ERROR: Failed to obtain JFrog access token after $MAX_RETRIES attempts."
  exit 1
fi
echo "JFrog admin access token obtained."

# --- 10. Configure the plugin ---
vault write artifactory/config/admin \
  url="${JFROG_URL}" \
  access_token="$TOKEN" \
  bypass_artifactory_tls_verification=true \
  allow_scope_override="opt-in" \
  use_expiring_tokens=true
echo "Vault configured with JFrog Artifactory plugin."

# --- 11. Create the default role ---
vault write artifactory/roles/rise \
  scope="artifact:rise-docker-local/**:r" \
  max_ttl=86400 \
  default_ttl=600 \
  allow_scope_override=true \
  allowed_scopes='["artifact:rise-docker-local/**:r","artifact:rise-docker-local/**:r,w"]'
echo "Role 'artifactory/roles/rise' created."

# --- 12. Keep container running ---
wait $VAULT_PID
