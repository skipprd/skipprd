# Improvements (highest impact, lowest risk)

This document records the safest, highest-impact improvements identified during the audit of the **plan → author → validate → review** loop for DBT project production.

Goals:
- Reduce looping / deadlocks (especially after validation failures).
- Reduce “schema drift” and dialect-caused failures during iterative edits.
- Keep guardrails, but make them **less brittle** and easier to reason about.
- Prefer additive, low-blast-radius changes.

Non-goals (for now):
- Full architectural rewrite of phases or plan representation.
- Replacing the LLM-driven authoring approach entirely.

---

## 1) Make “effective mutation” explicit (stop inferring)

**Problem**
- Guard state and phase routing depend on heuristic mutation detection, which is brittle across tools and output shapes.
- This is a major source of loops (“suite thinks nothing changed”) and accidental lockouts.

**Proposal**
- Standardize a single tool output field for all mutating tools:
  - `delta.files_changed: string[]`
  - `delta.ok: bool`
  - `delta.mutated: bool` (or derived from `files_changed`)
  - optional: `delta.bytes_added/removed`, `delta.sha_changes`
- Update guard logic to prefer this single `delta` field over tool-specific heuristics.
- Keep existing heuristics as a compatibility fallback during migration.

**Why it’s safe / high impact**
- Additive: doesn’t change tool behavior, only clarifies it.
- Makes guardrails **deterministic** and reduces “mystery state” failures.

**Where to implement**
- Mutation inference today:
  - `crates/react-suites/src/data_engineer/control_flow.rs` (`is_effective_mutation_step`)
- Tool registration / constraints:
  - `crates/react-suites/src/data_engineer/mod.rs` (authoring tool registry)

**Acceptance criteria**
- After any successful patch/write tool call, the suite reliably observes `mutated_since_fail=true` without relying on hash/diff guessing.
- No regressions in read-only phases.

---

## 2) Add Athena/Trino dialect guardrails + preflight checks (prevent common “drift” failures)

**Problem**
- Many “Column 'x' cannot be resolved” errors are not true upstream drift; they are SQL dialect/scoping issues (notably: Trino/Athena does not allow referencing select-list aliases in the same SELECT list).
- Iterative edits often introduce “normalize once, reuse alias” patterns that break in Trino.

**Proposal**
- Encode dialect invariants into prompts (authoring + repair):
  - “Do not reference a select-list alias inside another expression in the same SELECT; use a CTE/subquery.”
  - “Prefer a canonical staging shape: `source` → `normalized` → `typed` → `final`.”
- Add lightweight preflight static checks (cheap heuristics) before running `dbt build`:
  - Detect alias re-use in same select-list for common patterns (`*_clean` used later in same select).
  - Emit a targeted remediation hint (“wrap in a CTE”) rather than sending the system back into general repair.

**Why it’s safe / high impact**
- Doesn’t touch the state machine; prevents a high-frequency failure class.
- Improves first-attempt success rate and reduces repair loops.

**Where to implement**
- Error parsing helpers already exist:
  - `crates/react-suites/src/data_engineer/dbt_error.rs` (`extract_unresolved_columns`, failed model/test extraction)
- Prompt sources:
  - `crates/react-suites/src/prompts/cleanse.rs`
  - `crates/react-suites/src/prompts/model.rs`
  - (repair prompts) `crates/react-suites/src/data_engineer/dbt_repair/*`

**Acceptance criteria**
- For unresolved-column failures that are alias-scope issues, the system emits a deterministic “CTE needed” hint and avoids repeated wrong patches.

---

## 3) Keep `hard_mutation_only`, but allow bounded grounding reads (avoid “patch-first blindness”)

**Problem**
- When validation fails, `hard_mutation_only` can block exploration reads entirely, forcing mutation without one last grounding look.
- This increases the chance of wrong fixes, which increases consecutive failures.

**Proposal**
- In `hard_mutation_only`, allow a *bounded* read budget:
  - e.g. 1–2 `dbt_files op=get` calls for the exact failing file(s), or a single “read compiled SQL for failing model”.
- Do **not** clear the “mutation required next” constraint; only allow grounding.

**Why it’s safe / high impact**
- Preserves the intent: avoid read-only thrash when a fix is required.
- Improves accuracy of the next mutation and reduces repeated failures.

**Where to implement**
- Tool gating for hard mutation mode:
  - `crates/react-suites/src/data_engineer/mod.rs` (`hard_mutation_only` tool registry)

**Acceptance criteria**
- After a validation failure, the suite can still fetch the minimal facts needed to patch correctly, without reopening broad exploration.

---

## 4) Replace “3 strikes lockout” experience with automatic triage (same guard, better recovery)

**Problem**
- `MAX_CONSECUTIVE_BATCH_FAILURES` prevents infinite loops, but it stops progress without guaranteeing the next step is better informed.

**Proposal**
- Keep the guard, but when a batch fails:
  - Auto-extract and surface:
    - failing model(s) + file path(s)
    - unresolved columns
    - likely error class (`MissingSource` vs `SqlFailure` vs contract/YAML)
    - the single best “next patch target” file
  - Add a “suggested next action” string to the tool output and terminal summary.

**Why it’s safe / high impact**
- Doesn’t change execution behavior, only improves the recovery loop.
- Reduces time-to-fix and reduces repeated failed retries.

**Where to implement**
- Batch failure counter / lockout:
  - `crates/react-suites/src/data_engineer/tools/apply_next_batch.rs`
- Error classification and extraction:
  - `crates/react-suites/src/data_engineer/dbt_error.rs`

**Acceptance criteria**
- When lockout happens, the user/LLM sees a concise, accurate “here’s what failed and exactly what to fix next”.

---

## 5) Shrink LLM protocol surface via deterministic templates (reduce degrees of freedom)

**Problem**
- The system is fragile because the LLM is allowed to invent too much structure (CTEs, naming, flags, parsing) on every iteration.
- More degrees of freedom → more ways to fail → more guardrails needed.

**Proposal**
- Establish canonical templates for the most common artifacts:
  - staging model structure (CTE pipeline)
  - timestamp parsing / normalization
  - “raw + cleaned + flags” conventions
- Prompts should instruct: “Only fill in the variable parts; keep template structure unchanged.”

**Why it’s safe / high impact**
- Doesn’t reduce capability; it increases consistency.
- Prevents a large class of dialect and consistency issues.

**Where to implement**
- Authoring prompts and tools:
  - `crates/react-suites/src/data_engineer/tools/staging_model.rs`
  - `crates/react-suites/src/data_engineer/tools/gold_model.rs`
  - `crates/react-suites/src/prompts/*`

**Acceptance criteria**
- Generated staging models follow a consistent structure across datasets and across iterations.

---

## 6) Make review routing higher-bandwidth (keep `META.actionable`, add structured reasons)

**Problem**
- Review unify routing relies on a single bit (`actionable=true/false`) which is often too low-bandwidth to route cleanly without ping-pong.

**Proposal**
- Extend unify output (while keeping the existing `META:` prefix for compatibility):
  - `reasons: string[]`
  - `suggested_next_phase: "author" | "validate" | "plan"`
  - optionally: `priority: "blocker"|"high"|"medium"|"low"`

**Why it’s safe / high impact**
- Additive: existing routing can keep using `actionable`.
- Makes the “why are we looping?” explanation explicit and improves UX.

**Where to implement**
- Review prompts:
  - `crates/react-suites/src/data_engineer/review_batched.rs` (`system_prompt_for_unify`)

**Acceptance criteria**
- Review outputs contain a stable, parseable rationale for routing decisions.

---

## 7) Improve “validation failure summaries” as a first-class artifact

**Problem**
- Even when error summaries exist, they can be too verbose/noisy or miss the key actionable detail.
- This increases time-to-fix and encourages blind patching.

**Proposal**
- Treat DBT failure summary as a structured artifact:
  - `{error_class, failing_models, unresolved_columns, best_guess_cause, suggested_fix_pattern}`
- Ensure this structured summary is:
  - persisted in thread state
  - shown in terminal as a short line + multiline description

**Why it’s safe / high impact**
- Improves observability; doesn’t change authoring logic.
- Makes debugging and iteration faster, especially under guard constraints.

**Where to implement**
- Error parsing / summarization:
  - `crates/react-suites/src/data_engineer/dbt_error.rs`
- UI surfacing:
  - `react/src/ws/server.rs`
  - `react/src/ws/terminal.rs`

**Acceptance criteria**
- For a failed `dbt build`, terminal shows: (1) concise headline, (2) multi-line actionable detail, (3) failing model path(s).

---

## Suggested execution order

If we want maximum improvement with minimum disruption:
1. **Explicit mutation deltas** (Item 1)
2. **Dialect guardrails + preflight checks** (Item 2)
3. **Bounded grounding reads in `hard_mutation_only`** (Item 3)
4. **Automatic triage on failures + better lockout messaging** (Item 4)
5. **Deterministic templates for staging** (Item 5)
6. **Structured review routing** (Item 6)
7. **Structured validation summaries** (Item 7)

