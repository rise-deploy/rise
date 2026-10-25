# `database`

Owns the control-plane PostgreSQL instance, persistent generated password and
Secrets Manager connection URL. `enabled = false` creates no database resources.
The caller supplies subnets and a security group. `url_secret_arn` is ready only
after its secret version exists. Generated credentials remain in Terraform state.

This child is composed by [`rise-ecs`](../..). It declares provider requirements;
provider configuration and state belong to the caller.
