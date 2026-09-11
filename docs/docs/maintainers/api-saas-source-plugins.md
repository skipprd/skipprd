# API / SaaS source plugins

Skippr API and SaaS connectors are **committed Rust runtime source plugins**. They are not user-authored stream manifests and they are not a YAML mini-language.

This page defines the **generic** contract between:

- **Source plugins** — extract from an HTTP/API (or similar) system and declare landing semantics per namespace
- **Host (skipprd)** — persist contracts, ingest bronze data, run schema discovery
- **Data sink plugins** — land compacted batches according to each contract’s `write_policy`
- **Schema sink plugins** — align destination catalog/DDL with contracts and output metadata

Plugin-specific examples appear only under [Reference implementations](#reference-implementations).

See also [Runtime plugins](./runtime-plugins.md) and [Runtime Plugin Contract](./runtime-plugin-contract.md).

## Responsibilities

| Owned by source plugin | Owned by skipprd core |
| --- | --- |
| Auth, pagination, rate limits | Schema discovery (`skipprd discover`) |
| Checkpoints (extraction progress) | WAL, compaction, offsets |
| Faithful bronze JSON/records | `SourceNamespaceContract` persistence |
| `SourceNamespaceContract` per namespace | Sink write policy enforcement |
| Parent/child extraction when required | Business modeling (`sde model`) |

## End-to-end flow

```text
Source plugin                    Host (skipprd)                    Sinks
─────────────                    ──────────────                    ─────
source_namespace_contracts()  →  ContractsUpdate (control)      →  METADATA.source_contracts
sync() emits payloads         →  WAL / ingest / discover      →  sync_with_context + contract
                                schema sync worker           →  SchemaSink.sync_schema_request
```

**Contracts** describe *how* data is landed (append, replace by partition, etc.). **Output metadata / Arrow schema** describes *column shapes*. Both must be consistent for mutable report-style APIs.

---

## `SourceNamespaceContract`

Every namespace the source can emit must have a contract:

| Field | Purpose |
| --- | --- |
| `namespace` | Must match the ingest namespace string exactly |
| `primary_key` | Logical row identity (nested paths as segment arrays; dotted in sinks) |
| `cursor` | Field used for incremental progress (often a report date) |
| `partition_key` | Columns that form the physical partition for `replace_partition` (must match destination path/catalog partitions) |
| `write_policy` | How the configured data sink must land batches |
| `refresh_window` | Optional lookback window (days) for re-fetching mutable report data |
| `semantics` | Optional descriptor only (`event_stream`, `entity_state`, `mutable_report`) |

### `WritePolicy`

| Policy | Use when | Sink requirement |
| --- | --- | --- |
| `append` | Immutable or append-only events | Always supported |
| `merge_by_key` | Current state by business key | Manifest: `supports_merge_by_key` |
| `replace_partition` | Mutable reports or slices keyed by partition columns | Manifest: `supports_replace_partition` |
| `replace_table` | Small, bounded full snapshots | Manifest: `supports_replace_table` |

Structural validation (before persistence):

- `merge_by_key` requires non-empty `primary_key`
- `replace_partition` requires non-empty `partition_key`
- `namespace` and every field-path segment must be non-empty
- No duplicate `namespace` values in a single `ContractsUpdate`

The host also validates that the **pipeline’s configured data sink** supports every declared policy (from sink manifest metadata).

---

## Implementation checklist: source plugin

**Core types:** `DataSource`, `SourceNamespaceContract`, `SourceSyncContext` — `src/plugins/traits.rs`, `src/plugins/source_contract.rs`.

**Runtime wiring:** `crates/skippr-runtime-sdk/src/append_source_runtime.rs` (emits `ContractsUpdate` before `sync()`).

### Cargo / manifest

- [ ] `[package.metadata.skippr-plugin]` with `kind = "DataSource"` and `plugin_name`
- [ ] `[package.metadata.skippr-plugin.source_capability]` reflects real CDC / once / delete semantics
- [ ] Set `declares_namespace_contracts = true` when this source publishes namespace contracts (typical for API/SaaS)

### Runtime entry

- [ ] Connect via runtime SDK entry (e.g. `run_append_data_source_main`) over TCP control + data channels
- [ ] `RuntimeSourceCapabilityDescriptor.declares_namespace_contracts` matches manifest when contracts are used

### `DataSource` implementation

- [ ] `execution_contract()` — explicit once termination and CDC mode (bounded report sync vs streaming CDC are different choices)
- [ ] `source_namespace_contracts()` — **complete** list of contracts for all namespaces this plugin may emit; validate each with `contract.validate()` in tests
- [ ] `sync()` — call external API; map responses to bronze records; submit via `SourceSyncContext` (payload or prepared ingest batches)
- [ ] Preserve vendor field names in bronze (no flatten/rename in the source)

### Contracts on the wire

- [ ] Non-empty `source_namespace_contracts()` → runtime sends `ContractsUpdate` on the **control** channel **before** `sync()` (handled by append source runtime)
- [ ] Each update is **authoritative** (full replace of `pipeline.source_contracts`, not a merge) — see [Runtime `ContractsUpdate`](#runtime-contractsupdate-host-behavior)
- [ ] Empty `ContractsUpdate` clears all contracts (rare for SaaS; must be intentional)

### Checkpoints

- [ ] Stable checkpoint key per namespace (and/or cursor), versioned payload (`CheckpointEnvelope` + typed payload)
- [ ] Store **extraction progress** (e.g. last successful report period), not proof of downstream correctness
- [ ] For mutable reports: use `refresh_window` + `replace_partition` so late-arriving API data can be re-pulled
- [ ] Advance checkpoint after a **successful** API response, including responses with zero rows (idempotent periods still progress)

### Auth / HTTP

Prefer `plugins/shared/api_source/` where applicable:

- [ ] OAuth2 refresh, static bearer, optional service-account pattern
- [ ] Retry with backoff and rate-limit handling (see [Lessons learned](#lessons-learned-operations--design) — 429 defaults and proactive pacing)
- [ ] Pagination helpers and date/window planners for report APIs
- [ ] Document credential env/config in the plugin crate; provide **fixture-based tests** so CI does not need live credentials (env var pointing at fixture directory is a common pattern)

### Discover auto-sampling (required for report APIs)

`skipprd discover` must not pull full historical backfills or large bronze catalogs just to infer schemas. Sampling is **automatic** (not a `skippr.yml` knob):

- [ ] Read `SKIPPR_RUNTIME_EXECUTION_MODE` via `SKIPPR_RUNTIME_EXECUTION_MODE_ENV` (`discover` vs normal sync) — set by the host in [`append_source_runtime.rs`](https://github.com/skipprd/skipprd/blob/main/crates/skippr-runtime-sdk/src/append_source_runtime.rs)
- [ ] In discover mode: narrow **date window** (e.g. last 3 days), **minimal stream profile**, `lookback_days = 0`, **no checkpoint load/advance**
- [ ] `source_namespace_contracts()` still reflects the **configured** profile so discover knows all namespaces; only `sync()` API traffic is throttled
- [ ] Unit tests: discover window, minimal streams only, env detection, no checkpoint writes in discover
- [ ] For fan-out APIs (per-campaign child reports): discover must **not** run fan-out enumeration — only org-wide / minimal grains

Reference: `runtime_is_discover_mode()` and `streams_for_run(discover)` in `plugins/data_source/google_analytics/`.

### Tests (source)

- [ ] Response parsing → bronze rows (fixture JSON)
- [ ] Contracts: `write_policy`, keys, `partition_key`, `refresh_window` per namespace
- [ ] Config variants (stream/table selection, date range)
- [ ] Checkpoint serialize/deserialize + version check
- [ ] Auth happy path + missing-credentials error path

### CLI (`sde connect source`)

Ship operator wiring whenever a source is user-facing (not internal-only). Without this, users must hand-edit `skippr.yml` and public docs drift.

**`sde` (`crates/sde` in skipprd/sde)**

- [ ] `SourceKind::<Variant>` on the `connect source` subcommand (kebab-case CLI name, e.g. `google-analytics`)
- [ ] `source_plugin_and_config()` — `plugin_name` must match `[package.metadata.skippr-plugin] plugin_name` exactly (e.g. `"GoogleAnalytics"`)
- [ ] JSON keys in `source_plugin_and_config` must match the plugin’s `Deserialize` config struct field names (snake_case)
- [ ] `SourceConfig` variant in `public_config.rs` with `#[serde(rename = "snake_case_kind")]` for public `skippr.yaml`
- [ ] `translate.rs` — map public `kind` + fields into `skippr_input` (flat fields for runtime plugins; nested shapes only when the EL layer expects them)
- [ ] `cmd_connect_source` — interactive prompts for required fields when flags omitted; prefer `${ENV_VAR}` defaults for secrets
- [ ] `source_kind_str()` / legacy `kind_label` match arm for JSON connect output

**`react-suite-data-engineer` in skipprd/sde (`skippr_impl.rs`)**

- [ ] `skippr_plugin_name()` — explicit `"snake_kind" => "PluginName"` when `capitalize_first` would be wrong (e.g. `google_analytics` → `GoogleAnalytics`, not `Google_analytics`)

**Docs**

- [ ] Public connector page (`skippr-web` and/or `skipprd` inputs doc) with `sde connect source …` example and flag table
- [ ] `react/docs/docs/cli/connect.md` section for the new source

**Verification**

```bash
cargo test -p sde translate_google_analytics
cargo test -p sde translate_meta_instagram_ads
sde connect source <kebab-name> --help
```

Reference: `google-analytics` / `GoogleAnalytics` in `plugins/data_source/google_analytics/`, `meta-instagram-ads` / `MetaInstagramAds` in `plugins/data_source/meta_instagram_ads/`.

---

## Implementation checklist: data sink plugin

**Core types:** `DataSink`, `SinkWriteContext` — `src/plugins/traits.rs`.

The host invokes `sync_with_context` for compaction and WAL replay. Contract-aware sinks must not use the default `sync_with_context` that ignores `source_contract`.

### Cargo / manifest

- [ ] `kind = "DataSink"` and accurate `[package.metadata.skippr-plugin.sink_capability]`:
  - `supports_merge_by_key`
  - `supports_replace_partition`
  - `supports_replace_table`
- [ ] Flags must match actual implementation; host calls `validate_write_policy_for_sink` at startup and when contracts are applied

### `DataSink` implementation

- [ ] Override `sync_with_context`
- [ ] Resolve contract: `ctx.source_contract` → else `namespace_source_contract(namespace)` from `METADATA`
- [ ] Derive `write_policy` from contract; on CDC-encoded paths, typically force `append` and skip SaaS replace semantics
- [ ] Before non-append writes: `validate_write_policy_for_sink`, `ensure_source_contract_for_policy`
- [ ] **Hard-fail** unsupported policies (`io::Error`); never warn and fall back to append

### Per `write_policy` (implement or reject explicitly)

| Policy | Required behavior |
| --- | --- |
| `append` | Write batch to destination using existing layout/partition encoding |
| `replace_partition` | Determine partition scope from contract `partition_key` (usually values from the first row of the batch) and/or encoded filename partitions; **delete or overwrite** that scope, then write; register partition in catalog if applicable |
| `replace_table` | Delete or replace entire table/namespace scope, then write |
| `merge_by_key` | Upsert or equality-delete merge using `primary_key`; only if manifest allows |

Additional requirements for any sink:

- [ ] Partition columns in the **write path** must match partition columns in the **catalog** (schema sink responsibility for initial create)
- [ ] Contract partition columns should be ordered consistently with catalog partition key order when both are used
- [ ] Runtime protocol: optional `source_contract` on `SinkRunRequest` must roundtrip over bincode (no `skip_serializing_if` on contract fields)

### Catalog-specific patterns (when applicable)

**Object store + Hive-style catalog (e.g. S3 + Glue):**

- [ ] `replace_partition`: build delete prefix `…/col=value/…` from contract keys and batch values
- [ ] Apply contract partition segments to object key and partition-value list **before** filename/time layout segments when both exist
- [ ] Partition registration must match table `partition_keys` count and order

**Table-format catalogs (e.g. Iceberg):**

- [ ] Implement merge / partition replace / table replace via catalog-native operations
- [ ] Empty batch for non-append policies should **error**, not no-op

### Host integration (assumptions for all contract-aware sinks)

- [ ] Compaction and WAL replay pass `namespace_source_contract` into `sync_with_context`
- [ ] Startup: persisted contracts incompatible with configured sink → fail fast (`validate_pipeline_source_contracts_at_startup`)

### Tests (data sink)

- [ ] Partition scope derivation from batch (happy + empty batch + null key + missing column)
- [ ] Catalog partition metadata includes contract `partition_key` columns (where testable without cloud)
- [ ] Unsupported policy returns error

---

## Implementation checklist: schema sink plugin

**Core types:** `SchemaSink`, `SchemaSyncRequest` — `src/plugins/traits.rs`.

Schema sinks run as separate runtime subprocesses (`kind = "SchemaSink"`). They update destination DDL/catalog from `OutputMetadata` and must honor `source_contract` when partition semantics depend on it.

### Cargo / manifest

- [ ] `kind = "SchemaSink"`, `supports_schema = true`
- [ ] Pipeline config pairs `schema_plugin` with the primary data sink for that destination family

### `SchemaSink` implementation

- [ ] Override `sync_schema_request` (default implementation **ignores** `source_contract`)
- [ ] Resolve contract: `request.source_contract` → else `namespace_source_contract(namespace)`
- [ ] Thread contract into create/update catalog logic

### Catalog DDL (generic)

- [ ] **Create:** column definitions from `OutputMetadata`; add **partition key columns** from `contract.partition_key` when `write_policy` is `replace_partition` (or whenever the data sink expects catalog partitions to match contract paths)
- [ ] Partition names use contract dotted paths (e.g. `report_date`), not only pipeline `output_layout` derived names, when the data sink uses contract-driven paths
- [ ] **Update:** if the catalog cannot change partition keys in place, document that new namespaces require correct initial create; updates may only change data columns
- [ ] Partition column types: map from `OutputMetadata` where possible; sensible default otherwise

### Host integration

- [ ] Schema sync worker supplies `SchemaSyncRequest.source_contract` from `METADATA`
- [ ] `RuntimeSchemaSinkPlugin` passes `SchemaRunRequest.source_contract` to the child process
- [ ] Protocol tests: `SchemaRunRequest` and `SinkRunRequest` roundtrip with optional contract

### Tests (schema sink)

- [ ] Unit tests for “contract partition keys → catalog partition definition” (in schema crate or shared helper with data sink)
- [ ] Protocol roundtrips in `skipprd` (not plugin-specific)

---

## Runtime `ContractsUpdate` (host behavior)

**Code:** `SourceEvent::ContractsUpdate` — `src/runtime_plugins/host.rs`; `apply_runtime_source_namespace_contracts()` — `src/plugins/source_contract.rs`.

### Authoritative replace

Each `ContractsUpdate` is the **full** contract set for the current source run, not a patch.

- Omitted namespaces are removed from `pipeline.source_contracts`
- An empty vector clears all contracts
- Re-sending an identical set is a no-op (`changed == false`)

### Invalid contracts fail the run

1. Structural validation on every contract
2. Sink manifest capability check for each `write_policy`
3. On failure: abort source ingest work; fail the source run with `io::Error` (no silent append fallback; apply path does not panic on bad input)

Sinks may still read contracts from `SinkWriteContext` per batch and fall back to `METADATA`.

### Control before data

Runtime sources use separate **control** (contracts, schema state, completion) and **data** (payloads) channels. If data is processed before contracts, sinks may see no contract and default to **append**.

Host behavior:

1. Drain all buffered control frames at the start of each loop iteration before handling data
2. Prefer control I/O when both channels are ready (`select!` with `biased`)

### Contracts vs Arrow schema evolution

| Layer | Tracks | Where |
| --- | --- | --- |
| Source contracts | Landing semantics | `METADATA.source_contracts` |
| Arrow / output schema | Column shapes, superset evolution | `ARROW_SCHEMA`, `PIPELINE_SCHEMA_VERSION` — `src/ingest_work.rs` |

Applying contracts does **not** bump Arrow schema version. A `partition_key` in a contract does not add columns; discover/ingest must observe those fields in the data.

**Concurrency:** `METADATA` reads are consistent via `ArcSwap`. Writers clone, mutate, and store whole `PipelineMetadata`; concurrent writers can race (last store wins). Contract updates are ordered on the source control loop before data under normal operation.

---

## Shared helpers (`plugins/shared/api_source/`)

Reusable building blocks for API/SaaS **sources** (vendor-specific mapping stays in the source crate):

| Module | Purpose |
| --- | --- |
| `auth.rs` | Bearer, OAuth2 refresh, service account |
| `retry.rs` | HTTP retry / rate limits |
| `pagination.rs` | Page/token pagination |
| `date_window.rs` | Report lookback windows |
| `checkpoint.rs` | Versioned checkpoint envelopes |
| `json_extract.rs` | Row extraction helpers |

**`retry.rs` defaults (shared across connectors):** HTTP 429 uses a **30s** suggested delay when the vendor omits `Retry-After`; exponential backoff caps at **120s**; default **8** attempts. Per-plugin configs may raise `max_api_retries` and add a **pause between successful requests** (`request_interval_ms`) — retries alone are not enough when the catalog issues hundreds of report calls per sync.

---

## Lessons learned (operations & design)

Generic guidance from production-style SaaS connector work (especially mutable daily report APIs). Plugin-specific detail stays in [Reference implementations](#reference-implementations).

### Discover must self-limit API volume

Discover runs the same source plugin as sync. Without an in-plugin sampling knob, a `stream_profile: full` pipeline will issue a full backfill during discover and burn quota before schema inference completes.

| Discover | Normal sync |
| --- | --- |
| Short recent date window (e.g. 3 days) | `start_date` … effective end |
| Minimal bronze profile for HTTP calls | User-selected profile |
| No checkpoint read/write | Per-namespace checkpoints |
| No parent/child fan-out (if applicable) | Full enumeration when required |

Log clearly when sampling is active so operators do not confuse discover traffic with scheduled sync volume.

### Rate limits: retry backoff **and** proactive pacing

Most vendors return **HTTP 429** (or 503) under quota pressure.

1. **Use** [`RetryableHttpClient`](https://github.com/skipprd/skipprd/blob/main/plugins/shared/api_source/src/retry.rs) — do not hand-roll two-attempt retries.
2. **Treat 429 differently from 5xx** — missing `Retry-After` on 429 often needs tens of seconds, not sub-second delays.
3. **Expose plugin config** for `max_api_retries` and `request_interval_ms` (pause after each **successful** call). Defaults alone may be insufficient for large catalogs.
4. **Log at WARN** on retry with attempt number, delay, and request scope (namespace, date range) so logs are diagnosable.
5. **Curated catalogs** — more namespaces × more days × pagination ≈ quota risk; offer `minimal` / `standard` / `full` profiles and document tradeoffs on the public connector page.

**Stale-plugin smell:** an error like `failed after 5 attempts` when the crate default is already higher means the host is running an **old plugin binary** (see below), not that backoff is disabled.

### Local plugin builds vs published runtime binaries

`cargo build -p skipprd` rebuilds the **host** only. Each `plugins/data_source/<name>/` crate is a **separate** subprocess loaded at runtime.

For IDE and local dev:

```bash
cargo build -p skippr-plugin-data-source-<name> -p skipprd
python3 .github/scripts/local_runtime_plugins.py --config <skippr.yml> --pipeline <pipeline>
# export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR=<printed path>
```

Skippr IDE can build this manifest when **use local runtime plugins** is enabled (`skippr.dev.useLocalRuntimePlugins`). If sync behavior does not match source changes (retry counts, discover sampling, new config fields), rebuild the **plugin crate** and refresh the manifest before re-running sync.

**Protocol version mismatch** (`host=11 child=10` or similar) usually means the host was rebuilt but the child plugin was not — align both via `local_runtime_plugins.py` or a full plugin build.

### Bounded (`Finite`) sources and `sync --once`

Report-style sources should declare `SourceOnceContract::Finite` and exit when backfill completes. The host must **not** apply streaming idle timeouts that kill long finite backfills (IDE and CLI often use `sync --once`). Verify finite sources complete multi-hour runs without arbitrary 60s cutoffs.

### Tell source success apart from sink uploads

Host metrics can show **ingest/compactor progress** while the data sink reports **`Uploads total: 0`**.

| Signal | Meaning |
| --- | --- |
| `Messages Total` / `uploaded_rows` (compactor) rising | Source → WAL → compact path is receiving rows |
| `Uploads total: 0` | Athena/S3 sink path not committing (credentials, partition layout, tiny batches, or sink errors) |
| `msgs_total=0` on failed run | Source failed before meaningful ingest (auth, 429 on first call, config error) |

Fix the source first (429, credentials), then the sink (Glue database, S3 prefix, IAM, `replace_partition` scope).

### Pipeline + Athena: partition time field

For daily report bronze with a `date` column and `replace_partition` on `date`, the pipeline should usually set:

```yaml
transform:
  batch_time_fields: [date]
```

so the compactor and object layout align with contract partition keys. Missing `batch_time_fields` can yield very small batches and confusing sink behavior even when the source ingests successfully.

### Mutable metrics: contracts, not append

Ad platforms and analytics APIs **revise** past days (conversions, spend, etc.). Use `write_policy: replace_partition`, `partition_key` on the report date, and `refresh_window` / `lookback_days` on the source. Checkpoints record **extraction progress**, not “downstream is correct forever.”

Pair with Athena or Iceberg sinks that declare `supports_replace_partition` in the manifest; append-only sinks should fail validation at startup.

### Ship CLI, lineage, and plugin name mapping together

A source plugin without `sde connect`, `public_config`, `translate`, and `skippr_impl` / `lineage_builder` arms forces hand-edited YAML and produces lineage errors (“not supported by config translation”). Add these in the same PR as the plugin when the connector is user-facing.

### Optional-stream pattern for incompatible report grains

When a bronze namespace uses dimension/metric combinations that some accounts reject (400 / `INVALID_ARGUMENT`), mark the stream `optional` and skip **only that namespace** on known errors — do not fail the entire pipeline for one unsupported grain.

---

## Maintainer verification

```bash
# Host: contracts + protocol
cargo test -p skipprd --lib plugins::source_contract::tests
cargo test -p skipprd --lib runtime_plugins::protocol::tests
cargo test -p skipprd --test runtime_host_contracts

# Shared API helpers
cargo test -p skippr-plugin-shared-api-source

# Per-plugin (replace with your crate names)
cargo test -p <your-source-plugin>
cargo test -p <your-data-sink-plugin>
```

---

## Reference implementations

Use these for end-to-end examples only; new connectors should follow the generic checklists above.

| Role | Crate | Notes |
| --- | --- | --- |
| Source | `plugins/data_source/google_analytics/` | **Bronze grain catalog:** one namespace per daily fact grain (23 in `full` profile); avoid `runPivotReport`/custom reports in the plugin; `replace_partition` on `date`; fixtures via `SKIPPR_GA4_FIXTURE_DIR`. See [GA4 bronze & modeling](../concepts/ga4-bronze-and-modeling.md). |
| Source | `plugins/data_source/meta_instagram_ads/` | **Five** Instagram-scoped Marketing API insights namespaces; discover auto-samples 3 days × `account_daily`; `replace_partition` on `date`; fixtures via `SKIPPR_META_INSTAGRAM_ADS_FIXTURE_DIR`. See [Meta Instagram Ads plugin](./meta-instagram-ads-plugin.md). |
| Source | `plugins/data_source/bing_webmaster_tools/` | Bing Webmaster read APIs (`GetQueryStats`, `GetRankAndTrafficStats`, `GetPageStats`, `GetCrawlStats`); client-side date filtering; `replace_partition` on `date`; fixtures via `SKIPPR_BING_WEBMASTER_TOOLS_FIXTURE_DIR`. See [Bing Webmaster Tools plugin](./bing-webmaster-tools-plugin.md). |
| Source | `plugins/data_source/google_search_console/` | GSC Search Analytics daily grains; same mutable-report pattern as Bing. See [Google Search Console plugin](./google-search-console-plugin.md). |
| Data sink | `plugins/data_sink/athena/` | S3 + Glue; contract-driven partition delete; rejects `merge_by_key` |
| Data sink | `plugins/data_sink/iceberg/` | Native merge / replace partition / replace table |
| Schema sink | `plugins/schema_sink/glue/` | Glue DDL; merges `partition_key` into table on create (shared Athena helpers) |

---

## Implementation notes (all connectors)

- Bronze field names must survive to `skipprd discover` unchanged
- No business modeling in sources (attribution, metrics definitions, etc.)
- Sources do not define output schemas; discover after sample data exists
- Mutable HTTP report APIs: prefer `replace_partition` + `refresh_window` over append
- `merge_by_key` only when the API is true entity state **and** the sink manifest supports it
- Ship **source + data sink + schema sink** contract wiring together; partial integration fails at runtime or leaves catalog paths inconsistent with writes
- Implement **discover auto-sampling** and **rate-limit pacing** for every new report API connector — see [Lessons learned](#lessons-learned-operations--design)
