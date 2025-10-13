resource "google_project_service" "apis" {
  for_each = toset(["run.googleapis.com", "artifactregistry.googleapis.com", "secretmanager.googleapis.com", "iam.googleapis.com"])
  service  = each.key
  disable_on_destroy = false
}

resource "google_artifact_registry_repository" "repo" {
  depends_on    = [google_project_service.apis]
  repository_id = var.gcp_repo_name
  location      = var.gcp_region
  format        = "DOCKER"
  description   = "Docker repository for crabberbot."
}

resource "google_secret_manager_secret" "teloxide_token" {
  depends_on = [google_project_service.apis]
  secret_id  = "teloxide-token"
  replication {
    auto {}
  }
}

resource "google_secret_manager_secret" "telegram_api_id" {
  depends_on = [google_project_service.apis]
  secret_id  = "telegram-api-id"
  replication {
    auto {}
  }
}

resource "google_secret_manager_secret" "telegram_api_hash" {
  depends_on = [google_project_service.apis]
  secret_id  = "telegram-api-hash"
  replication {
    auto {}
  }
}

resource "google_secret_manager_secret" "webhook_url" {
  depends_on = [google_project_service.apis]
  secret_id  = "webhook-url"
  replication {
    auto {}
  }
}

resource "google_service_account" "bot_sa" {
  account_id   = "crabberbot-sa"
  display_name = "CrabberBot Service Account"
}

resource "google_cloud_run_v2_service" "api_server" {
  depends_on = [google_project_service.apis]
  name       = "telegram-bot-api-server"
  location   = var.gcp_region
  ingress    = "INGRESS_TRAFFIC_ALL"
  deletion_protection = false
  template {
    service_account = google_service_account.bot_sa.email
    scaling {
      min_instance_count = 0
      max_instance_count = 1
    }
    containers {
      image = "aiogram/telegram-bot-api:latest"
      ports { container_port = 8081 }
      env {
        name = "TELEGRAM_API_ID"
        value_source {
          secret_key_ref {
            secret = google_secret_manager_secret.telegram_api_id.secret_id
            version = "latest"
          }
        }
      }
      env {
        name = "TELEGRAM_API_HASH"
        value_source {
          secret_key_ref {
            secret = google_secret_manager_secret.telegram_api_hash.secret_id
            version = "latest"
          }
        }
      }
      env {
        name = "TELEGRAM_LOCAL"
        value = "1"
      }
      env {
        name = "TELEGRAM_VERBOSITY"
        value = "1"
      }
    }
  }
}

resource "google_cloud_run_v2_service" "bot" {
  depends_on          = [google_cloud_run_v2_service.api_server]
  name                = "crabberbot"
  location            = var.gcp_region
  ingress             = "INGRESS_TRAFFIC_ALL" # Keep this service public for webhooks.
  deletion_protection = false

  template {
    service_account = google_service_account.bot_sa.email
    scaling {
      min_instance_count = 0
      max_instance_count = 1
    }
    containers {
      image = "${var.gcp_region}-docker.pkg.dev/${var.gcp_project_id}/${google_artifact_registry_repository.repo.repository_id}/crabberbot:latest"
      ports { container_port = 80 }
      env {
        name  = "EXECUTION_ENVIRONMENT"
        value = "gcp"
      }
      env {
        name  = "TELOXIDE_API_URL"
        value = google_cloud_run_v2_service.api_server.uri
      }
      env {
        name  = "WEBHOOK_URL"
        value_source {
          secret_key_ref {
            secret  = google_secret_manager_secret.webhook_url.secret_id
            version = "latest"
          }
        }
      }
      env {
        name = "TELOXIDE_TOKEN"
        value_source {
          secret_key_ref {
            secret  = google_secret_manager_secret.teloxide_token.secret_id
            version = "latest"
          }
        }
      }
      env {
        name  = "RUST_LOG"
        value = "info,crabberbot=info"
      }
    }
  }
}

# This resource makes the "bot" service publicly accessible.
resource "google_cloud_run_v2_service_iam_member" "public_webhook_access" {
  project  = google_cloud_run_v2_service.bot.project
  location = google_cloud_run_v2_service.bot.location
  name     = google_cloud_run_v2_service.bot.name
  role     = "roles/run.invoker"
  member   = "allUsers"
}

# This resource allows the "bot" service to securely invoke the private "api_server" service.
resource "google_cloud_run_v2_service_iam_member" "bot_invokes_api_server" {
  project  = google_cloud_run_v2_service.api_server.project
  location = google_cloud_run_v2_service.api_server.location
  name     = google_cloud_run_v2_service.api_server.name
  role     = "roles/run.invoker"
  member   = "serviceAccount:${google_service_account.bot_sa.email}"
  # member   = "allUsers"
}

resource "google_project_iam_member" "secret_accessor_binding" {
  project = var.gcp_project_id
  role    = "roles/secretmanager.secretAccessor"
  member  = "serviceAccount:${google_service_account.bot_sa.email}"
}

resource "google_artifact_registry_repository_iam_member" "writer" {
  location   = google_artifact_registry_repository.repo.location
  repository = google_artifact_registry_repository.repo.repository_id
  role       = "roles/artifactregistry.writer"
  member     = "serviceAccount:${google_service_account.bot_sa.email}"
}

resource "google_project_iam_member" "run_admin" {
  project = var.gcp_project_id
  role    = "roles/run.admin"
  member  = "serviceAccount:${google_service_account.bot_sa.email}"
}

resource "google_project_iam_member" "service_account_user" {
  project = var.gcp_project_id
  role    = "roles/iam.serviceAccountUser"
  member  = "serviceAccount:${google_service_account.bot_sa.email}"
}

resource "google_iam_workload_identity_pool" "github_pool" {
  workload_identity_pool_id = "github-pool"
  display_name              = "GitHub Actions Pool"
}

resource "google_iam_workload_identity_pool_provider" "github_provider" {
  workload_identity_pool_id          = google_iam_workload_identity_pool.github_pool.workload_identity_pool_id
  workload_identity_pool_provider_id = "github-provider"
  
  attribute_mapping = {
    "google.subject" = "assertion.sub"
  }
  
  attribute_condition = "assertion.repository == '${var.gcp_github_repo}'"

  oidc { issuer_uri = "https://token.actions.githubusercontent.com" }
}

resource "google_service_account_iam_member" "allow_github_impersonation" {
  service_account_id = google_service_account.bot_sa.name
  role               = "roles/iam.workloadIdentityUser"
  member             = "principalSet://iam.googleapis.com/${google_iam_workload_identity_pool.github_pool.name}/*"
}
