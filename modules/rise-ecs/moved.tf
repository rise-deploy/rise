moved {
  from = aws_ecs_task_definition.rise
  to   = module.runtime.aws_ecs_task_definition.rise
}

moved {
  from = aws_ecs_task_definition.traefik
  to   = module.runtime.aws_ecs_task_definition.traefik
}

moved {
  from = aws_ecs_service.rise
  to   = module.runtime.aws_ecs_service.rise
}

moved {
  from = aws_ecs_service.traefik
  to   = module.runtime.aws_ecs_service.traefik
}

moved {
  from = aws_service_discovery_service.rise
  to   = module.runtime.aws_service_discovery_service.rise
}

moved {
  from = aws_service_discovery_service.traefik
  to   = module.runtime.aws_service_discovery_service.traefik
}

moved {
  from = aws_vpc.this
  to   = module.network.aws_vpc.this
}

moved {
  from = aws_internet_gateway.this
  to   = module.network.aws_internet_gateway.this
}

moved {
  from = aws_subnet.public
  to   = module.network.aws_subnet.public
}

moved {
  from = aws_subnet.private
  to   = module.network.aws_subnet.private
}

moved {
  from = aws_subnet.database
  to   = module.network.aws_subnet.database
}

moved {
  from = aws_route_table.public
  to   = module.network.aws_route_table.public
}

moved {
  from = aws_route.public_internet
  to   = module.network.aws_route.public_internet
}

moved {
  from = aws_route_table_association.public
  to   = module.network.aws_route_table_association.public
}

moved {
  from = aws_eip.nat
  to   = module.network.aws_eip.nat
}

moved {
  from = aws_nat_gateway.this
  to   = module.network.aws_nat_gateway.this
}

moved {
  from = aws_route_table.private
  to   = module.network.aws_route_table.private
}

moved {
  from = aws_route.private_nat
  to   = module.network.aws_route.private_nat
}

moved {
  from = aws_route_table_association.private
  to   = module.network.aws_route_table_association.private
}

moved {
  from = aws_route_table.database
  to   = module.network.aws_route_table.database
}

moved {
  from = aws_route_table_association.database
  to   = module.network.aws_route_table_association.database
}

moved {
  from = aws_vpc_endpoint.s3
  to   = module.network.aws_vpc_endpoint.s3
}

moved {
  from = aws_vpc_endpoint.interface
  to   = module.network.aws_vpc_endpoint.interface
}

moved {
  from = aws_ecs_cluster.this
  to   = module.cluster.aws_ecs_cluster.this
}

moved {
  from = aws_ecs_cluster_capacity_providers.this
  to   = module.cluster.aws_ecs_cluster_capacity_providers.this
}

moved {
  from = aws_cloudwatch_log_group.this
  to   = module.cluster.aws_cloudwatch_log_group.this
}

moved {
  from = aws_service_discovery_private_dns_namespace.this
  to   = module.cluster.aws_service_discovery_private_dns_namespace.this
}

moved {
  from = aws_security_group.edge
  to   = module.security.aws_security_group.edge
}

moved {
  from = aws_vpc_security_group_ingress_rule.edge_http
  to   = module.security.aws_vpc_security_group_ingress_rule.edge_http
}

moved {
  from = aws_vpc_security_group_ingress_rule.edge_https
  to   = module.security.aws_vpc_security_group_ingress_rule.edge_https
}

moved {
  from = aws_vpc_security_group_egress_rule.edge_to_traefik
  to   = module.security.aws_vpc_security_group_egress_rule.edge_to_traefik
}

moved {
  from = aws_security_group.traefik
  to   = module.security.aws_security_group.traefik
}

moved {
  from = aws_vpc_security_group_ingress_rule.traefik_from_edge
  to   = module.security.aws_vpc_security_group_ingress_rule.traefik_from_edge
}

moved {
  from = aws_vpc_security_group_ingress_rule.traefik_api_from_rise
  to   = module.security.aws_vpc_security_group_ingress_rule.traefik_api_from_rise
}

moved {
  from = aws_vpc_security_group_egress_rule.traefik_all
  to   = module.security.aws_vpc_security_group_egress_rule.traefik_all
}

moved {
  from = aws_security_group.control_plane
  to   = module.security.aws_security_group.control_plane
}

moved {
  from = aws_vpc_security_group_ingress_rule.control_plane_from_traefik
  to   = module.security.aws_vpc_security_group_ingress_rule.control_plane_from_traefik
}

moved {
  from = aws_vpc_security_group_egress_rule.control_plane_all
  to   = module.security.aws_vpc_security_group_egress_rule.control_plane_all
}

moved {
  from = aws_security_group.apps
  to   = module.security.aws_security_group.apps
}

moved {
  from = aws_vpc_security_group_ingress_rule.apps_from_traefik
  to   = module.security.aws_vpc_security_group_ingress_rule.apps_from_traefik
}

moved {
  from = aws_vpc_security_group_egress_rule.apps_all
  to   = module.security.aws_vpc_security_group_egress_rule.apps_all
}

moved {
  from = aws_security_group.database
  to   = module.security.aws_security_group.database
}

moved {
  from = aws_vpc_security_group_ingress_rule.database_from_control_plane
  to   = module.security.aws_vpc_security_group_ingress_rule.database_from_control_plane
}

moved {
  from = aws_security_group.efs
  to   = module.security.aws_security_group.efs
}

moved {
  from = aws_vpc_security_group_ingress_rule.efs_from_traefik
  to   = module.security.aws_vpc_security_group_ingress_rule.efs_from_traefik
}

moved {
  from = aws_security_group.vpc_endpoints
  to   = module.security.aws_security_group.vpc_endpoints
}

moved {
  from = aws_vpc_security_group_ingress_rule.vpc_endpoints_from_tasks
  to   = module.security.aws_vpc_security_group_ingress_rule.vpc_endpoints_from_tasks
}

moved {
  from = aws_security_group.dex
  to   = module.security.aws_security_group.dex
}

moved {
  from = aws_vpc_security_group_ingress_rule.dex_from_traefik
  to   = module.security.aws_vpc_security_group_ingress_rule.dex_from_traefik
}

moved {
  from = aws_vpc_security_group_egress_rule.dex_all
  to   = module.security.aws_vpc_security_group_egress_rule.dex_all
}

moved {
  from = aws_vpc_security_group_ingress_rule.edge_from_nat
  to   = module.security.aws_vpc_security_group_ingress_rule.edge_from_nat
}

moved {
  from = aws_lb.this
  to   = module.ingress.aws_lb.this
}

moved {
  from = aws_lb_target_group.traefik
  to   = module.ingress.aws_lb_target_group.traefik
}

moved {
  from = aws_lb_listener.http
  to   = module.ingress.aws_lb_listener.http
}

moved {
  from = aws_lb_listener.https
  to   = module.ingress.aws_lb_listener.https
}

moved {
  from = aws_lb_listener.alb_https
  to   = module.ingress.aws_lb_listener.alb_https
}

moved {
  from = aws_efs_file_system.acme
  to   = module.ingress.aws_efs_file_system.acme
}

moved {
  from = aws_efs_mount_target.acme
  to   = module.ingress.aws_efs_mount_target.acme
}

moved {
  from = aws_efs_access_point.acme
  to   = module.ingress.aws_efs_access_point.acme
}

moved {
  from = aws_route53_record.apex
  to   = module.ingress.aws_route53_record.apex
}

moved {
  from = aws_route53_record.wildcard
  to   = module.ingress.aws_route53_record.wildcard
}

moved {
  from = aws_iam_role.traefik
  to   = module.ingress.aws_iam_role.traefik
}

moved {
  from = aws_iam_role_policy.traefik
  to   = module.ingress.aws_iam_role_policy.traefik
}

moved {
  from = aws_db_subnet_group.this
  to   = module.database.aws_db_subnet_group.this
}

moved {
  from = aws_db_instance.this
  to   = module.database.aws_db_instance.this
}

moved {
  from = random_password.database
  to   = module.database.random_password.database
}

moved {
  from = aws_secretsmanager_secret.database_url
  to   = module.database.aws_secretsmanager_secret.database_url
}

moved {
  from = aws_secretsmanager_secret_version.database_url
  to   = module.database.aws_secretsmanager_secret_version.database_url
}

moved {
  from = random_bytes.jwt_signing_secret
  to   = module.secrets.random_bytes.jwt_signing_secret
}

moved {
  from = random_bytes.encryption_key
  to   = module.secrets.random_bytes.encryption_key
}

moved {
  from = aws_secretsmanager_secret.jwt_signing_secret
  to   = module.secrets.aws_secretsmanager_secret.jwt_signing_secret
}

moved {
  from = aws_secretsmanager_secret_version.jwt_signing_secret
  to   = module.secrets.aws_secretsmanager_secret_version.jwt_signing_secret
}

moved {
  from = aws_secretsmanager_secret.encryption_key
  to   = module.secrets.aws_secretsmanager_secret.encryption_key
}

moved {
  from = aws_secretsmanager_secret_version.encryption_key
  to   = module.secrets.aws_secretsmanager_secret_version.encryption_key
}

moved {
  from = aws_secretsmanager_secret.oidc_client_secret
  to   = module.secrets.aws_secretsmanager_secret.oidc_client_secret
}

moved {
  from = aws_secretsmanager_secret_version.oidc_client_secret
  to   = module.secrets.aws_secretsmanager_secret_version.oidc_client_secret
}

moved {
  from = aws_ecs_task_definition.dex
  to   = module.dex.aws_ecs_task_definition.dex
}

moved {
  from = aws_ecs_service.dex
  to   = module.dex.aws_ecs_service.dex
}

moved {
  from = aws_service_discovery_service.dex
  to   = module.dex.aws_service_discovery_service.dex
}
