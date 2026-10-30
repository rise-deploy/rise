# ECS runtime

Runs Rise and Traefik on an existing Fargate cluster. Each service has a task
definition and Cloud Map registration. The caller supplies networking, IAM,
the log group, configuration, and any load balancer or ACME certificate store.

The production `rise-ecs` module and the ECS E2E run workspace both consume
this module. They compose Rise's environment and labels with the sibling
`control-plane-env` module. E2E creates its own runtime for each run.

## Inputs and dependencies

Typed objects group cluster and discovery identifiers, task networking, IAM
roles, logging, and each service's settings. Service names also name task
families; discovery names and log stream prefixes are explicit so callers can
scope installations independently. Callers must mark credential inputs as
sensitive; that sensitivity propagates into the task definition.
`secret_environment` contains ECS `valueFrom` references.

`control_plane.health_check_grace_period_seconds` defaults to 1800 seconds.
During this startup budget, the ECS scheduler ignores unhealthy container
health checks. ECS can still stop excess tasks when the running count exceeds
the desired count. `control_plane.deployment_rollback` defaults to `true` and
controls whether a failed deployment restores the last completed version. Set
it to `false` when that version cannot read the database's current format.

`load_balancer_targets` maps port strings (`80` or `443`) to target group ARNs.
Its keys must be known during planning. `acme.enabled` must also be known during
planning; EFS identifiers may come from resources created in the same apply.
ACME uses a single Traefik replica with stop-before-start deployment because
the certificate file cannot have concurrent writers.

Callers must order runtime creation after secret values, load-balancer
listeners, and EFS mounts are ready. The module inherits its AWS provider and
creates no IAM, security groups, namespace, cluster, database, or log group.

## Routing and connectivity

Traefik's ECS provider discovers deployment servers. Its HTTP provider polls
Rise's `http://<rise-discovery-name>.<namespace>:3001/internal/traefik/config`
every five seconds for public routes. ECS catalog routers use the loopback
entry point `127.0.0.1:8083`.

The caller's security groups must permit Traefik to reach Rise on TCP
3000–3001 and deployed workloads on their application ports. Rise must reach
Traefik's API on TCP 8080. The API and Rise's internal listener must remain
restricted to their intended callers. Port 8082 serves Traefik's ping check.

`client_ip` says how the load balancer reports the client: `forwarded-headers`
(ALB) trusts `X-Forwarded-*` on `web`, and `proxy-protocol` (NLB) reads PROXY
protocol v2 on `web`, `websecure` and `ping`, each only from `trusted_cidrs` --
the load balancer's subnets. The default, `none`, is for Traefik reached
directly, where its peer is the client. A trusted peer can claim any address, so
the CIDRs are never `0.0.0.0/0`.

`rise` and `traefik` outputs expose service and discovery identifiers;
`traefik_command` exposes the effective routing configuration for inspection.
