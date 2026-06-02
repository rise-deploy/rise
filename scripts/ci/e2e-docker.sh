#!/usr/bin/env bash
set -euo pipefail

# End-to-end smoke test for the Rise Docker deployment backend.
#
# Brings up the reference standalone stack with the local/e2e overlay
# (docker-compose.standalone.yaml + docker-compose.standalone.local.yaml:
# Rise + Postgres + Dex + registry + Traefik over plain HTTP on rise.localhost),
# then exercises the Docker deployment controller end to end against a FRESH
# database (volumes are removed on teardown), proving no manual workarounds are
# needed:
#
#   (a) public  — a `public` (access_requirement None) project deploys, the
#       Rise API reports it Healthy WITHOUT any manual SQL, and it is
#       reachable through Traefik (HTTP 200). This exercises the bootstrap
#       controller-class stamping and Traefik v3.7.1 negotiating the host
#       Docker API directly (no socket-proxy/relay).
#   (b) private — a `private` (access_requirement Member) project deploys,
#       gets Traefik forwardAuth middleware labels stamped, redirects an
#       unauthenticated request with exactly 302 to the SAME (app) host's
#       /.rise/auth/signin page, has its /.rise/* path served by the rise
#       backend (not the app), and allows a request carrying a valid Rise JWT
#       session cookie (HTTP 200).
#
# Auth for the test is an HS256 JWT minted from the config's
# jwt_signing_secret (email=admin@example.com → admin), mirroring
# scripts/ci/e2e-minikube.sh's create_rise_ci_token. The same token is used
# both as the CLI bearer token (RISE_TOKEN) and as the `rise_jwt` ingress
# cookie (the ingress handler validates it via verify_jwt_skip_aud).
#
# Image is overridable via RISE_IMAGE_REPOSITORY / RISE_IMAGE_TAG (consumed by
# both compose files); CI sets RISE_IMAGE_TAG to the built image.

# Base file + local/e2e overlay (HTTP on rise.localhost; no TLS/ACME). Both -f
# files are passed to every compose invocation via COMPOSE_ARGS.
COMPOSE_FILE="${COMPOSE_FILE:-docker-compose.standalone.yaml}"
COMPOSE_LOCAL_OVERLAY="${COMPOSE_LOCAL_OVERLAY:-docker-compose.standalone.local.yaml}"
COMPOSE_ARGS=(-f "${COMPOSE_FILE}" -f "${COMPOSE_LOCAL_OVERLAY}")
RISE_IMAGE_REPOSITORY="${RISE_IMAGE_REPOSITORY:-ghcr.io/rise-deploy/rise}"
RISE_IMAGE_TAG="${RISE_IMAGE_TAG:-pr-358-b98adea}"
export RISE_IMAGE_REPOSITORY RISE_IMAGE_TAG

RISE_URL="${RISE_URL:-http://localhost:3000}"
TRAEFIK_URL="${TRAEFIK_URL:-http://localhost:80}"
# Must match server.public_url and server.jwt_signing_secret in
# config/docker.yaml (local defaults). The control plane is reached on
# 127.0.0.1:3000 but the token iss/aud must equal public_url
# (http://rise.localhost:3000), so app hosts ({project}.rise.localhost) are
# subdomains of the public_url host and validate_redirect_url accepts the
# same-host post-login redirect.
RISE_PUBLIC_URL="http://rise.localhost:3000"
RISE_JWT_SIGNING_SECRET_B64="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="

RUN_ID="${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}"
PUB_PROJECT="pub-e2e-${RUN_ID}"
PRIV_PROJECT="priv-e2e-${RUN_ID}"
WHOAMI_IMAGE="${WHOAMI_IMAGE:-traefik/whoami}"

RISE_CLI_BIN=""
RISE_CLI_EXTRACT_CONTAINER="rise-e2e-cli-extract"

# Scratch dir for the extracted CLI and curl response bodies. Kept under the
# working directory by default so it is reachable by the Docker daemon even
# under confined (e.g. snap) Docker installs, where the daemon cannot read an
# arbitrary host $TMPDIR for `docker cp`. Overridable via E2E_TMPDIR.
E2E_TMPDIR="${E2E_TMPDIR:-${PWD}/.rise-e2e-docker-tmp}"

log() { echo "[e2e-docker] $*"; }

base64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

# Mint an HS256 Rise token: HMAC-SHA256 over header.payload signed with the
# base64-decoded jwt_signing_secret. email=admin@example.com → admin. iss/aud
# are the server's public_url. Identical to e2e-minikube's create_rise_ci_token
# (which uses a different secret/url).
create_rise_ci_token() {
  local now exp header payload key_hex signature
  now="$(date +%s)"
  exp="$((now + 3600))"
  header="$(printf '{"alg":"HS256","typ":"JWT"}' | base64url)"
  payload="$(printf '{"sub":"rise-ci","email":"admin@example.com","name":"Rise CI","iat":%s,"exp":%s,"iss":"%s","aud":"%s"}' \
    "${now}" "${exp}" "${RISE_PUBLIC_URL}" "${RISE_PUBLIC_URL}" | base64url)"
  key_hex="$(printf '%s' "${RISE_JWT_SIGNING_SECRET_B64}" | openssl base64 -d -A | od -An -tx1 -v | tr -d ' \n')"
  signature="$(printf '%s.%s' "${header}" "${payload}" \
    | openssl dgst -sha256 -mac HMAC -macopt "hexkey:${key_hex}" -binary | base64url)"
  printf '%s.%s.%s' "${header}" "${payload}" "${signature}"
}

# Extract the rise CLI from the deployed image for an exact version match.
extract_cli() {
  local image="${RISE_IMAGE_REPOSITORY}:${RISE_IMAGE_TAG}"
  RISE_CLI_BIN="${E2E_TMPDIR}/rise"
  log "Extracting rise CLI from ${image}"
  docker rm -f "${RISE_CLI_EXTRACT_CONTAINER}" >/dev/null 2>&1 || true
  docker create --name "${RISE_CLI_EXTRACT_CONTAINER}" "${image}" >/dev/null
  docker cp "${RISE_CLI_EXTRACT_CONTAINER}:/usr/local/bin/rise" "${RISE_CLI_BIN}"
  docker rm "${RISE_CLI_EXTRACT_CONTAINER}" >/dev/null
  chmod +x "${RISE_CLI_BIN}"
}

rise_cli() {
  RISE_URL="${RISE_URL}" RISE_TOKEN="${RISE_TOKEN}" "${RISE_CLI_BIN}" "$@"
}

# Poll the Rise API until the project's latest deployment reports Healthy.
wait_for_healthy() {
  local project="$1" out
  log "Waiting for ${project} to become Healthy"
  for _ in $(seq 1 60); do
    out="$(rise_cli deployment list --project "${project}" --limit 3 2>/dev/null || true)"
    if printf '%s' "${out}" | grep -q "Healthy"; then
      log "${project} is Healthy"
      return 0
    fi
    sleep 5
  done
  log "ERROR: ${project} did not reach Healthy"
  rise_cli deployment list --project "${project}" --limit 5 || true
  return 1
}

# Curl a host through Traefik, retrying while Traefik registers the route.
# Route registration lags the Rise API's "Healthy" mark by a few seconds
# (Traefik observes the new container via the Docker provider asynchronously),
# so a 404 right after Healthy just means "not registered yet". Retries until
# the response is non-404, then echoes the final HTTP code. Extra curl args
# (e.g. --cookie) are passed through.
#
#   curl_through_traefik <host> <out_file> [curl args...]
curl_through_traefik() {
  local host="$1" out="$2"
  shift 2
  local code=000
  for _ in $(seq 1 30); do
    code="$(curl -sS -o "${out}" -w '%{http_code}' \
      -H "Host: ${host}" "$@" "${TRAEFIK_URL}/" 2>/dev/null || echo 000)"
    if [[ "${code}" != "404" ]]; then
      break
    fi
    sleep 2
  done
  printf '%s' "${code}"
}

cleanup() {
  local exit_code=$?
  if [[ ${exit_code} -ne 0 ]]; then
    log "Failure (exit ${exit_code}) — dumping diagnostics"
    docker compose "${COMPOSE_ARGS[@]}" ps || true
    docker logs rise-backend --tail 100 2>&1 || true
    docker logs rise-traefik --tail 50 2>&1 || true
  fi
  docker rm -f "${RISE_CLI_EXTRACT_CONTAINER}" >/dev/null 2>&1 || true
  log "Tearing down stack"
  # App containers are created by the Rise Docker controller, not by compose,
  # so `compose down` leaves them (and keeps the rise_default network in use).
  # Remove them explicitly first via their Rise bookkeeping label.
  local app_containers
  app_containers="$(docker ps -aq --filter "label=rise.dev/managed-by=rise" 2>/dev/null || true)"
  if [[ -n "${app_containers}" ]]; then
    # shellcheck disable=SC2086
    docker rm -f ${app_containers} >/dev/null 2>&1 || true
  fi
  docker compose "${COMPOSE_ARGS[@]}" down -v || true
  rm -rf "${E2E_TMPDIR}" || true
}
trap cleanup EXIT

mkdir -p "${E2E_TMPDIR}"

log "Bringing up standalone stack (${RISE_IMAGE_REPOSITORY}:${RISE_IMAGE_TAG})"
# Fresh volumes: a clean Postgres proves the org's deploymentControllerClass is
# stamped automatically at bootstrap (no manual SQL).
docker compose "${COMPOSE_ARGS[@]}" down -v || true
docker compose "${COMPOSE_ARGS[@]}" up -d

log "Waiting for Rise /health"
for _ in $(seq 1 60); do
  code="$(curl -fsS -o /dev/null -w '%{http_code}' "${RISE_URL}/health" 2>/dev/null || echo 000)"
  if [[ "${code}" == "200" ]]; then
    log "Rise is healthy"
    break
  fi
  sleep 2
done
if [[ "${code:-000}" != "200" ]]; then
  log "ERROR: Rise /health never returned 200 (last=${code:-000})"
  exit 1
fi

# Confirm Traefik negotiated the host Docker API (not "client version too old").
if docker logs rise-traefik 2>&1 | grep -q "Provider connection established with docker"; then
  log "Traefik negotiated the Docker provider:"
  docker logs rise-traefik 2>&1 | grep "Provider connection established with docker" | head -1
elif docker logs rise-traefik 2>&1 | grep -qi "client version .* is too old"; then
  log "ERROR: Traefik could not negotiate the Docker API (client version too old)"
  exit 1
fi

extract_cli
RISE_TOKEN="$(create_rise_ci_token)"
export RISE_TOKEN

# Smoke: protected API rejects unauthenticated requests.
code="$(curl -sS -o /dev/null -w '%{http_code}' "${RISE_URL}/api/v1/projects" 2>/dev/null || echo 000)"
if [[ "${code}" != "401" && "${code}" != "403" ]]; then
  log "ERROR: expected 401/403 for unauthenticated /api/v1/projects, got ${code}"
  exit 1
fi
log "Unauthenticated API correctly rejected (${code})"

############################################
# (a) PUBLIC: deploys, Healthy w/o SQL, reachable through Traefik.
############################################
log "Scenario (a): public project ${PUB_PROJECT}"
rise_cli project create "${PUB_PROJECT}" --access-class public --no-rise-toml
rise_cli deploy --project "${PUB_PROJECT}" --image "${WHOAMI_IMAGE}" --http-port 80 --replicas 1
wait_for_healthy "${PUB_PROJECT}"

log "Asserting public project reachable through Traefik"
pub_body="${E2E_TMPDIR}/pub_body.txt"
code="$(curl_through_traefik "${PUB_PROJECT}.rise.localhost" "${pub_body}")"
if [[ "${code}" != "200" ]]; then
  log "ERROR: expected 200 for public project, got ${code}"
  exit 1
fi
if ! grep -qi "Hostname:" "${pub_body}"; then
  log "ERROR: public response did not look like whoami output"
  cat "${pub_body}" || true
  exit 1
fi
log "Public project reachable (200, whoami output)"

############################################
# (b) PRIVATE: forwardAuth — blocks unauth, allows authed cookie.
############################################
log "Scenario (b): private project ${PRIV_PROJECT}"
rise_cli project create "${PRIV_PROJECT}" --access-class private --no-rise-toml
rise_cli deploy --project "${PRIV_PROJECT}" --image "${WHOAMI_IMAGE}" --http-port 80 --replicas 1
wait_for_healthy "${PRIV_PROJECT}"

log "Asserting forwardAuth middleware labels are stamped"
# The routable container (with Traefik + forwardAuth labels) can lag the API's
# "Healthy" mark briefly: the reconciler first creates the container, then
# recreates it with routing labels on the next tick. Poll until forwardAuth
# labels appear on the current app container.
labels=""
priv_container=""
for _ in $(seq 1 30); do
  priv_container="$(docker ps --filter "name=rise_${PRIV_PROJECT}" --format '{{.Names}}' | head -1)"
  if [[ -n "${priv_container}" ]]; then
    labels="$(docker inspect "${priv_container}" --format '{{json .Config.Labels}}')"
    if printf '%s' "${labels}" | grep -q "forwardauth.address" \
      && printf '%s' "${labels}" | grep -q '\.routers\..*\.middlewares'; then
      break
    fi
  fi
  sleep 2
done
if ! printf '%s' "${labels}" | grep -q "forwardauth.address"; then
  log "ERROR: private app container is missing forwardAuth labels"
  printf '%s\n' "${labels}"
  exit 1
fi
if ! printf '%s' "${labels}" | grep -q '\.routers\..*\.middlewares'; then
  log "ERROR: private app router is missing the middlewares label"
  printf '%s\n' "${labels}"
  exit 1
fi
log "forwardAuth labels present on ${priv_container}"
printf '%s' "${labels}" | tr ',' '\n' | grep -iE "forwardauth.address|routers\..*\.middlewares" | sed 's/^[[:space:]]*/[e2e-docker]   /'

log "Asserting unauthenticated request returns a same-host signin redirect"
priv_app_host="${PRIV_PROJECT}.rise.localhost"
priv_hdrs="${E2E_TMPDIR}/priv_hdrs.txt"
# Traefik forwardAuth mode (signin_redirect=1): unauthenticated MUST be exactly
# 302 (not 401) with a Location pointing at the SAME (app) host's
# /.rise/auth/signin page for this project — proving the cookie is set on the
# app host that forwardAuth reads (no cross-host control-plane redirect loop).
code="$(curl_through_traefik "${priv_app_host}" /dev/null -D "${priv_hdrs}")"
if [[ "${code}" != "302" ]]; then
  log "ERROR: expected exactly 302 for unauthenticated private request, got ${code}"
  cat "${priv_hdrs}" || true
  exit 1
fi
priv_location="$(grep -i '^location:' "${priv_hdrs}" | head -1 | tr -d '\r' | sed 's/^[Ll]ocation:[[:space:]]*//')"
log "Unauthenticated private request redirected (302) to: ${priv_location}"
# Location must be on the APP host and hit the /.rise/auth/signin page for this
# project (URL-encoded project name appears as project=<priv project>).
if ! printf '%s' "${priv_location}" | grep -q "//${priv_app_host}/.rise/auth/signin"; then
  log "ERROR: redirect Location is not the same-host /.rise/auth/signin page on ${priv_app_host}"
  exit 1
fi
if ! printf '%s' "${priv_location}" | grep -q "project=${PRIV_PROJECT}"; then
  log "ERROR: redirect Location does not carry project=${PRIV_PROJECT}"
  exit 1
fi
log "Redirect targets the same-host signin page for ${PRIV_PROJECT}"

log "Asserting /.rise/* on the private app host is served by the BACKEND"
# Traefik routes PathPrefix('/.rise') on every host to the rise-backend
# container (added to the compose by the deployment owner). The signin page
# there is allowlisted by ingress_auth (returns 200, not the app and not a
# forwardAuth block), proving the /.rise→backend route closes the bypass.
rise_path_body="${E2E_TMPDIR}/priv_rise_path_body.txt"
rise_path_code=000
for _ in $(seq 1 30); do
  rise_path_code="$(curl -sS -o "${rise_path_body}" -w '%{http_code}' \
    -H "Host: ${priv_app_host}" \
    "${TRAEFIK_URL}/.rise/auth/signin?project=${PRIV_PROJECT}" 2>/dev/null || echo 000)"
  if [[ "${rise_path_code}" != "404" ]]; then
    break
  fi
  sleep 2
done
if [[ "${rise_path_code}" != "200" ]]; then
  log "ERROR: expected 200 for /.rise/auth/signin on app host (backend-served), got ${rise_path_code}"
  cat "${rise_path_body}" || true
  exit 1
fi
# The whoami app would echo "Hostname:"; the signin page must NOT, proving the
# backend (not the app) served it.
if grep -qi "Hostname:" "${rise_path_body}"; then
  log "ERROR: /.rise path was served by the app (whoami), not the backend"
  cat "${rise_path_body}" || true
  exit 1
fi
log "/.rise/* on the app host is served by the backend (200, not the app)"

log "Asserting authenticated request (rise_jwt cookie) is allowed"
priv_body="${E2E_TMPDIR}/priv_body.txt"
code="$(curl_through_traefik "${PRIV_PROJECT}.rise.localhost" "${priv_body}" \
  --cookie "rise_jwt=${RISE_TOKEN}")"
if [[ "${code}" != "200" ]]; then
  log "ERROR: expected 200 for authenticated private request, got ${code}"
  cat "${priv_body}" || true
  exit 1
fi
if ! grep -qi "Hostname:" "${priv_body}"; then
  log "ERROR: authenticated private response did not look like whoami output"
  cat "${priv_body}" || true
  exit 1
fi
log "Authenticated private request allowed (200, whoami output)"

log "Docker backend E2E smoke tests completed successfully"
