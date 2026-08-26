# The Rise control plane's own store. Distinct from modules/rise-aws's
# `enable_rds`, which is about the RDS *extension* — Rise provisioning databases
# for user projects — and creates only a subnet group and security group.
#
# Migrations run in-process when the control plane starts, so there is no
# separate migration task to schedule.

resource "aws_db_subnet_group" "this" {
  count = local.create_database ? 1 : 0

  name       = "${local.name}-control-plane"
  subnet_ids = local.database_subnet_ids
  tags       = merge(local.tags, { Name = "${local.name}-control-plane" })
}

resource "aws_db_instance" "this" {
  count = local.create_database ? 1 : 0

  identifier     = "${local.name}-control-plane"
  engine         = "postgres"
  engine_version = var.db_engine_version
  instance_class = var.db_instance_class

  db_name  = "rise"
  username = "rise"
  password = random_password.database[0].result

  allocated_storage     = var.db_allocated_storage
  max_allocated_storage = var.db_allocated_storage * 4
  storage_type          = "gp3"
  storage_encrypted     = true

  db_subnet_group_name   = aws_db_subnet_group.this[0].name
  vpc_security_group_ids = [aws_security_group.database.id]
  multi_az               = var.db_multi_az
  publicly_accessible    = false

  backup_retention_period = var.db_backup_retention_days
  deletion_protection     = var.deletion_protection
  skip_final_snapshot     = !var.deletion_protection
  final_snapshot_identifier = var.deletion_protection ? (
    "${local.name}-control-plane-final"
  ) : null

  auto_minor_version_upgrade = true
  apply_immediately          = false

  tags = merge(local.tags, { Name = "${local.name}-control-plane" })

  lifecycle {
    # Rotating the password out of band should not read as drift Terraform wants
    # to undo. Rotating it *properly* means updating the secret too — see the
    # module README.
    ignore_changes = [password]
  }
}
