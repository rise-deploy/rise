# The Rise control plane's own store. Distinct from modules/rise-aws's
# `enable_rds`, which is about the RDS *extension* — Rise provisioning databases
# for user projects — and creates only a subnet group and security group.
#
# Migrations run in-process when the control plane starts, so there is no
# separate migration task to schedule.

resource "aws_db_subnet_group" "this" {
  count = var.enabled ? 1 : 0

  name       = "${var.name}-control-plane"
  subnet_ids = var.network.subnet_ids
  tags       = merge(var.tags, { Name = "${var.name}-control-plane" })
}

resource "aws_db_instance" "this" {
  count = var.enabled ? 1 : 0

  identifier     = "${var.name}-control-plane"
  engine         = "postgres"
  engine_version = var.postgres.engine_version
  instance_class = var.postgres.instance_class

  db_name  = "rise"
  username = "rise"
  password = random_password.database[0].result

  allocated_storage     = var.postgres.allocated_storage
  max_allocated_storage = var.postgres.allocated_storage * 4
  storage_type          = "gp3"
  storage_encrypted     = true

  db_subnet_group_name   = aws_db_subnet_group.this[0].name
  vpc_security_group_ids = [var.network.security_group_id]
  multi_az               = var.postgres.multi_az
  publicly_accessible    = false

  backup_retention_period = var.postgres.backup_retention_days
  deletion_protection     = var.postgres.deletion_protection
  skip_final_snapshot     = !var.postgres.deletion_protection
  final_snapshot_identifier = var.postgres.deletion_protection ? (
    "${var.name}-control-plane-final"
  ) : null

  auto_minor_version_upgrade = true
  apply_immediately          = false

  tags = merge(var.tags, { Name = "${var.name}-control-plane" })

  lifecycle {
    # Rotating the password out of band should not read as drift Terraform wants
    # to undo. Rotating it *properly* means updating the secret too — see the
    # rise-ecs README.
    ignore_changes = [password]
  }
}
