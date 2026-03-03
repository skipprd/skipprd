# React Architecture Review

## Scope

This review summarizes current architecture complexity in:

- `react/core`
- `react/suites/react-suites` (especially `data_engineer`)

It emphasizes **safe, high-impact** changes that reduce code and simplify behavior after the phase-gate refactor.

## Current State (Blunt Assessment)

### Overall

- `react-core`: **High architectural complexity**
- `react-suites`: **Medium-high**
- `data_engineer`: **High**

### Why

- Large "god modules" with mixed concerns.
- Duplicate cleanse/model execution shapes.
- Multiple state representations and many control details carried via JSON payloads.
- Residual layering overlap despite successful phase-gate cutover.

## Highest-Impact Smells

### 1) Oversized orchestration modules

- `react/suites/react-suites/src/data_engineer/mod.rs`
- `react/suites/react-suites/src/data_engineer/plan.rs`
- `react/core/src/session/mod.rs`
- `react/core/src/agent/mod.rs`

Risk:

- Broad blast radius for any change.
- Hard to reason about invariants.
- Refactor confidence depends on broad regression testing.

### 2) Cleanse/model duplication

Paths are structurally parallel but implemented separately in many places.

Risk:

- Bug fixes must be duplicated.
- Drift between tracks is likely.

### 3) Wide mutable state surface (`ExecutionState`)

Many orthogonal concerns live in one evolving state object.

Risk:

- Hidden coupling.
- Invalid or stale combinations are easier to produce.

### 4) String/JSON-heavy control metadata

- Transitions still depend on many `reason_detail: Option<Value>` payload shapes.

Risk:

- Compile-time safety reduced.
- Schema drift and runtime-only failures during refactors.

### 5) Boundary leakage between pure flow and runtime side effects

- Some modules mix phase logic, tool orchestration, persistence, and error shaping.

Risk:

- Hard to isolate and test deterministic behavior.
- Hard to delete old paths cleanly.

## Safe + High Impact Changes (Recommended First)

These are ordered to maximize simplification with minimal regression risk.

### A) Split `data_engineer/mod.rs` into phase executors

Safety: **High**  
Impact: **High**

What to do:

- Move phase-specific branches into:
  - `phase_plan.rs`
  - `phase_author.rs`
  - `phase_validate.rs`
  - `phase_publish.rs`
- Keep `mod.rs` as wiring and dispatch only.

Why safe:

- Mechanical extraction with behavior parity.
- Existing tests can be reused without semantic changes.

Code reduction effect:

- Significant local complexity reduction in `mod.rs`.

### B) Consolidate cleanse/model runner skeleton

Safety: **High**  
Impact: **High**

What to do:

- Introduce a shared track executor shape using `TrackKind`.
- Keep track-specific data loading/prompt details as parameters.
- Remove mirrored control-flow branches where structure is identical.

Why safe:

- Mostly deduplication of already equivalent logic.
- Leverages current typed phase-gate and transition APIs.

Code reduction effect:

- Removes duplicated branch trees and repeated transition payload code.

### C) Type reason-detail payloads for control-critical transitions

Safety: **Medium**  
Impact: **High**

What to do:

- Add typed `PhaseReasonDetail` variants for key reason codes.
- Keep freeform JSON only for non-critical diagnostic attachments.

Why high impact:

- Compile-time guarantees for transition metadata.
- Reduces runtime schema drift risk.

Why medium safety:

- Cross-cutting signature changes across many call sites.

### D) Decompose `ExecutionState` into typed sub-states

Safety: **Medium**  
Impact: **High**

What to do:

- Split into nested structs:
  - `PhaseState`
  - `RepairState`
  - `PublishState`
  - `ProbeState`
  - `ManifestState`
- Keep event reducer central, but target smaller state mutation surfaces.

Why high impact:

- Clear invariants.
- Lower coupling between unrelated transitions.

Why medium safety:

- Requires careful migration of event application and serialization.

### E) `react-core` state representation simplification

Safety: **Medium**  
Impact: **High**

What to do:

- Reduce duplicate run-state representations in core where feasible.
- Keep one canonical path for runtime-critical state derivation/projection.

Why high impact:

- Eliminates a class of drift bugs.

Why medium safety:

- Touches foundational runtime behavior.

## Lower-Risk Cleanup (Do Anytime)

### 1) Delete thin wrappers and stale compatibility paths

Safety: **High**  
Impact: **Medium**

- Continue removing transitional helpers once no longer needed.
- Keep only one entrypoint per operation (transition, block, state mutation).

### 2) Split oversized tool files by concern

Safety: **High**  
Impact: **Medium**

- Separate parsing/policy/rewrite logic from tool adapters.
- Improves readability and test targeting.

### 3) Keep enforcement tests for legacy-path prevention

Safety: **High**  
Impact: **Medium**

- Preserve source-level checks that prevent reintroduction of direct legacy paths.

## Suggested Execution Order

1. Extract phase executors from `mod.rs` (A).
2. Consolidate cleanse/model runner skeleton (B).
3. Delete remaining compatibility wrappers and flatten boundaries.
4. Introduce typed reason details (C).
5. Decompose `ExecutionState` (D).
6. Tackle `react-core` representation simplification (E).

## What Not To Do

- Do not add new abstraction layers that simply forward calls.
- Do not mix behavior changes with structural extraction in one step.
- Do not rely on runtime JSON shape checks where enums can encode invariants.

## Success Criteria

- Fewer LOC and fewer branch points in orchestration files.
- No duplicated cleanse/model control skeletons.
- Typed transition metadata for control-critical paths.
- Smaller state mutation surface with explicit sub-state ownership.
- Equivalent behavior validated by focused regression suites.
