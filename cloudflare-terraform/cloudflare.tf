resource "cloudflare_zero_trust_tunnel_cloudflared" "crabberbot_tunnel" {
  account_id = var.cloudflare_account_id
  name       = "Terraform crabberbot tunnel"
}

data "cloudflare_zero_trust_tunnel_cloudflared_token" "crabberbot_tunnel_token" {
  account_id = var.cloudflare_account_id
  tunnel_id  = cloudflare_zero_trust_tunnel_cloudflared.crabberbot_tunnel.id
}

resource "cloudflare_dns_record" "crabberbot" {
  zone_id = var.cloudflare_zone_id
  name    = "crabberbot"
  content = "${cloudflare_zero_trust_tunnel_cloudflared.crabberbot_tunnel.id}.cfargotunnel.com"
  type    = "CNAME"
  ttl     = 1
  proxied = true
}

resource "cloudflare_zero_trust_tunnel_cloudflared_config" "crabberbot_tunnel_config" {
  tunnel_id  = cloudflare_zero_trust_tunnel_cloudflared.crabberbot_tunnel.id
  account_id = var.cloudflare_account_id
  config = {
    ingress = [
      {
        hostname = "crabberbot.${var.cloudflare_zone}"
        service  = "http://crabberbot:8080"
      },
      {
        service = "http_status:404"
      }
    ]
  }
}

# Test version resources
resource "cloudflare_zero_trust_tunnel_cloudflared" "crabberbottest_tunnel" {
  account_id = var.cloudflare_account_id
  name       = "Terraform crabberbottest tunnel"
}

data "cloudflare_zero_trust_tunnel_cloudflared_token" "crabberbottest_tunnel_token" {
  account_id = var.cloudflare_account_id
  tunnel_id  = cloudflare_zero_trust_tunnel_cloudflared.crabberbottest_tunnel.id
}

resource "cloudflare_dns_record" "crabberbottest" {
  zone_id = var.cloudflare_zone_id
  name    = "crabberbottest"
  content = "${cloudflare_zero_trust_tunnel_cloudflared.crabberbottest_tunnel.id}.cfargotunnel.com"
  type    = "CNAME"
  ttl     = 1
  proxied = true
}

resource "cloudflare_zero_trust_tunnel_cloudflared_config" "crabberbottest_tunnel_config" {
  tunnel_id  = cloudflare_zero_trust_tunnel_cloudflared.crabberbottest_tunnel.id
  account_id = var.cloudflare_account_id
  config = {
    ingress = [
      {
        hostname = "crabberbottest.${var.cloudflare_zone}"
        service  = "http://crabberbot:8080"
      },
      {
        service = "http_status:404"
      }
    ]
  }
}
