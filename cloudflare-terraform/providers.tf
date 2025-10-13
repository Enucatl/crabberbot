terraform {
  required_providers {
    cloudflare = {
      source  = "cloudflare/cloudflare"
      version = ">= 5.3"
    }
    google = {
      source  = "hashicorp/google"
      version = ">= 5.0"
    }
  }
  required_version = ">= 1.2"
}

provider "cloudflare" {
  api_token = var.cloudflare_token
}

provider "random" {
}

provider "google" {
  project = var.gcp_project_id
  region  = var.gcp_region
}
