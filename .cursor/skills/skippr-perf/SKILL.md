---
name: skippr-perf
description: >-
  Run the skipprd e2e correctness + performance matrix on a production host
  build with local runtime plugins. Smoke A+B on this Mac first, then dispatch
  perf.yml on GitHub-hosted Linux x86 (ubuntu-latest), download flamegraphs, and rank bottlenecks in new code. Use when the user says /skippr-perf,
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
- [ ] gh workflow run perf.yml --ref this branch
- [ ] Watch until Linux x86 cells succeeded (matrix + flamegraph/heaptrack/perf-stat)
- [ ] python3 tests/perf/download.py (flamegraphs, folded stacks, perf-stat, reports)
- [ ] python3 tests/perf/analyze.py --artifacts artifacts/skippr-perf --base origin/main
- [ ] Fix hottest frames in new code first; do not publish PyPI/docs/latest
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

`.github/workflows/perf.yml` is **`workflow_dispatch` only** (not every engine tag). GitHub jobs run on GitHub-hosted Linux x86 (`ubuntu-latest`) — not the Mac self-hosted runner. Local smoke stays on this Mac. Profiling cells on that same Linux runner: flamegraph (ingest-A, copy-B, flatten), heaptrack (ingest-A, copy-B), perf-stat (ingest-A, copy-B). `linux_analyze` ranks `perf.folded` leaves against `origin/main...HEAD` new code. One runner label, not a second harness.

```bash
gh workflow run perf.yml --ref "$(git branch --show-current)"
gh run watch --exit-status
python3 tests/perf/download.py
python3 tests/perf/analyze.py --artifacts artifacts/skippr-perf --base origin/main
```

Read `bottlenecks.json` / stdout: hottest frames in new code first, then overall hot leaves. Download `flamegraph.svg`, `perf.folded`, `perf-stat.txt`, `perf-report.json`. Heaptrack on ingest-A/copy-B Linux is this harness (`.github/actions/e2e/heaptrack`), not bike_hire.

## Must not

- Compose or publish skippr-web / skipprd-docs
- Publish wheels to PyPI or write `latest` pointers
- Consume CHANGELOG Unreleased
- Bump plugin crate versions
- `aws s3 sync --delete`
- Write `cloudcycle-datalake-iceberg-*` or other EU production/staging buckets
- Treat WAL leftover import as a perf cell
