# `ingress`

Owns the public NLB or ALB, attached target groups, optional Route 53 records,
ACME storage and the managed Traefik task role. Network and security groups
are supplied by the caller. `target_groups` waits for listeners; `acme` waits
for EFS mounts; `traefik_role_arn` waits for the managed policy attachment.
These outputs connect directly to the shared runtime.

This child is composed by [`rise-ecs`](../..). It declares provider requirements;
provider configuration and state belong to the caller.
