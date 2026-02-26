# Data Engineer Root-Cause Path Map

This document traces where placeholder/ungrounded plans and non-canonical probe targets are accepted, rejected, or still able to persist.

## 1) Placeholder/Ungrounded plan paths

### A. Plan generation path (now gated before persistence)

- `crates/react-suites/src/data_engineer/mod.rs` plan-phase flow for cleanse:
  - Build candidates from `plan.tasks[*].dataset_id` + `plan.batches`.
  - Ground via `dataset_truth::build_grounded_raw_dataset_set(...)`.
  - Prune via `plan::prune_cleanse_plan_to_grounded_raw_datasets(...)`.
  - Persist via `plan::save_cleanse_plan_grounded(...)` only.
- `crates/react-suites/src/data_engineer/mod.rs` plan-phase flow for model:
  - Discover staging inputs via `dataset_truth::discover_staging_models_from_storage(...)`.
  - Prune via `plan::prune_model_plan_to_grounded_staging_models(...)`.
  - Persist via `plan::save_model_plan_grounded(...)` only.

These paths no longer checkpoint pre-grounded draft plans.

### B. Conversion gates (new typed boundary)

- `crates/react-suites/src/data_engineer/plan.rs`:
  - `GroundedCleansePlan::try_from(CleansePlan)` enforces strict cleanse grounding + semantic checks.
  - `GroundedModelPlan::try_from(ModelPlan)` enforces strict model grounding + semantic checks.
  - `save_cleanse_plan_grounded(...)` and `save_model_plan_grounded(...)` run prune/normalize + typed conversion before storage writes.

### C. Remaining persistence paths that can still bypass grounding conversion

The generic persistence APIs remain callable and are still used outside initial plan generation:

- `plan::save_cleanse_plan(...)`
- `plan::save_model_plan(...)`

Active call sites include:

- approval/rejection/cancel transitions (`approve_*_plan_draft_and_advance`, plan rejection handling),
- authoring-phase progress saves (`update_*_progress_from_log` paths),
- review/update paths (`review_batched.rs`),
- mutation tools that rewrite plan progress/status (`apply_next_batch.rs`, `apply_next_schema_batch.rs`, `staging_model.rs`, `gold_model.rs`),
- best-effort canonical-path rewrite during load (`load_cleanse_plan_by_key`).

Interpretation: plan generation is now hard-gated, but post-generation plan lifecycle writes still use the permissive save functions.

## 2) Non-canonical probe target paths

### A. Probe-tool boundary (now canonicalized)

- `crates/react-suites/src/data_engineer/probe_target.rs` defines canonical boundary:
  - `ProbeTarget::canonical_table(...)` parses via warehouse naming and returns canonical FQN.
  - `ProbeTarget::from_table_and_field(...)` normalizes table + field inputs.
- Shared strict parser for provider-side FQNs:
  - `react_core::providers::DatasetId::parse_fqn_strict(...)`.
- Enforced in:
  - `tools/sql_schema.rs`
  - `tools/sql_stats.rs`
  - `tools/sql_sample.rs`
  - `react/src/providers/catalog.rs` (`read_catalog` / `write_catalog` key generation)

On invalid input, all three tools return `probe_policy_invalid_target` with explicit error codes (`missing_table`, `invalid_table_format`, `missing_field`, `unknown_table`, `unknown_field`, etc).

### B. Remaining non-canonical acceptance windows

- Raw plan JSON can still include non-canonical dataset identifiers before the grounding prune step; those are filtered later rather than rejected at parse time.
- Historical plan objects loaded from storage are still deserialized directly and only partially normalized (e.g. expected model paths), not fully re-validated through `Grounded*Plan` conversion on load.

## 3) Why this map exists

This map is the baseline for completing hard-cutover tasks:

1. make grounded conversions the only persistence path for all plan lifecycle writes,
2. ensure load-time paths cannot re-persist invalid plan states,
3. keep probe normalization as a single contract at the tool boundary.
