---
name: skippr-perf-db
description: >-
  Run the skipprd ingest-then-query performance matrix on a production host
  build with local runtime plugins. Smoke WAL count/point and DuckDB UNION
  count on this Mac first, then dispatch perf-db.yml on GitHub-hosted Linux
  x86 (ubuntu-latest), download flamegraphs, generate ideas from the results,
  iterate until ideas are exhausted, and email a plain-English memo to
  paul@paulhudson.me after each round. Use when the user says /skippr-perf-db,
  skippr-perf-db, or asks for query/WAL/Iceberg UNION flamegraphs.
disable-model-invocation: true
---

# skippr-perf-db

Opt-in ingest→query matrix. Prove **correctness first**, then record timings,
`--log perf` / `skipprd::perf` events, and flamegraphs for `q-wal-count-file`,
`q-wal-point-file`, and `q-union-count-duckdb`. Wrap scripts read
`SKIPPR_LOG_LEVEL` through `skipprd_cli.log_args()` into `--log`. Do **not** run skipprd-docs.
Do **not** publish PyPI. Do **not** consume CHANGELOG
Unreleased. Do **not** bump plugin crates. Do **not** write CloudCycle EU
buckets. `aws s3 sync --delete` is illegal.

This is **not** skippr-perf (ingest/copy). Cells always `sync --once` then
`skipprd query --sql --plain`. WAL leftover import, Postgres CDC, Snowflake,
BigQuery, Databricks, Redshift, Athena Glue, and production JSON are out of
suite.

Repo: `skipprd/skipprd`. Reuse `tests/perf/download.py --workflow perf-db.yml`,
`tests/perf/analyze.py`, and `tests/perf/memo.py`. Do not copy those files.

Mail: `python3 tests/perf/memo.py --mail` → sibling `cloud/scripts/send-afk-mail.py`
(Gmail AFK, `paul@paulhudson.me`). MUST NOT Amazon SES.

## Loop (do not skip)

```
skippr-perf-db:
- [ ] Local smoke A (q-wal-count-file, q-wal-point-file) and B (q-union-count-duckdb)
- [ ] PERF logs parsed; query_collect present; counts match ingest rows; point returns id 1
- [ ] Commit harness/skill with hooks (never --no-verify)
- [ ] gh workflow run perf-db.yml --ref this branch
- [ ] Watch until Linux x86 cells finished. Cancel if skipprd has no timeout evidence.
- [ ] python3 tests/perf/download.py --workflow perf-db.yml --out artifacts/skippr-perf-db
- [ ] python3 tests/perf/analyze.py --artifacts artifacts/skippr-perf-db --base origin/main
- [ ] python3 tests/perf/memo.py --iteration N --artifacts … --bottlenecks … --run-url … --done … --mail
- [ ] Generate ideas from timings, failed cells, and hot leaves (overall first; new code if it is actually hot)
- [ ] Implement the top remaining idea. Local smoke again if product code changed.
- [ ] Commit (hooks), dispatch, watch, download, analyze, email, iterate
- [ ] Stop when ideas are exhausted
```

Exhausted means: no remaining idea with a clear test, **or** `q-wal-point-file`
and `q-union-count-duckdb` query `elapsed_ms` moved under 10% on two consecutive
GitHub runs after the last product change **and** no cell is still broken. Cap
**5** GitHub dispatches unless a skipprd frame is still over 10% of those two
cells' samples.

Each memo is a short plain-English note to `paul@paulhudson.me`: what the
numbers said, what broke, what changed, what is next. No tables. No markdown
fences.

## Local smoke (this Mac, before GitHub)

```bash
cargo build --release -p skipprd --bin skipprd \
  -p skippr-plugin-data-source-file \
  -p skippr-plugin-data-sink-file \
  -p skippr-plugin-data-sink-duckdb \
  -p skippr-plugin-schema-sink-duckdb
export SKIPPRD_BIN="$PWD/target/release/skipprd"
python3 tests/perf_db/run.py \
  --scenario q-wal-count-file,q-wal-point-file,q-union-count-duckdb --scale smoke
```

Fail the cell if required `PERF_EVENT` lines are missing (`sync`, `query_collect`).

## GitHub

`.github/workflows/perf-db.yml` is **`workflow_dispatch` only**. Jobs run on
GitHub-hosted Linux x86 (`ubuntu-latest`). Profiling cells: flamegraph and
perf-stat on `q-wal-count-file`, `q-wal-point-file`, `q-union-count-duckdb`.
Hottest frames must not be `admit_payload` / whole-seg `read_to_end` after the
catalog slice index lands. skipprd and wrap scripts fail closed after 10 minutes.

```bash
gh workflow run perf-db.yml --ref "$(git branch --show-current)"
gh run watch --exit-status
python3 tests/perf/download.py --workflow perf-db.yml --out artifacts/skippr-perf-db
python3 tests/perf/analyze.py --artifacts artifacts/skippr-perf-db --base origin/main
python3 tests/perf/memo.py --iteration N --artifacts artifacts/skippr-perf-db \
  --bottlenecks bottlenecks.json --run-url "$RUN_URL" --done "…" --mail
```

## Must not

- Compose or publish skippr-web / skipprd-docs
- Publish wheels to PyPI or write `latest` pointers
- Consume CHANGELOG Unreleased
- Bump plugin crate versions
- `aws s3 sync --delete`
- Write `cloudcycle-datalake-iceberg-*` or other EU production/staging buckets
- Treat WAL leftover import as a query cell
- Put Postgres CDC in this matrix until `sync --once` exits
- Amazon SES for the iteration memo (Gmail AFK only)
- Dual-catalog skippr-perf ingest/copy cells here
