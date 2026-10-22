---
name: skippr-perf
description: >-
  Run the skipprd e2e correctness + performance matrix on a production host
  build with local runtime plugins. Smoke A+B on this Mac first, then dispatch
  perf.yml on skippr-darwin-arm64. Use when the user says /skippr-perf,
  skippr-perf, or asks for ingest/copy flamegraphs or PERF logs.
disable-model-invocation: true
---

# skippr-perf

Opt-in performance matrix. Prove **correctness first**, then record timings, `--log perf` / `skipprd::perf` events, and flamegraphs for ingest-A and copy-B. Wrap scripts read `SKIPPR_LOG_LEVEL` through `skipprd_cli.log_args()` into `--log`. Do **not** run skipprd-docs. Do **not** publish PyPI. Do **not** consume CHANGELOG Unreleased. Do **not** bump plugin crates. Do **not** write CloudCycle EU buckets. `aws s3 sync --delete` is illegal.

WAL leftover **import is out of suite**. Snowflake, BigQuery, Databricks, Redshift, Athena Glue, live ads/CRM, and production JSON are out of suite — do not fake them.

Repo: `skipprd/skipprd`. In-process `skipprd benchmark` / `scripts/benchmark-ingest-local.sh` stay decode-unit tools; this skill is the product path: **release binary + local plugins + discover/sync/query**.

## Loop (do not skip)

```
skippr-perf:
- [ ] Local smoke A (File ingest exact-Arrow → Duckdb or SkipprLake) and B (File copy json→json)
- [ ] PERF logs parsed; exact_arrow_rows == total on A; identity on B
- [ ] Commit harness/skill with hooks (never --no-verify)
- [ ] gh workflow dispatch perf.yml on this branch
- [ ] Watch until all cells succeeded; download flamegraphs + perf-report.json
- [ ] Fix + iterate; do not publish PyPI/docs/latest
```

## Local smoke (this Mac, before GitHub)

Release host + only the plugins the cell needs (`USE_LOCAL_PLUGIN_CODE=1`):

```bash
cargo build --release -p skipprd --bin skipprd \
  -p skippr-plugin-data-source-file \
  -p skippr-plugin-data-sink-file \
  -p skippr-plugin-data-sink-duckdb \
  -p skippr-plugin-schema-sink-duckdb
export SKIPPRD_BIN="$PWD/target/release/skipprd"
python3 tests/perf/run.py --scenario ingest-a-duckdb,copy-b-json-json --scale smoke
```

Duckdb Iceberg is enough for A on a laptop. SkipprLake needs DynamoDB Local (Docker or Java 17+). Fail the cell if required `PERF_EVENT` lines are missing.

## GitHub

`.github/workflows/perf.yml` is **`workflow_dispatch` only** (not every engine tag). Full matrix: `runs-on: [self-hosted, skippr-darwin-arm64]`. Linux flamegraph/heaptrack/perf: `ubuntu-latest` with smaller N. If a `skippr-linux-x86` self-hosted label appears later, point the Linux job there in the same workflow — one runner label, not a second harness.

```bash
gh workflow dispatch perf.yml --ref "$(git branch --show-current)"
gh run watch --exit-status
```

Download `perf-report.json`, `perf.log`, and flamegraph SVG artifacts. Heaptrack on ingest-A Linux is this harness (`.github/actions/e2e/heaptrack`), not bike_hire.

## Must not

- Compose or publish skippr-web / skipprd-docs
- Publish wheels to PyPI or write `latest` pointers
- Consume CHANGELOG Unreleased
- Bump plugin crate versions
- `aws s3 sync --delete`
- Write `cloudcycle-datalake-iceberg-*` or other EU production/staging buckets
- Treat WAL leftover import as a perf cell
