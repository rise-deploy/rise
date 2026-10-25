# `dex`

Runs the optional demo identity provider with private service discovery and
public Traefik routing. `identity` supplies the public issuer, client and static
administrator. `runtime_services` orders service creation after Rise and Traefik.
Storage is in memory; task replacement loses sessions and refresh tokens.

This child is composed by [`rise-ecs`](../..). It declares provider requirements;
provider configuration and state belong to the caller.
