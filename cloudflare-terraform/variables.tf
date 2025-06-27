# Cloudflare variables
variable "zone" {
  description = "Domain name"
  type        = string
}

variable "zone_id" {
  description = "Zone ID for your domain"
  type        = string
}

variable "account_id" {
  description = "Account ID for your Cloudflare account"
  type        = string
  sensitive   = true
}

variable "email" {
  description = "Email address for your Cloudflare account"
  type        = string
  sensitive   = true
}

variable "token" {
  description = "Cloudflare API token"
  type        = string
  sensitive   = true
}
