#!/usr/bin/env bash
set -euo pipefail

NAMESPACE="${NAMESPACE:-rise-ci}"
RELEASE_NAME="${RELEASE_NAME:-rise-ci}"
IMAGE_REPOSITORY="${RISE_IMAGE_REPOSITORY:?RISE_IMAGE_REPOSITORY is required}"
IMAGE_TAG="${RISE_IMAGE_TAG:?RISE_IMAGE_TAG is required}"
RISE_PUBLIC_URL="http://rise.local"
RISE_CI_JWT_SIGNING_SECRET_B64="dGVzdC1qd3Qtc2VjcmV0LWtleS1mb3ItY2ktdGVzdGluZy1vbmx5LW5vdC1zZWN1cmU="

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
  docker run --rm --network host \
    -e "RISE_URL=http://127.0.0.1:3000" \
    -e "RISE_TOKEN=${RISE_TOKEN}" \
    "${IMAGE_REPOSITORY}:${IMAGE_TAG}" \
    "$@"
}

cleanup() {
  local exit_code=$?
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
  echo "Cleaning up Minikube"
  minikube delete || true
}
trap cleanup EXIT

echo "Ensuring clean Minikube environment"
minikube delete || true

echo "Starting Minikube"
minikube start --driver=docker --cpus=2 --memory=4096
minikube addons enable ingress

echo "Installing chart with CI image ${IMAGE_REPOSITORY}:${IMAGE_TAG}"
echo "Using CI values from helm/rise/values-ci.yaml"
cat helm/rise/values-ci.yaml
helm dependency build helm/rise

helm upgrade --install "${RELEASE_NAME}" ./helm/rise \
  --namespace "${NAMESPACE}" \
  --create-namespace \
  --values helm/rise/values-ci.yaml \
  --set "image.repository=${IMAGE_REPOSITORY}" \
  --set "image.tag=${IMAGE_TAG}" \
  --set "image.pullPolicy=Always" \
  --set-string "config.deployment_controller.auth_backend_url=http://${RELEASE_NAME}-server.${NAMESPACE}.svc.cluster.local:3000" \
  --set-string "config.deployment_controller.auth_signin_url=http://rise.local"

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
PROJECT_NAME="e2e-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}"
APP_NAMESPACE="rise-${PROJECT_NAME}"
rise_cli project create "${PROJECT_NAME}" --access-class public --no-rise-toml
rise_cli deploy --project "${PROJECT_NAME}" --image nginxinc/nginx-unprivileged:alpine --http-port 8080 --replicas 1

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

echo "Smoke test: helm upgrade is idempotent"
helm upgrade "${RELEASE_NAME}" ./helm/rise \
  --namespace "${NAMESPACE}" \
  --values helm/rise/values-ci.yaml \
  --set "image.repository=${IMAGE_REPOSITORY}" \
  --set "image.tag=${IMAGE_TAG}" \
  --set "image.pullPolicy=Always" \
  --set-string "config.deployment_controller.auth_backend_url=http://${RELEASE_NAME}-server.${NAMESPACE}.svc.cluster.local:3000" \
  --set-string "config.deployment_controller.auth_signin_url=http://rise.local"

echo "Minikube E2E smoke tests completed successfully"
