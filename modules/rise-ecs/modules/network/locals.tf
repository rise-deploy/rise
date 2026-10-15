locals {
  azs = slice(data.aws_availability_zones.available.names, 0, var.topology.availability_zone_count)

  # --- VPC ------------------------------------------------------------------
  create_vpc = var.vpc == null
  vpc_id     = local.create_vpc ? aws_vpc.this[0].id : var.vpc.id

  public_subnet_ids  = local.create_vpc ? [for s in aws_subnet.public : s.id] : var.vpc.public_subnet_ids
  private_subnet_ids = local.create_vpc ? [for s in aws_subnet.private : s.id] : var.vpc.private_subnet_ids
  database_subnet_ids = local.create_vpc ? [for s in aws_subnet.database : s.id] : (
    length(var.vpc.database_subnet_ids) > 0 ? var.vpc.database_subnet_ids : var.vpc.private_subnet_ids
  )

  # Keyed by something statically known in both modes -- AZ name when the module
  # creates the subnets, subnet id when they are brought. for_each cannot take
  # keys that are only known after apply, and in create mode the ids are exactly
  # that.
  private_subnets_by_key = local.create_vpc ? {
    for az in local.azs : az => aws_subnet.private[az].id
    } : {
    for id in var.vpc.private_subnet_ids : id => id
  }

  nat_gateway_count = local.create_vpc ? (
    var.topology.nat_gateway_mode == "per_az" ? var.topology.availability_zone_count :
    var.topology.nat_gateway_mode == "single" ? 1 : 0
  ) : 0
}
