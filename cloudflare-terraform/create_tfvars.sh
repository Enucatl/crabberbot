vault kv get -format=json -mount=secret cloudflare | jq -r '.data.data | to_entries[] | "\(.key)=\"\(.value)\""' | tee terraform.tfvars
