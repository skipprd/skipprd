---
name: skippr-regression
description: >-
  Run the skipprd e2e correctness + performance suite over time: ingest/copy
  and ingest-then-query (Iceberg ∪ WAL). Smoke on this Mac first, then dispatch
  skippr-regression.yml on GitHub-hosted Linux x86, download flamegraphs,
  generate ideas, iterate, and email a plain-English memo to paul@paulhudson.me
  after each round. Use when the user says /skippr-regression, skippr-regression,
  /skippr-perf, skippr-perf, /skippr-perf-db, skippr-perf-db, or asks for
  ingest/copy or WAL + Iceberg query flamegraphs, PERF logs, or the
  correctness+performance matrix.
disable-model-invocation: true
---

# skippr-regression

Opt-in **correctness + performance** suite. Prove correctness first, then
timings, `--log perf` / `skipprd::perf` events, flamegraphs, heaptrack, and
perf-stat. Ingest/copy and Iceberg∪WAL query are the **same** regression
loop. One GitHub workflow (`skippr-regression.yml`); one skill.

Wrap scripts read `SKIPPR_LOG_LEVEL` through `skipprd_cli.log_args()` into
`--log`. Do **not** run skipprd-docs. Do **not** publish PyPI. Do **not**
consume CHANGELOG Unreleased. Do **not** bump plugin crates. Do **not** write
CloudCycle EU buckets. `aws s3 sync --delete` is illegal.

WAL leftover **import is out of suite**. Snowflake, BigQuery, Databricks,
Redshift, Athena Glue, live ads/CRM, and production JSON are out of suite —
do not fake them. Jsonl File sink query cells are out of suite.

Repo: `skipprd/skipprd`. In-process `skipprd benchmark` /
`scripts/benchmark-ingest-local.sh` stay decode-unit tools; this skill is the
product path: **release binary + local plugins + discover/sync/query**.

Mail: `python3 tests/perf/memo.py --mail` → sibling
`cloud/scripts/send-afk-mail.py` (Gmail AFK, `paul@paulhudson.me`). MUST NOT
Amazon SES. Reuse `tests/perf/download.py`, `tests/perf/analyze.py`, and
`tests/perf/memo.py`. Do not copy those files.

`/skippr-release` and `/skippr-beta-release` wait on this suite’s SHA
batteries: `skippr-regression.yml` `linux_matrix` (not frame-pointer /
inferno / flamegraph / heaptrack / perf-stat). TPC-H SF10 is `/skippr-tpc`
and does not gate the tag.

## Loop (do not skip)

```
skippr-regression:
- [ ] Local smoke ingest/copy A+B (File ingest exact-Arrow → Duckdb or SkipprLake), B (File copy json→json), JSON File → Parquet File ingest, Skipprd CSV → DuckDB, DuckDB NDJSON/CSV baselines, File and Stdin ingest for every transform.compute filter/derive/compose cell and every CREATE TABLE measure, and File ingest then migrate on that data then continue ingest for every ALTER (historical compact and in-stream WAL hold)
- [ ] Local smoke query A (q-wal-count-iceberg, q-wal-point-iceberg) and B (q-union-count-duckdb, q-union-overlap-duckdb); SQL compose / CREATE TABLE / migrate historical (DuckDB drained) and stream (SkipprLake STREAM + live WAL pin) with continue ingest after ALTER then query; CDC SnapshotThenLogExactOnce for Postgres, MySQL, and DynamoDB when Docker is up (count / update / late-delete / reinsert / sum / overlap / iceberg-only) plus ALTER on existing CDC data then continue CDC then query (`q-cdc-union-update-pg`, `q-cdc-union-update-mysql`, `q-cdc-union-update-ddb`, `q-cdc-migrate-rename-pg`)
- [ ] PERF logs parsed; exact_arrow_rows == total on ingest-A; identity on copy-B; query_collect present; counts match ingest rows; point returns id 1; warehouse has parquet
- [ ] Commit harness/skill with hooks (never --no-verify)
- [ ] gh workflow run skippr-regression.yml --ref this branch
- [ ] Watch until Linux x86 cells finished (matrix + flamegraph/heaptrack/perf-stat). Do not wait 180m on a hung cell — cancel if skipprd has no timeout evidence.
- [ ] python3 tests/perf/download.py (flamegraphs, folded stacks, perf-stat, reports)
- [ ] python3 tests/perf/analyze.py --artifacts artifacts/skippr-regression --base origin/main
- [ ] python3 tests/perf/memo.py --iteration N --artifacts … --bottlenecks … --run-url … --done … --mail (once per workflow run)
- [ ] Generate ideas from timings, failed cells, and hot leaves (overall first; new code if it is actually hot)
- [ ] Implement the top remaining idea. Local smoke again if product code changed.
- [ ] Commit (hooks), dispatch skippr-regression.yml, watch, download, analyze, email, iterate
- [ ] Stop when ideas are exhausted
```

Exhausted means: no remaining idea with a clear test, **or** ingest-A and
copy-B `elapsed_ms` **and** `q-wal-count-iceberg` / `q-union-count-duckdb`
query `elapsed_ms` moved under 10% on two consecutive GitHub runs after the
last product change **and** no cell is still broken. Cap **5** GitHub
dispatches unless a skipprd frame is still over 10% of those
cells’ samples.

Each memo is a short plain-English note to `paul@paulhudson.me`: what the
numbers said, what broke, what changed, what is next. No tables. No markdown
fences.

## Ingest / copy

Postgres CDC is out of this matrix until `sync --once` actually exits
(release e2e still has it).

### Local smoke (this Mac, before GitHub)

Release host + only the plugins the cell needs (`USE_LOCAL_PLUGIN_CODE=1`):

```bash
cargo build --release -p skipprd --bin skipprd \
  -p skippr-plugin-data-source-file \
  -p skippr-plugin-data-source-stdin \
  -p skippr-plugin-data-sink-file \
  -p skippr-plugin-data-sink-duckdb \
  -p skippr-plugin-schema-sink-duckdb
export SKIPPRD_BIN="$PWD/target/release/skipprd"
python3 tests/perf/run.py --scenario ingest-a-duckdb,copy-b-json-json,ingest-json-parquet,ingest-csv-duckdb,duckdb-ndjson,duckdb-csv,duckdb-json-array,duckdb-insert-values,ingest-ops-duckdb,ingest-ops-filter-duckdb,ingest-ops-compose-stream-duckdb,ingest-aggregates-duckdb,ingest-aggregates-stream-duckdb,ingest-migrate-rename-duckdb,ingest-migrate-rename-stream-duckdb --scale smoke
```

Duckdb Iceberg is enough for A on a laptop. SkipprLake needs DynamoDB Local
(Docker or Java 17+). Fail the cell if required `PERF_EVENT` lines are
missing. DuckDB baseline cells generate NDJSON/CSV/JSON-array/INSERT SQL on
the fly (no checked-in fixtures). NDJSON and CSV COPY are DuckDB's fast
ingest paths; a nested JSON array and per-row INSERT VALUES are the slow
ones. Compare those to skipprd File JSON → DuckDB (`ingest-a-duckdb`) and
File CSV → DuckDB (`ingest-csv-duckdb`).

### GitHub

`.github/workflows/skippr-regression.yml` is **`workflow_dispatch` only** (not every
engine tag). One host build and one frame-pointer build serve ingest/copy and
query cells. GitHub jobs run on GitHub-hosted Linux x86 (`ubuntu-latest`) —
not the Mac self-hosted runner. Local smoke stays on this Mac. Profiling
cells on that same Linux runner: flamegraph (ingest-A, copy-B,
json→parquet, flatten, SQL compose File and Stdin, CREATE TABLE File and
Stdin, migrate rename historical and in-stream), heaptrack (ingest-A,
copy-B, json→parquet), perf-stat (ingest-A, copy-B, json→parquet).
`linux_matrix` still runs every catalog cell: each `transform.compute`
ingest cell (`filter`, `derive`, `compose`) and each CREATE TABLE measure
(`sum`, `count`, `both`) on File (historical) and Stdin (stream), plus each
ALTER (`rename`, `merge`, `drop`, `promote`) on already-ingested File data
then continue ingest (historical compact and in-stream WAL hold). The json→parquet cell is File ingest of a stable schema
(including RFC3339 timestamps) into File parquet. Query cells are `q-*` on
that same matrix. `linux_analyze` ranks
`perf.folded` leaves against `origin/main...HEAD` new code. One runner
label, not a second harness. skipprd and wrap scripts fail closed after 10
minutes (`COMMAND_TIMEOUT_SECONDS`); kill the process group.

```bash
gh workflow run skippr-regression.yml --ref "$(git branch --show-current)"
gh run watch --exit-status
python3 tests/perf/download.py
python3 tests/perf/analyze.py --artifacts artifacts/skippr-regression --base origin/main
python3 tests/perf/memo.py --iteration N --artifacts artifacts/skippr-regression \
  --bottlenecks bottlenecks.json --run-url "$RUN_URL" --done "…" --mail
```

Read `bottlenecks.json` / stdout: hottest frames in new code first **only if
they are actually hot**; otherwise overall hot leaves and failed cells.
Download `flamegraph.svg`, `perf.folded`, `perf-stat.txt`,
`perf-report.json`. Heaptrack on ingest-A/copy-B Linux is this harness
(`.github/actions/e2e/heaptrack`), not bike_hire.

## Query / Iceberg ∪ WAL

Ingest→query. Comparison is **file parquet Iceberg** (SkipprLake
`object_store: type: file`) against **DuckDB Iceberg**. The WAL profile
target is skipprd `query` over **WAL ∪ file Iceberg UNION** after
`sync --once` (compact + reclaim). Every query cell uses local fjall Skippr
Store, so a query process can pin WAL while ingest holds the local
`CatalogFence`. `query_pin` reports the frozen `(S, E, W)`: `wal_pin:
drained` cells assert `W \ E` was empty (lake only); `wal_pin: live` cells
(STREAM tail, overlap) assert the query read held WAL.

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
- continue ingest after migrate does not land the second batch (File count is not 2× ingest rows; CDC mutation after ALTER is missing)

Query cells always `sync --once` then `skipprd query --sql --plain --warmup`.
`--warmup` starts QueryRuntime, opens catalog connections, and runs the
statement once; `query_collect` elapsed is the **query time** of the second
collect, not skipprd process startup.

CDC query cells are in this matrix (`query_after_cdc`) for every
GitHub-local SnapshotThenLogExactOnce source: Postgres, MySQL, DynamoDB
(count, update, late-delete, reinsert, sum, overlap, iceberg-only). ALTER
cells migrate an already-ingested File lake or CDC lake, continue ingest /
CDC, then query. Skip CDC locally if Docker is down, fail closed on GitHub.
`sync --once` for CDC cells is wrapped with a 120s KILL. Do not put
ingest/copy CDC cells on the ingest half of `linux_matrix` (query_after_cdc is the suite home).
Dual-catalog ingest/copy cells do not belong on the query half.

### Local smoke (this Mac, before GitHub)

SkipprLake cells need DynamoDB Local (Docker). DuckDB cells need the DuckDB
CLI. Release host + only the plugins the cell needs
(`USE_LOCAL_PLUGIN_CODE=1`):

```bash
cargo build --release -p skipprd --bin skipprd --features iceberg-catalog-dynamodb
cargo build --release \
  -p skippr-plugin-data-source-file \
  -p skippr-plugin-data-source-postgres \
  -p skippr-plugin-data-source-mysql \
  -p skippr-plugin-data-source-dynamodb \
  -p skippr-plugin-data-sink-duckdb \
  -p skippr-plugin-schema-sink-duckdb \
  -p skippr-plugin-data-sink-skipprlake \
  -p skippr-plugin-schema-sink-skipprlake
export SKIPPRD_BIN="$PWD/target/release/skipprd"
python3 tests/perf_db/run.py \
  --scenario q-wal-count-iceberg,q-wal-point-iceberg,q-union-count-duckdb,q-ops-count-duckdb,q-aggregates-count-duckdb,q-migrate-rename-duckdb,q-cdc-union-update-pg,q-cdc-union-update-mysql,q-cdc-union-update-ddb,q-cdc-migrate-rename-pg --scale smoke
```

Fail the cell if required `PERF_EVENT` lines are missing (`sync`,
`query_collect`, `query_pin`, and `query_reconcile` on CDC merge cells).

### GitHub

The same `skippr-regression.yml` `linux_matrix` and `linux_profile` jobs run
query cells (`q-*`). Profiling cells: flamegraph and
perf-stat on `q-wal-count-iceberg`, `q-wal-point-iceberg`,
`q-union-count-duckdb`; flamegraph on `q-cdc-union-update-pg`,
`q-ops-count-duckdb`, `q-ops-stream-iceberg`, `q-migrate-rename-duckdb`,
`q-migrate-rename-stream-iceberg`; heaptrack on the two count cells.
`linux_matrix` also queries after SQL compose, CREATE TABLE both-measures,
every File ALTER (`rename`, `merge`, `drop`, `promote`) on historical
(drained DuckDB) and stream (SkipprLake STREAM with a live WAL pin) after
continue ingest, and every CDC cell above including ALTER-then-continue-CDC
for Postgres, MySQL, and DynamoDB. Hottest frames must not be
`admit_payload` / whole-seg `read_to_end` after the catalog slice index
lands. skipprd and wrap scripts fail closed after 10 minutes.

Read `bottlenecks.json` / stdout: hottest frames in new code first **only if
they are actually hot**; otherwise overall hot leaves and failed cells.
Download `flamegraph.svg`, `perf.folded`, `perf-stat.txt`, `heaptrack*`,
`perf-report.json`.

## Must not

- Compose or publish skippr-web / skipprd-docs
- Publish wheels to PyPI or write `latest` pointers
- Consume CHANGELOG Unreleased
- Bump plugin crate versions
- `aws s3 sync --delete`
- Write `cloudcycle-datalake-iceberg-*` or other EU production/staging buckets
- Treat WAL leftover import as a perf or query cell
- Put CDC ingest/copy cells on the ingest matrix until `sync --once` exits (CDC lives on query_after_cdc `q-*` cells)
- Query jsonl File sink (no Iceberg parquet)
- Amazon SES for the iteration memo (Gmail AFK only)
