output "vpc" {
  description = "Resolved network and stable subnet keys."
  value = {
    id                     = local.vpc_id
    public_subnet_ids      = local.public_subnet_ids
    private_subnet_ids     = local.private_subnet_ids
    database_subnet_ids    = local.database_subnet_ids
    private_subnets_by_key = local.private_subnets_by_key
  }
}
output "nat_public_ips" {
  description = "NAT addresses keyed by their configured index."
  value       = { for idx, eip in aws_eip.nat : idx => eip.public_ip }
}
