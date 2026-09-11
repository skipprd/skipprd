# skipprd Cloud Deploy Runner

Terraform is the bind for Skippr Cloud Runner: GitHub App installation,
`skipprd/skipprd-private` (pools `skipprd-linux-x64-16` and
`skipprd-darwin-arm64-8`), `skipprd/cloud` (pools `cloud-linux-x64-{4,8,16}`),
and public OSS repos (`skipprd`, `sde`, `react`, `skippr-ide`, `skipprdb`,
`homebrew-tap`, `vitepress-theme`) each with `oss-{name}-linux-x64-16` and
`oss-{name}-darwin-arm64-8` pools. Cluster leftover slots are shared; there is no pool concurrency cap.
Linux jobs boot a fresh `skippr-ubuntu-24.04-x64` image. Darwin jobs bind a
registered RunnerHost. `linux_x64_32` is not a Preview cattle pool.
Bake exclusivity is GitHub Actions `concurrency` on `cloud.yml`. Do not
Create* via OTP curl as the lasting path.

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
# skipprd/cloud (after first apply, or import if already created)
# terraform import cloud_deploy_runner_repository_binding.cloud 1311314317
# terraform import 'cloud_deploy_runner_pool.cloud["16"]' cloud-linux-x64-16
terraform plan
terraform apply
```

If state still has the singleton `cloud_deploy_runner_pool.cloud_linux_x64_16`,
move it before apply:

```bash
terraform state mv \
  cloud_deploy_runner_pool.cloud_linux_x64_16 \
  'cloud_deploy_runner_pool.cloud["16"]'
```

Webhook TLS for `https://deploy.eu-central-1.cloud.skippr.io` is a platform
certificate SAN, not this stack. The GitHub App webhook URL MUST be that
regional hostname — regionless `deploy.cloud.skippr.io` is not a product name.
