# Security groups, per run.
#
# They live here rather than in the bootstrap because Traefik does, and because
# a per-run group can simply *contain* the address the run is driven from. A
# persistent group would instead need the harness to authorize its address at
# start and revoke it at teardown -- and a run that died in between would leave
# the rule behind.

resource "aws_security_group" "edge" {
  name        = "${var.name}-${var.scope}-edge"
  description = "Traefik for ${var.scope}"
  vpc_id      = local.env.vpc_id
  tags        = merge(local.tags, { Name = "${var.name}-${var.scope}-edge" })
}

resource "aws_vpc_security_group_ingress_rule" "edge_from_client" {
  for_each = toset(var.authorized_cidrs)

  security_group_id = aws_security_group.edge.id
  cidr_ipv4         = each.value
  ip_protocol       = "tcp"
  from_port         = 80
  to_port           = 80
  description       = "The address this run is driven from"
}

# The harness dumps Traefik's router list when something is unreachable, which
# is the one diagnostic that says whether discovery worked. Same single address,
# for the length of the run.
resource "aws_vpc_security_group_ingress_rule" "edge_api_from_client" {
  for_each = toset(var.authorized_cidrs)

  security_group_id = aws_security_group.edge.id
  cidr_ipv4         = each.value
  ip_protocol       = "tcp"
  from_port         = 8080
  to_port           = 8080
  description       = "Traefik's API, read by the harness for diagnostics"
}

resource "aws_vpc_security_group_egress_rule" "edge_all" {
  security_group_id = aws_security_group.edge.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Backends, and the ECS API for the provider"
}

resource "aws_security_group" "internal" {
  name        = "${var.name}-${var.scope}-internal"
  description = "Rise control plane, Postgres, Dex and deployed workloads for ${var.scope}"
  vpc_id      = local.env.vpc_id
  tags        = merge(local.tags, { Name = "${var.name}-${var.scope}-internal" })
}

# One group for everything inside, mutually open. A test environment does not
# need the per-role segmentation the production module builds.
resource "aws_vpc_security_group_ingress_rule" "internal_self" {
  security_group_id            = aws_security_group.internal.id
  referenced_security_group_id = aws_security_group.internal.id
  ip_protocol                  = "-1"
  description                  = "Anything in the run may reach anything else"
}

resource "aws_vpc_security_group_ingress_rule" "internal_from_edge" {
  security_group_id            = aws_security_group.internal.id
  referenced_security_group_id = aws_security_group.edge.id
  ip_protocol                  = "-1"
  description                  = "Traefik to Rise, Dex and deployed workloads"
}

resource "aws_vpc_security_group_egress_rule" "internal_all" {
  security_group_id = aws_security_group.internal.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
}

# Rise reads Traefik's serverStatus for readiness, so the API port must admit
# the internal group. (Traefik reaching Rise for forwardAuth is already covered
# by `internal_from_edge` above.)
resource "aws_vpc_security_group_ingress_rule" "edge_from_internal" {
  security_group_id            = aws_security_group.edge.id
  referenced_security_group_id = aws_security_group.internal.id
  ip_protocol                  = "-1"
  description                  = "Rise polls the Traefik API for readiness"
}
