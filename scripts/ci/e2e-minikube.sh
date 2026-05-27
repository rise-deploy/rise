#!/usr/bin/env bash
set -euo pipefail

NAMESPACE="${NAMESPACE:-rise-ci}"
RELEASE_NAME="${RELEASE_NAME:-rise-ci}"
IMAGE_REPOSITORY="${RISE_IMAGE_REPOSITORY:?RISE_IMAGE_REPOSITORY is required}"
IMAGE_TAG="${RISE_IMAGE_TAG:?RISE_IMAGE_TAG is required}"
CLI_IMAGE_REPOSITORY="${RISE_CLI_IMAGE_REPOSITORY:-${IMAGE_REPOSITORY}}"
RISE_E2E_REGISTRY_MODE="${RISE_E2E_REGISTRY_MODE:-oci-client-auth}"
MINIKUBE_MEMORY="${MINIKUBE_MEMORY:-6144}"
MINIKUBE_CPUS="${MINIKUBE_CPUS:-4}"
RISE_E2E_ARTIFACT_DIR="${RISE_E2E_ARTIFACT_DIR:-.rise-e2e-artifacts}"
RISE_PUBLIC_URL="http://rise.local"
RISE_CI_JWT_SIGNING_SECRET_B64="dGVzdC1qd3Qtc2VjcmV0LWtleS1mb3ItY2ktdGVzdGluZy1vbmx5LW5vdC1zZWN1cmU="

case "${RISE_E2E_REGISTRY_MODE}" in
  oci-client-auth | jfrog-vault) ;;
  *)
    echo "Unsupported RISE_E2E_REGISTRY_MODE: ${RISE_E2E_REGISTRY_MODE}"
    exit 1
    ;;
esac

base64url() {
  openssl base64 -A | tr '+/' '-_' | tr -d '='
}

create_rise_ci_token() {
  local now exp header payload key_hex signature
  now="$(date +%s)"
  exp="$((now + 3600))"
  header="$(printf '{"alg":"HS256","typ":"JWT"}' | base64url)"
  payload="$(printf '{"sub":"rise-ci","email":"rise-ci@example.com","name":"Rise CI","iat":%s,"exp":%s,"iss":"%s","aud":"%s"}' "${now}" "${exp}" "${RISE_PUBLIC_URL}" "${RISE_PUBLIC_URL}" | base64url)"
  key_hex="$(printf '%s' "${RISE_CI_JWT_SIGNING_SECRET_B64}" | openssl base64 -d -A | od -An -tx1 -v | tr -d ' \n')"
  signature="$(printf '%s.%s' "${header}" "${payload}" | openssl dgst -sha256 -mac HMAC -macopt "hexkey:${key_hex}" -binary | base64url)"
  printf '%s.%s.%s' "${header}" "${payload}" "${signature}"
}

rise_cli() {
  local docker_args=(
    --rm
    --network host
    -e "RISE_URL=http://127.0.0.1:3000"
    -e "RISE_TOKEN=${RISE_TOKEN}"
    -e "MISE_TRUSTED_CONFIG_PATHS=${PWD}"
    -v "${PWD}:${PWD}"
    -w "${PWD}"
  )

  if [[ "${RISE_CLI_WITH_DOCKER:-false}" == "true" ]]; then
    docker_args+=(
      -v /var/run/docker.sock:/var/run/docker.sock
      -e DOCKER_CONFIG=/tmp/rise-docker-config
    )
  fi

  docker run "${docker_args[@]}" \
    --entrypoint /usr/local/bin/rise \
    "${CLI_IMAGE_REPOSITORY}:${IMAGE_TAG}" \
    "$@"
}

wait_for_jfrog_vault() {
  echo "Waiting for Vault-backed JFrog registry setup"
  echo "Streaming Vault setup logs until the Vault role is available"
  docker compose --profile jfrog-vault logs --follow --tail=100 vault &
  COMPOSE_LOGS_PID=$!

  for attempt in {1..120}; do
    if curl -fsS \
      -H "X-Vault-Token: root" \
      "http://127.0.0.1:8200/v1/artifactory/roles/rise" >/dev/null 2>&1; then
      kill "${COMPOSE_LOGS_PID}" >/dev/null 2>&1 || true
      wait "${COMPOSE_LOGS_PID}" >/dev/null 2>&1 || true
      COMPOSE_LOGS_PID=""
      return 0
    fi

    if (( attempt % 6 == 0 )); then
      docker compose --profile jfrog-vault ps jfrog vault || true
    fi
    sleep 5
  done

  echo "Timed out waiting for Vault Artifactory role"
  kill "${COMPOSE_LOGS_PID}" >/dev/null 2>&1 || true
  wait "${COMPOSE_LOGS_PID}" >/dev/null 2>&1 || true
  COMPOSE_LOGS_PID=""
  collect_jfrog_vault_artifacts
  exit 1
}

collect_jfrog_vault_artifacts() {
  local artifact_dir="${RISE_E2E_ARTIFACT_DIR}/jfrog-vault"
  local previous_log="${artifact_dir}/rise-backend-previous.log"

  mkdir -p "${artifact_dir}"
  docker compose --profile jfrog-vault ps jfrog vault > "${artifact_dir}/compose-ps.txt" 2>&1 || true
  docker compose --profile jfrog-vault logs --no-color jfrog > "${artifact_dir}/jfrog.log" 2>&1 || true
  docker compose --profile jfrog-vault logs --no-color vault > "${artifact_dir}/vault.log" 2>&1 || true

  if kubectl cluster-info >/dev/null 2>&1; then
    kubectl get pods -A > "${artifact_dir}/k8s-pods.txt" 2>&1 || true
    kubectl get events -A --sort-by=.metadata.creationTimestamp > "${artifact_dir}/k8s-events.txt" 2>&1 || true
    kubectl logs -n "${NAMESPACE}" \
      -l "app.kubernetes.io/instance=${RELEASE_NAME},app.kubernetes.io/component=server" \
      -c server --tail=-1 > "${artifact_dir}/rise-backend.log" 2>&1 || true
    kubectl logs -n "${NAMESPACE}" \
      -l "app.kubernetes.io/instance=${RELEASE_NAME},app.kubernetes.io/component=server" \
      -c wait-for-postgresql --tail=-1 > "${artifact_dir}/rise-postgresql-wait.log" 2>&1 || true
    if ! kubectl logs -n "${NAMESPACE}" \
      -l "app.kubernetes.io/instance=${RELEASE_NAME},app.kubernetes.io/component=server" \
      -c server --previous --tail=-1 > "${previous_log}" 2>&1; then
      rm -f "${previous_log}"
    fi
  fi
}

configure_minikube_jfrog_registry() {
  local network_name="rise_default"
  local registry_dir="/etc/containerd/certs.d/rise-jfrog:8082"

  echo "Connecting Minikube node to ${network_name}"
  for node in $(minikube node list | awk '{print $1}'); do
    docker network connect "${network_name}" "${node}" >/dev/null 2>&1 || true
    docker exec "${node}" mkdir -p "${registry_dir}"
    cat <<EOF | docker exec -i "${node}" cp /dev/stdin "${registry_dir}/hosts.toml"
[host."http://rise-jfrog:8082"]
EOF
  done
}

cleanup() {
  local exit_code=$?
  if [[ -n "${COMPOSE_LOGS_PID:-}" ]] && kill -0 "${COMPOSE_LOGS_PID}" >/dev/null 2>&1; then
    kill "${COMPOSE_LOGS_PID}" >/dev/null 2>&1 || true
    wait "${COMPOSE_LOGS_PID}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${PF_PID:-}" ]] && kill -0 "${PF_PID}" >/dev/null 2>&1; then
    kill "${PF_PID}" >/dev/null 2>&1 || true
  fi
  if [[ $exit_code -ne 0 ]]; then
    kubectl get pods -A || true
    kubectl get events -A --sort-by=.metadata.creationTimestamp | tail -n 200 || true
    kubectl logs -n "${NAMESPACE}" -l "app.kubernetes.io/instance=${RELEASE_NAME}" --all-containers --tail=200 || true
    kubectl logs -n "${NAMESPACE}" -l "app.kubernetes.io/instance=${RELEASE_NAME}" --all-containers --previous --tail=200 || true
    if [[ -n "${APP_NAMESPACE:-}" ]]; then
      kubectl get all -n "${APP_NAMESPACE}" || true
      kubectl describe deployments -n "${APP_NAMESPACE}" || true
      while IFS= read -r pod; do
        kubectl logs -n "${APP_NAMESPACE}" "${pod}" --all-containers --tail=200 || true
      done < <(kubectl get pods -n "${APP_NAMESPACE}" -o name 2>/dev/null || true)
    fi
    cat /tmp/rise-e2e-port-forward.log || true
  fi
  if [[ "${RISE_E2E_REGISTRY_MODE}" == "jfrog-vault" ]]; then
    collect_jfrog_vault_artifacts
  fi
  echo "Cleaning up Minikube"
  minikube delete || true
  if [[ "${RISE_E2E_REGISTRY_MODE}" == "jfrog-vault" ]]; then
    echo "Cleaning up JFrog/Vault services"
    docker compose --profile jfrog-vault stop vault jfrog || true
    docker compose --profile jfrog-vault rm -fsv vault jfrog || true
  fi
}
trap cleanup EXIT

echo "Ensuring clean Minikube environment"
minikube delete || true

if [[ "${RISE_E2E_REGISTRY_MODE}" == "jfrog-vault" ]]; then
  echo "Starting JFrog and Vault services"
  docker compose --profile jfrog-vault up -d jfrog vault
  wait_for_jfrog_vault
fi

echo "Starting Minikube"
minikube_args=(--driver=docker --cpus="${MINIKUBE_CPUS}" --memory="${MINIKUBE_MEMORY}")
if [[ "${RISE_E2E_REGISTRY_MODE}" == "jfrog-vault" ]]; then
  minikube_args+=(--insecure-registry=rise-jfrog:8082)
fi
minikube start "${minikube_args[@]}"
minikube addons enable ingress

if [[ "${RISE_E2E_REGISTRY_MODE}" == "jfrog-vault" ]]; then
  configure_minikube_jfrog_registry
fi

echo "Installing chart with CI image ${IMAGE_REPOSITORY}:${IMAGE_TAG}"
echo "Using CI values from helm/rise/values-ci.yaml"
cat helm/rise/values-ci.yaml

helm_args=(
  --namespace "${NAMESPACE}"
  --create-namespace
  --values helm/rise/values-ci.yaml
  --set "image.repository=${IMAGE_REPOSITORY}"
  --set "image.tag=${IMAGE_TAG}"
  --set "image.pullPolicy=Always"
  --set-string "config.deployment_controller.auth_backend_url=http://${RELEASE_NAME}-server.${NAMESPACE}.svc.cluster.local:3000"
  --set-string "config.deployment_controller.auth_signin_url=http://rise.local"
)

if [[ "${RISE_E2E_REGISTRY_MODE}" == "jfrog-vault" ]]; then
  helm_args+=(
    --set-string "config.registry.type=jfrog"
    --set-string "config.registry.registry_host=rise-jfrog:8082"
    --set-string "config.registry.client_registry_url=localhost:3082"
    --set-string "config.registry.docker_repo_key=rise-docker-local"
    --set-string "config.registry.token_provider.type=vault"
    --set-string "config.registry.token_provider.vault_addr=http://host.minikube.internal:8200"
    --set-string "config.registry.token_provider.vault_token=root"
    --set-string "config.registry.token_provider.vault_mount_path=artifactory"
    --set-string "config.registry.token_provider.vault_role=rise"
    --set "config.registry.mint_pull_secrets=true"
  )
fi

helm upgrade --install "${RELEASE_NAME}" ./helm/rise \
  "${helm_args[@]}"

echo "Waiting for workloads to become ready"
kubectl wait --namespace "${NAMESPACE}" --for=condition=Available deployment -l "app.kubernetes.io/instance=${RELEASE_NAME}" --timeout=10m
kubectl wait --namespace "${NAMESPACE}" --for=condition=Ready pod -l "app.kubernetes.io/instance=${RELEASE_NAME}" --timeout=10m

server_service="$(kubectl get svc -n "${NAMESPACE}" -l "app.kubernetes.io/instance=${RELEASE_NAME},app.kubernetes.io/component=server" -o jsonpath='{.items[0].metadata.name}')"
if [[ -z "${server_service}" ]]; then
  echo "Failed to locate server service"
  exit 1
fi

echo "Port-forwarding ${server_service}"
kubectl -n "${NAMESPACE}" port-forward "svc/${server_service}" 3000:3000 >/tmp/rise-e2e-port-forward.log 2>&1 &
PF_PID=$!
sleep 5

echo "Smoke test: /health endpoint"
http_code="$(curl --silent --show-error --connect-timeout 5 --max-time 30 --output /dev/null --write-out "%{http_code}" "http://127.0.0.1:3000/health")"
if [[ "${http_code}" != "200" ]]; then
  echo "Expected 200 for health check, got ${http_code}"
  exit 1
fi

echo "Smoke test: protected API returns auth error"
http_code="$(curl --silent --show-error --connect-timeout 5 --max-time 30 --output /dev/null --write-out "%{http_code}" "http://127.0.0.1:3000/api/v1/projects")"
if [[ "${http_code}" != "401" && "${http_code}" != "403" ]]; then
  echo "Expected 401/403 for unauthenticated request, got ${http_code}"
  exit 1
fi

echo "Smoke test: rise project create and rise deploy"
RISE_TOKEN="$(create_rise_ci_token)"
export RISE_TOKEN
PROJECT_NAME="e2e-${RISE_E2E_REGISTRY_MODE}-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}"
APP_NAMESPACE="rise-${PROJECT_NAME}"
rise_cli project create "${PROJECT_NAME}" --access-class public --no-rise-toml
if [[ "${RISE_E2E_REGISTRY_MODE}" == "jfrog-vault" ]]; then
  RISE_CLI_WITH_DOCKER=true rise_cli deploy \
    --project "${PROJECT_NAME}" \
    --backend docker:build \
    --container-cli docker \
    --http-port 8080 \
    --replicas 1 \
    tests/e2e-minikube/jfrog-vault-fixture
else
  rise_cli deploy --project "${PROJECT_NAME}" --image nginxinc/nginx-unprivileged:alpine --http-port 8080 --replicas 1
fi

echo "Waiting for app namespace ${APP_NAMESPACE}"
for _ in {1..60}; do
  if kubectl get namespace "${APP_NAMESPACE}" >/dev/null 2>&1; then
    break
  fi
  sleep 2
done
kubectl get namespace "${APP_NAMESPACE}" >/dev/null

echo "Waiting for deployed app workload to become available"
for _ in {1..60}; do
  if [[ -n "$(kubectl get deployments -n "${APP_NAMESPACE}" -o name 2>/dev/null)" ]]; then
    break
  fi
  sleep 5
done
if [[ -z "$(kubectl get deployments -n "${APP_NAMESPACE}" -o name 2>/dev/null)" ]]; then
  echo "Expected at least one app deployment in ${APP_NAMESPACE}"
  exit 1
fi
kubectl wait --namespace "${APP_NAMESPACE}" --for=condition=Available deployment --all --timeout=5m

echo "Waiting for Rise to mark deployment healthy"
deployment_list=""
for _ in {1..30}; do
  deployment_list="$(rise_cli deployment list --project "${PROJECT_NAME}" --limit 5)"
  printf '%s\n' "${deployment_list}"
  if [[ "${deployment_list}" == *"Healthy"* ]]; then
    break
  fi
  sleep 5
done
if [[ "${deployment_list}" != *"Healthy"* ]]; then
  echo "Expected Rise deployment status to become Healthy"
  exit 1
fi

echo "Smoke test: logs flow to Loki and Rise API can query them"

deployment_id="$(curl -sS -H "Authorization: Bearer ${RISE_TOKEN}" \
  "http://127.0.0.1:3000/api/v1/projects/${PROJECT_NAME}/deployments" \
  | jq -r '.[0].deployment_id')"
if [[ -z "${deployment_id}" || "${deployment_id}" == "null" ]]; then
  echo "Failed to resolve deployment_id from Rise API"
  exit 1
fi
echo "Resolved deployment_id=${deployment_id}"

# nginx-unprivileged's readiness/liveness probes (kube-probe) hit / on
# port 8080 every 10s, producing access-log lines on PID 1's stdout that
# kubelet captures and Alloy ships to Loki — so we don't need to inject
# any additional traffic to have something to assert on.

start_ts="$(date -u -d '-10 minutes' +%Y-%m-%dT%H:%M:%SZ)"
end_ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
counts_url="http://127.0.0.1:3000/api/v1/projects/${PROJECT_NAME}/deployments/${deployment_id}/logs/counts?start=${start_ts}&end=${end_ts}&step_seconds=60"

total=0
info=0
body=""
for _ in {1..30}; do
  body="$(curl -sS -H "Authorization: Bearer ${RISE_TOKEN}" "${counts_url}" || true)"
  total="$(printf '%s' "${body}" | jq '[.buckets[].total] | add // 0' 2>/dev/null || echo 0)"
  info="$(printf '%s' "${body}" | jq '[.buckets[].info]  | add // 0' 2>/dev/null || echo 0)"
  if (( total > 0 && info > 0 )); then
    break
  fi
  sleep 3
done

echo "Log counts: total=${total} info=${info}"
if (( total == 0 )); then
  echo "Expected /logs/counts to report total>0; last response: ${body}"
  exit 1
fi
if (( info == 0 )); then
  echo "Expected /logs/counts to report info bucket > 0; last response: ${body}"
  exit 1
fi

# Exercise the SSE log stream via the CLI. `rise logs` is non-follow by
# default (`--follow` is opt-in); in non-follow mode the server returns
# the backlog and closes the SSE stream, so the CLI exits naturally.
# (Cannot wrap with `timeout`: rise_cli is a bash function and `timeout`
# only invokes executables. If the server ever hangs the job-level
# timeout still catches it.)
lines="$(rise_cli deployment logs --project "${PROJECT_NAME}" "${deployment_id}" --tail 20 | wc -l)"
echo "rise logs returned ${lines} line(s)"
if (( lines == 0 )); then
  echo "Expected 'rise logs' to print at least one line"
  exit 1
fi

echo "Smoke test: helm upgrade is idempotent"
helm upgrade "${RELEASE_NAME}" ./helm/rise \
  "${helm_args[@]}"

echo "Minikube E2E smoke tests completed successfully"
