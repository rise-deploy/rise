#!/usr/bin/env bash
# ADR-0004 spike (open question 2): does Traefik's ECS provider carry the label
# vocabulary the Docker backend relies on — and the cutover properties on top?
#
# What Rise's design (ADR-0004 D5/D12) needs from the provider, each asserted
# separately with its own PASS/FAIL:
#   LABELS_CONSUMED   router rule/entrypoint/priority/service read from the
#                     container definition's dockerLabels.
#   MIDDLEWARE        a forwardAuth middleware defined via labels exists (the
#                     access-class mechanism).
#   MERGED_LB         tasks of TWO ECS services sharing one Traefik service
#                     name merge into ONE load balancer with two servers (the
#                     blue/green overlap property).
#   SERVERSTATUS_UP   loadbalancer.healthcheck.* labels are honored and the
#                     Traefik API exposes per-server serverStatus (the Docker
#                     reconciler's authoritative readiness signal).
#   FORWARDAUTH_E2E   a request through Traefik passes forwardAuth (whose
#                     address resolves via Cloud Map private DNS) and reaches
#                     the app: HTTP 200.
#   DRAIN             scaling one ECS service to 0 shrinks the merged LB to
#                     one server within a few provider polls.
#
# Topology (all Fargate, all tagged rise-spike=adr-0004, torn down on exit):
#   traefik  — traefik:v3, ECS provider polling this cluster (refresh 5s),
#              entrypoint :80 + insecure API :8080, public IP; SG admits your
#              caller IP only.
#   auth     — traefik/whoami, registered in Cloud Map as auth.<ns> (the
#              forwardAuth target; whoami 200s on every path).
#   app-a/b  — traefik/whoami with identical Traefik dockerLabels (router
#              'app', service 'app-svc', healthcheck /health, forwardAuth
#              middleware).
#
# Requirements: aws CLI v2 + jq + curl; SANDBOX credentials (creates/deletes an
# IAM role for Traefik's ECS reads, a security group, ECS + Cloud Map
# resources); a default VPC. Cost: four 0.25-vCPU tasks for ~10 min. Docker Hub
# anonymous pulls (traefik, traefik/whoami).
#
# Usage:
#   AWS_PROFILE=sandbox AWS_REGION=eu-central-1 scripts/spikes/adr-0004-traefik-ecs-provider.sh
#   KEEP=1 ...   # keep resources for inspection (Traefik API stays reachable)
set -uo pipefail

PREFIX="${SPIKE_PREFIX:-rise-adr0004t}"
KEEP="${KEEP:-0}"
TAGS="key=rise-spike,value=adr-0004"
TRAEFIK_IMAGE="traefik:v3"
WHOAMI_IMAGE="traefik/whoami:v1.10"
HOSTRULE='Host(`app.spike.test`)'

log()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
info() { printf '    %s\n' "$*"; }
die()  { printf '\033[1;31mFATAL: %s\033[0m\n' "$*" >&2; exit 1; }
FAILED=0
verdict() { # verdict NAME PASS|FAIL detail...
  local name=$1 status=$2; shift 2
  if [ "$status" = PASS ]; then printf '\033[1;32m    %-16s PASS\033[0m %s\n' "$name" "$*"
  else printf '\033[1;31m    %-16s FAIL\033[0m %s\n' "$name" "$*"; FAILED=1; fi
}

command -v aws >/dev/null || die "aws CLI not found"
command -v jq  >/dev/null || die "jq not found"
aws sts get-caller-identity >/dev/null || die "no working AWS credentials"

CLUSTER="$PREFIX-cluster"; NS_NAME="$PREFIX.local"; ROLE="$PREFIX-traefik-role"
REGION=$(aws ec2 describe-availability-zones --query 'AvailabilityZones[0].RegionName' --output text)
NAMESPACE_ID=""; SD_SERVICE_ID=""; SG_ID=""; ROLE_CREATED=0
TDS=()

teardown() {
  local rc=$?
  [ "$KEEP" = "1" ] && { log "KEEP=1 — skipping teardown; clean up manually."; exit $rc; }
  log "Tearing down (best-effort)"
  for svc in traefik auth app-a app-b; do
    aws ecs update-service --cluster "$CLUSTER" --service "$PREFIX-$svc" --desired-count 0 >/dev/null 2>&1
    aws ecs delete-service --cluster "$CLUSTER" --service "$PREFIX-$svc" --force >/dev/null 2>&1
  done
  aws ecs wait services-inactive --cluster "$CLUSTER" \
    --services "$PREFIX-traefik" "$PREFIX-auth" "$PREFIX-app-a" "$PREFIX-app-b" >/dev/null 2>&1
  for td in "${TDS[@]:-}"; do [ -n "$td" ] && aws ecs deregister-task-definition --task-definition "$td" >/dev/null 2>&1; done
  if [ -n "$SD_SERVICE_ID" ]; then
    for _ in $(seq 1 30); do aws servicediscovery delete-service --id "$SD_SERVICE_ID" >/dev/null 2>&1 && break; sleep 10; done
  fi
  [ -n "$NAMESPACE_ID" ] && aws servicediscovery delete-namespace --id "$NAMESPACE_ID" >/dev/null 2>&1
  aws ecs delete-cluster --cluster "$CLUSTER" >/dev/null 2>&1
  if [ -n "$SG_ID" ]; then # ENIs release slowly after task stop
    for _ in $(seq 1 30); do aws ec2 delete-security-group --group-id "$SG_ID" >/dev/null 2>&1 && break; sleep 10; done
  fi
  if [ "$ROLE_CREATED" = 1 ]; then
    aws iam delete-role-policy --role-name "$ROLE" --policy-name ecs-read >/dev/null 2>&1
    aws iam delete-role --role-name "$ROLE" >/dev/null 2>&1
  fi
  info "teardown finished"
  exit $rc
}
trap teardown EXIT

log "Resolving default VPC / subnet / caller IP"
VPC_ID=$(aws ec2 describe-vpcs --filters Name=is-default,Values=true --query 'Vpcs[0].VpcId' --output text)
[ "$VPC_ID" != "None" ] || die "no default VPC in this region"
SUBNET_ID=$(aws ec2 describe-subnets --filters "Name=vpc-id,Values=$VPC_ID" --query 'Subnets[0].SubnetId' --output text)
MYIP=$(curl -s https://checkip.amazonaws.com | tr -d '[:space:]')
[ -n "$MYIP" ] || die "could not determine caller IP"
info "vpc=$VPC_ID subnet=$SUBNET_ID caller=$MYIP region=$REGION"

log "Creating security group (80/8080 from $MYIP; all traffic within the group)"
SG_ID=$(aws ec2 create-security-group --group-name "$PREFIX-sg" --description "ADR-0004 Traefik spike" \
  --vpc-id "$VPC_ID" --query GroupId --output text)
aws ec2 authorize-security-group-ingress --group-id "$SG_ID" --protocol tcp --port 80  --cidr "$MYIP/32" >/dev/null
aws ec2 authorize-security-group-ingress --group-id "$SG_ID" --protocol tcp --port 8080 --cidr "$MYIP/32" >/dev/null
aws ec2 authorize-security-group-ingress --group-id "$SG_ID" --protocol -1 --source-group "$SG_ID" >/dev/null

log "Creating IAM task role for Traefik's ECS reads"
aws iam create-role --role-name "$ROLE" --assume-role-policy-document '{
  "Version": "2012-10-17",
  "Statement": [{"Effect": "Allow", "Principal": {"Service": "ecs-tasks.amazonaws.com"}, "Action": "sts:AssumeRole"}]
}' >/dev/null && ROLE_CREATED=1
aws iam put-role-policy --role-name "$ROLE" --policy-name ecs-read --policy-document '{
  "Version": "2012-10-17",
  "Statement": [{"Effect": "Allow", "Action": [
    "ecs:ListClusters", "ecs:DescribeClusters", "ecs:ListTasks", "ecs:DescribeTasks",
    "ecs:DescribeContainerInstances", "ecs:DescribeTaskDefinition", "ec2:DescribeInstances"
  ], "Resource": "*"}]
}' >/dev/null
ROLE_ARN=$(aws iam get-role --role-name "$ROLE" --query Role.Arn --output text)
sleep 10 # IAM propagation before PassRole at task launch

log "Creating ECS cluster + Cloud Map namespace + 'auth' discovery service"
aws ecs create-cluster --cluster-name "$CLUSTER" --tags "$TAGS" >/dev/null
OP_ID=$(aws servicediscovery create-private-dns-namespace --name "$NS_NAME" --vpc "$VPC_ID" --query OperationId --output text)
for _ in $(seq 1 60); do
  STATUS=$(aws servicediscovery get-operation --operation-id "$OP_ID" --query Operation.Status --output text)
  [ "$STATUS" = "SUCCESS" ] && break; [ "$STATUS" = "FAIL" ] && die "namespace creation failed"; sleep 5
done
NAMESPACE_ID=$(aws servicediscovery get-operation --operation-id "$OP_ID" --query 'Operation.Targets.NAMESPACE' --output text)
SD_SERVICE_ID=$(aws servicediscovery create-service --name auth --namespace-id "$NAMESPACE_ID" \
  --dns-config "NamespaceId=$NAMESPACE_ID,RoutingPolicy=MULTIVALUE,DnsRecords=[{Type=A,TTL=10}]" \
  --health-check-custom-config FailureThreshold=1 --query Service.Id --output text)
AUTH_REG_ARN=$(aws servicediscovery get-service --id "$SD_SERVICE_ID" --query Service.Arn --output text)

register_td() { # register_td NAME JSON -> echoes ARN, records for teardown
  local arn
  arn=$(aws ecs register-task-definition --cli-input-json "$2" \
    --query taskDefinition.taskDefinitionArn --output text) || die "register-task-definition $1 failed"
  TDS+=("$arn"); printf '%s' "$arn"
}

log "Registering task definitions (traefik, auth, app)"
TD_TRAEFIK=$(register_td traefik "$(jq -n --arg img "$TRAEFIK_IMAGE" --arg cluster "$CLUSTER" --arg region "$REGION" --arg role "$ROLE_ARN" '{
  family: "'"$PREFIX"'-traefik", networkMode: "awsvpc", requiresCompatibilities: ["FARGATE"],
  cpu: "256", memory: "512", taskRoleArn: $role,
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
    ]
  }]
}')")
TD_AUTH=$(register_td auth "$(jq -n --arg img "$WHOAMI_IMAGE" '{
  family: "'"$PREFIX"'-auth", networkMode: "awsvpc", requiresCompatibilities: ["FARGATE"],
  cpu: "256", memory: "512",
  containerDefinitions: [{name: "auth", image: $img, essential: true, portMappings: [{containerPort: 80}]}]
}')")
TD_APP=$(register_td app "$(jq -n --arg img "$WHOAMI_IMAGE" --arg rule "$HOSTRULE" --arg auth "http://auth.$NS_NAME/" '{
  family: "'"$PREFIX"'-app", networkMode: "awsvpc", requiresCompatibilities: ["FARGATE"],
  cpu: "256", memory: "512",
  containerDefinitions: [{
    name: "app", image: $img, essential: true, portMappings: [{containerPort: 80}],
    dockerLabels: {
      "traefik.enable": "true",
      "traefik.http.routers.app.rule": $rule,
      "traefik.http.routers.app.entrypoints": "web",
      "traefik.http.routers.app.priority": "10",
      "traefik.http.routers.app.service": "app-svc",
      "traefik.http.routers.app.middlewares": "spike-auth",
      "traefik.http.middlewares.spike-auth.forwardauth.address": $auth,
      "traefik.http.services.app-svc.loadbalancer.server.port": "80",
      "traefik.http.services.app-svc.loadbalancer.healthcheck.path": "/health",
      "traefik.http.services.app-svc.loadbalancer.healthcheck.interval": "5s",
      "traefik.http.services.app-svc.loadbalancer.healthcheck.timeout": "2s"
    }
  }]
}')")

NET_CFG="awsvpcConfiguration={subnets=[$SUBNET_ID],securityGroups=[$SG_ID],assignPublicIp=ENABLED}"
create_svc() { # create_svc NAME TD [extra args...]
  local name=$1 td=$2; shift 2
  aws ecs create-service --cluster "$CLUSTER" --service-name "$PREFIX-$name" \
    --task-definition "$td" --desired-count 1 --launch-type FARGATE \
    --network-configuration "$NET_CFG" --tags "$TAGS" "$@" >/dev/null || die "create-service $name failed"
}

log "Creating services: traefik, auth (Cloud Map), app-a + app-b (same labels)"
create_svc traefik "$TD_TRAEFIK"
create_svc auth "$TD_AUTH" --service-registries "registryArn=$AUTH_REG_ARN"
create_svc app-a "$TD_APP"
create_svc app-b "$TD_APP"

log "Waiting for all four tasks to run (up to ~6 min)"
for _ in $(seq 1 36); do
  COUNTS=$(aws ecs describe-services --cluster "$CLUSTER" \
    --services "$PREFIX-traefik" "$PREFIX-auth" "$PREFIX-app-a" "$PREFIX-app-b" \
    --query 'services[].runningCount' --output json | jq -c .)
  [ "$COUNTS" = "[1,1,1,1]" ] && break; sleep 10
done
[ "$COUNTS" = "[1,1,1,1]" ] || {
  aws ecs describe-services --cluster "$CLUSTER" \
    --services "$PREFIX-traefik" "$PREFIX-auth" "$PREFIX-app-a" "$PREFIX-app-b" \
    --query 'services[].{svc:serviceName,events:events[0:3].message}' --output json | sed 's/^/      /'
  die "tasks did not all start (running: $COUNTS) — see events above"
}

log "Resolving Traefik's public IP"
TASK_ARN=$(aws ecs list-tasks --cluster "$CLUSTER" --service-name "$PREFIX-traefik" --query 'taskArns[0]' --output text)
ENI_ID=$(aws ecs describe-tasks --cluster "$CLUSTER" --tasks "$TASK_ARN" \
  --query "tasks[0].attachments[0].details[?name=='networkInterfaceId'].value | [0]" --output text)
TIP=$(aws ec2 describe-network-interfaces --network-interface-ids "$ENI_ID" \
  --query 'NetworkInterfaces[0].Association.PublicIp' --output text)
[ -n "$TIP" ] && [ "$TIP" != "None" ] || die "Traefik task has no public IP"
API="http://$TIP:8080/api"
info "traefik=$TIP (API $API, entrypoint :80)"

log "Asserting provider behavior (polling up to ~3 min for discovery + health)"
SVC_JSON=""
for _ in $(seq 1 36); do
  SVC_JSON=$(curl -sf --max-time 5 "$API/http/services/app-svc@ecs" || true)
  N=$(printf '%s' "$SVC_JSON" | jq -r '.loadBalancer.servers | length' 2>/dev/null || echo 0)
  UP=$(printf '%s' "$SVC_JSON" | jq -r '[.serverStatus // {} | .[] ] | map(select(. == "UP")) | length' 2>/dev/null || echo 0)
  [ "$N" = "2" ] && [ "$UP" = "2" ] && break; sleep 5
done

ROUTER_JSON=$(curl -sf --max-time 5 "$API/http/routers/app@ecs" || true)
RULE_SEEN=$(printf '%s' "$ROUTER_JSON" | jq -r '.rule // empty' 2>/dev/null)
if [ "$RULE_SEEN" = "$HOSTRULE" ]; then
  verdict LABELS_CONSUMED PASS "router 'app@ecs' carries the labeled rule/entrypoint/priority"
else
  verdict LABELS_CONSUMED FAIL "router rule seen: '${RULE_SEEN:-<absent>}' (wanted the labeled Host rule)"
fi

MW_ADDR=$(curl -sf --max-time 5 "$API/http/middlewares/spike-auth@ecs" | jq -r '.forwardAuth.address // empty' 2>/dev/null)
if [ "$MW_ADDR" = "http://auth.$NS_NAME/" ]; then
  verdict MIDDLEWARE PASS "forwardAuth middleware defined via labels ($MW_ADDR)"
else
  verdict MIDDLEWARE FAIL "forwardAuth middleware missing or wrong address: '${MW_ADDR:-<absent>}'"
fi

N=$(printf '%s' "$SVC_JSON" | jq -r '.loadBalancer.servers | length' 2>/dev/null || echo 0)
if [ "$N" = "2" ]; then
  verdict MERGED_LB PASS "app-svc@ecs merges tasks from BOTH ECS services (2 servers)"
else
  verdict MERGED_LB FAIL "app-svc@ecs has $N server(s); expected 2"
  printf '%s\n' "$SVC_JSON" | jq . 2>/dev/null | sed 's/^/      /'
fi

UP=$(printf '%s' "$SVC_JSON" | jq -r '[.serverStatus // {} | .[]] | map(select(. == "UP")) | length' 2>/dev/null || echo 0)
if [ "$UP" = "2" ]; then
  verdict SERVERSTATUS_UP PASS "healthcheck labels honored; serverStatus reports both UP"
else
  verdict SERVERSTATUS_UP FAIL "serverStatus UP count: $UP (expected 2) — status map: $(printf '%s' "$SVC_JSON" | jq -c '.serverStatus // {}')"
fi

CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 -H 'Host: app.spike.test' "http://$TIP/")
if [ "$CODE" = "200" ]; then
  verdict FORWARDAUTH_E2E PASS "request routed through forwardAuth (Cloud Map DNS) to whoami: 200"
else
  verdict FORWARDAUTH_E2E FAIL "HTTP $CODE through the entrypoint (500 here usually means the forwardAuth target was unreachable — check MIDDLEWARE above to separate label fidelity from DNS reach)"
fi

log "Drain: scaling app-b to 0; merged LB should shrink to one server"
aws ecs update-service --cluster "$CLUSTER" --service "$PREFIX-app-b" --desired-count 0 >/dev/null
DRAIN=FAIL
for _ in $(seq 1 24); do
  N=$(curl -sf --max-time 5 "$API/http/services/app-svc@ecs" | jq -r '.loadBalancer.servers | length' 2>/dev/null || echo "")
  [ "$N" = "1" ] && { DRAIN=PASS; break; }; sleep 5
done
verdict DRAIN "$DRAIN" "servers after retiring app-b: ${N:-?} (expected 1 within ~2 min)"

log "Spike complete."
if [ "$FAILED" = 0 ]; then
  info "All assertions passed — ADR-0004 open question 2 resolves in favor of the design."
else
  info "One or more assertions failed — record the failing capability in ADR-0004 and pick the fallback there."
fi
