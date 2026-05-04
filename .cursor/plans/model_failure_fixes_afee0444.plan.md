---
name: Model Failure Fixes
overview: Fix the model authoring failures by aligning Snowflake validation with dbt relation names, allowing successful repaired files to reconcile before batch locks fire, and tightening schema/patch recovery behavior.
todos:
  - id: fix-snowflake-relation-format
    content: Align Snowflake dbt-model relation formatting for gold validation and staging schema lookup
    status: pending
  - id: reconcile-before-lock
    content: Run existing SQL reconciliation before model batch-lock checks
    status: pending
  - id: schema-contract-recovery
    content: Route repaired SQL with missing schema stanza to model schema authoring
    status: pending
  - id: patch-contract-hardening
    content: Improve file patch guidance/errors and add focused tests
    status: pending
  - id: verify-fresh-run
    content: Run focused tests and a fresh local sync/model validation
    status: pending
isProject: false
---

# Model Failure Fixes

## Goals
- Make `gold_model` validation check the same physical Snowflake relations dbt builds.
- Prevent a batch lock from hiding durable progress after a manual/model file repair.
- Ensure SQL repair does not leave schema-contract work stranded.
- Reduce malformed `file op=patch` attempts and cover the behavior with focused tests.

## Implementation Plan

1. Fix Snowflake relation identity used during gold SQL validation.
- Update [`crates/skipprd-react-suite-data-engineer/src/modules/provider-snowflake/src/snowflake_impl.rs`](crates/skipprd-react-suite-data-engineer/src/modules/provider-snowflake/src/snowflake_impl.rs) so generated/dbt relation validation does not quote lowercase schema/table identifiers into exact-case Snowflake objects.
- Prefer a provider-level method or helper that formats dbt-managed relations consistently with dbt manifest output, rather than changing raw-source quoting semantics globally.
- Add tests around the current failure case: `ANALYTICS.cursor_semantic_validation_fresh_20260502_2252_silver.stg_...` should validate as the unquoted/dbt-style relation, not `ANALYTICS."cursor..._silver"."stg..."`.

2. Populate staging source schemas from dbt/warehouse using the corrected relation form.
- Update [`crates/skipprd-react-suite-data-engineer/src/dataset_truth.rs`](crates/skipprd-react-suite-data-engineer/src/dataset_truth.rs), especially `staging_relation_prefix`, `gold_relation_prefix`, and `record_staging_output_schemas`, to use the same relation formatting path as validation.
- Keep raw source schema behavior separate from dbt model relation behavior, since raw sources in `schema.yml` intentionally use Snowflake quoting.
- Verify the plan no longer logs `grounded_inputs[...].source_schema is empty` for materialized staging refs after dbt build.

3. Reconcile existing SQL before enforcing the model batch lock.
- In [`crates/skipprd-react-suite-data-engineer/src/phase_author.rs`](crates/skipprd-react-suite-data-engineer/src/phase_author.rs), move or duplicate `reconcile_existing_model_sql_from_storage` so it runs before `check_batch_lock_and_loopback` for the next pending SQL item.
- If `models/marts/fct_order_items.sql` exists, mark the SQL checklist done and save the plan before returning any lock outcome.
- Add a regression test for: failure budget exhausted, expected SQL file now exists, author phase marks SQL done instead of emitting batch-lock failure.

4. Make schema-contract recovery explicit after repaired SQL exists.
- Keep [`crates/skipprd-react-suite-data-engineer/src/phase_author.rs`](crates/skipprd-react-suite-data-engineer/src/phase_author.rs) behavior that advances from SQL to schema checklist when SQL exists.
- Add/adjust reconciliation so dbt-built SQL plus missing `models/schema.yml` stanza routes to `apply_next_model_schema_batch`, not back to SQL authoring or batch lock.
- Add a test for `fct_order_items.sql` present but schema stanza absent: next action should be schema authoring for `fct_order_items`.

5. Tighten `file op=patch` repair guidance and handling.
- Review [`crates/skipprd-react-suite-data-engineer/src/prompts/shared.rs`](crates/skipprd-react-suite-data-engineer/src/prompts/shared.rs), [`crates/skipprd-react-suite-data-engineer/src/patch_contract.rs`](crates/skipprd-react-suite-data-engineer/src/patch_contract.rs), and [`crates/skipprd-react-suite-data-engineer/src/tools/dbt_files.rs`](crates/skipprd-react-suite-data-engineer/src/tools/dbt_files.rs).
- Make examples single-file only and explicitly reject `*** Begin Patch`/multi-file envelopes with a recovery hint that includes `path` plus hunks-only `@@` patch text.
- Optionally add a normalization path for `*** Begin Patch` only if it contains exactly one file; otherwise fail with a clearer actionable error.

6. Verify locally with focused tests, then one fresh model run.
- Run targeted Rust tests for Snowflake relation formatting, plan reconciliation, and patch contract behavior.
- Run a fresh local `sync` + `model` using the same MSSQL to Snowflake path.
- Confirm from S3/dbt artifacts that `fct_order_items` has SQL and schema contract, dbt `run_results.json` passes, the thread log does not repeat Snowflake quoted-lowercase schema failures, and the plan completes instead of batch-locking.

## Expected Result
- Gold model validation and dbt build agree on relation identity.
- Repaired durable files are recognized before retry budgets block progress.
- The model command advances cleanly from SQL authoring to schema contracts to validation.
- Patch mistakes produce short, actionable failures instead of wasting repair loops.