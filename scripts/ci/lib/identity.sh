#!/usr/bin/env bash
# Shared workload-identity e2e assertions, sourced by both the Docker
# (scripts/ci/e2e-docker.sh) and Kubernetes (scripts/ci/e2e-minikube.sh)
# orchestrators. The deployed fixture (tests/e2e-identity-fixture) does the
# token exchange and RS256/JWKS verification *inside* the container and reports
# the result as JSON, so these assertions are identical on every backend — the
# only per-backend difference is HOW the app is reached (Traefik vs.
# port-forward), which the caller passes in as a base URL + optional Host header.
#
# Public functions:
#   assert_workload_identity        <base_url> <host_header> <project> \
#                                   <file_name> <file_audience> <exchange_audience>
#   assert_identity_token_refreshes <base_url> <host_header> <file_name> <timeout_secs>
#
# host_header may be empty (e.g. when reaching the app via kubectl port-forward).
#
# These require `jq` and `curl` on PATH and rely on `set -euo pipefail` in the
# caller; failures `return 1` so the caller's `||` / trap handles diagnostics.

IDENTITY_LOG_PREFIX="${IDENTITY_LOG_PREFIX:-e2e-identity}"

_id_log() { echo "[${IDENTITY_LOG_PREFIX}] $*"; }

# Fetch GET /identity into <out_file>, retrying while the route/app warms up
# (a proxy can answer 404/000/502 for a few seconds after the API marks the
# deployment Healthy). Succeeds once the response is HTTP 200 AND parses as JSON.
# Prints nothing; returns 0 on success, 1 after exhausting the budget.
#
#   _id_fetch <base_url> <host_header> <query> <out_file>
_id_fetch() {
  local base_url="$1" host="$2" query="$3" out="$4"
  local code curl_args
  for _ in $(seq 1 45); do
    curl_args=(-sS -o "${out}" -w '%{http_code}' --max-time 20)
    if [[ -n "${host}" ]]; then
      curl_args+=(-H "Host: ${host}")
    fi
    code="$(curl "${curl_args[@]}" "${base_url}/identity?${query}" 2>/dev/null || echo 000)"
    if [[ "${code}" == "200" ]] && jq -e . "${out}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  _id_log "ERROR: could not fetch a JSON /identity response from ${base_url} (last code=${code:-000})"
  cat "${out}" 2>/dev/null || true
  return 1
}

# assert_workload_identity <base_url> <host_header> <project> \
#                          <file_name> <file_audience> <exchange_audience>
#
# Asserts the full contract in one shot:
#   - the bootstrap credential is present in the container;
#   - the pre-minted file token (from [identity.audiences]) is a genuine
#     Rise-signed JWT (signature verifies against the JWKS) with the declared
#     audience;
#   - an on-demand exchange of the credential yields a genuine JWT with the
#     requested audience, the project-bound subject, and the Rise issuer.
assert_workload_identity() {
  local base_url="$1" host="$2" project="$3"
  local file_name="$4" file_audience="$5" exchange_audience="$6"
  local out
  out="$(mktemp)"

  _id_log "Asserting workload identity for project '${project}' via ${base_url} (host=${host:-none})"
  if ! _id_fetch "${base_url}" "${host}" "file=${file_name}&audience=${exchange_audience}" "${out}"; then
    rm -f "${out}"
    return 1
  fi

  # The expected subject binds the token to this project. The e2e deploys carry
  # no environment, so the env segment is the literal `<null>`; we assert the
  # project-bound prefix to stay robust if an environment is ever introduced.
  local expected_sub_prefix="rise:proj:${project}:env:"

  if ! jq -e \
    --arg faud "${file_audience}" \
    --arg xaud "${exchange_audience}" \
    --arg subp "${expected_sub_prefix}" \
    '
      (.credential_present == true)
      and (.file_token.present == true)
      and (.file_token.signature_valid == true)
      and (.file_token.claims.aud == $faud)
      and (.exchanged_token.signature_valid == true)
      and (.exchanged_token.claims.aud == $xaud)
      and ((.exchanged_token.claims.sub // "") | startswith($subp))
      and ((.file_token.claims.sub // "") | startswith($subp))
      and ((.exchanged_token.claims.iss // "") | length > 0)
      and (.exchanged_token.claims.iss == .issuer)
    ' "${out}" >/dev/null 2>&1; then
    _id_log "ERROR: workload identity assertion failed. /identity response:"
    jq . "${out}" 2>/dev/null | sed "s/^/[${IDENTITY_LOG_PREFIX}]   /" || cat "${out}"
    rm -f "${out}"
    return 1
  fi

  _id_log "Workload identity verified:"
  jq -c '{credential_present,
          file_token: {valid: .file_token.signature_valid, aud: .file_token.claims.aud, sub: .file_token.claims.sub},
          exchanged_token: {valid: .exchanged_token.signature_valid, aud: .exchanged_token.claims.aud, iss: .exchanged_token.claims.iss}}' \
    "${out}" | sed "s/^/[${IDENTITY_LOG_PREFIX}]   /"
  rm -f "${out}"
  return 0
}

# assert_identity_token_refreshes <base_url> <host_header> <file_name> <timeout_secs>
#
# Proves the controller re-mints the pre-minted token file in place (at half its
# TTL) and that the updated file propagates to the running container. Captures
# the file token's `jti`, then polls until a DIFFERENT, still-signature-valid
# `jti` appears within the timeout. Requires the deployment to run with a short
# identity_token_ttl_seconds so a refresh occurs inside the window.
assert_identity_token_refreshes() {
  local base_url="$1" host="$2" file_name="$3" timeout="${4:-180}"
  local out first_jti cur_jti valid waited=0 interval=5
  out="$(mktemp)"

  if ! _id_fetch "${base_url}" "${host}" "file=${file_name}" "${out}"; then
    rm -f "${out}"
    return 1
  fi
  first_jti="$(jq -r '.file_token.claims.jti // empty' "${out}")"
  if [[ -z "${first_jti}" ]]; then
    _id_log "ERROR: file token has no jti claim; cannot observe refresh. Response:"
    jq . "${out}" 2>/dev/null || cat "${out}"
    rm -f "${out}"
    return 1
  fi
  _id_log "Observing in-place token refresh (initial jti=${first_jti}, timeout=${timeout}s)"

  while (( waited < timeout )); do
    sleep "${interval}"
    waited=$((waited + interval))
    if ! _id_fetch "${base_url}" "${host}" "file=${file_name}" "${out}"; then
      continue
    fi
    cur_jti="$(jq -r '.file_token.claims.jti // empty' "${out}")"
    valid="$(jq -r '.file_token.signature_valid' "${out}")"
    if [[ -n "${cur_jti}" && "${cur_jti}" != "${first_jti}" ]]; then
      if [[ "${valid}" != "true" ]]; then
        _id_log "ERROR: token rotated (jti=${cur_jti}) but the new token failed signature verification"
        jq . "${out}" 2>/dev/null || cat "${out}"
        rm -f "${out}"
        return 1
      fi
      _id_log "Token refreshed in place after ~${waited}s (jti ${first_jti} -> ${cur_jti}, signature still valid)"
      rm -f "${out}"
      return 0
    fi
  done

  _id_log "ERROR: file token did not refresh within ${timeout}s (jti stayed ${first_jti})"
  rm -f "${out}"
  return 1
}
