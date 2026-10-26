data "aws_availability_zones" "available" {
  state = "available"

  filter {
    name   = "opt-in-status"
    values = ["opt-in-not-required"]
  }
}
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
