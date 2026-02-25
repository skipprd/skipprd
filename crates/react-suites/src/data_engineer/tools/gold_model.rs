use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use tokio::task::JoinSet;
use tracing::info;

use sha2::{Digest, Sha256};

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

use crate::data_engineer::dbt_repair::remediate::active_provider_dialect;
use crate::data_engineer::{naming, plan, project_fs, sql_first};

fn emit_trace(ctx: &AgentCtx, line: impl Into<String>) {
    if let Some(tx) = ctx.trace_tx.as_ref() {
        let _ = tx.send(line.into());
    }
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

fn normalize_folder(folder: Option<&str>) -> String {
    match folder.unwrap_or("marts").trim().to_lowercase().as_str() {
        "core" => "core".to_string(),
        _ => "marts".to_string(),
    }
}

fn gold_model_rel_path(folder: &str, name: &str) -> String {
    format!("models/{}/{}.sql", folder, name)
}

fn athena_alias_reuse_hint(msg: &str, model_rel_path: &str, model_name: &str) -> Option<Value> {
    let m = msg.to_ascii_lowercase();
    if !m.contains("select-list alias") {
        return None;
    }
    Some(serde_json::json!({
        "kind": "athena_select_alias_reuse",
        "model_name": model_name,
        "model_path": model_rel_path,
        "issue": msg,
        "fix": "Split into CTE + outer select: compute intermediate aliases in an inner CTE/subquery, then reference them only from the outer SELECT."
    }))
}

fn staging_rel_path_from_input(input: &str) -> String {
    let t = input.trim();
    if t.contains('/') || t.ends_with(".sql") {
        // Treat as project-relative path.
        return t.to_string();
    }
    format!("models/staging/{}.sql", t)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = s[..max].to_string();
    out.push_str("\n-- [truncated]\n");
    out
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct GoldModelItem {
    name: String,
    #[serde(default)]
    folder: Option<String>, // "marts" | "core"
    #[serde(default)]
    goal: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    instructions: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct GoldModelArgs {
    #[serde(default)]
    items: Vec<GoldModelItem>,
}

fn build_gold_sys_prompt(
    provider: &str,
    dialect: &str,
    max_items: usize,
    provider_rules: &str,
) -> String {
    format!(
        "You are an expert analytics engineer.\n\
         Task: write GOLD model query as PLAIN SQL (no dbt config, no Jinja).\n\
         Provider: {provider}\n\
         Dialect: {dialect}\n\
         Output MUST be valid JSON only: {{\"sql\":\"...\", \"notes\":[...]}}.\n\
         You MUST reference inputs ONLY via the provided placeholders (e.g. __INPUT_0__).\n\
           - Do NOT use ref() / source() / Jinja in this step.\n\
           - The system will replace placeholders with real silver relations for validation, then with dbt ref() for materialization.\n\
         \n\
         CRITICAL gold rules:\n\
         - You MUST write a SELECT-based dbt model.\n\
         - Gold models MUST ONLY read from silver models under models/staging/ using ref('stg_*').\n\
         - Gold models MUST NOT call source() anywhere.\n\
         - IMPORTANT: The user payload may include plan invariants/notes; invariants are hard requirements.\n\
         - Prefer minimal, stable columns for business use; do not invent fields.\n\
         - CRITICAL: Do NOT select or reference any column not present in inputs[].schema_columns for that input.\n\
           If you need a field that does not exist in silver, put it in notes and do NOT guess.\n\
         - Use provided inputs[].schema_columns (from the warehouse/catalog) as ground truth for available columns + types.\n\
\n\
         ANALYST_NOTES_CONTRACT_V1\n\
         Analyst mindset (CRITICAL — include these in `notes` BEFORE writing SQL):\n\
         - Business question: one sentence describing the decision this model supports.\n\
         - Entity definition: what the table represents (e.g., what counts as a “customer/order”), based ONLY on available columns.\n\
         - Grain: one clear sentence. If you dedupe/aggregate, say exactly how and what you might lose.\n\
         - Time axis: which timestamp/date drives analysis (and what it means). If no suitable time column exists, say so.\n\
         - Metric definitions: list 2–4 metrics this table enables (definitions + caveats), grounded in available columns.\n\
         - Assumptions + evidence gaps: list any semantic assumptions you made because the domain isn’t explicit in the data.\n\
           For each gap, recommend the smallest validation probe (e.g., null rate, distinctness, top values) that would confirm/refute it.\n\
\n\
         - IMPORTANT time handling (consistency):\n\
           - If an input column is already typed as timestamp/date/timestamptz/datetime, use it directly; do NOT re-cast it to the same type.\n\
           - Do NOT narrow time zones: never cast timestamptz -> timestamp.\n\
           - If you need parsed timestamps but the input only has string-ish fields, do NOT try_cast in gold; instead note that silver should add a cleaned timestamp column.\n\
         - Dialect/provider compatibility:\n\
{provider_rules}\
         - Batch throughput: you will be asked to create up to {max_items} models per call.\n\
         \n"
        ,
        provider_rules = provider_rules
    )
}

fn render_plan_driven_instructions(
    invariants: &[String],
    checklist: &[crate::data_engineer::plan::PlanChecklistItem],
) -> String {
    let mut out = String::new();
    if !invariants.is_empty() {
        out.push_str("Plan invariants (MUST satisfy):\n");
        for inv in invariants.iter() {
            let t = inv.trim();
            if t.is_empty() {
                continue;
            }
            out.push_str("- ");
            out.push_str(t);
            out.push('\n');
        }
        out.push('\n');
    }
    let mut any = false;
    for it in checklist.iter() {
        let has_details = it
            .details
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let include =
            it.status != crate::data_engineer::plan::ChecklistItemStatus::Done || has_details;
        if !include {
            continue;
        }
        if !any {
            out.push_str("Plan checklist (remaining work):\n");
            any = true;
        }
        let origin = match it.origin {
            crate::data_engineer::plan::ChecklistOrigin::Initial => "initial",
            crate::data_engineer::plan::ChecklistOrigin::ReviewActionable => "review_actionable",
        };
        out.push_str("- ");
        out.push_str(it.label.trim());
        out.push_str(" (id=");
        out.push_str(it.checklist_item_id.trim());
        out.push_str(", status=");
        out.push_str(&format!("{:?}", it.status));
        out.push_str(", origin=");
        out.push_str(origin);
        out.push(')');
        if let Some(d) = it
            .details
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            out.push_str(": ");
            out.push_str(d);
        }
        out.push('\n');
    }
    if any {
        out.push('\n');
    }
    out.trim().to_string()
}

fn combine_instructions(user_instructions: &str, plan_instructions: &str) -> String {
    let ui = user_instructions.trim();
    let pi = plan_instructions.trim();
    if ui.is_empty() && pi.is_empty() {
        return String::new();
    }
    if ui.is_empty() {
        return pi.to_string();
    }
    if pi.is_empty() {
        return ui.to_string();
    }
    format!("User instructions:\n{}\n\n{}", ui, pi)
}

#[derive(Clone)]
pub struct GoldModelTool;

#[async_trait]
impl Tool for GoldModelTool {
    fn name(&self) -> &'static str {
        "gold_model"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let parsed_args: GoldModelArgs = serde_json::from_value(args.clone())
            .map_err(|e| format!("gold_model args parse error: {e}"))?;
        if parsed_args.items.is_empty() {
            return Err("gold_model requires args.items (non-empty)".to_string());
        }
        let max_items = 5usize;
        if parsed_args.items.len() > max_items {
            return Err(format!(
                "gold_model supports at most {max_items} items per call (got {}). Split into batches.",
                parsed_args.items.len()
            ));
        }

        let dialect = crate::config::resolved_config_from_ctx(ctx)
            .map(active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());
        let provider_name = crate::config::resolved_config_from_ctx(ctx)
            .map(|cfg| cfg.providers.warehouse.kind.as_str())
            .unwrap_or("unknown");
        let provider_prompt_rules = {
            let mut out = String::new();
            for rule in ctx.warehouse.sql_prompt_rules().into_iter() {
                out.push_str("           - ");
                out.push_str(rule);
                out.push('\n');
            }
            out
        };
        let sys = build_gold_sys_prompt(provider_name, &dialect, max_items, &provider_prompt_rules);

        // Plan-first authoring: if there is an active model plan, use task invariants/notes as the
        // default authoring instructions (and merge with any explicit item.instructions overrides).
        let plan_opt = plan::load_model_plan(ctx).await;
        let global_semantic_context = ctx
            .storage
            .get_json(&ctx.keyspace.semantic_key(
                &ctx.scope,
                react_core::providers::catalog::types::GLOBAL_SEMANTIC_DATASET_ID,
            ))
            .await
            .ok()
            .unwrap_or(serde_json::Value::Null);

        let base = ctx
            .keyspace
            .dbt_prefix(&ctx.scope)
            .trim_end_matches('/')
            .to_string();
        let query = ctx.warehouse.clone();
        let (target_container, silver_ns) = crate::config::resolved_config_from_ctx(ctx)
            .map(|cfg| {
                let container = cfg.providers.warehouse.container.clone();
                let base_schema = cfg.providers.dbt.naming.target_schema.clone();
                let silver_suffix = cfg.providers.dbt.naming.silver_suffix.clone();
                let db = if base_schema.trim().is_empty() {
                    // Fallback: do not guess; leave empty so we skip schema probing.
                    "".to_string()
                } else {
                    format!("{}_{}", base_schema.trim(), silver_suffix.trim())
                };
                (container, db)
            })
            .unwrap_or_else(|| ("".to_string(), "".to_string()));

        let mut written: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut remediation_hints: Vec<Value> = Vec::new();
        let mut succeeded_item_names: Vec<String> = Vec::new();
        let mut canonical_folder_by_name: HashMap<String, String> = HashMap::new();

        for it in parsed_args.items.iter() {
            let name = it.name.trim();
            if name.is_empty() {
                errors.push("gold_model item.name is required".to_string());
                continue;
            }
            if it.inputs.is_empty() {
                errors.push(format!("{name}: gold_model item.inputs is required (list of stg_* model names or paths)"));
                continue;
            }

            let requested_folder = normalize_folder(it.folder.as_deref());
            let core_rel = gold_model_rel_path("core", name);
            let marts_rel = gold_model_rel_path("marts", name);
            let core_exists = ctx
                .storage
                .get_bytes(&format!("{}/{}", base, core_rel))
                .await
                .is_ok();
            let marts_exists = ctx
                .storage
                .get_bytes(&format!("{}/{}", base, marts_rel))
                .await
                .is_ok();
            if core_exists && marts_exists {
                errors.push(format!(
                    "{name}: model exists in both canonical folders (models/core and models/marts). Keep exactly one canonical location before authoring."
                ));
                continue;
            }
            let folder = if core_exists {
                "core".to_string()
            } else if marts_exists {
                "marts".to_string()
            } else {
                requested_folder
            };
            if let Some(prev) = canonical_folder_by_name.get(name) {
                if prev != &folder {
                    errors.push(format!(
                        "{name}: conflicting target folders in this call ('{}' vs '{}'). Use one canonical folder for this model name.",
                        prev, folder
                    ));
                    continue;
                }
            } else {
                canonical_folder_by_name.insert(name.to_string(), folder.clone());
            }
            let rel_path = gold_model_rel_path(&folder, name);

            // Load the inputs to ground the LLM in actual silver SQL.
            let max_fetch_concurrency = 3usize;
            let storage = ctx.storage.clone();
            let query2 = query.clone();
            let target_container2 = target_container.clone();
            let silver_ns2 = silver_ns.clone();
            let mut set: JoinSet<(usize, String, String, String, String, Vec<(String, String)>)> =
                JoinSet::new();
            // (idx, input, rel_path, content, derived_relation_fqn, schema_cols)
            let mut fetched: Vec<(usize, String, String, String, String, Vec<(String, String)>)> =
                Vec::new();
            for (idx, inp) in it.inputs.iter().cloned().enumerate() {
                while set.len() >= max_fetch_concurrency {
                    if let Some(res) = set.join_next().await {
                        if let Ok(v) = res {
                            fetched.push(v);
                        }
                    }
                }
                let storage2 = storage.clone();
                let rel = staging_rel_path_from_input(&inp);
                let key = format!("{}/{}", base, rel);
                let rel2 = rel.clone();
                let query3 = query2.clone();
                let target_container3 = target_container2.clone();
                let silver_ns3 = silver_ns2.clone();
                set.spawn(async move {
                    let content = storage2
                        .get_bytes(&key)
                        .await
                        .ok()
                        .map(|b| String::from_utf8_lossy(&b).to_string())
                        .unwrap_or_default();

                    let alias = rel2
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .trim_end_matches(".sql")
                        .to_string();
                    let derived_fqn = if !target_container3.trim().is_empty()
                        && !silver_ns3.trim().is_empty()
                        && !alias.trim().is_empty()
                    {
                        format!("{}.{}.{}", target_container3, silver_ns3, alias)
                    } else {
                        "".to_string()
                    };
                    let schema_cols = if !derived_fqn.is_empty() {
                        query3.schema(&derived_fqn).await.unwrap_or_default()
                    } else {
                        vec![]
                    };

                    (idx, inp, rel2, content, derived_fqn, schema_cols)
                });
            }

            while let Some(res) = set.join_next().await {
                if let Ok(v) = res {
                    fetched.push(v);
                }
            }
            fetched.sort_by_key(|(idx, _, _, _, _, _)| *idx);
            let mut input_blocks: Vec<Value> = Vec::new();
            for (_idx, inp, rel, content, derived_fqn, schema_cols) in fetched.into_iter() {
                let cols_json: Vec<Value> = schema_cols
                    .into_iter()
                    .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
                    .collect();
                if content.trim().is_empty() {
                    input_blocks.push(serde_json::json!({
                        "input": inp,
                        "path": rel,
                        "ok": false,
                        "error": "missing or empty input model SQL",
                        "derived_relation_fqn": derived_fqn,
                        "schema_columns": cols_json
                    }));
                } else {
                    input_blocks.push(serde_json::json!({
                        "input": inp,
                        "path": rel,
                        "ok": true,
                        "sql": truncate(&content, 20_000),
                        "derived_relation_fqn": derived_fqn,
                        "schema_columns": cols_json
                    }));
                }
            }

            let goal = if !it.goal.trim().is_empty() {
                it.goal.trim().to_string()
            } else {
                it.description.trim().to_string()
            };
            if goal.is_empty() {
                errors.push(format!("{name}: provide item.goal (or item.description) describing grain + business intent"));
                continue;
            }

            let (
                plan_invariants,
                plan_checklist,
                plan_expected_model_path,
                plan_implementation_spec,
            ) = plan_opt
                .as_ref()
                .and_then(|p| p.tasks.iter().find(|t| t.name.trim() == name))
                .map(|t| {
                    (
                        t.invariants.clone(),
                        t.checklist.clone(),
                        t.expected_model_path.clone().unwrap_or_default(),
                        Some(t.implementation_spec.clone()),
                    )
                })
                .unwrap_or_else(|| (vec![], vec![], String::new(), None));
            let plan_instr = render_plan_driven_instructions(&plan_invariants, &plan_checklist);
            let effective_instructions = combine_instructions(&it.instructions, &plan_instr);

            let user_value = serde_json::json!({
                "model_name": name,
                "model_path": rel_path,
                "goal": goal,
                "global_semantic_context": global_semantic_context,
                "instructions": effective_instructions,
                "plan_invariants": plan_invariants,
                "plan_checklist": plan_checklist,
                "plan_implementation_spec": plan_implementation_spec,
                "plan_expected_model_path": plan_expected_model_path,
                "inputs": input_blocks,
                "existing_model_sql": ctx
                    .storage
                    .get_bytes(&format!("{}/{}", base, rel_path))
                    .await
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).to_string())
                    .unwrap_or_default()
                ,
                "sql_first": {
                    "input_placeholders": it.inputs.iter().enumerate().map(|(i, inp)| {
                        serde_json::json!({
                            "input": inp,
                            "placeholder": format!("__INPUT_{}__", i),
                            "materialize_ref": if inp.trim().contains('/') || inp.trim().ends_with(".sql") { String::new() } else { format!("{{{{ ref('{}') }}}}", inp.trim()) }
                        })
                    }).collect::<Vec<Value>>()
                }
            });

            let max_tokens = sql_first::sql_first_max_output_tokens(6500);
            let max_attempts = sql_first::sql_first_max_repair_attempts(4);
            let mut last_err = String::new();
            let mut prev_sql: Option<String> = None;
            let mut draft: Option<sql_first::SqlFirstDraft> = None;

            // Build placeholder replacement map for validation (placeholders -> quoted silver relations).
            let mut repl_validate: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let mut repl_materialize: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for (idx, inp) in it.inputs.iter().enumerate() {
                let ph = format!("__INPUT_{}__", idx);
                // Find derived_relation_fqn for this input (from input_blocks).
                let derived_fqn = input_blocks
                    .iter()
                    .find(|b| b.get("input").and_then(|v| v.as_str()) == Some(inp.as_str()))
                    .and_then(|b| b.get("derived_relation_fqn").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if derived_fqn.is_empty() {
                    errors.push(format!("{name}: cannot validate gold SQL: missing derived_relation_fqn for input '{inp}' (ensure silver models exist and are queryable)"));
                    continue;
                }
                let id = match ctx.warehouse.parse_dataset_fqn(&derived_fqn) {
                    Ok(id) => id,
                    Err(e) => {
                        errors.push(format!("{name}: invalid derived_relation_fqn '{derived_fqn}' for input '{inp}': {e}"));
                        continue;
                    }
                };
                repl_validate.insert(ph.clone(), ctx.warehouse.quote_fqn(&id));
                // Materialize: ref('stg_*') only for named inputs; path-like inputs are invalid for gold.
                if inp.trim().contains('/') || inp.trim().ends_with(".sql") {
                    errors.push(format!(
                        "{name}: gold inputs must be stg_* names, not paths ('{inp}')"
                    ));
                    continue;
                }
                repl_materialize.insert(ph.clone(), format!("{{{{ ref('{}') }}}}", inp.trim()));
            }
            if errors.iter().any(|e| {
                e.starts_with(&format!("{name}: gold inputs must"))
                    || e.starts_with(&format!("{name}: cannot validate"))
            }) {
                continue;
            }

            let mut ok = false;
            for attempt in 1..=max_attempts {
                let mut v = user_value.clone();
                if attempt > 1 {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("attempt".to_string(), serde_json::json!(attempt));
                        obj.insert(
                            "previous_sql".to_string(),
                            serde_json::json!(prev_sql.clone().unwrap_or_default()),
                        );
                        obj.insert("error".to_string(), serde_json::json!(last_err.clone()));
                        obj.insert("instruction".to_string(), serde_json::json!("Fix the SQL. Output JSON only: {\"sql\":\"...\",\"notes\":[...]}. Must only use __INPUT_n__ placeholders. Must not reference columns outside inputs[].schema_columns. Prefer CTEs + explicit select list. Do NOT use Jinja/macros (ref/source/doc) in the draft."));
                    }
                }

                let prompt_id = if attempt == 1 {
                    "data_engineer.tools.gold_model.sql_first"
                } else {
                    "data_engineer.tools.gold_model.sql_first_repair"
                };
                let temp = if attempt == 1 { 0.12 } else { 0.08 };
                let sys_msg = if attempt == 1 {
                    sys.clone()
                } else {
                    build_gold_sys_prompt(
                        provider_name,
                        &dialect,
                        max_items,
                        &provider_prompt_rules,
                    )
                };

                let d = match sql_first::llm_draft_sql_json(
                    ctx,
                    sys_msg,
                    v.to_string(),
                    prompt_id,
                    max_tokens,
                    temp,
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        last_err = e;
                        if attempt >= max_attempts {
                            errors.push(format!("{name}: sql draft failed: {last_err}"));
                        }
                        continue;
                    }
                };

                match sql_first::validate_sql_quick(ctx, &d.sql, &repl_validate).await {
                    Ok(()) => {
                        draft = Some(d);
                        ok = true;
                        break;
                    }
                    Err(err) => {
                        last_err = err.clone();
                        prev_sql = Some(d.sql.clone());
                        if let Some(h) = athena_alias_reuse_hint(&err, &rel_path, name) {
                            remediation_hints.push(h);
                        }
                        if attempt >= max_attempts {
                            errors.push(format!("{name}: sql validation failed: {err}"));
                        }
                        continue;
                    }
                }
            }
            if !ok {
                continue;
            }
            let draft = draft.expect("ok implies draft");
            let intent =
                match crate::data_engineer::authoring_ir::compile_sql_first_draft(&draft.sql, &draft.notes) {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push(format!("{name}: authoring_ir compile failed: {e}"));
                        continue;
                    }
                };

            // Materialize: replace placeholders with ref().
            let dbt_sql = sql_first::apply_placeholders(&intent.sql, &repl_materialize);
            if naming::contains_source_call(&dbt_sql) {
                errors.push(format!(
                    "{name}: invalid gold SQL: contains source(). Gold must only read from silver via ref('stg_*')."
                ));
                continue;
            }
            if !naming::contains_ref_call(&dbt_sql) {
                errors.push(format!(
                    "{name}: invalid gold SQL: must reference at least one silver model via ref('stg_*')."
                ));
                continue;
            }
            if let Some(msg) = ctx.warehouse.unsupported_sql_reason(&dbt_sql) {
                errors.push(format!(
                    "{name}: unsupported SQL for provider '{provider_name}': {msg}"
                ));
                if let Some(h) = athena_alias_reuse_hint(&msg, &rel_path, name) {
                    remediation_hints.push(h);
                }
                continue;
            }

            let existing_sql = ctx
                .storage
                .get_bytes(&format!("{}/{}", base, rel_path))
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();
            let base_sha256 = if existing_sql.is_empty() {
                None
            } else {
                Some(sha256_hex(&existing_sql))
            };
            let patch_text = project_fs::hunks_only_full_replace_patch(&existing_sql, &dbt_sql);
            let outcome = match project_fs::apply_patch(
                ctx,
                None,
                &rel_path,
                &patch_text,
                base_sha256.as_deref(),
                Some(!existing_sql.is_empty()),
                project_fs::PatchApplyKind::UnifiedDiff,
            )
            .await
            {
                Ok(o) => o,
                Err(e) => {
                    errors.push(format!("{name}: materialize apply_patch failed: {e}"));
                    continue;
                }
            };
            if let Err(e) = ctx
                .storage
                .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/sql")
                .await
            {
                emit_trace(ctx, format!("failed to save {}: {}", rel_path, e));
                errors.push(format!("{name}: failed to write gold model: {e}"));
                continue;
            }
            emit_trace(ctx, format!("saved {}", rel_path));
            written.push(outcome.key);
            succeeded_item_names.push(name.to_string());

            for n in intent.notes {
                let nt = n.trim();
                if !nt.is_empty() {
                    notes.push(format!("{name}: {nt}"));
                }
            }
        }

        // Dedup notes to keep response bounded.
        let mut seen: HashSet<String> = HashSet::new();
        let mut out_notes: Vec<String> = Vec::new();
        for n in notes {
            if seen.insert(n.clone()) {
                out_notes.push(n);
            }
            if out_notes.len() >= 50 {
                break;
            }
        }

        info!(
            target: "gold_model",
            items = parsed_args.items.len(),
            written = written.len(),
            ok = errors.is_empty(),
            "gold_model finished"
        );

        Ok(serde_json::json!({
            "ok": errors.is_empty(),
            "written_keys": written,
            "notes": out_notes,
            "remediation_hints": remediation_hints,
            "errors": errors,
            "succeeded_item_names": succeeded_item_names
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::ChatMessage;
    use react_core::llm::LargeLanguageModel;
    use react_core::providers::{QueryProvider, QueryResult};
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockWarehouse;

    #[async_trait]
    impl QueryProvider for MockWarehouse {
        async fn query(&self, _sql: &str) -> Result<QueryResult, String> {
            Ok(QueryResult {
                header: vec![],
                rows: vec![],
                meta: None,
            })
        }
        async fn schema(&self, _dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
            Ok(vec![("order_id".to_string(), "string".to_string())])
        }
        async fn sample(
            &self,
            _dataset_fqn: &str,
            _limit: usize,
        ) -> Result<Vec<Vec<String>>, String> {
            Ok(vec![])
        }
    }

    #[async_trait]
    impl react_core::providers::DatasetCatalogProvider for MockWarehouse {
        async fn list_datasets(&self) -> Result<Vec<react_core::providers::DatasetId>, String> {
            Ok(vec![])
        }
        async fn get_dataset_schema(
            &self,
            dataset: &react_core::providers::DatasetId,
        ) -> Result<Vec<(String, String)>, String> {
            self.schema(&dataset.fqn()).await
        }
        async fn get_dataset_stats(
            &self,
            _dataset: &react_core::providers::DatasetId,
            _max_fields: usize,
        ) -> Result<
            (
                react_core::discover::stats::DatasetFieldStats,
                react_core::providers::catalog::types::DatasetStats,
            ),
            String,
        > {
            Err("not used".to_string())
        }
    }

    impl react_core::providers::WarehouseNaming for MockWarehouse {
        fn kind(&self) -> &'static str {
            "mock"
        }
        fn parse_dataset_fqn(
            &self,
            dataset_fqn: &str,
        ) -> Result<react_core::providers::DatasetId, String> {
            let parts: Vec<&str> = dataset_fqn.split('.').collect();
            if parts.len() != 3 {
                return Err("invalid fqn".to_string());
            }
            Ok(react_core::providers::DatasetId {
                catalog: parts[0].to_string(),
                database: parts[1].to_string(),
                table: parts[2].to_string(),
            })
        }
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }

    #[derive(Default)]
    struct MockLlm {
        resp: String,
    }

    impl LargeLanguageModel for MockLlm {
        fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            Ok(self.resp.clone())
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    fn minimal_cfg() -> Arc<crate::config::ReactResolvedConfig> {
        Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved {
                bucket: "b".to_string(),
            },
            scope: RequestScope {
                tenant: "t".to_string(),
                workspace: "w".to_string(),
                project_id: "p".to_string(),
            },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                warehouse: crate::config::WarehouseResolved {
                    kind: "athena".to_string(),
                    container: "AwsDataCatalog".to_string(),
                    namespace: "test_raw".to_string(),
                    extras: serde_json::json!({"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"}),
                },
                catalog: crate::config::CatalogResolved {
                    enabled: false,
                    refresh_secs: 60,
                    max_concurrency: 8,
                },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: "athena".to_string(),
                    naming: crate::config::DbtNamingResolved {
                        target_schema: "test".to_string(),
                        silver_suffix: "silver".to_string(),
                        gold_suffix: "warehouse".to_string(),
                    },
                    runner: "host".to_string(),
                    docker_image: None,
                    docker_platform: None,
                    docker_network: None,
                    docker_mount_aws_dir: false,
                },
                vector: crate::config::VectorResolved { enabled: false },
            },
        })
    }

    fn make_ctx(storage: Arc<dyn StorageAdapter>, llm: Arc<dyn LargeLanguageModel>) -> AgentCtx {
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: None,
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage,
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(MockWarehouse::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        }
    }

    fn analyst_notes() -> serde_json::Value {
        serde_json::json!([
            "Business question: Provide an orders lens for operational/finance decisions.",
            "Entity definition: One row represents a single order as defined by the available order identifier(s).",
            "Grain: One row per order (no aggregation beyond order grain).",
            "Time axis: Use the best available order timestamp/date column; if missing, note the gap.",
            "Metric definitions: Order count; revenue/amount if a numeric amount column exists; status counts if status exists.",
            "Assumptions & gaps: Column meanings are inferred from names; validate via null rate, distinctness, and top values for key fields."
        ])
    }

    #[test]
    fn gold_sys_prompt_includes_bigquery_alias_scope_rule() {
        let sys = build_gold_sys_prompt(
            "bigquery",
            "Google BigQuery (Standard SQL)",
            3,
            "           - If Provider is bigquery (Google BigQuery Standard SQL), never reference a SELECT-list alias inside another expression in the same SELECT list. If one derived field depends on another, split into CTE/subquery + outer SELECT.\n           - If Provider is bigquery, use SAFE_CAST(...) for tolerant casts (not try_cast).\n",
        );
        assert!(sys.contains("never reference a SELECT-list alias"));
        assert!(sys.contains("SAFE_CAST"));
    }

    #[tokio::test]
    async fn gold_model_writes_mart_and_injects_config() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let base = "t/w/p/dbt";
        // Seed an input staging model so the tool can ground the prompt.
        let stg_key = format!("{}/models/staging/stg_test_raw_raw_orders.sql", base);
        storage
            .put_bytes(
                &stg_key,
                "select * from {{ source('test_raw','raw_orders') }}".as_bytes(),
                "text/sql",
            )
            .await
            .expect("seed staging");

        let llm = Arc::new(MockLlm {
            resp: serde_json::json!({
                "sql": "select * from __INPUT_0__",
                "notes": analyst_notes()
            })
            .to_string(),
        });
        let ctx = make_ctx(storage.clone(), llm);
        let tool = GoldModelTool;

        let out = tool
            .call(
                serde_json::json!({
                    "items": [{
                        "name": "fct_orders",
                        "folder": "marts",
                        "goal": "Orders fact at order grain.",
                        "inputs": ["stg_test_raw_raw_orders"]
                    }]
                }),
                &ctx,
            )
            .await
            .expect("tool call");

        assert!(
            out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "out={}",
            out
        );
        let written = out
            .get("written_keys")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(written.len(), 1);
        let key = written[0].as_str().unwrap_or("").to_string();
        let bytes = storage.get_bytes(&key).await.expect("written file exists");
        let content = String::from_utf8_lossy(&bytes).to_string();
        // Hard-cutover portability: do not inject `schema=` into model configs (dbt_project.yml governs schema).
        assert!(!content.contains("config(schema="));
        assert!(content.contains("alias=\"fct_orders\""));
        assert!(content
            .to_ascii_lowercase()
            .contains("ref('stg_test_raw_raw_orders')"));
    }

    #[tokio::test]
    async fn gold_model_rejects_source_calls() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let base = "t/w/p/dbt";
        let stg_key = format!("{}/models/staging/stg_test_raw_raw_orders.sql", base);
        storage
            .put_bytes(
                &stg_key,
                "select * from {{ source('test_raw','raw_orders') }}".as_bytes(),
                "text/sql",
            )
            .await
            .expect("seed staging");

        let llm = Arc::new(MockLlm {
            resp: serde_json::json!({
                "sql": "select * from {{ source('test_raw','raw_orders') }}",
                "notes": analyst_notes()
            })
            .to_string(),
        });
        let ctx = make_ctx(storage.clone(), llm);
        let tool = GoldModelTool;

        let out = tool
            .call(
                serde_json::json!({
                    "items": [{
                        "name": "fct_orders",
                        "folder": "marts",
                        "goal": "Orders fact at order grain.",
                        "inputs": ["stg_test_raw_raw_orders"]
                    }]
                }),
                &ctx,
            )
            .await
            .expect("tool call");

        assert!(
            !out.get("ok").and_then(|v| v.as_bool()).unwrap_or(true),
            "out={}",
            out
        );
        let errs = out
            .get("errors")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(!errs.is_empty());
    }

    #[tokio::test]
    async fn gold_model_supports_multiple_inputs_and_missing_inputs_do_not_crash() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let base = "t/w/p/dbt";
        // Seed two input staging models; leave one missing to simulate partial availability.
        let stg_orders_key = format!("{}/models/staging/stg_test_raw_raw_orders.sql", base);
        let stg_users_key = format!("{}/models/staging/stg_test_raw_raw_users.sql", base);
        storage
            .put_bytes(
                &stg_orders_key,
                "select 1 as order_id".as_bytes(),
                "text/sql",
            )
            .await
            .expect("seed orders staging");
        storage
            .put_bytes(&stg_users_key, "select 1 as user_id".as_bytes(), "text/sql")
            .await
            .expect("seed users staging");

        let llm = Arc::new(MockLlm {
            resp: serde_json::json!({
                "sql": "select * from __INPUT_0__",
                "notes": analyst_notes()
            })
            .to_string(),
        });
        let ctx = make_ctx(storage.clone(), llm);
        let tool = GoldModelTool;

        let out = tool
            .call(
                serde_json::json!({
                    "items": [{
                        "name": "fct_orders",
                        "folder": "marts",
                        "goal": "Orders fact at order grain.",
                        "inputs": [
                            "stg_test_raw_raw_orders",
                            "stg_test_raw_raw_users",
                            "stg_test_raw_raw_missing"
                        ]
                    }]
                }),
                &ctx,
            )
            .await
            .expect("tool call");

        // Tool may still succeed (missing inputs are passed to the LLM as ok=false blocks).
        let written = out
            .get("written_keys")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(written.len(), 1);
    }

    #[tokio::test]
    async fn gold_model_uses_model_plan_invariants_and_notes_as_default_instructions() {
        #[derive(Clone)]
        struct CapturingLlm {
            resp: String,
            captured_instructions: Arc<Mutex<Option<String>>>,
        }
        impl LargeLanguageModel for CapturingLlm {
            fn chat(
                &self,
                messages: &[ChatMessage],
                _options: &react_core::llm::LlmCallOptions,
            ) -> Result<String, String> {
                let user = messages
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                if let Ok(v) = serde_json::from_str::<Value>(&user) {
                    let instr = v
                        .get("instructions")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string());
                    if let Ok(mut g) = self.captured_instructions.lock() {
                        *g = instr;
                    }
                }
                Ok(self.resp.clone())
            }
            fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
                Ok(vec![])
            }
        }

        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let base = "t/w/p/dbt";
        // Seed an input staging model so the tool can ground the prompt.
        let stg_key = format!("{}/models/staging/stg_test_raw_raw_orders.sql", base);
        storage
            .put_bytes(&stg_key, "select 1 as order_id".as_bytes(), "text/sql")
            .await
            .expect("seed staging");

        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let llm = Arc::new(CapturingLlm {
            resp: serde_json::json!({
                "sql": "select * from __INPUT_0__",
                "notes": analyst_notes()
            })
            .to_string(),
            captured_instructions: captured.clone(),
        });

        let mut ctx = make_ctx(storage.clone(), llm);
        ctx.thread_id = Some("t1".to_string());

        // Seed an approved model plan with invariants/checklist for this model.
        let plan_key = crate::data_engineer::plan::new_model_plan_key(&ctx);
        let plan = crate::data_engineer::plan::ModelPlan {
            plan_key: plan_key.clone(),
            status: crate::data_engineer::plan::PlanStatus::Approved,
            project_snapshot: serde_json::Value::Null,
            tasks: vec![crate::data_engineer::plan::ModelTask {
                name: "fct_orders".to_string(),
                folder: "marts".to_string(),
                goal: "Orders fact at order grain.".to_string(),
                inputs: vec!["stg_test_raw_raw_orders".to_string()],
                expected_model_path: Some("models/marts/fct_orders.sql".to_string()),
                invariants: vec!["Grain: exactly 1 row per order_pk.".to_string()],
                implementation_spec: crate::data_engineer::plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per order_id".to_string(),
                    inputs: vec!["stg_test_raw_raw_orders".to_string()],
                    joins: vec![],
                    metrics: vec![crate::data_engineer::plan::MetricSpec {
                        name: "orders".to_string(),
                        definition: "count(*) of orders".to_string(),
                        caveats: vec![],
                    }],
                    output_fields: vec![crate::data_engineer::plan::OutputFieldSpec {
                        name: "order_id".to_string(),
                        kind: crate::data_engineer::plan::FieldKind::Clean,
                        source_columns: vec!["order_id".to_string()],
                        expression: "order_id passthrough from staging".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                },
                status: crate::data_engineer::plan::TaskStatus::Pending,
                checklist: vec![crate::data_engineer::plan::PlanChecklistItem {
                    checklist_item_id: "sql_model".to_string(),
                    label: "Author gold SQL".to_string(),
                    details: Some(
                        "Filter out invalid orders based on silver validity flags.".to_string(),
                    ),
                    status: crate::data_engineer::plan::ChecklistItemStatus::Pending,
                    origin: crate::data_engineer::plan::ChecklistOrigin::Initial,
                    origin_step_idx: None,
                    evidence: vec![],
                }],
            }],
            batches: vec![vec!["fct_orders".to_string()]],
            work_groups: vec![],
            mutations: vec![],
            progress: crate::data_engineer::plan::PlanProgress::default(),
        };
        crate::data_engineer::plan::save_model_plan(&ctx, &plan)
            .await
            .unwrap();

        let tool = GoldModelTool;
        let out = tool
            .call(
                serde_json::json!({
                    "items": [{
                        "name": "fct_orders",
                        "folder": "marts",
                        "goal": "Orders fact at order grain.",
                        "inputs": ["stg_test_raw_raw_orders"]
                    }]
                }),
                &ctx,
            )
            .await
            .expect("tool call");

        assert!(out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
        let got = captured
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default();
        assert!(got.contains("Plan invariants"));
        assert!(got.contains("exactly 1 row"));
        assert!(got.contains("Plan checklist"));
        assert!(got.contains("validity flags"));
        assert!(!plan_key.trim().is_empty());
    }
}
