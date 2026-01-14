## DBT validation target and S3 layout

- Target adapter for validation/compile is `datafusion`. Ensure `DBT_PROFILES_DIR` (or `profiles_dir` in calls) provides a profile compatible with the `datafusion` target.
- DBT project files (ReAct-owned) are stored in S3 under `<tenant>/<workspace>/<project_id>/dbt/`.
  - `dbt_project.yml`
  - `models/schema.yml` (sources)
  - `models/<dataset_id>/stg_<dataset_id>.sql` (staging; `dataset_id` is encoded for safe S3 keys)
  - Compiled artifacts uploaded to `<tenant>/<workspace>/<project_id>/dbt/target/` after successful `dbt compile`/`dbt build`.
# Skippr

## OpenAPI schema-first (Ask WebSocket)

To generate Rust models from the Ask WebSocket OpenAPI schema and (optionally) wire updates:

1. Ensure the spec is present at `docs/openapi/ask-ws.yaml`.
2. Use the helper script to run OpenAPI Generator (Docker or local jar):

```bash
scripts/gen-openapi.sh
```

This will generate Rust models under `src/ws/api_gen/`. Only `components/schemas` are used for model generation. The WebSocket path exists for documentation. The server additionally enforces strict request validation and UUID v4 thread IDs.

## Start the WebSocket server (Ask API)

Run the server locally (default port 8787 shown; choose any open port):

```bash
cargo run -p react -- serve --port 8787 --log
```

Connect a WebSocket client to:

- `ws://localhost:8787/`

Send JSON frames matching `docs/openapi/ask-ws.yaml`. Example requests:

```json
{"type":"list"}
```

```json
{"type":"new","question":"What were total rides last week?"}
```

```json
{"type":"open","thread_id":"<uuid>","question":"Continue."}
```

```json
{"type":"user","thread_id":"<uuid>","text":"We rent e-bikes in NYC and care about weekend demand."}
```

Notes:
- No authentication is required (for now).
- The server strictly rejects unknown properties and uses UUID v4 thread IDs.
- Full schemas and examples are in `docs/openapi/ask-ws.yaml`.

### What is Skippr?

Skippr is a tool for data ingestion and transformation. It is designed to ingest data from a source and transform it into a destination datalake/warehouse.

### Logging
- By default, diagnostic logs are disabled; only user-facing output is printed.
- Enable logs with the global flag:
  - `--log` to print INFO-level logs (configurable via `RUST_LOG`, e.g., `RUST_LOG=debug`).
- Example:
```bash
cargo run -- --log sync
```

## CLI Commands

This repository is a Cargo workspace with two crates:

- `skippr`: ingest + plugins + `sqlrt` (DataFusion runtime)
- `react`: engine-agnostic ReAct runtime + Athena/Glue provider + WebSocket server

Use either a built binary or run via cargo:

```bash
cargo run -p skippr -- <command> [flags]
```

Global flags:
- `--log`: enable logging (INFO by default; override with `RUST_LOG=debug` etc.)

### discover
Discover schemas, build stats, and write catalog/semantic to S3 for a pipeline.
Automatically runs an embeddings sync (LanceDB on S3) at the end.

Flags:
- `-p, --pipeline <name>`: pipeline name (optional)
- `--log`: stream verbose logs while discovering

Example:
```bash
cargo run -- discover --pipeline picnic --log
```

### sync
Run the ingestion/sync loop for a pipeline (reads source → writes S3 parquet, stats, manifest).

Flags:
- `-p, --pipeline <name>`: pipeline name (optional)

Example:
```bash
AWS_PROFILE=circles-prod SKIPPR_S3_BUCKET=asgsdag-datalake \
cargo run -- sync --pipeline picnic
```

### query
Interactive SQL (DataFusion) over registered S3 Parquet + optional WAL; supports `pipeline.namespace` (ingest/sqlrt convention) and unqualified `namespace`.

Flags:
- `-s, --sql "<SQL>"`: one-shot SELECT to execute
- `--watch <seconds>`: auto-refresh during interactive run
- `--plain`: print CSV-like output to stdout (non-interactive)

Examples:
```bash
# Pretty TUI, single run
cargo run -- query --sql "SELECT * FROM picnic.track LIMIT 5"

# Plain stdout
cargo run -- query --plain --sql "SELECT event, COUNT(*) AS c FROM track GROUP BY event ORDER BY c DESC LIMIT 10"

# Interactive editor with watch
cargo run -- query --sql "SELECT COUNT(*) FROM picnic.track" --watch 5
```

Special tables:
- `deadletters`: query quarantined rows (if enabled and present).

### schema
Utility for inspecting a pipeline’s local output buffer schema using listing tables.

Flags:
- `-p, --pipeline <name>`: pipeline name (required)

Example:
```bash
cargo run -- schema --pipeline picnic
```

### sql-help
Show Skippr SQL extensions and examples, or export docs.

Flags:
- `-c, --command <name>`: filter to one command (optional)
- `-o, --output <path>`: write docs to a file (optional)
- `-f, --format <md|html|json>`: output format (default: md)

Examples:
```bash
cargo run -- sql-help
cargo run -- sql-help --command "SHOW STATS" --output docs/sql.md --format md
```

### benchmark
Generate synthetic inputs to evaluate ingest throughput/overheads.

Flags:
- `-f, --num-files <N>`: number of files
- `-r, --records-per-file <N>`
- `-s, --record-size <bytes>`
- `-n, --name <label>`: benchmark name (default: baseline)
- `-d, --description <text>`: freeform label

Example:
```bash
cargo run -- benchmark -f 100 -r 50000 -s 800 --name bigfiles --description "IO pipeline sanity"
```

### llm
LLM/ReAct entrypoints for chat, embeddings, cleansing, and modeling. Configure LLM via env (see Configuration).

Flags:
- `--chat "<prompt>"`: single chat turn with the configured chat model
- `--cleanse`: interactive ReAct cleansing (cross-namespace), with human approval; writes DBT model SQL
- `--model`: interactive ReAct modeling (cross-namespace), with human approval; after approval calls approve_and_save_artifact to write:
  - Models at `dbt/models/<namespace>/<name>.sql` (raw text)
  - MetricFlow at `dbt/metrics/<namespace>/<name>.yaml` (raw text)
  - Also appends versioned copies under `_versions/<name>/<timestamp>.*`
- `--embed "<text>"` (repeatable): embed one or more texts
- `--ask "<question>"`: SQL agent to answer dataset questions
- `--top_k <N>`: top-K rows/docs to consider (default: 5)

Notes:
- `ask` auto-discovers across pipelines; do not pass a pipeline for `ask`.
- Embeddings are stored in LanceDB on S3 (uses `SKIPPR_S3_BUCKET`).
- WebSocket API supports multiple agents per thread. Send `agentType` (ask | cleanse | model) on `new`/`open`. Default is `ask`. Switching agents keeps the same `thread_id` and records a `switch_agent` step.

Examples:
```bash
# One-off chat
LLM_PROVIDER=OPENAI LLM_API_KEY=sk-... \
cargo run -- llm --chat "Summarize the latest datasets."

# Cleanse suggestions for a namespace
cargo run -- llm --cleanse

# MetricFlow modeling
cargo run -- llm --model

# Ask questions of your data (no pipeline flag)
cargo run -- llm --ask "What were daily active users last week?"
```

## Product Roadmap

These are planned features; scope and sequence may evolve.

### Ingestion
- [x] Deadletter queues for failed records
  - [x] Capture and store failed records with error and metadata for debugging.
  - [x] SQL query access (DataFusion) to deadletter records.
  - [ ] Replay capabilities to reprocess after fixing issues.
  - [ ] Configurable retention and alerting.
- [ ] S3-backed offset database (replace sled)
  - [ ] Custom S3-backed KV store optimized for append-heavy writes and very fast reads.
  - [ ] Efficient batch key lookups; favor sequential ranges but resilient to slight shuffles.
- [x] TigerBeetle TigerStyle deterministic WAL segment commits
    - [x] Commit markers for each WAL segment and visibility gating.
    - [x] Commit offsets to DB after WAL commit marker durably persisted. (Already recover offsets via WAL index on start)
- [x] S3-backed WAL segments
    - [x] Refactor write-ahead log segments to configurable persists to S3 (default to local disk).
    - [x] Design for determinisium and consistency
    - [x] Update `query.rs` to be able to query the WAL from S3
- [x] Refacotor println to tracing
    - [x] Replace all `println!` calls with `tracing` macros for structured logging.
    - [x] Configure logging levels and formats via environment variables or config files.

### LLM ReAcT Integration
- [ ] LLM-driven data source exploration
  - [ ] Use LLMs to analyze and summarize unknown datasets.
  - [ ] Generate schema suggestions and data quality insights.

### Data Modeling and Querying
- [ ] External datasets (zero-copy)
  - [ ] Discover external tables/files in situ and query them without ingestion.
  - [ ] Support pushdown where possible; treat as first-class queryable sources.
- [ ] Datasets abstraction
  - [ ] Formalize `datasets`: internal (ingested/managed) and external (discovered/zero-copy).
  - [ ] Central metadata/catalog entries for schemas, partitions, retention, and ownership.
- [ ] Views and virtual views
  - [ ] Non-destructive, query-backed views for cleansing and data modeling.
  - [ ] Allow value-level transformations while reading from underlying sources.
- [ ] Field-level attribute-based access control (ABAC)
  - [ ] Enforce per-field policies on source data; propagate through views, transforms, and SQL.
  - [ ] Policy evaluation integrated into planning/pushdown phases.
- [ ] End-to-end data lineage
  - [ ] Track lineage from source data through views, transformations, and SQL queries.
- [ ] Source/dataset derivation lineage
  - [ ] Model which datasets are derived from which sources/datasets for provenance graphs.
- [ ] LLM-based document indexing
  - [ ] Index document and external datasets via tokenization/embeddings for semantic search.

### Datalake/Warehouse Outputs
 - [ ] Apache Iceberg tables
   - [ ] Replace existing Hive table format with Iceberg for ACID, schema evolution, and time travel.


### Project Structure

- `src/` - Source code for the Skippr CLI and library
- `data/` - Data directory for the Skippr CLI
- `target/` - Build output for the Skippr CLI

### Building the Project

#### Local Development Build
```bash
cargo run sync
```

#### Release Builds

For MacOS:
```bash
SDKROOT=$(xcrun -sdk macosx12.3 --show-sdk-path) \
MACOSX_DEPLOYMENT_TARGET=$(xcrun -sdk macosx12.3 --show-sdk-platform-version) \
cargo build --release --target=x86_64-apple-darwin
```

For Linux:
```bash
cargo build --target x86_64-unknown-linux-gnu --release
```

### Configuration

Skippr is configured through environment variables. Here are the key configuration options:

#### Data Source Configuration
- `DATA_SOURCE_PLUGIN_NAME` - Source plugin to use (e.g. 's3', 's3_inventory', 'stdin')
- `DATA_SOURCE_S3_BUCKET` - Source S3 bucket
- `DATA_SOURCE_S3_PREFIX` - Source S3 prefix
- `DATA_SOURCE_BATCH_SIZE_BYTES` - Batch size in bytes
- `DATA_SOURCE_EVENT_TYPE_FIELDS` - Fields to use for event type

#### Transform Configuration  
- `TRANSFORM_FLATTEN_EVENTS` - Whether to flatten nested JSON events
- `TRANSFORM_NAMESPACE_FIELDS` - Fields to use for namespacing
- `TRANSFORM_BATCH_TIME_FIELDS` - Fields to use for time-based partitioning
- `TRANSFORM_BATCH_TIME_UNIT` - Time unit for partitioning (day/year)

#### Output Configuration
- `DATA_OUTPUT_PLUGIN_NAME` - Output plugin to use (e.g. 'athena')
- `DATA_OUTPUT_S3_BUCKET` - Destination S3 bucket
- `DATA_OUTPUT_S3_PREFIX` - Destination S3 prefix
- `SCHEMA_OUTPUT_PLUGIN_NAME` - Schema output plugin (e.g. 'glue')
- `SCHEMA_OUTPUT_GLUE_DATABASE_NAME` - Glue database name
- `DATA_OUTPUT_ATHENA_WORKGROUP_NAME` - Athena workgroup name

#### JSON Parsing Configuration
- `SKIPPR_ENABLE_SINGLE_QUOTE_PARSING` - Enable parsing of JSON with single quotes (default: false)
- `SKIPPR_ENABLE_UNICODE_PARSING` - Enable parsing of Unicode prefixed strings like u'string' (default: false)

#### JSON Parser Performance
Skippr includes a high-performance JSON parser optimized for various data formats:
- Standard JSON processing: up to **165% faster** than conventional parsers
- Single quote JSON processing: up to **112% faster**
- Unicode marker processing: up to **133% faster**
- Concatenated JSON objects: up to **40% faster**
- Complex nested structures: **8-14% faster**

The optimized parser includes:
- Fast-path detection to skip unnecessary processing
- Minimized memory allocations
- Single-pass character processing
- Efficient handling of special formats
- Byte-level optimizations

### Data Type Detection

Skippr automatically detects data types from your input data. Here are some key behaviors to be aware of:

#### Timestamp Detection
- Integer values are considered timestamps only if they are 10-11 digits (seconds) or 13 digits (milliseconds)
- For security and data quality reasons, only timestamps after January 1, 2010 are recognized as valid timestamps
- This prevents small integers from being incorrectly identified as timestamps

### Example Usage

Basic S3 to Athena pipeline:
```bash
AWS_PROFILE=skippr-test \
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=skippr-e2e-sample-data \
DATA_SOURCE_S3_PREFIX=bike-hire \
DATA_SOURCE_BATCH_SIZE_BYTES=10048000 \
DATA_OUTPUT_PLUGIN_NAME=athena \
DATA_OUTPUT_S3_BUCKET=skippr-e2e-sample-data-output \
DATA_OUTPUT_S3_PREFIX=bikehire \
SCHEMA_OUTPUT_PLUGIN_NAME=glue \
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=bikehire \
DATA_OUTPUT_ATHENA_WORKGROUP_NAME=bikehire \
PIPELINE_NAME=bikehire \
SKIPPR_S3_BUCKET=your-skippr-bucket \
DATA_DIR=./data \
APP_ENV=dev \
cargo run sync
```

### Docker Usage

```bash
docker run --platform=linux/x86_64 \
-e AWS_DEFAULT_REGION=eu-west-1 \
-e AWS_ACCESS_KEY_ID=$AWS_ACCESS_KEY_ID \
-e AWS_SECRET_ACCESS_KEY=$AWS_SECRET_ACCESS_KEY \
-e DATA_SOURCE_PLUGIN_NAME=s3_inventory \
-e DATA_SOURCE_S3_BUCKET=source-bucket \
-e DATA_SOURCE_S3_PREFIX=/example/inventory-dir \
-e DATA_OUTPUT_S3_BUCKET=dest-bucket \
-e DATA_OUTPUT_S3_PREFIX=example \
-e SCHEMA_OUTPUT_GLUE_DATABASE_NAME=test123 \
-e DATA_OUTPUT_ATHENA_WORKGROUP_NAME=test123 \
-e PIPELINE_NAME=test123 \
-e SKIPPR_S3_BUCKET=your-skippr-bucket \
-e DATA_DIR=./ \
-e APP_ENV=test \
-v `pwd`/data:/data \
skippr/skipprd:v3.1.0
```

### LLMs

For local and remote LLM setup, auto-tuning, and model guidance, see `LLM.md`.

### DBT Validation (compile-first)

- Validation runs `dbt deps` → `dbt parse` → `dbt compile --target <target>`, with optional `dbt build` (preferred) or `dbt run`.
- Default target is `datafusion` if `DBT_TARGET` is not set.
- You can pass `profiles_dir`, `target`, `run` (bool), and `build` (bool) to the `dbt_validate` tool API.
- Global registration of DBT models only uses compiled outputs under `dbt/target/compiled/` discovered on S3.

# Performance Analysis

### mpstat

Analysis of Your CPU Efficiency (mpstat Output)
Your system's CPU utilization shows:

~64% user time (%usr) → Your program is consuming a significant portion of CPU.
~11-12% system time (%sys) → Kernel operations (syscalls, memory management, etc.).
~23-25% idle time (%idle) → Some CPU cycles are still available.
Very low %iowait (~0.05%) → Not bottlenecked by disk I/O.
Low %irq and %softirq → Not impacted by excessive interrupts.
Is Your Program Efficient?
✅ Yes, it's fairly efficient.
Here's why:

Good CPU utilization (~64%): Your program is making full use of CPU without overloading it.
Low I/O wait (~0.05%): The program is not stuck waiting for disk access, meaning it efficiently processes data in RAM.
Low IRQ & SoftIRQ: No excessive hardware/network interrupts, indicating that performance is CPU-bound rather than being slowed down by other hardware.
Potential Areas for Optimization
High %sys (11-12%)

If the workload is CPU-intensive but involves frequent syscalls (e.g., file operations, networking, memory allocations), consider reducing context switches or optimizing system calls.
Solution: Profile the system calls using:
bash
Copy
Edit
strace -c -p <pid_of_your_program>
If too many syscalls, batch operations together.
Moderate Idle Time (23-25%)

While this is normal, if your program should be fully using all CPU cores, check for possible inefficiencies in threading or parallelization.
Solution: If running multi-threaded, ensure workload is evenly distributed using htop or numactl --hardware.
Check for Load Balancing Across Cores

Your core usage is fairly uniform (good!), but slight variations in %idle indicate some cores might be underutilized.
Solution: Use CPU affinity (taskset) or thread pinning to distribute workload more evenly.
bash
Copy
Edit
taskset -c 0-7 your_program
Final Verdict:
Your program is well-optimized with no major inefficiencies. 🚀

### vmstat 1

Analysis of Your vmstat Output
Your system appears to be running under a heavy load but is handling it relatively well. Let's break down the key metrics.

1️⃣ CPU Usage (us, sy, id, wa)
User (us): ~60-70%
→ Your program is consuming a significant portion of CPU time. This suggests a CPU-bound workload (e.g., computations, processing).
System (sy): ~10-13%
→ Kernel activity is moderate, meaning system calls, memory management, and I/O operations are happening but not excessive.
Idle (id): ~18-29%
→ Some CPU cycles are still free, but the system is fairly busy.
I/O Wait (wa): 0% consistently
→ Excellent! Your CPU is not blocked by slow disk operations. Your program efficiently keeps things in memory.
✅ Conclusion:
Your program is efficiently utilizing CPU without excessive system overhead. If you'd like to increase CPU usage efficiency, consider threading optimization (if applicable).

2️⃣ Load and Process Activity (r, b)
Run queue (r): ~16-37 processes
→ High, meaning many processes are actively waiting for CPU time. If the r value is higher than your CPU core count, then processes are competing for CPU.
Blocked (b): 0
→ Good sign! No processes are stuck waiting for I/O or unresponsive disk operations.
✅ Conclusion:
Your CPU is under high usage but handling it well. If r exceeds core count for long periods, performance tuning may be needed.

3️⃣ Memory Usage (swpd, free, buff, cache)
Swap (swpd): 0
→ No swap usage → Excellent! Your system has enough RAM to handle the workload without swapping to disk.
Free Memory (free): ~52GB
→ Plenty of RAM available, so memory pressure is not an issue.
Buffers (buff) and Cache (cache):
Buffers: ~220MB
Cache: ~2.5GB
→ The system is keeping recently used files in cache, which is normal.
✅ Conclusion:
Your system is not memory-constrained and has no swap usage—this is very good for performance!

4️⃣ Disk and I/O (bi, bo)
Block In (bi): ~0-311 KB/s

Block Out (bo): ~948-21,116 KB/s

→ Low to moderate I/O activity, mostly writes (bo).
→ No backlog (b is 0), so the system isn't stuck waiting for I/O.

✅ Conclusion:
Your system is not I/O-bound, meaning disk performance is not a bottleneck.

5️⃣ System Calls and Context Switches (in, cs)
Interrupts (in): ~80,000 - 107,000

Context switches (cs): ~1.7M - 1.9M

→ Very high context switches, which suggests:

A highly multi-threaded workload.
Frequent CPU task switching (potential inefficiency).
📌 Possible Optimization:

Check if too many small tasks are switching between CPU cores.
Use CPU affinity (taskset) to pin processes to cores and reduce task switching overhead.
Profile thread activity with:
bash
Copy
Edit
perf sched record
perf sched report
Final Verdict:
✅ Your system is handling the load efficiently.
✅ No swap usage, low I/O wait, and plenty of RAM available.
✅ Your workload is heavily CPU-bound but not I/O constrained.
🔸 Context switching is high—consider tuning threading and CPU affinity for optimization.




### sudo strace -c -p 12244


Analysis of Your strace Output
This output shows the syscalls your process (PID 12244) is making, their time consumption, and call frequency.

1️⃣ Key Findings
clock_nanosleep dominates (92.09%)
→ Your program spends most of its time sleeping (likely due to rate-limiting, waiting for an event, or inefficient delays).
futex calls (2.12%) with 31 errors
→ Indicates thread synchronization overhead, possibly due to contention.
restart_syscall (2.02%)
→ Occurs when a syscall is interrupted by a signal and needs to restart (common in multi-threaded apps).
File system activity (statx, unlinkat, openat, getdents64, mkdir)
→ Your program is frequently checking file statuses, reading directories, and creating/deleting files.
2️⃣ Potential Bottlenecks & Optimizations
🔹 clock_nanosleep (92%) → Too Much Sleeping!
Possible causes:

Intentional delay (e.g., polling loops, rate-limiting).
Inefficient waiting strategy (e.g., busy-waiting with sleep).
Fix:
Replace sleep with event-driven waits (e.g., epoll, select for I/O tasks).
Reduce unnecessary sleep calls.
🔹 futex Calls (2.12%) → Thread Contention?
Possible causes:

High thread synchronization overhead (lock contention).
Thread pool inefficiency.
Fix:
Reduce locking (e.g., use lock-free data structures or fine-grained locks).
Check thread pool settings (e.g., oversubscribed threads may cause contention).
Profile with:
bash
Copy
Edit
perf record -g -p 12244
perf report
🔹 restart_syscall (2%) → Signal Interruptions
Possible causes:

Signals (e.g., SIGALRM, SIGHUP) interrupting syscalls.
Fix:
Check which signals are causing interruptions:
bash
Copy
Edit
strace -p 12244 -e signal
Modify signal handling (e.g., SA_RESTART flag).
🔹 File System Activity (Stat, Open, Delete, Create)
High statx calls (1,257 times) → Frequent file checks.
Many unlinkat, mkdir, openat calls → File modifications.
Fix:
Cache file metadata instead of frequent statx calls.
Batch file operations when possible.
Use async I/O (io_uring, aio).
3️⃣ Final Verdict
✅ Your program is functional but could be optimized.
🔸 Major inefficiency: clock_nanosleep (92% sleep time).
🔸 Possible thread contention: futex calls (2%).
🔸 Too many file system lookups.

