output "cloudflare_tunnel_token" {
  description = "The token for the Cloudflare Tunnel. Use this with `cloudflared tunnel run --token`."
  value       = data.cloudflare_zero_trust_tunnel_cloudflared_token.crabberbot_tunnel_token.token
  sensitive   = true
}

output "cloudflare_test_tunnel_token" {
  description = "The token for the test Cloudflare Tunnel."
  value       = data.cloudflare_zero_trust_tunnel_cloudflared_token.crabberbottest_tunnel_token.token
  sensitive   = true
}

output "gcp_bot_webhook_url" {
  description = "The public HTTPS URL for your bot's webhook on GCP."
  value       = google_cloud_run_v2_service.bot.uri
}

output "gcp_workload_identity_provider" {
  description = "The full name of the Workload Identity Provider for GitHub Actions."
  value       = google_iam_workload_identity_pool_provider.github_provider.name
}

output "gcp_bot_service_account_email" {
  description = "The email of the service account for the bot on GCP."
  value       = google_service_account.bot_sa.email
}
