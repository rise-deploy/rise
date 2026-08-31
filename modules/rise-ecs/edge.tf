# ADR-0005 D5: an NLB in TCP passthrough fronts Traefik, and Traefik terminates
# TLS with ACME HTTP-01. The alternative -- ACM on the load balancer -- would
# cost every custom domain its automatic certificate, since ACM has no HTTP-01
# and each domain would gain a DNS validation step the user has to complete.

locals {
  is_nlb = var.edge_mode == "nlb-traefik-acme"
}

resource "aws_lb" "this" {
  name               = local.name
  load_balancer_type = local.is_nlb ? "network" : "application"
  internal           = false
  subnets            = local.public_subnet_ids
  security_groups    = [aws_security_group.edge.id]

  enable_cross_zone_load_balancing = local.is_nlb ? true : null
  enable_deletion_protection       = var.deletion_protection

  tags = merge(local.tags, { Name = local.name })

  lifecycle {
    precondition {
      condition     = length(local.public_subnet_ids) > 0
      error_message = "The edge load balancer needs public subnets. Provide vpc.public_subnet_ids, or let the module create the VPC."
    }

    precondition {
      condition     = var.edge_mode != "alb-acm" || var.acm_certificate_arn != null
      error_message = "edge_mode \"alb-acm\" requires acm_certificate_arn."
    }

    precondition {
      condition     = !local.acme_enabled || var.acme_email != null
      error_message = "edge_mode \"nlb-traefik-acme\" requires acme_email for Let's Encrypt registration."
    }

    # VPC endpoints reach AWS services only. ACME HTTP-01 needs Traefik to talk
    # to Let's Encrypt, so an egress-free VPC does not merely make issuance slow
    # -- the certificate never arrives, and nothing reports an error.
    precondition {
      condition     = !local.acme_enabled || var.nat_gateway_mode != "none"
      error_message = "ACME needs internet egress: nat_gateway_mode \"none\" cannot reach Let's Encrypt. Use a NAT gateway, or edge_mode \"alb-acm\"."
    }

    precondition {
      condition     = var.deploy_dex || var.oidc_issuer != null
      error_message = "Set oidc_issuer to an existing identity provider, or deploy_dex = true for a self-contained demo."
    }

    precondition {
      condition     = var.registry_type != "ecr" || var.ecr_push_role_arn != null
      error_message = "registry_type \"ecr\" requires ecr_push_role_arn (modules/rise-aws `push_role_arn`)."
    }
  }
}

resource "aws_lb_target_group" "traefik" {
  for_each = local.is_nlb ? toset(["80", "443"]) : toset(["80"])

  name        = "${local.name}-${each.value}"
  port        = tonumber(each.value)
  protocol    = local.is_nlb ? "TCP" : "HTTP"
  target_type = "ip" # awsvpc tasks have no instance to register.
  vpc_id      = local.vpc_id

  # Traefik's own ping entrypoint, so a task that is up but not yet routing does
  # not receive traffic.
  health_check {
    protocol = "HTTP"
    path     = "/ping"
    port     = "8082"
  }

  deregistration_delay = 30

  tags = local.tags
}

resource "aws_lb_listener" "http" {
  load_balancer_arn = aws_lb.this.arn
  port              = 80
  protocol          = local.is_nlb ? "TCP" : "HTTP"

  default_action {
    type             = local.is_nlb ? "forward" : "redirect"
    target_group_arn = local.is_nlb ? aws_lb_target_group.traefik["80"].arn : null

    dynamic "redirect" {
      for_each = local.is_nlb ? [] : [1]

      content {
        port        = "443"
        protocol    = "HTTPS"
        status_code = "HTTP_301"
      }
    }
  }
}

resource "aws_lb_listener" "https" {
  count = local.is_nlb ? 1 : 0

  load_balancer_arn = aws_lb.this.arn
  port              = 443
  protocol          = "TCP"

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.traefik["443"].arn
  }
}

# ALB mode terminates here and forwards plaintext to Traefik, which still routes
# and still runs forwardAuth -- the access-class path must not fork (D5).
resource "aws_lb_listener" "alb_https" {
  count = local.is_nlb ? 0 : 1

  load_balancer_arn = aws_lb.this.arn
  port              = 443
  protocol          = "HTTPS"
  ssl_policy        = "ELBSecurityPolicy-TLS13-1-2-2021-06"
  certificate_arn   = var.acm_certificate_arn

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.traefik["80"].arn
  }
}

# --- ACME certificate store --------------------------------------------------

resource "aws_efs_file_system" "acme" {
  count = local.acme_enabled ? 1 : 0

  encrypted      = true
  creation_token = "${local.name}-acme"

  lifecycle_policy {
    transition_to_ia = "AFTER_30_DAYS"
  }

  tags = merge(local.tags, { Name = "${local.name}-acme" })
}

resource "aws_efs_mount_target" "acme" {
  for_each = local.acme_enabled ? local.private_subnets_by_key : {}

  file_system_id  = aws_efs_file_system.acme[0].id
  subnet_id       = each.value
  security_groups = [aws_security_group.efs[0].id]
}

# Traefik refuses an acme.json that is not 0600, and the official image runs as
# root, so the access point owns the directory as uid 0 with 0700.
resource "aws_efs_access_point" "acme" {
  count = local.acme_enabled ? 1 : 0

  file_system_id = aws_efs_file_system.acme[0].id

  posix_user {
    uid = 0
    gid = 0
  }

  root_directory {
    path = "/traefik"

    creation_info {
      owner_uid   = 0
      owner_gid   = 0
      permissions = "0700"
    }
  }

  tags = local.tags
}

# --- DNS ---------------------------------------------------------------------
#
# The wildcard is required, not a nicety: projects are served at
# <project>.<domain>, and groups and environments add another label.

resource "aws_route53_record" "apex" {
  count = var.route53_zone_id != null ? 1 : 0

  zone_id = var.route53_zone_id
  name    = var.ingress_domain
  type    = "A"

  alias {
    name                   = aws_lb.this.dns_name
    zone_id                = aws_lb.this.zone_id
    evaluate_target_health = true
  }
}

resource "aws_route53_record" "wildcard" {
  count = var.route53_zone_id != null ? 1 : 0

  zone_id = var.route53_zone_id
  name    = "*.${var.ingress_domain}"
  type    = "A"

  alias {
    name                   = aws_lb.this.dns_name
    zone_id                = aws_lb.this.zone_id
    evaluate_target_health = true
  }
}
