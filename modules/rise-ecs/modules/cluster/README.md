# `cluster`

Creates or resolves the ECS cluster and private DNS namespace. The log group
is managed for either cluster mode. `cluster`, `discovery` and `logging` expose
the identities consumed by service modules.

This child is composed by [`rise-ecs`](../..). It declares provider requirements;
provider configuration and state belong to the caller.
