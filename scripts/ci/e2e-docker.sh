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

# The workload-identity scenario builds a fixture image and pushes it to the
# in-compose registry; the Docker controller then pulls it via the HOST daemon
# (the backend mounts the host socket). The compose default registry_url
# (rise-registry:5000) is only resolvable INSIDE rise_default, so point the
# backend's pull URL at the loopback-published registry (127.0.0.1:5000), which
# the host daemon can reach and treats as insecure. Push (client_registry_url=
# localhost:5000) and pull (127.0.0.1:5000) hit the same registry. Public-image
# scenarios are unaffected (they pull from their own registries, not this one).
export RISE_REGISTRY_URL="${RISE_REGISTRY_URL:-127.0.0.1:5000}"

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
# Cutover scenarios. Kept short so the group-scoped Traefik service name
# ({project}-default-app, sanitized) stays well under any length limit.
CUT_PROJECT="cut-e2e-${RUN_ID}"
CUT_RECREATE_PROJECT="cutr-e2e-${RUN_ID}"
ID_PROJECT="id-e2e-${RUN_ID}"
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

# Sanitize a string into a Traefik router/service name exactly as the Docker
# controller does (src/.../docker/labels.rs::sanitize_router_name): lowercase,
# every char outside [a-z0-9-] collapsed to a single '-', trimmed of leading/
# trailing '-'. The group-scoped service base is sanitize("{project}-{group}-
# {container}"); for a single-container deploy group=default, container=app.
sanitize_router_name() {
  printf '%s' "$1" \
    | tr '[:upper:]' '[:lower:]' \
    | sed -E 's/[^a-z0-9-]+/-/g; s/-+/-/g; s/^-+//; s/-+$//'
}

# Query the internal Traefik API (http://rise-traefik:8080), which is NOT
# published to the host (only :80 is). We reach it from inside the rise_default
# compose network by exec'ing the already-running `dex` container, which ships
# `wget` (it uses wget in its own healthcheck) — so no extra image is pulled.
# Prints the response body to stdout; non-zero exit on transport failure.
#
#   traefik_api <path>   e.g. traefik_api "/api/http/services/foo@docker"
traefik_api() {
  local path="$1"
  docker compose "${COMPOSE_ARGS[@]}" exec -T dex \
    wget -q -O - "http://rise-traefik:8080${path}" 2>/dev/null
}

# Poll an app host through Traefik for a fixed number of rounds and FAIL the
# moment any response is a 5xx (or a transport error). Returns 0 only if every
# observed response was < 500. Used to assert the rolling cutover has no 5xx gap:
# the old servers must keep serving until the new ones are confirmed UP.
#
#   assert_no_5xx_gap <host> <rounds> <sleep_secs>
assert_no_5xx_gap() {
  local host="$1" rounds="$2" gap="$3" code i
  for ((i = 1; i <= rounds; i++)); do
    code="$(curl -sS -o /dev/null -w '%{http_code}' \
      -H "Host: ${host}" "${TRAEFIK_URL}/" 2>/dev/null || echo 000)"
    if [[ "${code}" == "000" || "${code}" -ge 500 ]]; then
      log "ERROR: cutover gap — request through Traefik returned ${code} (expected 2xx/3xx)"
      return 1
    fi
    sleep "${gap}"
  done
  return 0
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

log "Asserting app container has the local rise.localhost->backend extra_hosts entry"
# LOCAL-DEV: the controller injects `rise.localhost:<backend-ip>` into every app
# container's HostConfig.ExtraHosts so apps can reach the issuer/control plane
# (validate rise_jwt, OIDC discovery). The container can be (re)created across a
# reconcile tick, so poll until the entry appears.
pub_extra_hosts=""
pub_container=""
for _ in $(seq 1 30); do
  pub_container="$(docker ps --filter "name=rise_${PUB_PROJECT}" --format '{{.Names}}' | head -1)"
  if [[ -n "${pub_container}" ]]; then
    pub_extra_hosts="$(docker inspect "${pub_container}" --format '{{.HostConfig.ExtraHosts}}')"
    if printf '%s' "${pub_extra_hosts}" | grep -q "rise.localhost:"; then
      break
    fi
  fi
  sleep 2
done
if ! printf '%s' "${pub_extra_hosts}" | grep -q "rise.localhost:"; then
  log "ERROR: app container ${pub_container} is missing the rise.localhost extra_hosts entry"
  printf 'ExtraHosts: %s\n' "${pub_extra_hosts}"
  exit 1
fi
log "extra_hosts present on ${pub_container}: ${pub_extra_hosts}"

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

############################################
# (c) HEALTH-ROLLING CUTOVER: durable regression defense against the
#     serverStatus-shape bug class. A health-rolling cutover gates retirement of
#     the old active deployment on Traefik's per-server health (serverStatus). If
#     the gate signal is shaped wrong (or absent), the gate silently no-ops. This
#     scenario exercises the REAL external contract end to end:
#       1. deploy with a health_check + 2 replicas (cutover defaults to
#          health-rolling) and confirm it serves 200 via Traefik;
#       2. assert the Traefik API actually EXPOSES a non-empty TOP-LEVEL
#          serverStatus map (the gate signal) for the group-scoped service;
#       3. redeploy a changed revision and assert NO 5xx gap across the cutover.
############################################
log "Scenario (c): health-rolling cutover ${CUT_PROJECT}"

# A health_check requires a `[containers.<name>]` block in rise.toml (the
# single-image `--image` path has none), so deploy from a generated project
# dir. The `app` container pins the same already-pulled whoami image (which
# answers 200 on `/`, so `/` is a valid health path — no new image dependency),
# port 80, 2 replicas, and an explicit health_check. With a health_check present
# and the default health-rolling strategy, the controller stamps the per-server
# Traefik healthcheck labels, so Traefik publishes serverStatus.
CUT_DIR="${E2E_TMPDIR}/cut"
mkdir -p "${CUT_DIR}"
cat >"${CUT_DIR}/rise.toml" <<EOF
[project]
name = "${CUT_PROJECT}"

[containers.app]
image = "${WHOAMI_IMAGE}"
port = 80
replicas = 2

[containers.app.deploy.health_check]
path = "/"
EOF

rise_cli project create "${CUT_PROJECT}" --access-class public --no-rise-toml
# Deploy reads rise.toml from the path arg; REV=1 so the next deploy's changed
# env forces a fresh revision (new env-hash) in the same group → a cutover.
rise_cli deploy "${CUT_DIR}" --project "${CUT_PROJECT}" --env REV=1
wait_for_healthy "${CUT_PROJECT}"

log "Asserting cutover project serves 200 through Traefik"
cut_body="${E2E_TMPDIR}/cut_body.txt"
code="$(curl_through_traefik "${CUT_PROJECT}.rise.localhost" "${cut_body}")"
if [[ "${code}" != "200" ]]; then
  log "ERROR: expected 200 for cutover project, got ${code}"
  cat "${cut_body}" || true
  exit 1
fi
if ! grep -qi "Hostname:" "${cut_body}"; then
  log "ERROR: cutover response did not look like whoami output"
  cat "${cut_body}" || true
  exit 1
fi
log "Cutover project reachable (200, whoami output)"

# The controller's group-scoped Traefik service name is
# `{cap(sanitize("{project}-{group}-{container}"),48)}-{hex16}`, where the
# 16-hex suffix is a stable hash of the structured tuple that keeps the name
# injective across tenants (see `group_service_base`). The hash makes the name
# impractical to re-derive in shell, so we read the AUTHORITATIVE service name
# straight off a running app container's
# `traefik.http.services.<svc>.loadbalancer.server.port` label and use that for
# the serverStatus query. We still guard against naming drift by asserting it
# matches the readable, sanitized base (defaults group=default, container=app)
# followed by a 16-hex suffix.
CUT_SVC_BASE="$(sanitize_router_name "${CUT_PROJECT}-default-app")"

cut_label_svc=""
for _ in $(seq 1 30); do
  cut_container="$(docker ps --filter "name=rise_${CUT_PROJECT}" --format '{{.Names}}' | head -1)"
  if [[ -n "${cut_container:-}" ]]; then
    # Extract the service name from the loadbalancer.server.port label key
    # (traefik.http.services.<svc>.loadbalancer.server.port). This is exactly
    # the service the serverStatus query targets.
    cut_label_svc="$(docker inspect "${cut_container}" --format '{{json .Config.Labels}}' \
      | tr ',' '\n' \
      | sed -nE 's/.*"traefik\.http\.services\.([^.]+)\.loadbalancer\.server\.port".*/\1/p' \
      | head -1)"
    if [[ -n "${cut_label_svc}" ]]; then
      break
    fi
  fi
  sleep 2
done
if [[ -z "${cut_label_svc}" ]]; then
  log "ERROR: could not read the Traefik service name off the cutover app container labels"
  exit 1
fi
# Drift guard: the readable base (CUT_PROJECT is short, so it isn't truncated)
# plus the injective 16-hex suffix the controller appends.
if [[ ! "${cut_label_svc}" =~ ^${CUT_SVC_BASE}-[0-9a-f]{16}$ ]]; then
  log "ERROR: container-label service '${cut_label_svc}' does not match expected '${CUT_SVC_BASE}-<hex16>'"
  exit 1
fi
CUT_SVC="${cut_label_svc}"
log "Group-scoped Traefik service (from container label): ${CUT_SVC}@docker"

log "Asserting Traefik API exposes a non-empty TOP-LEVEL serverStatus for ${CUT_SVC}@docker"
# THE core regression assertion: the serverStatus gate signal must exist and be
# correctly shaped. Traefik v3.x puts serverStatus at the TOP LEVEL of the
# service object (a sibling of loadBalancer), keyed by server URL
# (http://{ip}:{port}) with UP/DOWN values. We assert:
#   - the service object exists (.name present),
#   - the TOP-LEVEL .serverStatus is a non-empty object,
#   - every key looks like http://host:port,
#   - every value is UP or DOWN (and at least one reaches UP).
# The serverStatus map populates only once Traefik has run a health check, so
# poll until it appears (or fail after the budget).
cut_svc_json="${E2E_TMPDIR}/cut_svc.json"
cut_status_ok=0
for _ in $(seq 1 30); do
  if ! traefik_api "/api/http/services/${CUT_SVC}@docker" >"${cut_svc_json}" 2>/dev/null; then
    sleep 2
    continue
  fi
  # Require a parseable service object whose TOP-LEVEL serverStatus is a
  # non-empty map of http://host:port -> UP/DOWN with at least one UP.
  if jq -e '
        (.name != null)
        and ((.serverStatus // {}) | type == "object")
        and ((.serverStatus // {}) | length > 0)
        and ((.serverStatus // {}) | to_entries
              | all(.key | test("^http://[^/]+:[0-9]+$")))
        and ((.serverStatus // {}) | to_entries
              | all(.value | test("^(UP|DOWN)$"; "i")))
        and ((.serverStatus // {}) | to_entries
              | any(.value | test("^UP$"; "i")))
      ' "${cut_svc_json}" >/dev/null 2>&1; then
    cut_status_ok=1
    break
  fi
  sleep 2
done
if [[ "${cut_status_ok}" != "1" ]]; then
  log "ERROR: Traefik API did not expose a valid non-empty top-level serverStatus for ${CUT_SVC}@docker"
  log "Last Traefik service payload:"
  cat "${cut_svc_json}" || true
  exit 1
fi
log "Traefik API exposes a non-empty top-level serverStatus (gate signal present):"
jq -c '.serverStatus' "${cut_svc_json}" | sed 's/^/[e2e-docker]   /'

log "Forcing a cutover (changed revision) and asserting no 5xx gap across the rollout"
# Redeploy a changed revision (new env → new env-hash → new deployment in the
# same group → a health-rolling cutover). Poll the app URL through Traefik for
# the duration of the rollout and assert every response is 2xx/3xx: the rolling
# overlap promise is that the old servers keep serving until the new servers are
# confirmed UP, so there must be NO 5xx gap.
#
# The deploy returns once the deployment is created; the cutover then proceeds
# asynchronously over several reconcile ticks. Poll across that window. (We
# assert "no 5xx", not strict request counting: in CI the exact tick timing is
# not deterministic, so a hard "served by new revision within N seconds" check
# would be flaky. The no-5xx invariant is the durable, non-flaky contract.)
rise_cli deploy "${CUT_DIR}" --project "${CUT_PROJECT}" --env REV=2
if ! assert_no_5xx_gap "${CUT_PROJECT}.rise.localhost" 40 1; then
  log "ERROR: observed a 5xx gap during the health-rolling cutover"
  rise_cli deployment list --project "${CUT_PROJECT}" --limit 5 || true
  exit 1
fi
# After the polling window the new revision must be Healthy and still serving.
wait_for_healthy "${CUT_PROJECT}"
code="$(curl_through_traefik "${CUT_PROJECT}.rise.localhost" "${cut_body}")"
if [[ "${code}" != "200" ]]; then
  log "ERROR: expected 200 after cutover, got ${code}"
  exit 1
fi
log "Health-rolling cutover completed with no 5xx gap (final 200)"

# Assert the NEW revision actually TOOK OVER, not just that "something is Healthy".
# wait_for_healthy only greps for "Healthy" anywhere, and during the rolling
# overlap the OLD (REV=1) deployment is Healthy too — so a no-op cutover where the
# new servers never join would still pass the no-5xx + Healthy checks above. The
# durable end state after convergence is: every running app container for the
# cutover project carries the REV=2 env we deployed, and NO running container
# still carries REV=1 (the old replicas have been retired). We read it straight
# off the running containers' `.Config.Env` (the same `docker inspect` idiom used
# for the label assertions above). Poll until convergence so we assert the settled
# state, not a race-y mid-cutover snapshot.
log "Asserting the new revision (REV=2) became the active/serving one"
cut_rev_ok=0
for _ in $(seq 1 30); do
  # All running app containers for the cutover project (replicas=2 → expect ≥1).
  mapfile -t cut_app_containers < <(
    docker ps --filter "name=rise_${CUT_PROJECT}" --format '{{.Names}}'
  )
  if [[ "${#cut_app_containers[@]}" -eq 0 ]]; then
    sleep 2
    continue
  fi
  saw_rev2=0
  saw_rev1=0
  for c in "${cut_app_containers[@]}"; do
    env_json="$(docker inspect "${c}" --format '{{json .Config.Env}}' 2>/dev/null || echo '[]')"
    if printf '%s' "${env_json}" | jq -e 'any(.[]; . == "REV=2")' >/dev/null 2>&1; then
      saw_rev2=1
    fi
    if printf '%s' "${env_json}" | jq -e 'any(.[]; . == "REV=1")' >/dev/null 2>&1; then
      saw_rev1=1
    fi
  done
  if [[ "${saw_rev2}" -eq 1 && "${saw_rev1}" -eq 0 ]]; then
    cut_rev_ok=1
    break
  fi
  sleep 2
done
if [[ "${cut_rev_ok}" != "1" ]]; then
  log "ERROR: REV=2 did not become the sole active revision after cutover convergence"
  log "(expected every running rise_${CUT_PROJECT} app container to carry REV=2 and none to carry REV=1)"
  for c in "${cut_app_containers[@]}"; do
    log "  ${c} env: $(docker inspect "${c}" --format '{{json .Config.Env}}' 2>/dev/null || echo '?')"
  done
  rise_cli deployment list --project "${CUT_PROJECT}" --limit 5 || true
  exit 1
fi
log "New revision took over: all running ${CUT_PROJECT} app containers carry REV=2, none carry REV=1"

############################################
# (d) RECREATE STRATEGY: the recreate cutover path must ALSO converge to 200.
#     Recreate mode now uses the same 2xx-3xx readiness contract, so a deploy
#     under RISE_CUTOVER_STRATEGY=recreate must still come up Healthy and serve
#     200. Kept lightweight: we flip the backend's cutover strategy in place
#     (env override + recreate the rise service) rather than standing up a second
#     full stack, then deploy a fresh project and assert it converges to 200.
############################################
log "Scenario (d): recreate-strategy convergence ${CUT_RECREATE_PROJECT}"
log "Switching the rise backend to RISE_CUTOVER_STRATEGY=recreate"
# config/docker.yaml reads cutover_strategy from ${RISE_CUTOVER_STRATEGY:-health-rolling}.
# Set it for the backend and recreate just the rise service so it re-reads config;
# the Postgres state (org bootstrap, existing projects) is preserved.
RISE_CUTOVER_STRATEGY=recreate \
  docker compose "${COMPOSE_ARGS[@]}" up -d --no-deps --force-recreate rise

log "Waiting for Rise /health after the recreate-strategy switch"
for _ in $(seq 1 60); do
  code="$(curl -fsS -o /dev/null -w '%{http_code}' "${RISE_URL}/health" 2>/dev/null || echo 000)"
  if [[ "${code}" == "200" ]]; then
    break
  fi
  sleep 2
done
if [[ "${code:-000}" != "200" ]]; then
  log "ERROR: Rise /health never returned 200 after recreate-strategy switch (last=${code:-000})"
  exit 1
fi

# Re-mint the token (a fresh process; the secret is unchanged so the old token
# would still validate, but re-minting keeps the iat/exp fresh).
RISE_TOKEN="$(create_rise_ci_token)"
export RISE_TOKEN

# A single-replica recreate deploy of the same whoami image must converge to 200.
rise_cli project create "${CUT_RECREATE_PROJECT}" --access-class public --no-rise-toml
rise_cli deploy --project "${CUT_RECREATE_PROJECT}" --image "${WHOAMI_IMAGE}" --http-port 80 --replicas 1
wait_for_healthy "${CUT_RECREATE_PROJECT}"

log "Asserting recreate-strategy project serves 200 through Traefik"
cutr_body="${E2E_TMPDIR}/cutr_body.txt"
code="$(curl_through_traefik "${CUT_RECREATE_PROJECT}.rise.localhost" "${cutr_body}")"
if [[ "${code}" != "200" ]]; then
  log "ERROR: expected 200 for recreate-strategy project, got ${code}"
  cat "${cutr_body}" || true
  exit 1
fi
if ! grep -qi "Hostname:" "${cutr_body}"; then
  log "ERROR: recreate-strategy response did not look like whoami output"
  cat "${cutr_body}" || true
  exit 1
fi
log "Recreate strategy converges to 200 (whoami output)"

############################################
# (e) WORKLOAD IDENTITY: a deployed app with `[identity.audiences]` receives a
#     bootstrap credential + a pre-minted token file, can exchange the credential
#     for a fresh token, and the controller re-mints the token file in place. The
#     fixture (tests/e2e-identity-fixture) verifies the RS256/JWKS signatures
#     INSIDE the container and reports JSON, so the assertions (shared with the
#     K8s e2e via scripts/ci/lib/identity.sh) are backend-agnostic. The fixture
#     is built from source and pushed to the in-compose registry, exercising the
#     Docker build→push→pull path end to end.
############################################
log "Scenario (e): workload identity ${ID_PROJECT}"
# shellcheck source=scripts/ci/lib/identity.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/identity.sh"
IDENTITY_LOG_PREFIX="e2e-docker"

ID_APP_HOST="${ID_PROJECT}.rise.localhost"
rise_cli project create "${ID_PROJECT}" --access-class public --no-rise-toml
rise_cli deploy tests/e2e-identity-fixture \
  --project "${ID_PROJECT}" \
  --backend docker:build \
  --http-port 8080 \
  --replicas 1
wait_for_healthy "${ID_PROJECT}"

# Full contract: credential present, pre-minted file token (aud=rise-e2e-audience)
# is signature-valid, on-demand exchange yields a signature-valid token with the
# requested audience and the project-bound subject.
if ! assert_workload_identity "${TRAEFIK_URL}" "${ID_APP_HOST}" "${ID_PROJECT}" \
  "e2e" "rise-e2e-audience" "rise-e2e-exchange"; then
  log "ERROR: workload identity assertion failed for ${ID_PROJECT}"
  exit 1
fi

# In-place refresh: with RISE_IDENTITY_TOKEN_TTL_SECONDS=60 the controller
# re-mints the token file at ~30s; the reconciler runs every 5s and re-uploads it
# to the live container.
if ! assert_identity_token_refreshes "${TRAEFIK_URL}" "${ID_APP_HOST}" "e2e" 120; then
  log "ERROR: workload identity token did not refresh for ${ID_PROJECT}"
  exit 1
fi
log "Workload identity verified end to end (issuance, pre-minted file, in-place refresh)"

log "Docker backend E2E smoke tests completed successfully"
