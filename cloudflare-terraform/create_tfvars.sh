umask 077
vault kv get -format=json -mount=secret cloudflare | jq -r '.data.data | to_entries[] | "cloudflare_\(.key)=\"\(.value)\""' > terraform.tfvars
