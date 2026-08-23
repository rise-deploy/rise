# Three tiers. Public carries the load balancer and NAT; everything that runs
# code — Traefik, Rise, Dex and every app task — sits in private with no public
# IP; the database tier has no route out at all.

resource "aws_vpc" "this" {
  count = local.create_vpc ? 1 : 0

  cidr_block           = var.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true # Cloud Map private DNS needs both.

  tags = merge(local.tags, { Name = local.name })
}

resource "aws_internet_gateway" "this" {
  count = local.create_vpc ? 1 : 0

  vpc_id = aws_vpc.this[0].id
  tags   = merge(local.tags, { Name = local.name })
}

# Keyed by AZ rather than counted by index: with count, adding or reordering an
# AZ renumbers every subnet and Terraform destroys and recreates them all —
# which, for the subnets an RDS instance lives in, is not a cosmetic event.
resource "aws_subnet" "public" {
  for_each = local.create_vpc ? toset(local.azs) : toset([])

  vpc_id            = aws_vpc.this[0].id
  availability_zone = each.value
  cidr_block        = cidrsubnet(var.vpc_cidr, 8, index(local.azs, each.value))

  tags = merge(local.tags, { Name = "${local.name}-public-${each.value}" })
}

# Deliberately large. Every Fargate task takes an ENI, and the cutover model
# overlaps two deployments, so an install transiently holds twice its steady
# task count. A /24 would be a real ceiling at a few hundred tasks; this is not.
resource "aws_subnet" "private" {
  for_each = local.create_vpc ? toset(local.azs) : toset([])

  vpc_id            = aws_vpc.this[0].id
  availability_zone = each.value
  cidr_block        = cidrsubnet(var.vpc_cidr, 4, index(local.azs, each.value) + 1)

  tags = merge(local.tags, { Name = "${local.name}-private-${each.value}" })
}

resource "aws_subnet" "database" {
  for_each = local.create_vpc ? toset(local.azs) : toset([])

  vpc_id            = aws_vpc.this[0].id
  availability_zone = each.value
  cidr_block        = cidrsubnet(var.vpc_cidr, 8, index(local.azs, each.value) + 64)

  tags = merge(local.tags, { Name = "${local.name}-database-${each.value}" })
}

# --- Routing -----------------------------------------------------------------

resource "aws_route_table" "public" {
  count = local.create_vpc ? 1 : 0

  vpc_id = aws_vpc.this[0].id
  tags   = merge(local.tags, { Name = "${local.name}-public" })
}

resource "aws_route" "public_internet" {
  count = local.create_vpc ? 1 : 0

  route_table_id         = aws_route_table.public[0].id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = aws_internet_gateway.this[0].id
}

resource "aws_route_table_association" "public" {
  for_each = local.create_vpc ? aws_subnet.public : {}

  subnet_id      = each.value.id
  route_table_id = aws_route_table.public[0].id
}

resource "aws_eip" "nat" {
  count  = local.nat_gateway_count
  domain = "vpc"
  tags   = merge(local.tags, { Name = "${local.name}-nat-${count.index}" })
}

resource "aws_nat_gateway" "this" {
  count = local.nat_gateway_count

  allocation_id = aws_eip.nat[count.index].id
  subnet_id     = aws_subnet.public[local.azs[count.index]].id
  tags          = merge(local.tags, { Name = "${local.name}-${count.index}" })

  depends_on = [aws_internet_gateway.this]
}

# One route table per AZ even in "single" mode, so switching to per_az later
# changes routes rather than restructuring the VPC.
resource "aws_route_table" "private" {
  for_each = local.create_vpc ? toset(local.azs) : toset([])

  vpc_id = aws_vpc.this[0].id
  tags   = merge(local.tags, { Name = "${local.name}-private-${each.value}" })
}

resource "aws_route" "private_nat" {
  for_each = local.create_vpc && local.nat_gateway_count > 0 ? toset(local.azs) : toset([])

  route_table_id         = aws_route_table.private[each.value].id
  destination_cidr_block = "0.0.0.0/0"
  nat_gateway_id = aws_nat_gateway.this[
    var.nat_gateway_mode == "per_az" ? index(local.azs, each.value) : 0
  ].id
}

resource "aws_route_table_association" "private" {
  for_each = local.create_vpc ? aws_subnet.private : {}

  subnet_id      = each.value.id
  route_table_id = aws_route_table.private[each.value.availability_zone].id
}

resource "aws_route_table" "database" {
  count = local.create_vpc ? 1 : 0

  vpc_id = aws_vpc.this[0].id
  tags   = merge(local.tags, { Name = "${local.name}-database" })
}

resource "aws_route_table_association" "database" {
  for_each = local.create_vpc ? aws_subnet.database : {}

  subnet_id      = each.value.id
  route_table_id = aws_route_table.database[0].id
}

# --- VPC endpoints -----------------------------------------------------------

# Created in every mode, including with a NAT gateway. It is free, and ECR layer
# blobs are served from S3 — an endpoints-only VPC with ecr.api and ecr.dkr but
# no S3 gateway fails on the pull rather than on the API call, which is a
# genuinely confusing way to lose an afternoon. With NAT it also keeps image
# layers off the NAT's per-GB charge, which is where that bill comes from.
resource "aws_vpc_endpoint" "s3" {
  count = local.create_vpc ? 1 : 0

  vpc_id            = aws_vpc.this[0].id
  service_name      = "com.amazonaws.${local.region}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids = concat(
    [for rt in aws_route_table.private : rt.id],
    [aws_route_table.database[0].id]
  )

  tags = merge(local.tags, { Name = "${local.name}-s3" })
}

resource "aws_vpc_endpoint" "interface" {
  for_each = local.create_vpc && var.enable_vpc_endpoints ? toset([
    "ecr.api", "ecr.dkr", "logs", "ssm", "secretsmanager", "sts", "kms", "elasticfilesystem"
  ]) : toset([])

  vpc_id              = aws_vpc.this[0].id
  service_name        = "com.amazonaws.${local.region}.${each.value}"
  vpc_endpoint_type   = "Interface"
  subnet_ids          = [for s in aws_subnet.private : s.id]
  security_group_ids  = [aws_security_group.vpc_endpoints[0].id]
  private_dns_enabled = true

  tags = merge(local.tags, { Name = "${local.name}-${each.value}" })
}
