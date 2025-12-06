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

output "gcp_bot_service_account_email" {
  description = "The email of the service account for the bot on GCP."
  value       = google_service_account.bot_sa.email
}
