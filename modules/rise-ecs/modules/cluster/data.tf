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
