ReAct Agent (LanceDB + DataFusion)
==================================

Overview
--------
The ReAct agent iteratively plans, calls tools, observes results, and repeats until it returns a final SQL and answer. It is generic (no assumptions about field names), uses LanceDB on S3 for embeddings, and DataFusion for SQL.

Storage
-------
- LanceDB collections (per pipeline): `{tenant}/{workspace}/{pipeline}/lance/catalog_items`
  - Schema: id, kind('dataset'|'field'|'doc'), namespace, field?, text, vector(float32[]), meta(json), epoch
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
- Cleansing suggestions write DBT models under `dbt/models/<namespace>/*.sql` upon approval.
- MetricFlow YAML is written under `dbt/metrics/<namespace>/*.yaml` upon approval.

Embedding Sources
-----------------
- Catalog dataset/field snippets (with short stats).
- Documentation chunks (when present).


