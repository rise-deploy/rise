# Public subnets only. Every task in this environment gets a public IP, which is
# what removes the need for a NAT gateway ($32/month idle) and a load balancer:
# Traefik is reached directly on its task's address.
#
# Acceptable here because this is a disposable test account and the security
# group admits only the IP the harness authorizes for the duration of a run.

resource "aws_vpc" "this" {
  cidr_block           = var.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true # Cloud Map private DNS needs both.

  tags = merge(local.tags, { Name = var.name })
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id
  tags   = merge(local.tags, { Name = var.name })
}

resource "aws_subnet" "public" {
  for_each = toset(local.azs)

  vpc_id                  = aws_vpc.this.id
  availability_zone       = each.value
  cidr_block              = cidrsubnet(var.vpc_cidr, 4, index(local.azs, each.value))
  map_public_ip_on_launch = true

  tags = merge(local.tags, { Name = "${var.name}-public-${each.value}" })
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.this.id
  tags   = merge(local.tags, { Name = "${var.name}-public" })
}

resource "aws_route" "internet" {
  route_table_id         = aws_route_table.public.id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = aws_internet_gateway.this.id
}

resource "aws_route_table_association" "public" {
  for_each = aws_subnet.public

  subnet_id      = each.value.id
  route_table_id = aws_route_table.public.id
}

# Free, and it keeps ECR layer pulls off the public path. ECR blobs come from S3.
resource "aws_vpc_endpoint" "s3" {
  vpc_id            = aws_vpc.this.id
  service_name      = "com.amazonaws.${var.region}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = [aws_route_table.public.id]

  tags = merge(local.tags, { Name = "${var.name}-s3" })
}

# -----------------------------------------------------------------------------
# Security groups
#
# The edge group starts closed. The harness authorizes its own address at run
# start and revokes it at teardown, so between runs nothing is reachable -- which
# is what keeps a persistent, publicly-addressed control plane defensible.
# -----------------------------------------------------------------------------

resource "aws_security_group" "edge" {
  name        = "${var.name}-edge"
  description = "Traefik. Ingress is opened per run by the e2e harness."
  vpc_id      = aws_vpc.this.id
  tags        = merge(local.tags, { Name = "${var.name}-edge" })
}

resource "aws_vpc_security_group_egress_rule" "edge_all" {
  security_group_id = aws_security_group.edge.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Backends, and the ECS API for the provider"
}

resource "aws_security_group" "internal" {
  name        = "${var.name}-internal"
  description = "Rise control plane, Postgres, Dex and deployed workloads"
  vpc_id      = aws_vpc.this.id
  tags        = merge(local.tags, { Name = "${var.name}-internal" })
}

# One group for everything inside, mutually open. A test environment does not
# need the per-role segmentation the production module builds, and collapsing it
# keeps the per-run apply from needing to touch security groups at all.
resource "aws_vpc_security_group_ingress_rule" "internal_self" {
  security_group_id            = aws_security_group.internal.id
  referenced_security_group_id = aws_security_group.internal.id
  ip_protocol                  = "-1"
  description                  = "Anything in the environment may reach anything else"
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

# Traefik reaches Rise for forwardAuth, and Rise reads Traefik's serverStatus.
resource "aws_vpc_security_group_ingress_rule" "edge_from_internal" {
  security_group_id            = aws_security_group.edge.id
  referenced_security_group_id = aws_security_group.internal.id
  ip_protocol                  = "-1"
  description                  = "Rise polls the Traefik API for readiness"
}
