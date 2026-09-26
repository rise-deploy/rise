module "network" {
  source = "./modules/network"

  name = local.name
  tags = local.tags
  vpc  = var.vpc
  topology = {
    cidr                    = var.vpc_cidr
    availability_zone_count = var.availability_zone_count
    nat_gateway_mode        = var.nat_gateway_mode
    enable_vpc_endpoints    = var.enable_vpc_endpoints
  }
  region                     = local.region
  endpoint_security_group_id = module.security.groups.vpc_endpoints
}

module "cluster" {
  source = "./modules/cluster"

  name    = local.name
  tags    = local.tags
  cluster = var.cluster
  vpc_id  = local.vpc_id
  discovery = {
    id   = var.cloud_map_namespace_id
    name = local.namespace_name
  }
  logging = {
    name           = local.log_group_name
    retention_days = var.log_retention_days
  }
  enable_container_insights = var.enable_container_insights
}

module "security" {
  source = "./modules/security"

  name = local.name
  tags = local.tags
  network = {
    vpc_id           = local.vpc_id
    create_endpoints = local.create_vpc && var.enable_vpc_endpoints
    nat_public_ips   = module.network.nat_public_ips
  }
  ingress_cidr_blocks = var.ingress_cidr_blocks
  acme_enabled        = local.acme_enabled
  deploy_dex          = var.deploy_dex
}

module "ingress" {
  source = "./modules/ingress"

  name = local.name
  tags = local.tags
  network = {
    vpc_id                 = local.vpc_id
    public_subnet_ids      = local.public_subnet_ids
    private_subnets_by_key = module.network.vpc.private_subnets_by_key
  }
  security_groups = module.security.groups
  edge = {
    mode                = var.edge_mode
    acm_certificate_arn = var.acm_certificate_arn
    deletion_protection = var.deletion_protection
  }
  dns = {
    create  = local.create_dns_records
    zone_id = var.route53_zone_id
    domain  = var.ingress_domain
  }
  traefik_role = {
    create     = local.create_traefik_task_role
    arn        = var.traefik_task_role_arn
    account_id = local.account_id
  }
}

module "database" {
  source = "./modules/database"

  name    = local.name
  tags    = local.tags
  enabled = local.create_database
  network = {
    subnet_ids        = local.database_subnet_ids
    security_group_id = module.security.groups.database
  }
  postgres = {
    engine_version        = var.db_engine_version
    instance_class        = var.db_instance_class
    allocated_storage     = var.db_allocated_storage
    multi_az              = var.db_multi_az
    backup_retention_days = var.db_backup_retention_days
    deletion_protection   = var.deletion_protection
  }
  secret_recovery_window_days = var.secret_recovery_window_days
}

module "secrets" {
  source = "./modules/secrets"

  name                 = local.name
  tags                 = local.tags
  oidc_client_secret   = local.oidc_client_secret
  recovery_window_days = var.secret_recovery_window_days
}

module "dex" {
  source = "./modules/dex"

  name        = local.name
  tags        = local.tags
  enabled     = var.deploy_dex
  cluster_arn = local.cluster_arn
  discovery = {
    namespace_id = local.namespace_id
    name         = local.dex_discovery_name
  }
  network = {
    subnet_ids        = local.private_subnet_ids
    security_group_id = module.security.groups.dex
  }
  logging = {
    group_name = module.cluster.logging.name
    region     = local.region
  }
  task = {
    image              = var.dex_image
    execution_role_arn = var.execution_role_arn
    cpu_architecture   = var.cpu_architecture
  }
  identity = {
    issuer                = local.oidc_issuer
    client_id             = var.oidc_client_id
    client_secret         = local.oidc_client_secret
    public_url            = local.public_url
    admin_email           = coalesce(var.dex_admin_email, var.admin_email)
    admin_password_bcrypt = var.dex_admin_password_bcrypt
  }
  ingress = {
    domain       = var.ingress_domain
    entrypoint   = local.traefik_entrypoint
    acme_enabled = local.acme_enabled
  }
  runtime_services = [module.runtime.rise.service_id, module.runtime.traefik.service_id]
}
