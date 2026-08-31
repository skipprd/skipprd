# skipprd Cloud Deploy Runner

Terraform is the bind for Skippr Cloud Runner: GitHub App installation,
`skipprd/skipprd-private`, and pool `skipprd-linux-x64-16`. Do not Create* via
OTP curl as the lasting path.

Provider: `skippr/cloud` (Preview). Use a directory-issued operator SigV4 key
(`CLOUD_OPERATOR_ACCESS_KEY_ID` / `CLOUD_OPERATOR_SECRET_ACCESS_KEY`). Unset
`CLOUD_BEARER_TOKEN` so the provider does not prefer a leftover JWT.

```bash
export CLOUD_ROOT=/path/to/skippr/cloud
export TF_CLI_CONFIG_FILE="$(mktemp)"
cat >"$TF_CLI_CONFIG_FILE" <<EOF
provider_installation {
  dev_overrides {
    "skippr/cloud" = "$CLOUD_ROOT/providers/terraform"
  }
  direct {}
}
EOF
(cd "$CLOUD_ROOT/providers/terraform" && go build -o terraform-provider-cloud .)

unset CLOUD_BEARER_TOKEN CLOUD_ACCESS_TOKEN
terraform init -backend=false
terraform import cloud_deploy_runner_github_installation.app 156978374
terraform import cloud_deploy_runner_repository_binding.app 678245671
terraform import cloud_deploy_runner_pool.linux_x64_16 skipprd-linux-x64-16
terraform plan
terraform apply
```

Webhook TLS for `https://deploy.eu-central-1.cloud.skippr.io` is a platform
certificate SAN, not this stack.
