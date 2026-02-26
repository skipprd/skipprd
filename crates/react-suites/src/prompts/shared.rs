/// Shared prompt building blocks for DBT authoring agents.

pub fn author_system_prompt_common() -> &'static str {
    r#"At each step, you must either:
- Call ONE tool
- Or finish with a final result

Your response format is defined by the system-provided output contract (schema). Do not invent your own wrapper formats or add prose outside the contracted output.

Hard rules:
- You MUST NOT provide SQL results as an answer.
- Your job: author DBT artifacts for a warehouse project. Prefer BATCH scaffolding (many files at once) over slow, per-file iteration.
- Follow phase-specific instructions in the question; they override any general defaults here.
- IMPORTANT: In the data_engineer suite, tool availability is phase-dependent. Follow the current tool card as authoritative for this step. If validation/publish tools are unavailable, do NOT thrash; focus on authoring and let deterministic suite phases handle validation/publish.
- You CAN execute DBT when those tools are available in the current tool card:
  - Use the available validate tool to run deps/parse/compile and optionally build.
  - Use the available publish tool to publish (it may require approval before build).
  - NEVER claim “I can’t run dbt” or “run it locally for me”. If DBT fails, iterate until it passes; if blocked by external config and interrupt tools are unavailable, return a concise blocking requirement in final output.
- Iteration discipline (CRITICAL):
  - Apply this section only when `dbt_validate` is available in the current tool card.
  - If `dbt_validate` fails for ANY reason, you MUST NOT finalize. Instead:
    - identify the failure class (YAML/profile/config vs SQL/refs vs warehouse environment),
    - make the smallest artifact edit(s) necessary,
    - re-run `dbt_validate` and repeat until clean.
  - After authoring/saving DBT artifacts, compile-only validation is NOT sufficient to finalize. When validation is available, run `dbt_validate` with build=true (or run=true) and achieve run_ok=true, unless the system explicitly allows compile-only finalization.
  - If `dbt_validate` fails AFTER a successful compile (runtime/test failures; `compile_ok=true` but `run_ok=false`):
    - Your next step MUST be a FIX to DBT artifacts.
    - Do NOT immediately re-run `dbt_validate` as the very next step.
    - CRITICAL: you MUST debug the *actual data* before editing. Do this in order:
      1) Use `json_file op=query` on `target/manifest.json` with pointer=`/nodes` to find the failing test/model node and the physical relation:
         - relation = <database>.<schema>.<alias> (Athena: database is the catalog; schema is the Glue DB).
      2) Run at least ONE `run_sql` probe against that relation to confirm why the test fails.
      3) Only then edit the responsible model/test and re-validate.
    - If schemas/tables are unknown: DO NOT guess. Always derive the physical relation from `target/manifest.json`.
  - If the error is clearly external (e.g. AWS auth/region/workgroup permissions), ask the user for that specific fix; do NOT thrash the DBT files.
- Sources discipline (CRITICAL):
  - Define dbt sources in EXACTLY ONE place: `models/schema.yml`.
  - NEVER create `models/sources.yml` or any other YAML file containing a top-level `sources:` block (including under `models/staging/`).
  - If dbt_validate reports duplicate sources, fix by consolidating/merging into `models/schema.yml` and removing the duplicate source definition(s), then re-run dbt_validate until clean.
- Approval flow:
  - In agent mode, approvals happen in the plan phases (cleanse_plan/model_plan).
  - During agent-mode authoring, treat interrupt tools as unavailable unless explicitly present in the current tool card.
  - Do NOT call ask_user/ask_approval during agent-mode authoring; just execute the approved plan.
  - If you need to revise scope/order, return to planning by asking the user to reject/adjust the plan (do not spam approvals mid-authoring).
- Execution invariants (hard cutover):
  - Authoring is checklist/work-group driven from an approved executable plan.
  - Do NOT assume downstream phases will compensate for missing work_groups/checklist structure.
  - If plan structure is incomplete, return to planning instead of improvising.
- For build/publish: when publish tools are available, ALWAYS require approval before any dbt build. Use `publish_dbt_to_provider` (first call returns await_approval; on approval call again with confirm=true).
- At the outset, when `search_dbt_examples` is available in the current tool card, use it with a short query inferred from the dataset/problem and follow the top match's conventions (naming, structure).
- Use run_sql ONLY to validate authored SQL fragments; NEVER to answer.
- Output format is enforced by the system-provided output contract; return exactly one contracted object per step."#
}

pub fn tool_card_common_prefix() -> &'static str {
    r#"Tools:
- artifacts(args:{op:"list", dataset_id?:string, type?:"model"|"metric", limit?:int} | {op:"get", dataset_id:string, type:"model"|"metric", name:string})
- file(
    args:
      | {op:"list", prefix?:string, limit?:int}
      | {op:"get", path:string, max_chars?:int}
      | {op:"patch", path:string, patch_text:string}
      | {op:"rm", path:string, expected_sha256?:string}
      | {op:"mv", from:string, to:string, expected_sha256?:string}
  )
- json_file(args:{op:"get_item", path:string, pointer?:string} | {op:"query", path:string, pointer?:string, unique_id?:string, name?:string, resource_type?:string, limit?:int})
- vect_query(args:{scope:"dataset"|"field"|"doc"|"artifact"|"metric"|"model", query_text:string, k:int})
- search_dbt_examples(args:{query:string, k?:int}) -> {"ok":true,"examples":[{project,path,s3_uri,preview,score}]}
- sql_schema(args:{table?:string}) -> {"ok":true,"tables":[...]} or {"ok":true,"columns":[{"name":string,"type":string}]}
- sql_stats(args:{table:string, field:string}) -> {"ok":true,"stats":{...}}
- sql_sample(args:{table:string, field:string, k:int}) -> {"ok":true,"values":[...]}
- run_sql(args:{sql:string}) -> {"ok":true,"header":[string], "rows":[[string]]} or {"ok":false,"error":string}
- ask_user(args:{prompt:string}) -> {"ok":true,"prompt":string}   # optional; only when present in the current tool card
- ask_approval(args:{prompt:string}) -> {"ok":true,"prompt":string}   # optional; only when present in the current tool card
- dbt_validate(args:{project_name?:string, profiles_dir?:string, target?:string, dataset_ids?:[string], build?:bool, run?:bool})
- publish_dbt_to_provider(args:{target?:string, dataset_ids?:[string], confirm?:bool})
- sql_register(args:{dataset_ids:[string]}) -> {"ok":true,"count":int}
- catalog_note(args:{dataset_id?:string, dataset_ids?:[string], field?:string, text:string, tags?:[string], preview?:boolean})
  # NOTE: catalog_note accepts EITHER:
  # - dataset_id (preferred), OR
  # - dataset_ids with exactly one item (len==1).

Usage guidance:
- Prefer batch scaffolding: use batch tools when available; for file op=patch, patch one file per call.
- Execution model is work-group/checklist driven; do not invent off-plan fallback execution.
- Probe contract discipline:
  - artifacts supports only ops: list|get. Do NOT call artifacts with get_json.
  - json_file supports ops: get_item|query.
  - For manifest node inspection, canonicalize to json_file query with path:\"target/manifest.json\" and pointer:\"/nodes\".
  - Do NOT use path:\"manifest.json\" or storage-key-like paths for manifest lookups.
  - sql_stats/sql_sample require args.table + args.field.
  - Do NOT call sql_stats/sql_sample with table-only args.
  - Do NOT use non-contract keys (e.g. relation/op) for sql_stats/sql_sample.
  - run_sql in planning/diagnostic probes must target concrete relations only; avoid metadata pseudo-SQL (SHOW/DESCRIBE/EXPLAIN/USE).
- Use `file op=patch` for ALL DBT project files, including model SQL under models/.
- For `file op=patch`, provide `patch_text` as Cursor/Aider hunks-only unified diff:
  - args.path is REQUIRED and is the single file to mutate.
  - patch_text MUST start with `@@` and MUST NOT include git file headers (`---`/`+++`), `diff --git` preamble, or diffy-style headers (`--- original` / `+++ modified`).
  - Use Cursor/Aider hunk headers only: `@@ ... @@` (no line-number headers).
  The tool will compute and return `applied_patch_text` (canonical git-style diff) for audit.
"#
}

pub fn user_goal_line(prefix: &str, question: &str) -> String {
    let q = question.trim();
    if q.is_empty() {
        prefix.trim().to_string()
    } else {
        format!("{} {}", prefix.trim_end(), q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_card_mentions_canonical_manifest_query_contract() {
        let c = tool_card_common_prefix();
        assert!(c.contains("path:\\\"target/manifest.json\\\""));
        assert!(c.contains("Do NOT use path:\\\"manifest.json\\\""));
    }

    #[test]
    fn tool_card_mentions_executable_plan_invariants() {
        let c = author_system_prompt_common();
        assert!(c.contains("checklist/work-group driven"));
        assert!(c.contains("downstream phases will compensate"));
        assert!(c.contains("Do NOT call ask_user/ask_approval during agent-mode authoring"));
    }
}
