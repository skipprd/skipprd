---
name: skippr-perf-db
description: >-
  Run the skipprd ingest-then-query performance matrix: File parquet Iceberg
  versus DuckDB, plus WAL ∪ file Iceberg UNION. Smoke correctness and timings
  on this Mac first, then dispatch perf-db.yml on GitHub-hosted Linux x86
  (ubuntu-latest), download flamegraphs/heaptrack/perf-stat, generate ideas,
  iterate until ideas are exhausted, and email a plain-English memo to
  paul@paulhudson.me after each round. Use when the user says /skippr-perf-db,
  skippr-perf-db, or asks for WAL + Iceberg query flamegraphs versus DuckDB.
disable-model-invocation: true
---

# skippr-perf-db

Opt-in ingest→query matrix. **Correctness first**, then timings, `--log perf` /
`skipprd::perf` events, flamegraphs, heaptrack, and perf-stat.

Comparison is **file parquet Iceberg** (SkipprLake `object_store: type: file`)
against **DuckDB Iceberg**. The WAL profile target is skipprd `query` over
**WAL ∪ file Iceberg UNION** after `sync --once` (compact + reclaim). Every
query cell uses the DynamoDB Local Skippr Store, so a query process can pin
WAL while a held ingest owns the writer lease. `query_pin` reports the frozen
`(S, E, W)`: `wal_pin: drained` cells assert `W \ E` was empty (lake only);
`wal_pin: live` cells (STREAM tail, overlap) assert the query read held WAL.

Held cells write `hold_before_compaction_sink` to the local pipeline root
`{DATA_DIR}/{workspace}_{pipeline}/failpoint`. If `sync --once` exits before
the measured query, the hold never engaged and the cell fails.

Fail the cell when:

- required `PERF_EVENT` phases are missing (`sync`, `query_collect`, `query_pin`)
- `count(*)` does not equal ingest rows
- point query does not return id `1`
- a `query_name` cell returns anything but exactly one row
- `wal_pin` does not match the last `query_pin`
- the compaction hold never engaged
- the warehouse has no `*.parquet` after compact

Wrap scripts read `SKIPPR_LOG_LEVEL` through `skipprd_cli.log_args()` into
`--log`. Do **not** run skipprd-docs. Do **not** publish PyPI. Do **not**
consume CHANGELOG Unreleased. Do **not** bump plugin crates. Do **not** write
CloudCycle EU buckets. `aws s3 sync --delete` is illegal.

This is **not** skippr-perf (ingest/copy). Cells always `sync --once` then
`skipprd query --sql --plain --warmup`. `--warmup` starts QueryRuntime, opens
catalog connections, and runs the statement once; `query_collect` elapsed is
the **query time** of the second collect, not skipprd process startup. Jsonl
File sink query cells are out of suite.
WAL leftover import, Snowflake, BigQuery, Databricks, Redshift,
Athena Glue, and production JSON are out of suite. Postgres CDC query cells
are in this matrix (`query_after_cdc`); skip locally if Docker is down, fail
closed on GitHub. `sync --once` for those cells is wrapped with a 120s KILL.

Repo: `skipprd/skipprd`. Reuse `tests/perf/download.py --workflow perf-db.yml`,
`tests/perf/analyze.py`, and `tests/perf/memo.py`. Do not copy those files.

Mail: `python3 tests/perf/memo.py --mail` → sibling `cloud/scripts/send-afk-mail.py`
(Gmail AFK, `paul@paulhudson.me`). MUST NOT Amazon SES.

## Loop (do not skip)

```
skippr-perf-db:
- [ ] Local smoke A (q-wal-count-iceberg, q-wal-point-iceberg) and B (q-union-count-duckdb, q-union-overlap-duckdb); CDC smoke when Docker is up
- [ ] PERF logs parsed; query_collect present; counts match ingest rows; point returns id 1; warehouse has parquet
- [ ] Commit harness/skill with hooks (never --no-verify)
- [ ] gh workflow run perf-db.yml --ref this branch
- [ ] Watch until Linux x86 cells finished (matrix + flamegraph/heaptrack/perf-stat). Cancel if skipprd has no timeout evidence.
- [ ] python3 tests/perf/download.py --workflow perf-db.yml --out artifacts/skippr-perf-db
- [ ] python3 tests/perf/analyze.py --artifacts artifacts/skippr-perf-db --base origin/main
- [ ] python3 tests/perf/memo.py --iteration N --artifacts … --bottlenecks … --run-url … --done … --mail
- [ ] Generate ideas from timings, failed cells, and hot leaves (overall first; new code if it is actually hot)
- [ ] Implement the top remaining idea. Local smoke again if product code changed.
- [ ] Commit (hooks), dispatch, watch, download, analyze, email, iterate
- [ ] Stop when ideas are exhausted
```

Exhausted means: no remaining idea with a clear test, **or** `q-wal-count-iceberg`
and `q-union-count-duckdb` query `elapsed_ms` moved under 10% on two consecutive
GitHub runs after the last product change **and** no cell is still broken. Cap
**5** GitHub dispatches unless a skipprd frame is still over 10% of those two
cells' samples.

Each memo is a short plain-English note to `paul@paulhudson.me`: what the
numbers said, what broke, what changed, what is next. No tables. No markdown
fences.

## Local smoke (this Mac, before GitHub)

SkipprLake cells need DynamoDB Local (Docker). DuckDB cells need the DuckDB CLI.
Release host + only the plugins the cell needs (`USE_LOCAL_PLUGIN_CODE=1`):

```bash
cargo build --release -p skipprd --bin skipprd --features offset-store-dynamodb
cargo build --release \
  -p skippr-plugin-data-source-file \
  -p skippr-plugin-data-sink-duckdb \
  -p skippr-plugin-schema-sink-duckdb \
  -p skippr-plugin-data-sink-skipprlake \
  -p skippr-plugin-schema-sink-skipprlake
export SKIPPRD_BIN="$PWD/target/release/skipprd"
python3 tests/perf_db/run.py \
  --scenario q-wal-count-iceberg,q-wal-point-iceberg,q-union-count-duckdb --scale smoke
```

Fail the cell if required `PERF_EVENT` lines are missing (`sync`, `query_collect`,
`query_pin`, and `query_reconcile` on CDC merge cells).

## GitHub

`.github/workflows/perf-db.yml` is **`workflow_dispatch` only**. Jobs run on
GitHub-hosted Linux x86 (`ubuntu-latest`). Profiling cells: flamegraph and
perf-stat on `q-wal-count-iceberg`, `q-wal-point-iceberg`, `q-union-count-duckdb`;
flamegraph on `q-cdc-union-update-pg`; heaptrack on the two count cells. Hottest frames must not be `admit_payload` /
whole-seg `read_to_end` after the catalog slice index lands. skipprd and wrap
scripts fail closed after 10 minutes.

```bash
gh workflow run perf-db.yml --ref "$(git branch --show-current)"
gh run watch --exit-status
python3 tests/perf/download.py --workflow perf-db.yml --out artifacts/skippr-perf-db
python3 tests/perf/analyze.py --artifacts artifacts/skippr-perf-db --base origin/main
python3 tests/perf/memo.py --iteration N --artifacts artifacts/skippr-perf-db \
  --bottlenecks bottlenecks.json --run-url "$RUN_URL" --done "…" --mail
```

Read `bottlenecks.json` / stdout: hottest frames in new code first **only if they
are actually hot**; otherwise overall hot leaves and failed cells. Download
`flamegraph.svg`, `perf.folded`, `perf-stat.txt`, `heaptrack*`, `perf-report.json`.

## Must not

- Compose or publish skippr-web / skipprd-docs
- Publish wheels to PyPI or write `latest` pointers
- Consume CHANGELOG Unreleased
- Bump plugin crate versions
- `aws s3 sync --delete`
- Write `cloudcycle-datalake-iceberg-*` or other EU production/staging buckets
- Treat WAL leftover import as a query cell
- Put skippr-perf ingest/copy Postgres CDC cells here (query_after_cdc is allowed)
- Query jsonl File sink (no Iceberg parquet)
- Amazon SES for the iteration memo (Gmail AFK only)
- Dual-catalog skippr-perf ingest/copy cells here
