# `security`

Owns security groups and communication rules for the load balancer, Traefik,
Rise, workloads, database, ACME storage, VPC endpoints and optional Dex.
`groups` exposes IDs by service role. Network topology and optional-service
flags determine which rules exist.

This child is composed by [`rise-ecs`](../..). It declares provider requirements;
provider configuration and state belong to the caller.
