#!/usr/bin/env bash
# Bring up (or tear down) the AWS-side stack the ECS e2e backend runs against.
#
# The whole Rise stack runs IN the cluster — Traefik, Postgres, Dex and the Rise
# control plane, each as its own single-container ECS service. That is not just
# convenient: it is the only way Traefik can reach Rise for the forwardAuth
# subrequest, which is what lets the private-ingress-auth and
# route-access-override scenarios run rather than being declared skips. It also
# matches the production topology ADR-0004 D15 describes.
#
# Two chicken-and-egg problems shape the ordering:
#
#   1. The ingress URL templates need a domain, but the only public IP belongs to
#      a Fargate task that does not exist yet. Solved by starting Traefik FIRST,
#      reading its public IP, and deriving `<ip>.nip.io` — nip.io resolves
#      `*.1.2.3.4.nip.io` to 1.2.3.4, so Host-based routing works against an
#      address that only exists at run time.
#   2. Rise must reach Postgres and Dex, and Traefik must reach Rise, but every
#      task's IP is assigned at start. Solved with a Cloud Map private DNS
#      namespace — the mechanism scripts/spikes/adr-0004-cloudmap-sharing.sh
#      already verified.
#
# Everything is tagged `rise-e2e` and torn down on exit. KEEP=1 leaves the stack
# up for inspection (the Traefik dashboard is on :8080, restricted to your IP).
#
# Usage:
#   AWS_PROFILE=… AWS_REGION=eu-central-1 RISE_IMAGE_TAG=<published-tag> \
#     tests/e2e/aws/ecs-stack.sh up      # prints shell-eval'able exports
#   … tests/e2e/aws/ecs-stack.sh down
#
# Requirements: aws CLI v2, jq, curl. The account needs its Fargate on-demand
# vCPU quota raised above the fresh-account default of 6 — the control plane
# alone uses ~1.5 vCPU and every app task rounds up to 0.5.
set -uo pipefail

PREFIX="${RISE_E2E_PREFIX:-rise-e2e}"
NS_NAME="${PREFIX}.local"
TAG_KEY="rise-e2e"
TAG_VALUE="${PREFIX}"
STATE_FILE="${RISE_E2E_STATE_FILE:-/tmp/${PREFIX}-stack.env}"

TRAEFIK_IMAGE="${RISE_E2E_TRAEFIK_IMAGE:-traefik:v3.7.10}"
POSTGRES_IMAGE="${RISE_E2E_POSTGRES_IMAGE:-public.ecr.aws/docker/library/postgres:18-alpine}"
DEX_IMAGE="${RISE_E2E_DEX_IMAGE:-dexidp/dex:v2.45.1}"
RISE_IMAGE="${RISE_IMAGE_REPOSITORY:-ghcr.io/rise-deploy/rise}:${RISE_IMAGE_TAG:-}"

# Must match tests/e2e/src/backend/ecs.rs and config/ecs.yaml's defaults.
JWT_SECRET="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
POSTGRES_PASSWORD="rise123"

log()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*" >&2; }
info() { printf '    %s\n' "$*" >&2; }
die()  { printf '\033[1;31mFATAL: %s\033[0m\n' "$*" >&2; exit 1; }

command -v aws >/dev/null || die "aws CLI not found"
command -v jq  >/dev/null || die "jq not found"

CLUSTER="$PREFIX"
ROLE_EXEC="$PREFIX-exec-role"
ROLE_TASK="$PREFIX-task-role"
LOG_GROUP="/$PREFIX"

# ── teardown ────────────────────────────────────────────────────────────────

teardown() {
  log "Tearing down the ECS e2e stack (best-effort)"
  local services
  services=$(aws ecs list-services --cluster "$CLUSTER" --query 'serviceArns[]' --output text 2>/dev/null || true)
  for svc in $services; do
    aws ecs update-service --cluster "$CLUSTER" --service "$svc" --desired-count 0 >/dev/null 2>&1
    aws ecs delete-service --cluster "$CLUSTER" --service "$svc" --force >/dev/null 2>&1
  done
  # Tasks must stop before Cloud Map services will delete.
  for _ in $(seq 1 30); do
    local remaining
    remaining=$(aws ecs list-tasks --cluster "$CLUSTER" --query 'length(taskArns)' --output text 2>/dev/null || echo 0)
    [ "$remaining" = "0" ] && break
    sleep 10
  done

  local ns_id
  ns_id=$(aws servicediscovery list-namespaces \
    --query "Namespaces[?Name=='$NS_NAME'].Id | [0]" --output text 2>/dev/null || true)
  if [ -n "$ns_id" ] && [ "$ns_id" != "None" ]; then
    local sd_services
    sd_services=$(aws servicediscovery list-services \
      --filters "Name=NAMESPACE_ID,Values=$ns_id" --query 'Services[].Id' --output text 2>/dev/null || true)
    for sd in $sd_services; do
      local instances
      instances=$(aws servicediscovery list-instances --service-id "$sd" --query 'Instances[].Id' --output text 2>/dev/null || true)
      for inst in $instances; do
        aws servicediscovery deregister-instance --service-id "$sd" --instance-id "$inst" >/dev/null 2>&1
      done
      for _ in $(seq 1 12); do
        aws servicediscovery delete-service --id "$sd" >/dev/null 2>&1 && break
        sleep 5
      done
    done
    for _ in $(seq 1 12); do
      aws servicediscovery delete-namespace --id "$ns_id" >/dev/null 2>&1 && break
      sleep 10
    done
  fi

  # Task definitions accumulate revisions; deregister the ones we created.
  for family in "$PREFIX-traefik" "$PREFIX-postgres" "$PREFIX-dex" "$PREFIX-rise"; do
    local revs
    revs=$(aws ecs list-task-definitions --family-prefix "$family" --query 'taskDefinitionArns[]' --output text 2>/dev/null || true)
    for rev in $revs; do
      aws ecs deregister-task-definition --task-definition "$rev" >/dev/null 2>&1
    done
  done

  aws ecs delete-cluster --cluster "$CLUSTER" >/dev/null 2>&1
  aws logs delete-log-group --log-group-name "$LOG_GROUP" >/dev/null 2>&1

  local sg_id
  sg_id=$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$PREFIX-sg" \
    --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)
  if [ -n "$sg_id" ] && [ "$sg_id" != "None" ]; then
    # ENIs release slowly after a task stops.
    for _ in $(seq 1 30); do
      aws ec2 delete-security-group --group-id "$sg_id" >/dev/null 2>&1 && break
      sleep 10
    done
  fi

  for role in "$ROLE_EXEC" "$ROLE_TASK"; do
    local policies
    policies=$(aws iam list-role-policies --role-name "$role" --query 'PolicyNames[]' --output text 2>/dev/null || true)
    for pol in $policies; do
      aws iam delete-role-policy --role-name "$role" --policy-name "$pol" >/dev/null 2>&1
    done
    local attached
    attached=$(aws iam list-attached-role-policies --role-name "$role" --query 'AttachedPolicies[].PolicyArn' --output text 2>/dev/null || true)
    for arn in $attached; do
      aws iam detach-role-policy --role-name "$role" --policy-arn "$arn" >/dev/null 2>&1
    done
    aws iam delete-role --role-name "$role" >/dev/null 2>&1
  done

  rm -f "$STATE_FILE"
  info "teardown finished"
}

if [ "${1:-up}" = "down" ]; then
  teardown
  exit 0
fi

# ── bring-up ────────────────────────────────────────────────────────────────

[ -n "${RISE_IMAGE_TAG:-}" ] || die "RISE_IMAGE_TAG must name a published image tag (ECS pulls it from GHCR)"
aws sts get-caller-identity >/dev/null || die "no working AWS credentials"

REGION=$(aws configure get region 2>/dev/null || echo "${AWS_REGION:-}")
[ -n "$REGION" ] || die "set AWS_REGION or configure a default region"
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)

log "Resolving default VPC / subnet / caller IP"
VPC_ID=$(aws ec2 describe-vpcs --filters Name=is-default,Values=true --query 'Vpcs[0].VpcId' --output text)
[ "$VPC_ID" != "None" ] || die "no default VPC in $REGION"
SUBNET_ID=$(aws ec2 describe-subnets --filters "Name=vpc-id,Values=$VPC_ID" \
  --query 'Subnets[0].SubnetId' --output text)
MYIP=$(curl -s --max-time 10 https://checkip.amazonaws.com | tr -d '[:space:]')
[ -n "$MYIP" ] || die "could not determine the caller's public IP"
info "vpc=$VPC_ID subnet=$SUBNET_ID caller=$MYIP region=$REGION"

log "Creating security group"
SG_ID=$(aws ec2 create-security-group --group-name "$PREFIX-sg" \
  --description "Rise ECS e2e" --vpc-id "$VPC_ID" \
  --tag-specifications "ResourceType=security-group,Tags=[{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
  --query GroupId --output text) \
  || SG_ID=$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$PREFIX-sg" \
       --query 'SecurityGroups[0].GroupId' --output text)
# Only the caller reaches the edge; everything inside the group talks freely.
aws ec2 authorize-security-group-ingress --group-id "$SG_ID" --protocol tcp --port 80   --cidr "$MYIP/32" >/dev/null 2>&1
aws ec2 authorize-security-group-ingress --group-id "$SG_ID" --protocol tcp --port 8080 --cidr "$MYIP/32" >/dev/null 2>&1
aws ec2 authorize-security-group-ingress --group-id "$SG_ID" --protocol -1 --source-group "$SG_ID" >/dev/null 2>&1
info "sg=$SG_ID"

log "Creating IAM roles"
ASSUME='{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ecs-tasks.amazonaws.com"},"Action":"sts:AssumeRole"}]}'
aws iam create-role --role-name "$ROLE_EXEC" --assume-role-policy-document "$ASSUME" \
  --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null 2>&1
aws iam attach-role-policy --role-name "$ROLE_EXEC" \
  --policy-arn arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy >/dev/null 2>&1
# The execution role resolves the SSM SecureStrings Rise writes for secret env.
aws iam put-role-policy --role-name "$ROLE_EXEC" --policy-name ssm-secrets --policy-document "{
  \"Version\": \"2012-10-17\",
  \"Statement\": [{
    \"Effect\": \"Allow\",
    \"Action\": [\"ssm:GetParameters\", \"kms:Decrypt\"],
    \"Resource\": \"*\"
  }]
}" >/dev/null

aws iam create-role --role-name "$ROLE_TASK" --assume-role-policy-document "$ASSUME" \
  --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null 2>&1
# The Rise control plane's own AWS identity: it reconciles ECS and writes SSM.
# Scoped to this cluster and the Rise parameter path; PassRole is restricted to
# the two roles this stack created, never an arbitrary role.
aws iam put-role-policy --role-name "$ROLE_TASK" --policy-name rise-controller --policy-document "{
  \"Version\": \"2012-10-17\",
  \"Statement\": [
    {\"Effect\": \"Allow\", \"Action\": [\"ecs:*\"], \"Resource\": \"*\"},
    {\"Effect\": \"Allow\", \"Action\": [\"ec2:DescribeInstances\", \"ec2:DescribeNetworkInterfaces\"], \"Resource\": \"*\"},
    {\"Effect\": \"Allow\", \"Action\": [\"servicediscovery:*\"], \"Resource\": \"*\"},
    {\"Effect\": \"Allow\", \"Action\": [\"logs:CreateLogGroup\", \"logs:CreateLogStream\", \"logs:PutLogEvents\", \"logs:DescribeLogStreams\", \"logs:FilterLogEvents\"], \"Resource\": \"*\"},
    {\"Effect\": \"Allow\", \"Action\": [\"ssm:PutParameter\", \"ssm:DeleteParameter\", \"ssm:DeleteParameters\", \"ssm:GetParameter\", \"ssm:GetParameters\", \"ssm:GetParametersByPath\", \"ssm:AddTagsToResource\"], \"Resource\": \"arn:aws:ssm:$REGION:$ACCOUNT:parameter/rise/*\"},
    {\"Effect\": \"Allow\", \"Action\": [\"kms:Decrypt\", \"kms:Encrypt\", \"kms:GenerateDataKey\"], \"Resource\": \"*\"},
    {\"Effect\": \"Allow\", \"Action\": \"iam:PassRole\", \"Resource\": [
        \"arn:aws:iam::$ACCOUNT:role/$ROLE_EXEC\",
        \"arn:aws:iam::$ACCOUNT:role/$ROLE_TASK\"
    ]}
  ]
}" >/dev/null
EXEC_ROLE_ARN="arn:aws:iam::$ACCOUNT:role/$ROLE_EXEC"
TASK_ROLE_ARN="arn:aws:iam::$ACCOUNT:role/$ROLE_TASK"
sleep 10  # IAM propagation before PassRole at task launch

log "Creating cluster, log group and Cloud Map namespace"
aws ecs create-cluster --cluster-name "$CLUSTER" \
  --tags "key=$TAG_KEY,value=$TAG_VALUE" >/dev/null
aws logs create-log-group --log-group-name "$LOG_GROUP" \
  --tags "$TAG_KEY=$TAG_VALUE" >/dev/null 2>&1

NAMESPACE_ID=$(aws servicediscovery list-namespaces \
  --query "Namespaces[?Name=='$NS_NAME'].Id | [0]" --output text 2>/dev/null)
if [ -z "$NAMESPACE_ID" ] || [ "$NAMESPACE_ID" = "None" ]; then
  OP_ID=$(aws servicediscovery create-private-dns-namespace \
    --name "$NS_NAME" --vpc "$VPC_ID" --query OperationId --output text)
  for _ in $(seq 1 60); do
    STATUS=$(aws servicediscovery get-operation --operation-id "$OP_ID" --query Operation.Status --output text)
    [ "$STATUS" = "SUCCESS" ] && break
    [ "$STATUS" = "FAIL" ] && die "Cloud Map namespace creation failed"
    sleep 5
  done
  NAMESPACE_ID=$(aws servicediscovery get-operation --operation-id "$OP_ID" \
    --query 'Operation.Targets.NAMESPACE' --output text)
fi
info "namespace=$NAMESPACE_ID ($NS_NAME)"

sd_service() { # sd_service NAME -> echoes ARN
  local name=$1 existing
  existing=$(aws servicediscovery list-services --filters "Name=NAMESPACE_ID,Values=$NAMESPACE_ID" \
    --query "Services[?Name=='$name'].Arn | [0]" --output text 2>/dev/null)
  if [ -n "$existing" ] && [ "$existing" != "None" ]; then printf '%s' "$existing"; return; fi
  aws servicediscovery create-service --name "$name" --namespace-id "$NAMESPACE_ID" \
    --dns-config "NamespaceId=$NAMESPACE_ID,RoutingPolicy=MULTIVALUE,DnsRecords=[{Type=A,TTL=10}]" \
    --health-check-custom-config FailureThreshold=1 \
    --query Service.Arn --output text
}

NET_CFG="awsvpcConfiguration={subnets=[$SUBNET_ID],securityGroups=[$SG_ID],assignPublicIp=ENABLED}"

register_td() { # register_td JSON -> echoes ARN
  aws ecs register-task-definition --cli-input-json "$1" \
    --query taskDefinition.taskDefinitionArn --output text
}

wait_running() { # wait_running SERVICE_NAME
  local svc=$1
  for _ in $(seq 1 60); do
    local running
    running=$(aws ecs describe-services --cluster "$CLUSTER" --services "$svc" \
      --query 'services[0].runningCount' --output text 2>/dev/null || echo 0)
    [ "$running" != "0" ] && [ "$running" != "None" ] && return 0
    sleep 10
  done
  aws ecs describe-services --cluster "$CLUSTER" --services "$svc" \
    --query 'services[0].events[0:5].message' --output json >&2
  die "service $svc did not reach RUNNING (see events above; a Fargate vCPU quota of 6 on a fresh account is a common cause)"
}

task_public_ip() { # task_public_ip SERVICE_NAME
  local svc=$1 task_arn eni
  task_arn=$(aws ecs list-tasks --cluster "$CLUSTER" --service-name "$svc" --query 'taskArns[0]' --output text)
  eni=$(aws ecs describe-tasks --cluster "$CLUSTER" --tasks "$task_arn" \
    --query "tasks[0].attachments[0].details[?name=='networkInterfaceId'].value | [0]" --output text)
  aws ec2 describe-network-interfaces --network-interface-ids "$eni" \
    --query 'NetworkInterfaces[0].Association.PublicIp' --output text
}

# --- 1. Traefik, so we learn the public IP the domain is derived from ---
log "Starting Traefik"
# Registered in Cloud Map so the Rise control plane can read Traefik's API for
# per-server `serverStatus` — the authoritative readiness signal. Without a
# reachable API, any project declaring a health_check never becomes Healthy.
TRAEFIK_SD=$(sd_service traefik)
TD_TRAEFIK=$(register_td "$(jq -n \
  --arg img "$TRAEFIK_IMAGE" --arg cluster "$CLUSTER" --arg region "$REGION" \
  --arg exec_role "$EXEC_ROLE_ARN" --arg task_role "$TASK_ROLE_ARN" \
  --arg family "$PREFIX-traefik" --arg lg "$LOG_GROUP" '{
  family: $family, networkMode: "awsvpc", requiresCompatibilities: ["FARGATE"],
  cpu: "256", memory: "512", executionRoleArn: $exec_role, taskRoleArn: $task_role,
  containerDefinitions: [{
    name: "traefik", image: $img, essential: true,
    portMappings: [{containerPort: 80}, {containerPort: 8080}],
    command: [
      "--providers.ecs=true",
      ("--providers.ecs.clusters=" + $cluster),
      "--providers.ecs.exposedByDefault=false",
      "--providers.ecs.refreshSeconds=5",
      ("--providers.ecs.region=" + $region),
      "--entrypoints.web.address=:80",
      "--api.insecure=true",
      "--log.level=INFO"
    ],
    logConfiguration: {logDriver: "awslogs", options: {
      "awslogs-group": $lg, "awslogs-region": $region, "awslogs-stream-prefix": "traefik"}}
  }]}')")
aws ecs create-service --cluster "$CLUSTER" --service-name "$PREFIX-traefik" \
  --task-definition "$TD_TRAEFIK" --desired-count 1 --launch-type FARGATE \
  --network-configuration "$NET_CFG" --service-registries "registryArn=$TRAEFIK_SD" \
  --tags "key=$TAG_KEY,value=$TAG_VALUE" >/dev/null 2>&1
wait_running "$PREFIX-traefik"
TIP=$(task_public_ip "$PREFIX-traefik")
[ -n "$TIP" ] && [ "$TIP" != "None" ] || die "Traefik task has no public IP"
DOMAIN="${TIP}.nip.io"
info "traefik=$TIP  domain=$DOMAIN"

# --- 2. Postgres and Dex, discoverable by Cloud Map ---
log "Starting Postgres (ephemeral — a fresh DB per run exercises bootstrap)"
PG_SD=$(sd_service postgres)
TD_PG=$(register_td "$(jq -n --arg img "$POSTGRES_IMAGE" --arg pw "$POSTGRES_PASSWORD" \
  --arg exec_role "$EXEC_ROLE_ARN" --arg family "$PREFIX-postgres" \
  --arg lg "$LOG_GROUP" --arg region "$REGION" '{
  family: $family, networkMode: "awsvpc", requiresCompatibilities: ["FARGATE"],
  cpu: "512", memory: "1024", executionRoleArn: $exec_role,
  containerDefinitions: [{
    name: "postgres", image: $img, essential: true,
    portMappings: [{containerPort: 5432}],
    environment: [
      {name: "POSTGRES_USER", value: "rise"},
      {name: "POSTGRES_PASSWORD", value: $pw},
      {name: "POSTGRES_DB", value: "rise"}
    ],
    healthCheck: {command: ["CMD-SHELL", "pg_isready -U rise"], interval: 10, timeout: 5, retries: 5},
    logConfiguration: {logDriver: "awslogs", options: {
      "awslogs-group": $lg, "awslogs-region": $region, "awslogs-stream-prefix": "postgres"}}
  }]}')")
aws ecs create-service --cluster "$CLUSTER" --service-name "$PREFIX-postgres" \
  --task-definition "$TD_PG" --desired-count 1 --launch-type FARGATE \
  --network-configuration "$NET_CFG" --service-registries "registryArn=$PG_SD" \
  --tags "key=$TAG_KEY,value=$TAG_VALUE" >/dev/null 2>&1

log "Starting Dex"
DEX_SD=$(sd_service dex)
DEX_ISSUER="http://dex.${NS_NAME}:5556/dex"
# Dex reads a config FILE, which Fargate cannot bind-mount. Ship the repo's own
# config as base64 with the issuer patched to the Cloud Map address, and
# materialize it in the container. The harness reaches Dex through Traefik; the
# token's `iss` is this Cloud Map value, which is what Rise validates against.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DEX_CONFIG_B64=$(sed "s|^issuer:.*|issuer: ${DEX_ISSUER}|" "$REPO_ROOT/dev/dex/config.yaml" | base64 -w0)
TD_DEX=$(register_td "$(jq -n --arg img "$DEX_IMAGE" --arg cfg "$DEX_CONFIG_B64" \
  --arg exec_role "$EXEC_ROLE_ARN" --arg family "$PREFIX-dex" --arg domain "$DOMAIN" \
  --arg lg "$LOG_GROUP" --arg region "$REGION" '{
  family: $family, networkMode: "awsvpc", requiresCompatibilities: ["FARGATE"],
  cpu: "256", memory: "512", executionRoleArn: $exec_role,
  containerDefinitions: [{
    name: "dex", image: $img, essential: true, user: "0:0",
    portMappings: [{containerPort: 5556}],
    entryPoint: ["/bin/sh", "-c"],
    command: ["echo \"$DEX_CONFIG_B64\" | base64 -d > /tmp/dex.yaml && exec /usr/local/bin/dex serve /tmp/dex.yaml"],
    environment: [{name: "DEX_CONFIG_B64", value: $cfg}],
    dockerLabels: {
      "traefik.enable": "true",
      "traefik.http.routers.dex.rule": ("Host(`dex." + $domain + "`)"),
      "traefik.http.routers.dex.entrypoints": "web",
      "traefik.http.services.dex.loadbalancer.server.port": "5556"
    },
    logConfiguration: {logDriver: "awslogs", options: {
      "awslogs-group": $lg, "awslogs-region": $region, "awslogs-stream-prefix": "dex"}}
  }]}')")
aws ecs create-service --cluster "$CLUSTER" --service-name "$PREFIX-dex" \
  --task-definition "$TD_DEX" --desired-count 1 --launch-type FARGATE \
  --network-configuration "$NET_CFG" --service-registries "registryArn=$DEX_SD" \
  --tags "key=$TAG_KEY,value=$TAG_VALUE" >/dev/null 2>&1

wait_running "$PREFIX-postgres"
wait_running "$PREFIX-dex"

# --- 3. The Rise control plane ---
log "Starting the Rise control plane ($RISE_IMAGE)"
RISE_SD=$(sd_service rise)
PUBLIC_URL="http://rise.${DOMAIN}"
# Traefik reaches Rise over Cloud Map for the forwardAuth subrequest. This one
# line is what makes private-ingress-auth and route-access-override runnable.
AUTH_BACKEND_URL="http://rise.${NS_NAME}:3000"
TD_RISE=$(register_td "$(jq -n --arg img "$RISE_IMAGE" --arg family "$PREFIX-rise" \
  --arg exec_role "$EXEC_ROLE_ARN" --arg task_role "$TASK_ROLE_ARN" \
  --arg domain "$DOMAIN" --arg public_url "$PUBLIC_URL" --arg ns "$NS_NAME" \
  --arg cluster "$CLUSTER" --arg subnet "$SUBNET_ID" --arg sg "$SG_ID" \
  --arg region "$REGION" --arg lg "$LOG_GROUP" --arg secret "$JWT_SECRET" \
  --arg pw "$POSTGRES_PASSWORD" --arg issuer "$DEX_ISSUER" \
  --arg auth_backend "$AUTH_BACKEND_URL" --arg exec_arn "$EXEC_ROLE_ARN" \
  --arg task_arn "$TASK_ROLE_ARN" '{
  family: $family, networkMode: "awsvpc", requiresCompatibilities: ["FARGATE"],
  cpu: "512", memory: "1024", executionRoleArn: $exec_role, taskRoleArn: $task_role,
  containerDefinitions: [{
    name: "rise", image: $img, essential: true,
    command: ["backend", "server"],
    portMappings: [{containerPort: 3000}],
    environment: [
      {name: "DATABASE_URL", value: ("postgres://rise:" + $pw + "@postgres." + $ns + ":5432/rise")},
      {name: "RISE_CONFIG_DIR", value: "/etc/rise"},
      {name: "RISE_CONFIG_RUN_MODE", value: "ecs"},
      {name: "PUBLIC_URL", value: $public_url},
      {name: "RISE_JWT_SIGNING_SECRET", value: $secret},
      {name: "RISE_INGRESS_DOMAIN", value: $domain},
      {name: "RISE_INGRESS_SCHEME", value: "http"},
      {name: "DEX_ISSUER", value: $issuer},
      {name: "RISE_AUTH_BACKEND_URL", value: $auth_backend},
      {name: "RISE_TRAEFIK_API_URL", value: ("http://traefik." + $ns + ":8080")},
      {name: "RISE_ECS_CLUSTER", value: $cluster},
      {name: "RISE_ECS_SUBNETS", value: $subnet},
      {name: "RISE_ECS_SECURITY_GROUPS", value: $sg},
      {name: "RISE_ECS_ASSIGN_PUBLIC_IP", value: "true"},
      {name: "RISE_ECS_EXECUTION_ROLE_ARN", value: $exec_arn},
      {name: "RISE_ECS_TASK_ROLE_ARN", value: $task_arn},
      {name: "RISE_ECS_LOG_GROUP", value: $lg},
      {name: "AWS_REGION", value: $region},
      {name: "RISE_MAX_REPLICAS", value: "10"},
      {name: "RISE_SSRF_ALLOW_PRIVATE", value: "true"},
      {name: "RISE_SSRF_ALLOW_HTTP", value: "true"}
    ],
    dockerLabels: {
      "traefik.enable": "true",
      "traefik.http.routers.rise-cp.rule": ("Host(`rise." + $domain + "`)"),
      "traefik.http.routers.rise-cp.entrypoints": "web",
      "traefik.http.routers.rise-cp.service": "rise-cp",
      "traefik.http.services.rise-cp.loadbalancer.server.port": "3000",
      "traefik.http.routers.rise-dotrise.rule": "PathPrefix(`/.rise`)",
      "traefik.http.routers.rise-dotrise.entrypoints": "web",
      "traefik.http.routers.rise-dotrise.priority": "1000",
      "traefik.http.routers.rise-dotrise.service": "rise-cp"
    },
    logConfiguration: {logDriver: "awslogs", options: {
      "awslogs-group": $lg, "awslogs-region": $region, "awslogs-stream-prefix": "rise"}}
  }]}')")
aws ecs create-service --cluster "$CLUSTER" --service-name "$PREFIX-rise" \
  --task-definition "$TD_RISE" --desired-count 1 --launch-type FARGATE \
  --network-configuration "$NET_CFG" --service-registries "registryArn=$RISE_SD" \
  --tags "key=$TAG_KEY,value=$TAG_VALUE" >/dev/null 2>&1
wait_running "$PREFIX-rise"

log "Waiting for the Rise control plane to answer through Traefik"
READY=0
for _ in $(seq 1 60); do
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
    -H "Host: rise.${DOMAIN}" "http://${TIP}/health" || true)
  if [ "$code" = "200" ]; then READY=1; break; fi
  sleep 5
done
[ "$READY" = "1" ] || die "Rise did not become healthy — check CloudWatch log group $LOG_GROUP"

cat > "$STATE_FILE" <<EOF
export RISE_E2E_TRAEFIK_IP="$TIP"
export RISE_E2E_DOMAIN="$DOMAIN"
export RISE_E2E_PUBLIC_URL="$PUBLIC_URL"
export RISE_E2E_RISE_URL="http://rise.$DOMAIN"
export RISE_E2E_DEX_TOKEN_URL="http://dex.$DOMAIN/dex/token"
export RISE_E2E_DEX_ISSUER="$DEX_ISSUER"
export RISE_E2E_TRAEFIK_API="http://$TIP:8080"
export RISE_E2E_CLUSTER="$CLUSTER"
export RISE_E2E_REGION="$REGION"
EOF

log "Stack is up"
cat "$STATE_FILE"
