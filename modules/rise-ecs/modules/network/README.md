# `network`

Creates a VPC with public, private and database subnets, or resolves supplied
subnets after checking their VPC. `topology` controls the managed network.
`vpc` outputs subnet identities and stable keys; `nat_public_ips` supports
public OIDC discovery rules. The caller supplies the endpoint security group.

This child is composed by [`rise-ecs`](../..). It declares provider requirements;
provider configuration and state belong to the caller.
