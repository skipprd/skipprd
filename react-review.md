# React Simplification Audit (Core + Suites + Modules)

Date: 2026-03-03

## Scope and constraints

- Audited handwritten Rust under:
  - `react/core/src`
  - `react/suites/react-suites/src`
  - `react/modules/*/src`
- Excluded generated code.
- Cross-checked cleanup proposals against `ask-ws.yaml` contract constraints.
- Priority lens: compile-time guarantees, DRY, simplicity, and canonical mutation paths.

## Executive summary

The architecture is significantly cleaner than before, but not yet "as simple as possible."
The main remaining complexity comes from:

1. wide optional runtime contexts (`SuiteCtx`/`AgentCtx` style),
2. duplicated orchestration skeletons (especially cleanse/model track logic),
3. thin indirection wrappers that add seams without adding invariants,
4. a few canonical-path leaks (multiple equivalent state write/mutation entrypoints).

Most high-impact simplification is still available without changing behavior.

## Highest-impact, lowest-risk candidates

### Core

1) `react/core/src/keyspace.rs`
- Issue: repeated temporary `DefaultKeyspace { bucket: "" }` delegation in `LocalKeyspace`.
- Why complex: large duplication and harder key-layout evolution.
- Cleanup: extract shared key-layout helpers or hold embedded `DefaultKeyspace` once.
- Risk: low.
- Impact: medium.

2) `react/core/src/providers/warehouse.rs`
- Issue: duplicated `"warehouse provider not configured"` result branches.
- Why complex: repetitive error boilerplate.
- Cleanup: helper `not_configured<T>() -> Result<T, String>`.
- Risk: low.
- Impact: low.

3) `react/core/src/session/mod.rs`
- Issue: repeated "known variants + Other(String)" enum serde pattern.
- Why complex: repeated conversion/serde glue for multiple enums.
- Cleanup: macro/helper for extensible enum serde pattern.
- Risk: low.
- Impact: medium.

4) `react/core/src/schema_registry.rs`
- Issue: validator compilation cost repeated at call sites.
- Why complex: runtime cost and repeated initialization logic.
- Cleanup: cache compiled schema validators in `OnceCell`.
- Risk: low.
- Impact: low to medium.

### Suites

5) `react/suites/react-suites/src/data_engineer/phase_validate.rs`
- Issue: repeated validate tool start/end recording and branch ceremony.
- Why complex: telemetry/plumbing drift risk across validate modes.
- Cleanup: shared helper `run_and_record_validate_step(...)`.
- Risk: low.
- Impact: high.

6) `react/suites/react-suites/src/data_engineer/phase_publish.rs`
- Issue: duplicated publish success/failure/retry transition patterns.
- Why complex: policy changes must be synchronized in parallel blocks.
- Cleanup: shared publish observation + retry helper(s).
- Risk: low to medium.
- Impact: high.

7) `react/suites/react-suites/src/data_engineer/tool_registry_builder.rs`
- Issue: inline file-tool wrappers and repeated gating logic.
- Why complex: policy spread and subtle divergence risk.
- Cleanup: centralize file tool policies behind typed enum (`ReadOnly`, `MutationOnly`, `SingleTargetMutation`).
- Risk: low to medium.
- Impact: high.

8) `react/suites/react-suites/src/data_engineer/phase_actions.rs` + `transition_dispatcher.rs`
- Issue: thin wrapper indirection.
- Why complex: additional seam with little invariant ownership.
- Cleanup: remove wrapper layer or make it sole typed transition facade.
- Risk: low.
- Impact: medium.

9) `react/suites/react-suites/src/data_engineer/loopback_intents.rs` + `phase_gate.rs`
- Issue: passthrough wrapper pattern.
- Why complex: lookup/navigation overhead for little additional value.
- Cleanup: merge intent predicates with gate evaluator; keep state mutation helpers only if they own transaction semantics.
- Risk: low.
- Impact: medium.

### Modules

10) `react/modules/provider-athena/src/athena_impl.rs` + `react/modules/provider-bigquery/src/lib.rs`
- Issue: duplicate SQL alias-reuse detector logic.
- Why complex: same lint behavior implemented twice.
- Cleanup: move shared SQL lint helper to core/provider-common utility.
- Risk: low.
- Impact: high.

11) `react/modules/provider-dbt/src/dbt_impl.rs`
- Issue: duplicated command runner logic (`host` vs `docker` labeled command variants).
- Why complex: process execution/logging semantics duplicated.
- Cleanup: single command runner abstraction with pluggable command builder.
- Risk: medium.
- Impact: high.

12) `react/modules/provider-dbt/src/dbt_impl.rs`
- Issue: stringly `DbtRunnerConfig.mode`.
- Why complex: runtime typo class.
- Cleanup: enum `DbtRunnerMode { Host, Docker }`.
- Risk: low.
- Impact: medium.

13) `react/modules/provider-vector-lance/src/lance_store.rs` + `global_lance_store.rs`
- Issue: near-duplicate upsert/query implementations.
- Why complex: behavior drift risk (already slight `meta` handling differences).
- Cleanup: extract shared table IO module; keep thin wrappers.
- Risk: medium.
- Impact: high.

## Medium/high-risk structural simplifications

1) `react/core/src/suite.rs`
- Issue: `SuiteCtx` optional-provider bag encourages runtime checks and illegal states.
- Cleanup: introduce suite-specific typed contexts with required providers.
- Risk: medium.
- Impact: very high.

2) `react/core/src/scope.rs`
- Issue: raw `String` scope ids.
- Cleanup: validated newtypes (`TenantId`, `WorkspaceId`, `ProjectId`) at construction boundary.
- Risk: medium.
- Impact: high.

3) `react/core/src/session/store_io.rs` + `materialization.rs`
- Issue: multiple public thread-state write semantics and fallback key behavior.
- Cleanup:
  - fail fast on key build errors (remove `invalid/...` fallback),
  - reduce public write paths to one canonical API with explicit write mode internalized.
- Risk: medium.
- Impact: very high.

4) `react/suites/react-suites/src/data_engineer/phase_plan.rs` + `phase_author.rs`
- Issue: mirrored cleanse/model skeletons with track-specific branch trees.
- Cleanup: typed `TrackOps` adapter so one shared flow owns orchestration and track strategy owns deltas.
- Risk: medium to high.
- Impact: very high.

5) `react/suites/react-suites/src/data_engineer/progress_controller.rs`
- Issue: very large flat `ExecutionState` + manual snapshot/setter mapping.
- Cleanup: direct nested substate ownership in struct and reducer methods scoped by substate.
- Risk: high.
- Impact: very high.

6) `react/suites/react-suites/src/data_engineer/mod.rs`
- Issue: still very large orchestration hub.
- Cleanup: split phase loop/dispatch/bootstrap orchestration into focused modules and keep phase dispatch table explicit.
- Risk: high.
- Impact: high.

## Canonical-path violations to fix first

1) `react/core/src/session/store_io.rs`
- `key()` / `state_key()` fallback to `invalid/...` namespace instead of failing fast.

2) `react/core/src/session` write surfaces
- overlapping `put_thread_state`, `put_thread_state_replace`, and materialization write behavior increase semantic drift risk.

3) control-state mutation surfaces
- typed and untyped save/mutate surfaces coexist; keep one canonical reducer-centric typed mutation path.

## Trait/contract consolidation opportunities

Current design is directionally correct, but there is still contract layering that can likely be reduced:

- `WorkflowSuiteContract` + `WorkflowPolicy`:
  - keep separate only if you need independent swapping/composition.
  - otherwise collapse into one suite workflow contract trait.

- transition wrappers:
  - remove thin wrappers that only forward to dispatcher.
  - keep one transition API and make it the only public write seam.

- context contracts:
  - prefer strongly typed suite context over one global optional bag.

Heuristic:
- Keep a trait only if it has multiple realistic implementations now, enforces a hard dependency boundary, or materially improves compile-time guarantees/tests.

## OpenAPI compatibility guardrails (`ask-ws.yaml`)

All cleanup proposals must preserve these contract rules:

1) discriminator fields and enum values are fixed.
- `type` and `kind` unions must remain exact.

2) mixed naming is intentional.
- preserve existing `snake_case` and `camelCase` fields exactly as specified.

3) `additionalProperties: false` schemas cannot gain ad-hoc fields.

4) nullability and required fields are contract-critical.
- do not change absent-vs-null behavior without spec update.

5) sequencing/meta fields (`v`, `server_time`, `seq`, ids, timestamps) are required in specific responses and must remain stable.

6) ask final payload requirements remain strict (`payload.answer` and `payload.sql` for ask final).

## Recommended phased execution

### Phase A (safe, high impact)
- dedup `phase_validate` and `phase_publish` plumbing.
- centralize file tool policy wrappers.
- consolidate duplicated provider lint/parser helpers.

### Phase B (contract tightening)
- typed `DbtRunnerMode`.
- scope/id newtypes.
- shrink thin wrapper seams around transitions/intents.

### Phase C (structural simplification)
- track adapter unification for plan/author.
- canonical session write path consolidation.
- `ExecutionState` nested substate ownership refactor.

### Phase D (final flattening)
- reduce `data_engineer/mod.rs` to orchestration wiring only.
- enforce with source tests: one transition API, one reducer mutation boundary, one control-state mutation path.

## Exit criteria for "as simple as possible"

- One transition write seam.
- One reducer mutation seam.
- One control-state persistence/mutation seam.
- No mirrored cleanse/model orchestration skeletons.
- No control-critical inline JSON literals.
- Suite contexts compile-time enforce required providers.
- API contract parity maintained with `ask-ws.yaml`.
