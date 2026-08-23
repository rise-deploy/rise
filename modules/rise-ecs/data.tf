data "aws_caller_identity" "current" {}
data "aws_region" "current" {}
data "aws_partition" "current" {}

data "aws_availability_zones" "available" {
  state = "available"

  filter {
    name   = "opt-in-status"
    values = ["opt-in-not-required"]
  }
}

# Brought-in subnets, read back so their VPC can be checked against the VPC the
# caller named. Terraform would otherwise discover the mismatch at apply, several
# minutes in, as an opaque ECS or RDS error.
data "aws_subnet" "brought" {
  for_each = local.create_vpc ? toset([]) : toset(concat(
    var.vpc.private_subnet_ids,
    var.vpc.public_subnet_ids,
    var.vpc.database_subnet_ids
  ))

  id = each.value

  lifecycle {
    postcondition {
      condition     = self.vpc_id == var.vpc.id
      error_message = "Subnet ${self.id} is in VPC ${self.vpc_id}, not the ${var.vpc.id} given as vpc.id."
    }
  }
}

# Mirrors EcsBackend::test_connection, which refuses to start against a cluster
# that is not ACTIVE. Failing in `terraform plan` beats failing at Rise startup.
data "aws_ecs_cluster" "brought" {
  count = local.create_cluster ? 0 : 1

  cluster_name = var.cluster.name

  lifecycle {
    postcondition {
      condition     = self.status == "ACTIVE"
      error_message = "ECS cluster ${var.cluster.name} is ${self.status}, not ACTIVE."
    }
  }
}
