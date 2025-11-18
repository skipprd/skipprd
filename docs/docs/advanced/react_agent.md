ReAct Agent (LanceDB + DataFusion)
==================================

Overview
--------
The ReAct agent iteratively plans, calls tools, observes results, and repeats until it returns a final SQL and answer. It is generic (no assumptions about field names), uses LanceDB on S3 for embeddings, and DataFusion for SQL.

Storage
-------
- LanceDB collections (per pipeline): `{tenant}/{workspace}/{pipeline}/lance/catalog_items`
  - Schema: id, kind('dataset'|'field'|'doc'|'artifact'), namespace, field?, text, vector(float32[]), meta(json), epoch
- Threads: `{tenant}/{workspace}/{pipeline}/threads/{thread_id}.json`
- DBT/MetricFlow: `{tenant}/{workspace}/{pipeline}/dbt/...`

CLI
---
- Build embeddings:
  - `skippr llm --embeddings-sync`
- Ask a question (hybrid SQL + docs):
  - `skippr llm --ask "How many daily active users do we have?"`
- Cleansing (one suggestion at a time; approve/edit inline):
  - `skippr llm --cleanse <namespace>`
- Modeling (MetricFlow; approve/edit inline):
  - `skippr llm --model <namespace>`

Guardrails
----------
- SQL: SELECT-only, enforce LIMIT. Avoid `range()` over timestamps; prefer `date_trunc`.
- JSON-only tool I/O. Short retries on invalid JSON.

Approvals and Outputs
---------------------
- Unified behavior (CLI and WS): after user approval, the agent calls `approve_and_save_artifact` to validate and save.
- Artifacts are saved as raw text with stable logical names:
  - Models: `dbt/models/<namespace>/<name>.sql`
  - MetricFlow: `dbt/metrics/<namespace>/<name>.yaml`
- Version history is appended under:
  - Models: `dbt/models/<namespace>/_versions/<name>/<timestamp>.sql`
  - MetricFlow: `dbt/metrics/<namespace>/_versions/<name>/<timestamp>.yaml`

Embedding Sources
-----------------
- Catalog dataset/field snippets (with short stats).
- Documentation chunks (when present).
- Artifacts (MetricFlow preferred over models). The Ask agent prioritizes artifact hits over raw tables/docs/stats.


