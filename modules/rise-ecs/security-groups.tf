# One group per role, each admitting only its actual caller. The load balancer
# gets its own group specifically so the Traefik group can reference it: an NLB
# with IP targets preserves the client source address, so without an LB group
# Traefik would have to admit the ingress CIDRs directly.

resource "aws_security_group" "edge" {
  name        = "${local.name}-edge"
  description = "Rise edge load balancer"
  vpc_id      = local.vpc_id
  tags        = merge(local.tags, { Name = "${local.name}-edge" })
}

resource "aws_vpc_security_group_ingress_rule" "edge_http" {
  for_each = toset(var.ingress_cidr_blocks)

  security_group_id = aws_security_group.edge.id
  cidr_ipv4         = each.value
  from_port         = 80
  to_port           = 80
  ip_protocol       = "tcp"
  description       = "HTTP (ACME HTTP-01 challenge and the redirect to HTTPS)"
}

resource "aws_vpc_security_group_ingress_rule" "edge_https" {
  for_each = toset(var.ingress_cidr_blocks)

  security_group_id = aws_security_group.edge.id
  cidr_ipv4         = each.value
  from_port         = 443
  to_port           = 443
  ip_protocol       = "tcp"
  description       = "HTTPS"
}

resource "aws_vpc_security_group_egress_rule" "edge_to_traefik" {
  for_each = toset(["80", "443", "8082"])

  security_group_id            = aws_security_group.edge.id
  referenced_security_group_id = aws_security_group.traefik.id
  from_port                    = tonumber(each.value)
  to_port                      = tonumber(each.value)
  ip_protocol                  = "tcp"
  description                  = "Forward to Traefik (8082 is the health-check entrypoint)"
}

resource "aws_security_group" "traefik" {
  name        = "${local.name}-traefik"
  description = "Rise ingress router"
  vpc_id      = local.vpc_id
  tags        = merge(local.tags, { Name = "${local.name}-traefik" })
}

resource "aws_vpc_security_group_ingress_rule" "traefik_from_edge" {
  for_each = toset(["80", "443", "8082"])

  security_group_id            = aws_security_group.traefik.id
  referenced_security_group_id = aws_security_group.edge.id
  from_port                    = tonumber(each.value)
  to_port                      = tonumber(each.value)
  ip_protocol                  = "tcp"
  description                  = "From the edge load balancer"
}

# Traefik's API is served without authentication (--api.insecure), so this rule
# is the only thing containing it. Widening it exposes the dashboard and the
# serverStatus endpoint to whatever is added.
resource "aws_vpc_security_group_ingress_rule" "traefik_api_from_rise" {
  security_group_id            = aws_security_group.traefik.id
  referenced_security_group_id = aws_security_group.control_plane.id
  from_port                    = 8080
  to_port                      = 8080
  ip_protocol                  = "tcp"
  description                  = "Rise reads serverStatus for deployment readiness"
}

resource "aws_vpc_security_group_egress_rule" "traefik_all" {
  security_group_id = aws_security_group.traefik.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Backends, ACME, and the ECS API"
}

resource "aws_security_group" "control_plane" {
  name        = "${local.name}-control-plane"
  description = "Rise control plane"
  vpc_id      = local.vpc_id
  tags        = merge(local.tags, { Name = "${local.name}-control-plane" })
}

# Public control-plane traffic and forwardAuth use 3000. Traefik pulls the
# deployment routing snapshot from the internal listener on 3001.
resource "aws_vpc_security_group_ingress_rule" "control_plane_from_traefik" {
  security_group_id            = aws_security_group.control_plane.id
  referenced_security_group_id = aws_security_group.traefik.id
  from_port                    = 3000
  to_port                      = 3001
  ip_protocol                  = "tcp"
  description                  = "Routed traffic, forwardAuth, and dynamic routing configuration"
}

resource "aws_vpc_security_group_egress_rule" "control_plane_all" {
  security_group_id = aws_security_group.control_plane.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "AWS APIs, the database, and OIDC discovery"
}

# The group every deployed workload lands in — this is what
# RISE_ECS_SECURITY_GROUPS names. Per-group network isolation, tracked as a gap
# on the ECS backend, is the feature that would subdivide it.
resource "aws_security_group" "apps" {
  name        = "${local.name}-apps"
  description = "Rise deployed workloads"
  vpc_id      = local.vpc_id
  tags        = merge(local.tags, { Name = "${local.name}-apps" })
}

resource "aws_vpc_security_group_ingress_rule" "apps_from_traefik" {
  security_group_id            = aws_security_group.apps.id
  referenced_security_group_id = aws_security_group.traefik.id
  from_port                    = 1
  to_port                      = 65535
  ip_protocol                  = "tcp"
  description                  = "Routed traffic on whatever port a project declares"
}

resource "aws_vpc_security_group_egress_rule" "apps_all" {
  security_group_id = aws_security_group.apps.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Egress for deployed workloads"
}

resource "aws_security_group" "database" {
  name        = "${local.name}-database"
  description = "Rise control-plane database"
  vpc_id      = local.vpc_id
  tags        = merge(local.tags, { Name = "${local.name}-database" })
}

# Only the control plane. Deployed workloads get their own databases through the
# RDS extension; they have no business reaching this one.
resource "aws_vpc_security_group_ingress_rule" "database_from_control_plane" {
  security_group_id            = aws_security_group.database.id
  referenced_security_group_id = aws_security_group.control_plane.id
  from_port                    = 5432
  to_port                      = 5432
  ip_protocol                  = "tcp"
  description                  = "PostgreSQL from the Rise control plane"
}

resource "aws_security_group" "efs" {
  count = local.acme_enabled ? 1 : 0

  name        = "${local.name}-efs"
  description = "Traefik ACME certificate store"
  vpc_id      = local.vpc_id
  tags        = merge(local.tags, { Name = "${local.name}-efs" })
}

resource "aws_vpc_security_group_ingress_rule" "efs_from_traefik" {
  count = local.acme_enabled ? 1 : 0

  security_group_id            = aws_security_group.efs[0].id
  referenced_security_group_id = aws_security_group.traefik.id
  from_port                    = 2049
  to_port                      = 2049
  ip_protocol                  = "tcp"
  description                  = "NFS from Traefik"
}

resource "aws_security_group" "vpc_endpoints" {
  count = local.create_vpc && var.enable_vpc_endpoints ? 1 : 0

  name        = "${local.name}-vpc-endpoints"
  description = "Interface endpoints for AWS services"
  vpc_id      = local.vpc_id
  tags        = merge(local.tags, { Name = "${local.name}-vpc-endpoints" })
}

resource "aws_vpc_security_group_ingress_rule" "vpc_endpoints_from_tasks" {
  for_each = local.create_vpc && var.enable_vpc_endpoints ? {
    traefik       = aws_security_group.traefik.id
    control_plane = aws_security_group.control_plane.id
    apps          = aws_security_group.apps.id
  } : {}

  security_group_id            = aws_security_group.vpc_endpoints[0].id
  referenced_security_group_id = each.value
  from_port                    = 443
  to_port                      = 443
  ip_protocol                  = "tcp"
  description                  = "HTTPS from ${each.key}"
}

resource "aws_security_group" "dex" {
  count = var.deploy_dex ? 1 : 0

  name        = "${local.name}-dex"
  description = "Demo identity provider"
  vpc_id      = local.vpc_id
  tags        = merge(local.tags, { Name = "${local.name}-dex" })
}

resource "aws_vpc_security_group_ingress_rule" "dex_from_traefik" {
  count = var.deploy_dex ? 1 : 0

  security_group_id            = aws_security_group.dex[0].id
  referenced_security_group_id = aws_security_group.traefik.id
  from_port                    = 5556
  to_port                      = 5556
  ip_protocol                  = "tcp"
  description                  = "Routed traffic"
}

resource "aws_vpc_security_group_egress_rule" "dex_all" {
  count = var.deploy_dex ? 1 : 0

  security_group_id = aws_security_group.dex[0].id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
}

# The demo IdP's issuer is public, so the control plane discovers it by going
# out through the NAT and back in at the edge. If ingress_cidr_blocks has been
# narrowed to an office range, that hairpin is blocked and Rise fails to start
# with an OIDC discovery error that points nowhere near the cause -- so admit
# the NAT addresses explicitly.
resource "aws_vpc_security_group_ingress_rule" "edge_from_nat" {
  for_each = var.deploy_dex ? {
    for idx, eip in aws_eip.nat : idx => eip
  } : {}

  security_group_id = aws_security_group.edge.id
  cidr_ipv4         = "${each.value.public_ip}/32"
  from_port         = 443
  to_port           = 443
  ip_protocol       = "tcp"
  description       = "Control-plane OIDC discovery against the demo Dex"
}
