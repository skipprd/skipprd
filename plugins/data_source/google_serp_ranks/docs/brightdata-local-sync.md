# Local Bright Data SERP sync (Up Foundry datalake)

```bash
export AWS_PROFILE=skippr-prod
export BRIGHTDATA_API_KEY="<from Secrets Manager upfoundry/brightdata>"
export BRIGHTDATA_ZONE=serp_api1

TENANT_ID="76504ed9-9d6b-415e-812d-fd74cfc93244"
DOMAIN="example.com"  # target site for rank check
KEYWORD="pizza"

# Build plugin (macOS)
cd skipprd
cargo build --release -p skippr-plugin-data-source-google-serp-ranks

# Generate runtime manifest
mkdir -p /tmp/upfoundry-serp-test/manifests
# … write skippr.yml with picnic_google_serp_ranks pipeline …
python3 .github/scripts/local_runtime_plugins.py \
  --config /tmp/upfoundry-serp-test/skippr.yml \
  --pipeline picnic_google_serp_ranks \
  --output-dir /tmp/upfoundry-serp-test/manifests \
  --release

export SKIPPR_CONFIG_FILE=/tmp/upfoundry-serp-test/skippr.yml
export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR=/tmp/upfoundry-serp-test/manifests
export USE_LOCAL_PLUGIN_CODE=1
export WORKSPACE_NAME="${TENANT_ID}-upfoundry"
export SKIPPR_S3_BUCKET=upfoundry-prod-datalake

skipprd sync --once --pipeline picnic_google_serp_ranks
```

Query Athena:

```sql
SELECT * FROM upfoundry_76504ed9_9d6b_415e_812d_fd74cfc93244.google_serp_ranks_target_rank_daily
ORDER BY run_date DESC LIMIT 10;
```
