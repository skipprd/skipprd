use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tokio::task::JoinSet;
use tracing::info;

use react_core::agent::AgentCtx;
use react_core::keyspace::encode_key_component;
use react_core::tools::Tool;

use crate::dialect::active_provider_dialect;
use crate::{naming, plan, sql_first};

use super::model_authoring_engine::{self as engine, build_provider_prompt_rules, dedup_notes};

fn normalize_folder(folder: Option<&str>) -> String {
    match folder.unwrap_or("marts").trim().to_lowercase().as_str() {
        "core" => "core".to_string(),
        _ => "marts".to_string(),
    }
}

fn gold_model_rel_path(folder: &str, name: &str) -> String {
    format!("models/{}/{}.sql", folder, name)
}

fn existing_gold_model_path_for_name(
    name: &str,
    core_exists: bool,
    marts_exists: bool,
) -> Result<Option<String>, String> {
    if core_exists && marts_exists {
        return Err(format!(
            "{name}: model exists in both canonical folders (models/core and models/marts). Keep exactly one canonical location before authoring."
        ));
    }
    if core_exists {
        Ok(Some(gold_model_rel_path("core", name)))
    } else if marts_exists {
        Ok(Some(gold_model_rel_path("marts", name)))
    } else {
        Ok(None)
    }
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
    grounded_inputs: Vec<crate::plan_types::GroundedModelInput>,
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
         You MUST reference inputs ONLY via the provided placeholders (e.g. __INPUT_0__).\n\
           - Do NOT use ref() / source() / Jinja in this step.\n\
           - The system will replace placeholders with real silver relations for validation, then with dbt ref() for materialization.\n\
         \n\
         CRITICAL gold rules:\n\
         - You MUST write a SELECT-based dbt model.\n\
         - Gold models read from silver models (stg_*) or other gold models in the plan via ref(). NO source().\n\
         - Gold models MUST NOT call source() anywhere.\n\
         - IMPORTANT: The user payload may include plan invariants/notes; invariants are hard requirements.\n\
         - If the payload includes authoring_mode=\"targeted_patch_from_current_file\", preserve correct existing logic and apply only the required change.\n\
         - If authoring_mode=\"full_replacement_from_current_spec\", treat existing_model_sql as stale context only; write a fresh full query from the current plan_implementation_spec.\n\
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

use super::plan_prompt_helpers::{combine_instructions, render_plan_driven_instructions};

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
        let max_items = crate::plan_progress::MAX_BATCH_SIZE;
        if parsed_args.items.len() > max_items {
            return Err(format!(
                "gold_model supports at most {max_items} items per call (got {}). Split into batches.",
                parsed_args.items.len()
            ));
        }

        let dialect = crate::resolved_config_from_ctx(ctx)
            .map(active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());
        let provider_name = crate::resolved_config_from_ctx(ctx)
            .and_then(|cfg| crate::de_config::de_config_from_resolved(cfg))
            .map(|p| p.warehouse.kind.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let wh = crate::ctx_ext::actx_warehouse(ctx);
        let provider_prompt_rules = build_provider_prompt_rules(wh.as_ref().map(|w| w.as_ref()));
        let sys =
            build_gold_sys_prompt(&provider_name, &dialect, max_items, &provider_prompt_rules);

        // Plan-first authoring: if there is an active model plan, use task invariants/notes as the
        // default authoring instructions (and merge with any explicit item.instructions overrides).
        let plan_opt = plan::load_model_plan(ctx).await.ok().flatten();
        let global_semantic_context = ctx
            .storage()
            .get_json(&ctx.keyspace().scoped_key(
                ctx.scope(),
                &[
                    "semantic",
                    &format!(
                        "{}.yaml",
                        encode_key_component(crate::providers::GLOBAL_SEMANTIC_DATASET_ID)
                    ),
                ],
            ))
            .await
            .ok()
            .unwrap_or(serde_json::Value::Null);

        let base = ctx
            .keyspace()
            .scoped_prefix(ctx.scope(), &["dbt"])
            .trim_end_matches('/')
            .to_string();
        let query = crate::ctx_ext::actx_warehouse(ctx)
            .expect("warehouse provider required for gold_model");
        let mut written: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut succeeded_item_names: Vec<String> = Vec::new();
        let mut canonical_folder_by_name: HashMap<String, String> = HashMap::new();
        let mut plan_output_field_names: Vec<String> = Vec::new();

        for it in parsed_args.items.iter() {
            let name = it.name.trim();
            if name.is_empty() {
                errors.push("gold_model item.name is required".to_string());
                continue;
            }
            if it.inputs.is_empty() {
                errors.push(format!("{name}: gold_model item.inputs is required (list of model names: stg_* or intra-plan gold)"));
                continue;
            }

            let plan_task_opt = plan_opt
                .as_ref()
                .and_then(|p| p.tasks.iter().find(|t| t.name.trim() == name));
            let plan_expected_path_early = plan_task_opt
                .and_then(|t| t.expected_model_path.clone())
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
            let requested_folder = normalize_folder(it.folder.as_deref());
            let core_rel = gold_model_rel_path("core", name);
            let marts_rel = gold_model_rel_path("marts", name);
            let core_exists = ctx
                .storage()
                .get_bytes(&format!("{}/{}", base, core_rel))
                .await
                .is_ok();
            let marts_exists = ctx
                .storage()
                .get_bytes(&format!("{}/{}", base, marts_rel))
                .await
                .is_ok();
            let existing_rel =
                match existing_gold_model_path_for_name(name, core_exists, marts_exists) {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                };
            let rel_path = if let Some(path) = plan_expected_path_early.as_ref() {
                path.clone()
            } else {
                let folder = existing_rel
                    .as_deref()
                    .and_then(|rel| {
                        if rel.contains("/core/") {
                            Some("core")
                        } else if rel.contains("/marts/") {
                            Some("marts")
                        } else {
                            None
                        }
                    })
                    .unwrap_or(requested_folder.as_str())
                    .to_string();
                gold_model_rel_path(&folder, name)
            };
            let folder = if rel_path.contains("/core/") {
                "core".to_string()
            } else if rel_path.contains("/marts/") {
                "marts".to_string()
            } else {
                requested_folder
            };
            if let Some(existing_rel) = existing_rel.as_deref() {
                if existing_rel != rel_path {
                    errors.push(format!(
                        "{name}: existing model path '{existing_rel}' conflicts with approved plan path '{rel_path}'. Move or remove the stale artifact before authoring."
                    ));
                    continue;
                }
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

            // Load the inputs to ground the LLM in actual silver SQL.
            let effective_grounded_inputs = if it.grounded_inputs.is_empty() {
                plan_opt
                    .as_ref()
                    .and_then(|p| p.tasks.iter().find(|t| t.name.trim() == name))
                    .map(|t| t.grounded_inputs.clone())
                    .unwrap_or_default()
            } else {
                it.grounded_inputs.clone()
            };
            let grounded_by_input: std::collections::BTreeMap<
                String,
                crate::plan_types::GroundedModelInput,
            > = effective_grounded_inputs
                .iter()
                .cloned()
                .map(|g| (g.input_name.trim().to_string(), g))
                .collect();
            let max_fetch_concurrency = 3usize;
            let storage = ctx.storage().clone();
            let mut set: JoinSet<(usize, String, String, String, String, Vec<(String, String)>)> =
                JoinSet::new();
            // (idx, input, rel_path, content, relation_fqn, schema_cols)
            let mut fetched: Vec<(usize, String, String, String, String, Vec<(String, String)>)> =
                Vec::new();
            for (idx, inp) in it.inputs.iter().cloned().enumerate() {
                let Some(grounded) = grounded_by_input.get(inp.trim()).cloned() else {
                    errors.push(format!(
                        "{name}: missing grounded input relation for '{inp}' in approved model plan"
                    ));
                    continue;
                };
                while set.len() >= max_fetch_concurrency {
                    if let Some(res) = set.join_next().await {
                        if let Ok(v) = res {
                            fetched.push(v);
                        }
                    }
                }
                let storage2 = storage.clone();
                let rel = grounded.model_rel_path.clone();
                let key = format!("{}/{}", base, rel);
                let relation_fqn = grounded.relation_fqn.clone();
                let schema_cols: Vec<(String, String)> = grounded
                    .source_schema
                    .iter()
                    .map(|c| (c.name.clone(), c.data_type.clone()))
                    .collect();
                set.spawn(async move {
                    let content = storage2
                        .get_bytes(&key)
                        .await
                        .ok()
                        .map(|b| String::from_utf8_lossy(&b).to_string())
                        .unwrap_or_default();

                    (idx, inp, rel, content, relation_fqn, schema_cols)
                });
            }

            while let Some(res) = set.join_next().await {
                if let Ok(v) = res {
                    fetched.push(v);
                }
            }
            fetched.sort_by_key(|(idx, _, _, _, _, _)| *idx);
            let mut input_blocks: Vec<Value> = Vec::new();
            for (_idx, inp, rel, content, relation_fqn, schema_cols) in fetched.into_iter() {
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
                        "relation_fqn": relation_fqn,
                        "schema_columns": cols_json
                    }));
                } else {
                    input_blocks.push(serde_json::json!({
                        "input": inp,
                        "path": rel,
                        "ok": true,
                        "sql": truncate(&content, 20_000),
                        "relation_fqn": relation_fqn,
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
                plan_spec_digest,
            ) = plan_task_opt
                .map(|t| {
                    (
                        t.invariants.clone(),
                        t.checklist.clone(),
                        t.expected_model_path.clone().unwrap_or_default(),
                        t.implementation_spec.clone(),
                        plan_opt.as_ref().and_then(|p| {
                            crate::authoring_contract::model_task_spec_digest(&p.plan_key, t)
                        }),
                    )
                })
                .unwrap_or_else(|| (vec![], vec![], String::new(), None, None));
            if plan_output_field_names.is_empty() {
                if let Some(spec) = plan_implementation_spec.as_ref() {
                    plan_output_field_names =
                        spec.output_fields.iter().map(|f| f.name.clone()).collect();
                }
            }
            let plan_instr = render_plan_driven_instructions(&plan_invariants, &plan_checklist);
            let effective_instructions = combine_instructions(&it.instructions, &plan_instr);
            let existing_model_sql = ctx
                .storage()
                .get_bytes(&format!("{}/{}", base, rel_path))
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();
            let drift_reasons = plan_task_opt
                .map(|t| {
                    plan_opt
                        .as_ref()
                        .map(|p| {
                            crate::authoring_contract::model_sql_drift_reasons(
                                &p.plan_key,
                                t,
                                &existing_model_sql,
                            )
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let authoring_mode = if existing_model_sql.trim().is_empty() {
                "new_file"
            } else if crate::authoring_contract::model_sql_is_high_drift(&drift_reasons) {
                "full_replacement_from_current_spec"
            } else {
                "targeted_patch_from_current_file"
            };

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
                "existing_model_sql": existing_model_sql,
                "existing_model_drift_reasons": drift_reasons,
                "authoring_mode": authoring_mode,
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

            // Build placeholder replacement map for validation (placeholders -> quoted silver relations).
            let mut repl_validate: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let mut repl_materialize: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for (idx, inp) in it.inputs.iter().enumerate() {
                let ph = format!("__INPUT_{}__", idx);
                let relation_fqn = input_blocks
                    .iter()
                    .find(|b| b.get("input").and_then(|v| v.as_str()) == Some(inp.as_str()))
                    .and_then(|b| b.get("relation_fqn").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if relation_fqn.is_empty() {
                    errors.push(format!(
                        "{name}: cannot validate gold SQL: missing grounded relation_fqn for input '{inp}'"
                    ));
                    continue;
                }
                let id = match query.parse_dataset_fqn(&relation_fqn) {
                    Ok(id) => id,
                    Err(e) => {
                        errors.push(format!(
                            "{name}: invalid grounded relation_fqn '{relation_fqn}' for input '{inp}': {e}"
                        ));
                        continue;
                    }
                };
                repl_validate.insert(ph.clone(), query.format_dbt_model_relation_fqn(&id));
                if inp.trim().contains('/') || inp.trim().ends_with(".sql") {
                    errors.push(format!(
                        "{name}: gold inputs must be model names, not paths ('{inp}')"
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

            let has_unmaterialized_gold_dep = it
                .inputs
                .iter()
                .any(|inp| !crate::dataset_truth::is_staging_model_name(inp.trim()));
            let loop_config = engine::AuthorLoopConfig {
                max_tokens: max_tokens as usize,
                max_attempts,
                initial_prompt_id: "data_engineer.tools.gold_model.sql_first",
                repair_prompt_id: "data_engineer.tools.gold_model.sql_first_repair",
                reasoning_effort: crate::env_util::author_reasoning_effort(false),
                skip_warehouse_validation: has_unmaterialized_gold_dep,
            };
            let sys2 = sys.clone();
            let loop_result = engine::sql_first_author_loop(
                ctx,
                &loop_config,
                || sys2.clone(),
                &user_value,
                &repl_validate,
                name,
                &rel_path,
                |_d| Ok(()),
            )
            .await;
            let outcome = match loop_result {
                Ok(o) => o,
                Err(errs) => {
                    errors.extend(errs);
                    continue;
                }
            };
            let existing_sql = existing_model_sql;

            let write_result = engine::compile_and_write_model(
                ctx,
                &outcome.draft,
                plan_implementation_spec
                    .as_ref()
                    .map(|s| s.output_fields.as_slice())
                    .unwrap_or(&[]),
                &repl_materialize,
                |dbt_sql| {
                    if naming::contains_source_call(dbt_sql) {
                        return Err("invalid gold SQL: contains source(). Gold must only read from other models via ref().".to_string());
                    }
                    if !naming::contains_ref_call(dbt_sql) {
                        return Err("invalid gold SQL: must reference at least one model via ref().".to_string());
                    }
                    Ok(())
                },
                &existing_sql,
                &rel_path,
                plan_spec_digest.as_deref(),
            )
            .await;
            match write_result {
                Ok(result) => {
                    written.push(result.key);
                    succeeded_item_names.push(name.to_string());
                    for n in result.notes {
                        let nt = n.trim();
                        if !nt.is_empty() {
                            notes.push(format!("{name}: {nt}"));
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("{name}: {e}"));
                    continue;
                }
            }
        }

        let out_notes = dedup_notes(notes, 50);

        info!(
            target: "gold_model",
            items = parsed_args.items.len(),
            written = written.len(),
            ok = errors.is_empty(),
            "gold_model finished"
        );

        let classified_kind =
            errors
                .iter()
                .fold(crate::failure_kind::FailureKind::Unknown, |acc, e| {
                    let rhs =
                        crate::tools::batch_sql_runner::classify_authoring_batch_failure_kind(e);
                    if acc.is_transient() || rhs.is_transient() {
                        crate::failure_kind::FailureKind::InfraTransient
                    } else {
                        acc
                    }
                });
        let mut result = serde_json::json!({
            "ok": errors.is_empty(),
            "batch_failure_kind": if errors.is_empty() {
                Value::Null
            } else {
                serde_json::to_value(classified_kind)
                    .unwrap_or_else(|_| Value::String("unknown".to_string()))
            },
            "written_keys": written,
            "notes": out_notes,
            "errors": errors,
            "succeeded_item_names": succeeded_item_names
        });
        if !errors.is_empty() && !plan_output_field_names.is_empty() {
            result["expected_output_fields"] = serde_json::json!(plan_output_field_names);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{QueryProvider, QueryResult};
    use async_trait::async_trait;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::ChatMessage;
    use react_core::llm::LargeLanguageModel;
    use react_core::scope::RequestScope;
    use react_core::storage::StorageAdapter;
    use react_module_storage_memory::InMemoryStorageAdapter;
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
    impl crate::providers::DatasetCatalogProvider for MockWarehouse {
        async fn list_datasets(&self) -> Result<Vec<crate::providers::DatasetId>, String> {
            Ok(vec![])
        }
        async fn get_dataset_schema(
            &self,
            dataset: &crate::providers::DatasetId,
        ) -> Result<Vec<(String, String)>, String> {
            self.schema(&dataset.fqn()).await
        }
        async fn get_dataset_stats(
            &self,
            _dataset: &crate::providers::DatasetId,
            _max_fields: usize,
        ) -> Result<
            (
                crate::providers::DatasetFieldStats,
                crate::providers::DatasetStats,
            ),
            String,
        > {
            Err("not used".to_string())
        }

        fn evidence_capabilities(&self) -> crate::providers::ProviderEvidenceCapabilities {
            crate::providers::ProviderEvidenceCapabilities::schema_only("mock warehouse provider")
        }
    }

    impl crate::providers::WarehouseNaming for MockWarehouse {
        fn kind(&self) -> crate::de_config::WarehouseKind {
            crate::de_config::WarehouseKind::default()
        }
        fn parse_dataset_fqn(
            &self,
            dataset_fqn: &str,
        ) -> Result<crate::providers::DatasetId, String> {
            let parts: Vec<&str> = dataset_fqn.split('.').collect();
            if parts.len() != 3 {
                return Err("invalid fqn".to_string());
            }
            Ok(crate::providers::DatasetId {
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

    fn minimal_cfg() -> Arc<react_core::resolved_config::ReactResolvedConfig> {
        Arc::new(react_core::resolved_config::ReactResolvedConfig {
            server: react_core::resolved_config::ServerResolved { port: 1 },
            storage: react_core::resolved_config::StorageResolved {
                mode: react_core::resolved_config::StorageMode::Local,
                bucket: None,
                path: None,
                s3_credentials: None,
            },
            scope: RequestScope::parse("t", "w", "p").expect("valid test scope"),
            llm: react_core::resolved_config::LlmResolved::default(),
            suite_config: serde_json::json!({
                "warehouse": { "kind": "athena", "container": "AwsDataCatalog", "namespace": "test_raw", "extras": {"region":"eu-west-1","workgroup":"wg","result_s3":"s3://x/"} },
                "catalog": { "enabled": false, "refresh_secs": 60, "max_concurrency": 8 },
                "dbt": { "enabled": true, "target": "athena", "naming": { "target_schema": "test", "silver_suffix": "silver", "gold_suffix": "gold" }, "runner": "host" },
                "vector": { "enabled": false }
            }),
        })
    }

    fn make_ctx(storage: Arc<dyn StorageAdapter>, llm: Arc<dyn LargeLanguageModel>) -> AgentCtx {
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
        let warehouse: Arc<dyn crate::providers::WarehouseProvider> =
            Arc::new(MockWarehouse::default());
        let mut actx = react_core::agent::AgentCtxBuilder::new(
            llm,
            storage,
            scope.clone(),
            keyspace,
            Arc::new(react_core::agent::DefaultPolicy),
        )
        .top_k(1)
        .per_step_timeout_secs(1)
        .max_steps(1)
        .agent_name("test".to_string())
        .resolved_config(Some(minimal_cfg()))
        .build();
        actx.set_capability(Arc::new(crate::ctx_ext::WarehouseCap(warehouse)));
        actx
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

    #[test]
    fn existing_gold_model_path_detects_folder_collision() {
        let err = existing_gold_model_path_for_name("fct_orders", true, true)
            .expect_err("core/marts duplicate should be rejected");

        assert!(err.contains("both canonical folders"));
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
                        "inputs": ["stg_test_raw_raw_orders"],
                        "grounded_inputs": [{
                            "input_name": "stg_test_raw_raw_orders",
                            "model_rel_path": "models/staging/stg_test_raw_raw_orders.sql",
                            "relation_fqn": "catalog.db.stg_test_raw_raw_orders",
                            "source_schema": [{"name": "order_id", "data_type": "bigint"}]
                        }]
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
                        "inputs": ["stg_test_raw_raw_orders"],
                        "grounded_inputs": [{
                            "input_name": "stg_test_raw_raw_orders",
                            "model_rel_path": "models/staging/stg_test_raw_raw_orders.sql",
                            "relation_fqn": "catalog.db.stg_test_raw_raw_orders",
                            "source_schema": [{"name": "order_id", "data_type": "bigint"}]
                        }]
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
                        ],
                        "grounded_inputs": [
                            {
                                "input_name": "stg_test_raw_raw_orders",
                                "model_rel_path": "models/staging/stg_test_raw_raw_orders.sql",
                                "relation_fqn": "catalog.db.stg_test_raw_raw_orders",
                                "source_schema": [{"name": "order_id", "data_type": "bigint"}]
                            },
                            {
                                "input_name": "stg_test_raw_raw_users",
                                "model_rel_path": "models/staging/stg_test_raw_raw_users.sql",
                                "relation_fqn": "catalog.db.stg_test_raw_raw_users",
                                "source_schema": [{"name": "user_id", "data_type": "bigint"}]
                            },
                            {
                                "input_name": "stg_test_raw_raw_missing",
                                "model_rel_path": "models/staging/stg_test_raw_raw_missing.sql",
                                "relation_fqn": "catalog.db.stg_test_raw_raw_missing",
                                "source_schema": []
                            }
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
                    .find(|m| m.role == react_core::llm::ChatRole::User)
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
        ctx.set_thread_id(Some("t1".to_string()));

        // Seed an approved model plan with invariants/checklist for this model.
        let plan_key = crate::plan::new_model_plan_key(&ctx);
        let plan = crate::plan::ModelPlan {
            plan_key: plan_key.clone(),
            status: crate::plan::PlanStatus::Approved,
            project_snapshot: Default::default(),
            tasks: vec![crate::plan::ModelTask {
                name: "fct_orders".to_string(),
                folder: crate::plan::ModelFolder::Marts,
                goal: "Orders fact at order grain.".to_string(),
                inputs: vec!["stg_test_raw_raw_orders".to_string()],
                expected_model_path: Some("models/marts/fct_orders.sql".to_string()),
                invariants: vec!["Grain: exactly 1 row per order_pk.".to_string()],
                implementation_spec: Some(crate::plan::ModelImplementationSpec {
                    spec_version: 1,
                    grain: "1 row per order_id".to_string(),
                    inputs: vec!["stg_test_raw_raw_orders".to_string()],
                    joins: vec![],
                    metrics: vec![crate::plan::MetricSpec {
                        name: "orders".to_string(),
                        definition: "count(*) of orders".to_string(),
                        source_fields: vec!["order_id".to_string()],
                        caveats: vec![],
                    }],
                    output_fields: vec![crate::plan::OutputFieldSpec {
                        name: "order_id".to_string(),
                        kind: crate::plan::FieldKind::Clean,
                        lineage: vec![],
                        source_columns: vec!["order_id".to_string()],
                        expression: "order_id passthrough from staging".to_string(),
                        data_type: None,
                        nullable: true,
                        description: None,
                    }],
                    assumptions: vec![],
                    evidence_claim_refs: vec![crate::providers::SemanticClaimRef {
                        claim_id: "candidate_key:test_raw.raw_orders:order_id"
                            .to_string()
                            .into(),
                        kind: crate::providers::SemanticClaimKind::CandidateKey,
                        status: crate::providers::EvidenceStatus::Observed,
                    }],
                }),
                source_schema: vec![],
                grounded_inputs: vec![crate::plan_types::GroundedModelInput {
                    input_name: "stg_test_raw_raw_orders".to_string(),
                    model_rel_path: "models/staging/stg_test_raw_raw_orders.sql".to_string(),
                    relation_fqn: "AwsDataCatalog.test_silver.stg_test_raw_raw_orders".to_string(),
                    source_schema: vec![crate::plan_types::SourceColumnDef {
                        name: "order_id".to_string(),
                        data_type: "bigint".to_string(),
                    }],
                }],
                status: crate::plan::TaskStatus::Pending,
                checklist: vec![
                    crate::plan::PlanChecklistItem {
                        checklist_item_id: "sql_model".to_string(),
                        label: "Author gold SQL".to_string(),
                        details: Some(
                            "Filter out invalid orders based on silver validity flags.".to_string(),
                        ),
                        status: crate::plan::ChecklistItemStatus::Pending,
                        origin: crate::plan::ChecklistOrigin::Initial,
                        evidence: vec![],
                    },
                    crate::plan::PlanChecklistItem {
                        checklist_item_id: "schema_contract".to_string(),
                        label: "Author schema contract".to_string(),
                        details: None,
                        status: crate::plan::ChecklistItemStatus::Pending,
                        origin: crate::plan::ChecklistOrigin::Initial,
                        evidence: vec![],
                    },
                    crate::plan::PlanChecklistItem {
                        checklist_item_id: "validate".to_string(),
                        label: "Validate model".to_string(),
                        details: None,
                        status: crate::plan::ChecklistItemStatus::Pending,
                        origin: crate::plan::ChecklistOrigin::Initial,
                        evidence: vec![],
                    },
                ],
            }],
            batches: vec![vec!["fct_orders".to_string()]],
            work_groups: vec![
                crate::plan::PlanWorkGroup {
                    group_id: "wg_sql".to_string(),
                    label: "Author SQL".to_string(),
                    kind: crate::plan::WorkGroupKind::AuthorSql,
                    items: vec![crate::plan::WorkGroupItemRef {
                        task_id: "fct_orders".to_string(),
                        checklist_item_id: "sql_model".to_string(),
                    }],
                    depends_on_group_ids: None,
                },
                crate::plan::PlanWorkGroup {
                    group_id: "wg_schema".to_string(),
                    label: "Author schema".to_string(),
                    kind: crate::plan::WorkGroupKind::AuthorSchema,
                    items: vec![crate::plan::WorkGroupItemRef {
                        task_id: "fct_orders".to_string(),
                        checklist_item_id: "schema_contract".to_string(),
                    }],
                    depends_on_group_ids: Some(vec!["wg_sql".to_string()]),
                },
                crate::plan::PlanWorkGroup {
                    group_id: "wg_validate".to_string(),
                    label: "Validate".to_string(),
                    kind: crate::plan::WorkGroupKind::Validate,
                    items: vec![crate::plan::WorkGroupItemRef {
                        task_id: "fct_orders".to_string(),
                        checklist_item_id: "validate".to_string(),
                    }],
                    depends_on_group_ids: Some(vec!["wg_schema".to_string()]),
                },
            ],
            mutations: vec![],
            progress: crate::plan::PlanProgress::default(),
        };
        crate::plan::save_model_plan(&ctx, &plan).await.unwrap();

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
