resource "aws_service_discovery_service" "rise" {
  name = var.control_plane.discovery_name

  dns_config {
    namespace_id   = var.discovery.namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  # Instances must be deregistered before a Cloud Map service will delete, and
  # ECS deregisters them only as its own tasks drain. `terraform destroy` hits
  # the same wall and may need a retry; that is AWS's ordering, not a bug here.
  force_destroy = true

  tags = var.tags
}

resource "aws_service_discovery_service" "traefik" {
  name = var.traefik.discovery_name

  dns_config {
    namespace_id   = var.discovery.namespace_id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  force_destroy = true
  tags          = var.tags
}
