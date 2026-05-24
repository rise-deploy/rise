#!/usr/bin/env bash
# One-stop dev environment setup for Rise. Cross-platform (Linux + macOS).
#
# Usage:
#   ./scripts/dev-setup.sh             # interactive: do everything
#   ./scripts/dev-setup.sh hosts       # only /etc/hosts entries
#   ./scripts/dev-setup.sh docker      # only Docker insecure-registries
#   ./scripts/dev-setup.sh minikube    # bring up local minikube + helm install
#   ./scripts/dev-setup.sh k3s         # bring up local k3s (Linux only)
#   ./scripts/dev-setup.sh preflight   # hosts + docker, no cluster
#
# Run this script directly (not via a `mise` task dependency chain) so that
# sudo can prompt on a real TTY when needed.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
OS=$(uname -s)

REQUIRED_HOSTS=(rise-registry rise-jfrog rise.local)
REQUIRED_REGISTRIES='["rise-registry:5000","localhost:5000","127.0.0.1:5000","rise-jfrog:8082","localhost:3082"]'

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m OK\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m  !\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31m  x\033[0m %s\n' "$*" >&2; }
ask()  { local p=$1 d=${2:-} a; read -r -p "$p${d:+ [$d]}: " a; echo "${a:-$d}"; }

require_cmd() {
  local missing=()
  for c in "$@"; do command -v "$c" >/dev/null 2>&1 || missing+=("$c"); done
  if (( ${#missing[@]} > 0 )); then
    err "Missing required tools: ${missing[*]}"
    err "Install them first (most are managed by 'mise install')."
    exit 1
  fi
}

ensure_sudo() {
  # Cache sudo credentials up front so later commands run non-interactively
  # without surprising the user mid-script.
  if ! sudo -v; then
    err "sudo authentication failed"
    exit 1
  fi
  # Keep sudo alive while the script runs.
  ( while true; do sudo -n true; sleep 50; kill -0 "$$" 2>/dev/null || exit; done ) 2>/dev/null &
}

# ---- /etc/hosts ------------------------------------------------------------

setup_hosts() {
  log "Configuring /etc/hosts entries: ${REQUIRED_HOSTS[*]}"
  local tmp
  tmp=$(mktemp)
  # Drop any existing lines that contain one of our hostnames as a
  # whitespace-separated token. We avoid \< / \> word boundaries because BSD
  # awk on macOS doesn't honor them — that bug caused this dedup to silently
  # match nothing and entries to accumulate across runs. Field-based exact
  # comparison is portable.
  awk '
    {
      keep = 1
      for (i = 1; i <= NF; i++) {
        if ($i == "rise-registry" || $i == "rise-jfrog" || $i == "rise.local") {
          keep = 0
          break
        }
      }
      if (keep) print
    }
  ' /etc/hosts > "$tmp"
  {
    echo '127.0.0.1 rise-registry'
    echo '127.0.0.1 rise-jfrog'
    echo '127.0.0.1 rise.local'
  } >> "$tmp"
  if ! cmp -s "$tmp" /etc/hosts; then
    sudo cp "$tmp" /etc/hosts
    ok "/etc/hosts updated"
  else
    ok "/etc/hosts already configured"
  fi
  rm -f "$tmp"
}

check_hosts() {
  local missing=()
  for h in "${REQUIRED_HOSTS[@]}"; do
    awk -v h="$h" '
      {
        for (i = 1; i <= NF; i++) if ($i == h) { found = 1; exit }
      }
      END { exit !found }
    ' /etc/hosts || missing+=("$h")
  done
  (( ${#missing[@]} == 0 ))
}

# ---- Docker insecure-registries -------------------------------------------

daemon_json_path() {
  if [[ "$OS" == "Darwin" ]]; then
    echo "$HOME/.docker/daemon.json"
  else
    echo "/etc/docker/daemon.json"
  fi
}

write_daemon_json() {
  # $1 = target path, $2 = "sudo" or "" for the writer
  local target=$1 writer=$2 merged
  if [[ -f "$target" ]]; then
    merged=$(jq --argjson regs "$REQUIRED_REGISTRIES" \
      '.["insecure-registries"] = (((.["insecure-registries"] // []) + $regs) | unique)' "$target")
  else
    merged=$(jq -n --argjson regs "$REQUIRED_REGISTRIES" '{"insecure-registries": $regs}')
  fi
  if [[ -n "$writer" ]]; then
    printf '%s\n' "$merged" | $writer tee "$target" >/dev/null
  else
    mkdir -p "$(dirname "$target")"
    printf '%s\n' "$merged" > "$target"
  fi
}

wait_for_docker() {
  local timeout=${1:-180}
  log "Waiting up to ${timeout}s for the Docker daemon to come back up..."
  local i=0
  until docker info >/dev/null 2>&1; do
    if (( i >= timeout )); then
      err "Docker did not become ready within ${timeout}s"
      return 1
    fi
    sleep 2
    i=$(( i + 2 ))
  done
  ok "Docker daemon is responsive"
}

restart_docker_desktop_mac() {
  # `docker desktop restart` has been observed to return successfully while
  # leaving the engine dead, so we drive the restart explicitly: stop -> wait
  # for processes to exit -> relaunch via Launch Services (which reliably
  # brings the GUI + engine up) -> poll for the daemon.
  log "Stopping Docker Desktop"
  if docker desktop --help >/dev/null 2>&1; then
    docker desktop stop >/dev/null 2>&1 || true
  else
    # The bundle id is com.docker.docker regardless of whether the app is
    # named "Docker" or "Docker Desktop"; target it by id.
    osascript -e 'tell application id "com.docker.docker" to quit' >/dev/null 2>&1 || true
  fi

  # Wait for the GUI/engine processes to actually exit before relaunching.
  local i=0
  while pgrep -fq 'Docker Desktop|/Applications/Docker.app/Contents/MacOS/Docker'; do
    if (( i >= 30 )); then
      warn "Docker processes still running after 30s; force-killing"
      pkill -f 'Docker Desktop|/Applications/Docker.app/Contents/MacOS/Docker' || true
      sleep 2
      break
    fi
    sleep 1
    i=$(( i + 1 ))
  done

  log "Launching Docker Desktop"
  if ! open -b com.docker.docker; then
    err "Could not launch Docker Desktop via bundle id com.docker.docker."
    err "Start it manually from Spotlight or /Applications/Docker.app and re-run."
    return 1
  fi

  if ! wait_for_docker 180; then
    err "Docker daemon did not come up. Try launching Docker Desktop manually"
    err "(Spotlight -> Docker) and then re-run './scripts/dev-setup.sh minikube'."
    return 1
  fi
}

setup_docker() {
  require_cmd jq docker
  local target writer
  target=$(daemon_json_path)

  if ! docker info >/dev/null 2>&1; then
    err "Docker daemon is not reachable. Start Docker (Docker Desktop on macOS) and re-run."
    exit 1
  fi

  # Idempotency: if the required registries are already present, skip the
  # rewrite (and the needless Docker Desktop restart prompt on macOS).
  if check_docker; then
    ok "Docker insecure-registries already configured ($target)"
    return 0
  fi

  if [[ "$OS" == "Darwin" ]]; then
    writer=""
    log "Updating Docker Desktop daemon config at $target"
    write_daemon_json "$target" "$writer"
    warn "Docker Desktop must be restarted for insecure-registries to apply."
    if [[ "${RISE_DEV_RESTART_DOCKER:-}" != "0" ]]; then
      local ans
      ans=$(ask "Restart Docker Desktop now? (y/N)" "N")
      if [[ "$ans" =~ ^[Yy]$ ]]; then
        restart_docker_desktop_mac || warn "Automatic restart failed; restart Docker Desktop manually."
      else
        warn "Restart Docker Desktop manually before bringing up the cluster."
      fi
    fi
  else
    if [[ ! -d /etc/docker ]]; then
      warn "/etc/docker missing — is Docker installed natively? Skipping daemon.json update."
      warn "If you use Docker Desktop on Linux, set insecure-registries via its UI."
      return 0
    fi
    writer="sudo"
    log "Updating $target (requires sudo)"
    ensure_sudo
    write_daemon_json "$target" "$writer"
    if command -v systemctl >/dev/null 2>&1; then
      log "Restarting docker.service"
      sudo systemctl restart docker || warn "systemctl restart docker failed; restart manually."
    else
      warn "No systemctl found; restart the Docker daemon manually."
    fi
  fi
  ok "Docker insecure-registries configured"
}

check_docker() {
  local target
  target=$(daemon_json_path)
  if [[ ! -f "$target" ]]; then return 1; fi
  grep -q 'rise-registry:5000' "$target"
}

# ---- Host IP detection (cross-platform) -----------------------------------

get_host_ip() {
  # Print the host's outbound IPv4 to stdout, or nothing if it can't be
  # determined. MUST always return 0 — callers do their own empty-check, and
  # we don't want `set -e` to kill the script silently from inside a $().
  local ip=""
  case "$OS" in
    Linux)
      ip=$(ip -4 route get 1.1.1.1 2>/dev/null \
        | awk '{for (i = 1; i <= NF; i++) if ($i == "src") {print $(i + 1); exit}}') || true
      ;;
    Darwin)
      local iface
      iface=$(route -n get 1.1.1.1 2>/dev/null | awk '/interface:/ {print $2}') || true
      if [[ -n "$iface" ]]; then
        # `ifconfig` is more reliable than `ipconfig getifaddr`, which returns
        # empty (with rc=1) for cellular/tether/utun interfaces even when an
        # inet address is configured.
        ip=$(ifconfig "$iface" 2>/dev/null \
          | awk '/^[[:space:]]+inet / {print $2; exit}') || true
      fi
      ;;
  esac
  [[ -n "$ip" ]] && printf '%s\n' "$ip"
  return 0
}

# ---- .env management ------------------------------------------------------

write_dev_env_block() {
  local host_ip=$1
  local f="$REPO_ROOT/.env"
  if [[ -f "$f" ]]; then
    awk '
      /^# BEGIN RISE K8S DEV$/ { skip = 1; next }
      /^# END RISE K8S DEV$/   { skip = 0; next }
      !skip
    ' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
  fi
  cat >> "$f" <<EOF
# BEGIN RISE K8S DEV
export RISE_K8S_HOST_IP="${host_ip}"
export RISE_AUTH_BACKEND_URL="http://${host_ip}:3000"
export RISE_METACONTROLLER_WEBHOOK_BASE_URL="http://${host_ip}:3001"
# END RISE K8S DEV
EOF
  ok "Wrote .env block (RISE_K8S_HOST_IP=$host_ip)"
}

helm_install_rise_dev() {
  local host_ip=$1 skip_crds=${2:-}
  cd "$REPO_ROOT"
  helm dependency build helm/rise
  local args=(
    upgrade --install rise-dev ./helm/rise
    -f helm/rise/values-dev.yaml
    --set "metacontroller.webhookBaseUrl=http://${host_ip}:3001"
    --namespace rise --create-namespace
  )
  [[ -n "$skip_crds" ]] && args+=(--skip-crds)
  helm "${args[@]}"
}

# ---- Minikube --------------------------------------------------------------

setup_minikube() {
  require_cmd docker minikube kubectl helm
  cd "$REPO_ROOT"
  log "Starting local registry via docker compose"
  docker compose up -d registry

  local network=rise_default
  local reg_name=rise-registry
  local reg_dir="/etc/containerd/certs.d/localhost:5000"
  local jfrog_dir="/etc/containerd/certs.d/rise-jfrog:8082"

  # Start minikube only if it's not already reachable. Recreating wastes
  # 30-60s and nukes state the developer may want to keep — `minikube delete`
  # explicitly when a fresh cluster is wanted.
  #
  # `minikube status` parses container IPs and breaks when minikube has more
  # than one network attached (which our setup intentionally does). Use
  # kubectl reachability against the minikube context as the source of truth.
  if kubectl --context=minikube get nodes >/dev/null 2>&1; then
    ok "minikube cluster already reachable; skipping 'minikube start'"
  else
    log "Starting minikube"
    minikube start --driver=docker \
      --insecure-registry="${reg_name}:5000" \
      --insecure-registry="rise-jfrog:8082" \
      --memory=4096mb --cpus=2 --cni=calico
  fi

  log "Setting kubectl context to 'minikube'"
  kubectl config use-context minikube >/dev/null

  log "Ensuring ingress addon is enabled"
  minikube addons enable ingress

  log "Wiring containerd registry mirrors inside minikube"
  for node in $(minikube node list | awk '{print $1}'); do
    docker exec "${node}" mkdir -p "${reg_dir}"
    cat <<EOF | docker exec -i "${node}" cp /dev/stdin "${reg_dir}/hosts.toml"
[host."http://${reg_name}:5000"]
EOF
    docker exec "${node}" mkdir -p "${jfrog_dir}"
    cat <<EOF | docker exec -i "${node}" cp /dev/stdin "${jfrog_dir}/hosts.toml"
[host."http://rise-jfrog:8082"]
EOF
  done

  docker network connect "${network}" minikube 2>/dev/null || true

  if lsof -iTCP:8080 -sTCP:LISTEN >/dev/null 2>&1; then
    ok "Port 8080 already in use; assuming ingress port-forward is running"
  else
    log "Port-forwarding ingress-nginx-controller to localhost:8080 (HTTP) and 8443 (HTTPS)"
    kubectl port-forward --namespace ingress-nginx svc/ingress-nginx-controller 8080:80 8443:443 \
      >/dev/null 2>&1 &
    ok "Port-forward running in background (PID: $!)"
  fi

  local host_ip
  host_ip=$(get_host_ip)
  if [[ -z "$host_ip" ]]; then
    err "Could not determine host IP. Inspect 'route -n get 1.1.1.1' (Darwin) or 'ip route get 1.1.1.1' (Linux)."
    return 1
  fi
  write_dev_env_block "$host_ip"
  helm_install_rise_dev "$host_ip" ""
  ok "minikube ready. If you use direnv: run 'direnv reload'."
}

# ---- K3s (Linux only) -----------------------------------------------------

setup_k3s() {
  if [[ "$OS" != "Linux" ]]; then
    err "k3s is Linux-only. On macOS, use the 'minikube' command instead."
    exit 1
  fi
  require_cmd docker kubectl helm curl
  ensure_sudo
  cd "$REPO_ROOT"

  log "Writing /etc/rancher/k3s/registries.yaml"
  sudo mkdir -p /etc/rancher/k3s
  sudo tee /etc/rancher/k3s/registries.yaml > /dev/null <<EOF
mirrors:
  "rise-registry:5000":
    endpoint:
      - "http://rise-registry:5000"
  "rise-jfrog:8082":
    endpoint:
      - "http://localhost:3082"
EOF

  log "Starting local registry via docker compose"
  docker compose up -d registry

  log "Installing k3s"
  curl -sfL https://get.k3s.io \
    | INSTALL_K3S_EXEC="--flannel-backend=none --cluster-cidr=192.168.0.0/16 --disable-network-policy --disable=traefik" sh -
  sleep 10
  mkdir -p ~/.kube
  sudo cat /etc/rancher/k3s/k3s.yaml > ~/.kube/config

  log "Installing Calico + ingress-nginx"
  kubectl apply -f https://raw.githubusercontent.com/projectcalico/calico/v3.31.4/manifests/operator-crds.yaml --server-side --force-conflicts
  kubectl apply -f https://raw.githubusercontent.com/projectcalico/calico/v3.31.4/manifests/tigera-operator.yaml --server-side --force-conflicts
  kubectl apply -f https://raw.githubusercontent.com/projectcalico/calico/v3.31.4/manifests/custom-resources.yaml --server-side --force-conflicts
  helm upgrade --install ingress-nginx ingress-nginx \
    --repo https://kubernetes.github.io/ingress-nginx \
    --namespace ingress-nginx --create-namespace \
    --set controller.hostPort.enabled=true \
    --set controller.hostPort.ports.http=8080 \
    --set controller.hostPort.ports.https=8443 \
    --set controller.service.type=ClusterIP

  local host_ip
  host_ip=$(kubectl get node "$(hostname)" -o jsonpath='{.metadata.annotations.k3s\.io/internal-ip}')
  if [[ -z "$host_ip" ]]; then
    err "Could not determine k3s internal IP"
    return 1
  fi
  write_dev_env_block "$host_ip"
  kubectl apply --server-side --force-conflicts -f helm/rise/crds/
  helm_install_rise_dev "$host_ip" "--skip-crds"
  ok "k3s ready. K3s internal IP: $host_ip. If you use direnv: run 'direnv reload'."
}

# ---- Top-level commands ---------------------------------------------------

cmd_preflight() {
  setup_hosts
  setup_docker
}

cmd_all() {
  setup_hosts
  setup_docker

  local action
  if kubectl --context=minikube get nodes >/dev/null 2>&1; then
    if [[ "$OS" == "Linux" ]]; then
      action=$(ask "minikube already running. Action? [keep/recreate/k3s/skip]" "keep")
    else
      action=$(ask "minikube already running. Action? [keep/recreate/skip]" "keep")
    fi
  else
    if [[ "$OS" == "Linux" ]]; then
      action=$(ask "Bring up which cluster? [minikube/k3s/skip]" "minikube")
    else
      action=$(ask "Bring up minikube now? [minikube/skip]" "minikube")
    fi
  fi

  case "$action" in
    keep|minikube)
      setup_minikube
      ;;
    recreate)
      log "Deleting existing minikube cluster before recreate"
      minikube delete >/dev/null || warn "minikube delete reported an error; continuing"
      setup_minikube
      ;;
    k3s)
      setup_k3s
      ;;
    skip|"")
      log "Skipping cluster bring-up"
      ;;
    *)
      err "Unknown action: $action"
      exit 1
      ;;
  esac
}

usage() {
  sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

main() {
  local cmd=${1:-all}
  case "$cmd" in
    -h|--help|help) usage 0 ;;
    hosts)     setup_hosts ;;
    docker)    setup_docker ;;
    minikube)  setup_minikube ;;
    k3s)       setup_k3s ;;
    preflight) cmd_preflight ;;
    all)       cmd_all ;;
    *)         err "Unknown command: $cmd"; usage 1 ;;
  esac
}

main "$@"
