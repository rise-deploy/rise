output "groups" {
  description = "Security group IDs by service role."
  value = {
    edge          = aws_security_group.edge.id
    traefik       = aws_security_group.traefik.id
    control_plane = aws_security_group.control_plane.id
    apps          = aws_security_group.apps.id
    database      = aws_security_group.database.id
    efs           = try(aws_security_group.efs[0].id, null)
    vpc_endpoints = try(aws_security_group.vpc_endpoints[0].id, null)
    dex           = try(aws_security_group.dex[0].id, null)
  }
}
