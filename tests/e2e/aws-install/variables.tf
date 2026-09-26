variable "endpoint" {
  description = "Floci's edge endpoint. Every AWS service answers on it."
  type        = string
  default     = "http://localhost:4566"
}

variable "region" {
  type    = string
  default = "us-east-1"
}

variable "name" {
  description = "Prefix for everything the install creates, shared by both modules as it must be."
  type        = string
  default     = "rise-floci"
}

variable "ingress_domain" {
  type    = string
  default = "rise-floci.test"
}

variable "rise_image_tag" {
  description = <<-EOT
    Control-plane image. The install is exercised as infrastructure: the ECS
    service is created and Terraform does not wait for it to become healthy, so
    any published tag works.
  EOT
  type        = string
  default     = "latest"
}

variable "route53_zone_id" {
  description = "Hosted zone for `ingress_domain`, created by run.sh before the apply."
  type        = string
}
