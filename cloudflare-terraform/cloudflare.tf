# Creates a new remotely-managed tunnel for the crabberbot VM.
resource "cloudflare_zero_trust_tunnel_cloudflared" "crabberbot_tunnel" {
  account_id    = var.account_id
  name          = "Terraform crabberbot tunnel"
}

# Reads the token used to run the tunnel on the server.
data "cloudflare_zero_trust_tunnel_cloudflared_token" "crabberbot_tunnel_token" {
  account_id   = var.account_id
  tunnel_id   = cloudflare_zero_trust_tunnel_cloudflared.crabberbot_tunnel.id
}

# Creates the CNAME record that routes crabberbot.${var.zone} to the tunnel.
resource "cloudflare_dns_record" "crabberbot" {
  zone_id = var.zone_id
  name    = "crabberbot"
  content = "${cloudflare_zero_trust_tunnel_cloudflared.crabberbot_tunnel.id}.cfargotunnel.com"
  type    = "CNAME"
  ttl     = 1
  proxied = true
}

# Configures tunnel with a public hostname route for clientless access.
resource "cloudflare_zero_trust_tunnel_cloudflared_config" "crabberbot_tunnel_config" {
  tunnel_id  = cloudflare_zero_trust_tunnel_cloudflared.crabberbot_tunnel.id
  account_id = var.account_id
  config     = {
    ingress   = [
      {
        hostname = "crabberbot.${var.zone}"
        service  = "http://crabberbot:8080"
      },
      {
        service  = "http_status:404"
      }
    ]
  }
}
