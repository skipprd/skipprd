# Exhaustive Cleanliness Audit (Hard-Cutover)

## Scope audited

- Rust source files in:
  - `react/core/src`
  - `react/runtime/src` (including non-generated runtime code; generated OpenAPI models treated as generated output)
  - `react/suites/react-suites/src` (with deep focus on `data_engineer`)
  - `react/modules/*`
  - `skippr/src`
- Config surfaces inspected for drift:
  - `react/*.yaml`
  - `react/runtime/openapi/ws-core.yaml`
  - `react/suites/data_engineer/openapi/ws-data-engineer.yaml`

## Executive diagnosis

The codebase is materially better after recent hard cutovers, but still has several **high-leverage complexity concentrations**:

1. duplicated orchestration/control logic,
2. stringly-typed routing/contracts where enums should be authoritative,
3. optional runtime dependency surfaces that should be compile-time required,
4. dead/placeholder modules and wrapper indirection adding noise.

The best opportunities are to remove code by collapsing duplicate pathways into one typed seam.

---

## Critical findings

### 1) `data_engineer` has duplicated track orchestration (cleanse/model)
- **Files:** `react/suites/react-suites/src/data_engineer/phase_author.rs`, `phase_plan.rs`, `tools/apply_next_batch.rs`, `tools/apply_next_schema_batch.rs`, `controller_kernel.rs`, `tool_registry_builder.rs`
- **Smell:** near-parallel branches re-implement same state gates + transitions + batch logic.
- **Impact:** drift bugs, hard reasoning, high review cost.
- **Hard-cut refactor:** introduce one typed track adapter (`enum Track` + shared orchestration functions) and remove branch duplication.
- **Estimated deletion:** ~900-1400 LOC.

### 2) `SuiteCtx` still encodes invalid states via broad `Option` provider fields
- **Files:** `react/core/src/suite.rs` (+ all SuiteCtx consumers)
- **Smell:** required dependencies for suites are runtime-checked, not compile-time guaranteed.
- **Hard-cut refactor:** suite-specific typed context builders (e.g. `DataEngineerCtx`) with required fields non-optional.
- **Estimated deletion:** many scattered runtime guards.

### 3) Runtime WS server remains god-module with repeated request/response plumbing
- **Files:** `react/runtime/src/ws/server.rs`
- **Smell:** one file handles parsing, dispatch, projection, persistence, terminal emission.
- **Hard-cut refactor:** typed command dispatcher + extracted response helpers + split projection/event modules.

---

## High findings

### 4) Global config/env side effects in runtime config path
- **Files:** `react/runtime/src/config.rs`
- **Smell:** config resolution mutates process env (`set_env_if_unset` fan-out).
- **Risk:** hidden coupling, test interference, non-local behavior.
- **Hard-cut refactor:** eliminate env writes; pass resolved typed config through context only.

### 5) Hidden fallback behavior in provider parsing
- **Files:** `react/core/src/resolved_config.rs`
- **Smell:** loose provider parsing can silently select fallback behavior.
- **Hard-cut refactor:** fail-fast typed parse (`Result<Enum, ConfigError>`), no implicit provider fallback.

### 6) DBT template/config logic duplication
- **Files:** `react/modules/provider-dbt/src/dbt_impl.rs`
- **Smell:** duplicate `dbt_project.yml` construction and repair logic in multiple paths.
- **Hard-cut refactor:** one renderer + one sanitizer/init path.

### 7) Failure classification still heavily string-driven in several seams
- **Files:** `react/suites/react-suites/src/data_engineer/dbt_error.rs`, `failure_classifier.rs`, `tools/*`
- **Smell:** repeated `contains()/to_lowercase()` heuristics.
- **Hard-cut refactor:** typed failure contract at boundary, consumed downstream as enums.

---

## Focused field audit: `ExecutionState`

### Problem

`ExecutionState` is still a wide flat record with many partially overlapping fields. The current typed accessors (`phase_state()`, `repair_state()`, etc.) help, but they still perform copy/mapping over the same flat backing fields, which keeps mutation verbose and allows drift.

Current high-noise cluster is in `react/suites/react-suites/src/data_engineer/progress_controller.rs`:
- `last_validate`, `last_validate_ok`
- `last_failure_signature`, `last_error_class`, `last_failed_models`, `repair_backlog`
- `cleanse_plan_bootstrap_done`, `model_plan_bootstrap_done`
- `max_stall_count` persisted despite being effectively policy/config

### Proposed hard-cut simplification refactor

Refactor persisted shape to one nested canonical state (no mirror mapping layer):

```text
ExecutionState {
  schema_version,
  phase: PhaseState,
  repair: RepairState,
  publish: PublishState,
  manifest: ManifestState,
  telemetry: TelemetryState,   // last_validate, artifact_focus, last_mutation_summary, probe
  subjective_retry: Option<SubjectiveRetryState>
}
```

Then delete flat duplicated fields and the `set_*_state` mapping functions.

### Field-level keep/merge/remove proposal

- **Remove (derive instead):**
  - `last_validate_ok` -> derive from `telemetry.last_validate.ok`
  - `last_failed_models` -> derive from `repair.repair_backlog` or `telemetry.last_validate.failed_models`
- **Merge:**
  - `last_error_class` + `last_failure_signature.class` -> keep one canonical source in repair (`FailureSignature`)
  - `cleanse_plan_bootstrap_done` + `model_plan_bootstrap_done` -> `manifest.plan_bootstrap_done_by_track: BTreeMap<TrackKind, bool>` or fixed typed struct keyed by `TrackKind`
- **Move out of persisted state (policy, not thread fact):**
  - `max_stall_count` -> runtime config/constant (do not persist per-thread unless explicitly required)
- **Keep as canonical persisted facts:**
  - `mutation_epoch`, `repair_mode`, `repair_backlog`, `pending_loopback_intent`, `publish_plan`, `publish_approval`, `manifest_lookup`, `last_mutation_summary`

### Net effect

- Smaller state surface
- Fewer impossible combinations
- Less mutation boilerplate
- Stronger compile-time reasoning around state ownership
- Easier future invariants (because there is one owner per substate)

### Suggested implementation sequence

1. Introduce nested persisted shape behind `ExecutionStateV2` (typed).
2. Migrate all read/write to V2 in one hard cut (no dual-write fallback).
3. Delete flat legacy fields and all mapping/setter plumbing.
4. Rebuild invariants/tests around V2 only.

---

## Medium findings

### 10) Thin wrappers/re-export indirection still present
- **Files:** various runtime helper/provider pass-through modules
- **Smell:** no behavior, only navigation overhead.
- **Action:** inline/remove wrappers where one-line re-export adds no boundary value.

### 11) Dead/inert seams in `data_engineer`
- **Files:** e.g. disabled test blocks / no-op branches in orchestration paths
- **Smell:** partial legacy artifacts reduce trust.
- **Action:** delete dead code or re-enable with real tests.

### 12) Prompt/tool-card policy strings remain duplicated
- **Files:** `tool_registry_builder.rs`, `prompts/*`, authoring tools
- **Smell:** policy text and capability logic duplicated across modes.
- **Action:** data-driven capability matrix + card rendering from one descriptor model.

### 13) Vector/Lance query decode duplication
- **Files:** `react/modules/provider-vector-lance/src/lance_store.rs`, `global_lance_store.rs`
- **Smell:** near-identical decode logic.
- **Action:** single decode path reused by both stores.

---

## Immediate code deletion opportunities (low risk)

1. Remove unused WS helper stubs in `react/runtime/src/ws/server.rs`.
2. Remove pure wrapper modules that only `pub use` core symbols.
3. Remove disabled/no-op test and branch artifacts in `data_engineer`.
4. Consolidate repeated DBT project template builders to one function.

---

## Compiler-first simplification backlog (recommended order)

1. **Typed suite contexts**: replace optional dependency bags with suite-specific required contexts.
2. **Single orchestration kernel for cleanse/model** in `data_engineer`.
3. **Typed failure boundary** for dbt/tool execution outcomes.
4. **Runtime config purity**: eliminate env mutation side effects.
5. **WS server modularization** with typed command dispatch.

---

## Current validation status

- Recent hard-cutover changes compile/test in normal crate test flows.

---

## Conclusion

The highest-value simplification now is not patching individual bugs; it is removing duplicate orchestration and stringly contracts so behavior is enforced by types. The codebase can be made substantially smaller and easier to reason about by collapsing to one typed control kernel per subsystem and deleting wrapper/dead surfaces aggressively.
