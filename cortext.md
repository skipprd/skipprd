# Cortex Code CLI notes

Date: 2026-04-29

## What I tried

- Installed Snowflake Cortex Code CLI with the Snowflake install script.
  - Installed version: `Cortex Code v1.0.73`.
- Tried headless Cortex usage from this repository:
  - `cortex -w /Users/huders2000/Documents/sites/skippr/skipprd -p "..."`
  - Result: failed with `No Snowflake connection available`.
- Checked Cortex connection state:
  - `~/.snowflake/connections.toml` is absent.
  - `cortex connections list` reports no active connection and no configured connections.
- Checked Cortex local capability surface:
  - `cortex --help`
  - `cortex skill list`
  - `cortex mcp list`
- Used the Skippr bike-hire E2E configuration:
  - Config: `.github/actions/e2e/bike_hire/skipprd.yml`
  - Source: `s3://skippr-e2e-sample-data/bike-hire/`
  - Intended sink: Athena workgroup `bikehire`, Glue database `bikehire`, output bucket `s3://skippr-e2e-sample-data-output/bikehire`
- Probed the bike-hire S3 source directly with AWS CLI.
- Ran bounded Skippr commands against the bike-hire config:
  - `skipprd discover --pipeline bike_hire --output json`
  - `skipprd sync --pipeline bike_hire --once --output json`

## Cortex Code CLI observations

Cortex is an AI agent for working with Snowflake from a terminal. The public docs and installed CLI show support for:

- Catalog exploration: databases, tables, tags, lineage, object search, table details.
- SQL authoring and execution through Snowflake.
- Query explanation and optimization.
- dbt project creation and dbt-oriented workflows.
- Streamlit app generation for Snowflake data.
- Cortex Analyst workflows with semantic model YAML files.
- Cortex Agents and Snowflake Intelligence style agent workflows.
- Snowflake-native skills, including bundled skills for SQL authoring, dbt, semantic views, dynamic tables, Iceberg, data quality, lineage, Snowpark Python, warehouse/cost analysis, governance, ML, and Cortex AI functions.
- MCP configuration, local file work, shell/git/worktree operations, and plan/bypass modes.

In this environment Cortex could not be used against live data because no Snowflake connection is configured. Even `cortex search docs` returned `No Snowflake connections available`, so the practical test stopped at installation, help output, skill discovery, and connection failure behavior.

## Skippr bike-hire observations

The bike-hire source data is present and readable in AWS:

- `s3://skippr-e2e-sample-data/bike-hire/`
- 51 objects
- 279,827,188 compressed bytes
- Example object: `bike-hire/bikehire1.json.gz`
- `bikehire1.json.gz` contains 100,000 JSON-line records.
- Sample top-level fields include `rider_id`, `bike_id`, `event_type`, `message_type`, `event_date`, `isbn`, `trip`, `last_crank`, `crank_torques`, `hardware`, and `metadata`.
- The checked first 1,000 rows included event types `trip_resume`, `trip_end`, `trip_start`, and `trip_pause`.
- The existing Soda check expects `row_count = 5100000` for `bike_hire`.

The intended datalake was not currently materialized:

- Athena workgroup `bikehire` exists in `us-east-1`.
- Glue database `bikehire` does not currently exist.
- Athena `SHOW DATABASES` only showed `default` and the Iceberg E2E databases.
- `s3://skippr-e2e-sample-data-output/bikehire` had no listed objects.

Skippr command behavior in this run:

- `discover` resolved the published S3 runtime source plugin and completed successfully, but reported `namespaces_discovered=0` and `total_fields=0`.
- `sync --once` resolved S3 and Athena runtime plugins, completed successfully, but reported `total_rows=0`.
- The run persisted state under `s3://skippr-e2e-sample-data-output/skippr/test/bike_hire/`, including an empty metadata document and a metrics document showing zero ingested rows, zero WAL rows, and zero Parquet rows.
- `skipprd schema --pipeline bike_hire` currently panics with `listing table: Internal("No schema provided.")` after the empty metadata state is present.

That means I could validate the source and the Skippr control plane, but not a populated resulting Athena/Glue datalake from the current E2E config.

## How Cortex differs from Skippr

Cortex is an assistant and Snowflake workflow surface. It helps a user ask natural-language questions, generate and run SQL, create dbt projects, build Streamlit dashboards, work with semantic models, create Cortex Agents, and operate inside Snowflake's governed environment.

Skippr is a deterministic ingestion engine. It reads data from sources such as S3, infers schemas, buffers records through a WAL, compacts data into Parquet, writes to a lake target, tracks offsets, registers catalog metadata, and is designed around repeatable pipeline execution and failure recovery.

The most important boundary is data-plane ownership:

- Skippr moves and materializes data.
- Cortex reasons about and builds on data that Snowflake can already see.

For the bike-hire scenario, Skippr is the component that should turn raw gzipped JSON in S3 into Parquet plus catalog metadata. Cortex would become useful once that lake is exposed to Snowflake, for example through Snowflake-managed tables, external tables, Iceberg, or a Snowflake ingestion/dbt layer.

## What Skippr provides that Cortex does not

- Source-to-lake ingestion from S3 into Parquet.
- WAL-backed exactly-once style ingestion semantics and offset tracking.
- Compaction and object layout control.
- Athena/Glue-oriented lake creation independent of Snowflake.
- Runtime plugin execution for sources, sinks, and schema sinks.
- Operational pipeline commands such as `discover`, `sync`, `query`, `schema`, and `benchmark`.
- Lower-level control over buffering thresholds, WAL storage, partitioning, and schema approval.

## What Cortex provides that Skippr does not

- Natural-language interaction over Snowflake objects.
- AI-assisted SQL generation, execution, explanation, and optimization.
- Snowflake-native app and workflow generation, especially Streamlit and dbt.
- Semantic model and Cortex Analyst workflows.
- Cortex Agent creation and Snowflake Intelligence style workflows.
- Built-in Snowflake governance, lineage, cost, warehouse, and data-quality assistance through skills.
- A conversational operator experience with planning, approvals, local file edits, MCP, shell, git, and worktree tooling.

## How they could work together

The clean split is:

1. Use Skippr to ingest raw bike-hire S3 JSON into a durable queryable lake with Parquet and catalog metadata.
2. Expose the resulting lake to Snowflake, either by loading it into Snowflake, defining external tables, using Iceberg, or using a dbt/Snowflake transformation layer.
3. Use Cortex to explore the resulting objects, generate SQL, create semantic models, build dashboards, generate dbt transformations, and package agent workflows for analysts.

In short: Skippr is the ingestion and lake materialization engine; Cortex is the Snowflake-native AI operator and builder once the data is visible to Snowflake.

