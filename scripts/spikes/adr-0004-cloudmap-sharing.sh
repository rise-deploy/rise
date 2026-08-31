#!/usr/bin/env bash
# ADR-0004 spike (open question 1): can TWO ECS services carry serviceRegistries
# pointing at ONE Cloud Map service — and does draining one deregister only its
# own instances?
#
# This is the experiment gating ADR-0004's D10 (cross-container discovery during
# blue/green overlap). AWS documents that an ECS service may carry only one
# service registry, but is silent on sharing a registry ACROSS services; only a
# real account answers it.
#
# What it does (all resources tagged rise-spike=adr-0004, torn down on exit):
#   1. ECS cluster + private Cloud Map namespace + ONE Cloud Map service.
#   2. One Fargate task definition (busybox sleep; no ports, no exec role).
#   3. ECS service A registered into the Cloud Map service.
#   4. ECS service B registered into the SAME Cloud Map service.  <-- the crux
#   5. If B is accepted: wait for both tasks, assert the Cloud Map service
#      lists instances from BOTH ECS services.
#   6. Drain assertion: scale A to 0, assert only B's instance remains.
#
# Verdicts (printed at the end, exit 0 either way — a clean REJECTED is a
# successful spike):
#   SHARED_REGISTRATION_SUPPORTED  -> D10 stands as designed.
#   SHARED_REGISTRATION_REJECTED   -> adopt D10 fallback (a): the reconciler
#                                     registers instances itself via
#                                     servicediscovery RegisterInstance.
#   ...plus DRAIN_CLEAN / DRAIN_UNCLEAN when registration was supported.
#
# Requirements: aws CLI v2 + jq; credentials for a SANDBOX account (admin or
# ecs:* + servicediscovery:* + route53 via SLR + ec2:Describe* +
# iam:CreateServiceLinkedRole); a default VPC in the region; the default
# Fargate vCPU quota (6) is plenty. Cost: two 0.25-vCPU Fargate tasks for a few
# minutes — cents. Runtime: ~5-10 minutes.
#
# Usage:
#   AWS_PROFILE=rise-sandbox AWS_REGION=eu-west-1 scripts/spikes/adr-0004-cloudmap-sharing.sh
#   KEEP=1 ...   # skip teardown for manual inspection (clean up yourself!)
set -uo pipefail

PREFIX="${SPIKE_PREFIX:-rise-adr0004}"
KEEP="${KEEP:-0}"
IMAGE="public.ecr.aws/docker/library/busybox:1.36"
TAGS="key=rise-spike,value=adr-0004"

log()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
info() { printf '    %s\n' "$*"; }
die()  { printf '\033[1;31mFATAL: %s\033[0m\n' "$*" >&2; exit 1; }

command -v aws >/dev/null || die "aws CLI not found"
command -v jq  >/dev/null || die "jq not found"
aws sts get-caller-identity >/dev/null || die "no working AWS credentials"

CLUSTER="$PREFIX-cluster"
NS_NAME="$PREFIX.local"
FAMILY="$PREFIX-td"
SVC_A="$PREFIX-svc-a"
SVC_B="$PREFIX-svc-b"

NAMESPACE_ID=""
SD_SERVICE_ID=""
TD_ARN=""

# ---------- teardown (runs on any exit unless KEEP=1) ----------
teardown() {
  local rc=$?
  [ "$KEEP" = "1" ] && { log "KEEP=1 — skipping teardown; clean up manually."; exit $rc; }
  log "Tearing down (best-effort)"
  for svc in "$SVC_A" "$SVC_B"; do
    aws ecs update-service --cluster "$CLUSTER" --service "$svc" --desired-count 0 >/dev/null 2>&1
    aws ecs delete-service --cluster "$CLUSTER" --service "$svc" --force >/dev/null 2>&1
  done
  aws ecs wait services-inactive --cluster "$CLUSTER" --services "$SVC_A" "$SVC_B" >/dev/null 2>&1
  [ -n "$TD_ARN" ] && aws ecs deregister-task-definition --task-definition "$TD_ARN" >/dev/null 2>&1
  if [ -n "$SD_SERVICE_ID" ]; then
    # Instances must be gone before the Cloud Map service deletes; ECS
    # deregisters them on service delete, but give it time.
    for _ in $(seq 1 30); do
      aws servicediscovery delete-service --id "$SD_SERVICE_ID" >/dev/null 2>&1 && break
      sleep 10
    done
  fi
  if [ -n "$NAMESPACE_ID" ]; then
    aws servicediscovery delete-namespace --id "$NAMESPACE_ID" >/dev/null 2>&1
  fi
  aws ecs delete-cluster --cluster "$CLUSTER" >/dev/null 2>&1
  info "teardown finished"
  exit $rc
}
trap teardown EXIT

# ---------- setup ----------
log "Resolving default VPC / subnet / security group"
VPC_ID=$(aws ec2 describe-vpcs --filters Name=is-default,Values=true \
  --query 'Vpcs[0].VpcId' --output text)
[ "$VPC_ID" != "None" ] || die "no default VPC in this region; set one up or adapt the script"
SUBNET_ID=$(aws ec2 describe-subnets --filters "Name=vpc-id,Values=$VPC_ID" \
  --query 'Subnets[0].SubnetId' --output text)
SG_ID=$(aws ec2 describe-security-groups \
  --filters "Name=vpc-id,Values=$VPC_ID" Name=group-name,Values=default \
  --query 'SecurityGroups[0].GroupId' --output text)
info "vpc=$VPC_ID subnet=$SUBNET_ID sg=$SG_ID"

log "Creating ECS cluster $CLUSTER"
aws ecs create-cluster --cluster-name "$CLUSTER" --tags "$TAGS" >/dev/null

log "Creating private Cloud Map namespace $NS_NAME (async)"
OP_ID=$(aws servicediscovery create-private-dns-namespace \
  --name "$NS_NAME" --vpc "$VPC_ID" --query OperationId --output text)
for _ in $(seq 1 60); do
  STATUS=$(aws servicediscovery get-operation --operation-id "$OP_ID" \
    --query Operation.Status --output text)
  [ "$STATUS" = "SUCCESS" ] && break
  [ "$STATUS" = "FAIL" ] && die "namespace creation failed"
  sleep 5
done
[ "$STATUS" = "SUCCESS" ] || die "namespace creation timed out"
NAMESPACE_ID=$(aws servicediscovery get-operation --operation-id "$OP_ID" \
  --query 'Operation.Targets.NAMESPACE' --output text)
info "namespace=$NAMESPACE_ID"

log "Creating ONE Cloud Map service 'shared' in $NS_NAME"
SD_SERVICE_ID=$(aws servicediscovery create-service \
  --name shared --namespace-id "$NAMESPACE_ID" \
  --dns-config "NamespaceId=$NAMESPACE_ID,RoutingPolicy=MULTIVALUE,DnsRecords=[{Type=A,TTL=10}]" \
  --health-check-custom-config FailureThreshold=1 \
  --query Service.Id --output text)
REG_ARN=$(aws servicediscovery get-service --id "$SD_SERVICE_ID" \
  --query Service.Arn --output text)
info "cloud map service=$SD_SERVICE_ID"

log "Registering Fargate task definition $FAMILY"
TD_ARN=$(aws ecs register-task-definition --cli-input-json "{
  \"family\": \"$FAMILY\",
  \"networkMode\": \"awsvpc\",
  \"requiresCompatibilities\": [\"FARGATE\"],
  \"cpu\": \"256\", \"memory\": \"512\",
  \"containerDefinitions\": [{
    \"name\": \"app\",
    \"image\": \"$IMAGE\",
    \"command\": [\"sleep\", \"3600\"],
    \"essential\": true
  }]
}" --query taskDefinition.taskDefinitionArn --output text)
info "task definition=$TD_ARN"

NET_CFG="awsvpcConfiguration={subnets=[$SUBNET_ID],securityGroups=[$SG_ID],assignPublicIp=ENABLED}"

log "Creating ECS service A ($SVC_A) registered into the Cloud Map service"
aws ecs create-service --cluster "$CLUSTER" --service-name "$SVC_A" \
  --task-definition "$TD_ARN" --desired-count 1 --launch-type FARGATE \
  --network-configuration "$NET_CFG" \
  --service-registries "registryArn=$REG_ARN" \
  --tags "$TAGS" >/dev/null || die "service A creation failed — fix before the spike means anything"

log "THE CRUX: creating ECS service B ($SVC_B) with the SAME registryArn"
B_ERR=$(aws ecs create-service --cluster "$CLUSTER" --service-name "$SVC_B" \
  --task-definition "$TD_ARN" --desired-count 1 --launch-type FARGATE \
  --network-configuration "$NET_CFG" \
  --service-registries "registryArn=$REG_ARN" \
  --tags "$TAGS" 2>&1 >/dev/null)
if [ $? -ne 0 ]; then
  log "VERDICT: SHARED_REGISTRATION_REJECTED"
  info "CreateService for the second service was refused:"
  printf '%s\n' "$B_ERR" | sed 's/^/      /'
  info "-> ADR-0004 D10: adopt fallback (a) — the reconciler registers task IPs"
  info "   itself via servicediscovery RegisterInstance into the shared service."
  exit 0
fi
info "API accepted the second association — verifying it actually registers…"

log "Waiting for both services to run their task (up to ~6 min)"
for _ in $(seq 1 36); do
  COUNTS=$(aws ecs describe-services --cluster "$CLUSTER" --services "$SVC_A" "$SVC_B" \
    --query 'services[].runningCount' --output json | jq -c .)
  [ "$COUNTS" = "[1,1]" ] && break
  sleep 10
done
[ "$COUNTS" = "[1,1]" ] || {
  info "tasks did not both start; recent service events for diagnosis:"
  aws ecs describe-services --cluster "$CLUSTER" --services "$SVC_A" "$SVC_B" \
    --query 'services[].events[0:5].message' --output json | sed 's/^/      /'
  die "spike inconclusive — investigate events above"
}

log "Asserting the ONE Cloud Map service holds instances from BOTH ECS services"
VERDICT=""
for _ in $(seq 1 30); do
  INSTANCES=$(aws servicediscovery list-instances --service-id "$SD_SERVICE_ID" --output json)
  SVCS=$(printf '%s' "$INSTANCES" \
    | jq -r '[.Instances[].Attributes.ECS_SERVICE_NAME] | unique | sort | join(",")')
  if [ "$SVCS" = "$SVC_A,$SVC_B" ]; then VERDICT=supported; break; fi
  sleep 10
done
printf '%s' "$INSTANCES" | jq '[.Instances[] | {Id, ip: .Attributes.AWS_INSTANCE_IPV4, ecs_service: .Attributes.ECS_SERVICE_NAME}]'
if [ "$VERDICT" != "supported" ]; then
  log "VERDICT: SHARED_REGISTRATION_REJECTED (silently)"
  info "CreateService accepted the association but instances from both services"
  info "never co-appeared (saw: '$SVCS') — treat as unsupported; adopt fallback (a)."
  exit 0
fi
log "VERDICT: SHARED_REGISTRATION_SUPPORTED"
info "Both ECS services' tasks are instances of one Cloud Map service — D10 stands."

log "Drain assertion: scaling A to 0; only B's instance should remain"
aws ecs update-service --cluster "$CLUSTER" --service "$SVC_A" --desired-count 0 >/dev/null
DRAIN=""
for _ in $(seq 1 30); do
  SVCS=$(aws servicediscovery list-instances --service-id "$SD_SERVICE_ID" --output json \
    | jq -r '[.Instances[].Attributes.ECS_SERVICE_NAME] | unique | sort | join(",")')
  if [ "$SVCS" = "$SVC_B" ]; then DRAIN=clean; break; fi
  sleep 10
done
if [ "$DRAIN" = "clean" ]; then
  log "VERDICT: DRAIN_CLEAN — retiring service A deregistered only A's instances."
else
  log "VERDICT: DRAIN_UNCLEAN — after 5 min the instance set is '$SVCS'."
  info "Cutover on ECS would leak or drop discovery instances; record in ADR-0004."
fi

log "Spike complete."
