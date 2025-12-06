# --- CLOUDFLARE VARIABLES ---
variable "cloudflare_zone" {
  description = "Domain name for Cloudflare."
  type        = string
}

variable "cloudflare_zone_id" {
  description = "Zone ID for your domain in Cloudflare."
  type        = string
}

variable "cloudflare_account_id" {
  description = "Account ID for your Cloudflare account."
  type        = string
}

variable "cloudflare_token" {
  description = "Cloudflare API token."
  type        = string
  sensitive   = true
}

variable "cloudflare_email" {
  description = "Email address for your Cloudflare account"
  type        = string
  sensitive   = true
}

# --- GOOGLE CLOUD VARIABLES ---
variable "gcp_project_id" {
  type        = string
  description = "The Google Cloud project ID."
  default = "crabberbot"
}

variable "gcp_region" {
  type        = string
  description = "The Google Cloud region to deploy resources in."
  default     = "europe-west1"
}

variable "gcp_repo_name" {
  type        = string
  description = "The name for the Artifact Registry repository."
  default     = "crabberbot-repo"
}

variable "gcp_github_repo" {
  type        = string
  description = "Your GitHub repository in 'owner/repo' format."
  default     = "Enucatl/crabberbot"
}

variable "instance_name" {
  description = "Name for the GCE instance."
  type        = string
  default     = "crabberbot-server"
}
