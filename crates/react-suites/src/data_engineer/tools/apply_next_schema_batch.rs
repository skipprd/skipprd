use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::providers::DatasetCatalogProvider;
use react_core::tools::Tool;

use crate::data_engineer::naming;
use crate::data_engineer::plan;
use crate::data_engineer::plan::CleansePlan;
use crate::data_engineer::project_files;
use crate::data_engineer::project_fs;
use crate::data_engineer::tools::dbt_files;

fn parse_dataset_id_3(s: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = s.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let cat = parts[0].trim();
    let schema = parts[1].trim();
    let table = parts[2].trim();
    if cat.is_empty() || schema.is_empty() || table.is_empty() {
        return None;
    }
    Some((cat.to_string(), schema.to_string(), table.to_string()))
}

fn extract_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn mark_in_progress_cleanse_schema(plan: &mut CleansePlan, dataset_ids: &[String]) {
    for ds in dataset_ids.iter() {
        plan::cleanse_schema_contract_mark_in_progress(plan, ds);
    }
}

fn update_failure_counters(progress: &mut plan::PlanProgress, ok: bool) {
    if ok {
        progress.consecutive_batch_failures = 0;
        return;
    }
    progress.consecutive_batch_failures = progress.consecutive_batch_failures.saturating_add(1);
    progress.total_batch_failures = progress.total_batch_failures.saturating_add(1);
}

fn schema_yml_sys_prompt_staging() -> String {
    [
        "You are an expert analytics engineer.",
        "Task: author a dbt *staging/silver schema* YAML for ONE staging model file.",
        "Requirements:",
        "- Output MUST be valid JSON only.",
        "- Choose EXACTLY ONE patch primitive: replace_file OR replace_range OR replace_list.",
        "- Patch MUST modify ONLY expected_rel_path.",
        "- Do NOT add or reference columns not present in allowed_columns.",
        "- IMPORTANT: contract enforcement is disabled. Do NOT set models[].config.contract.enforced=true.",
        "- Prefer including all allowed_columns under models[].columns, but it is OK if some are missing while iterating.",
        "- data_type is optional (preferred when known, omit rather than guessing).",
        "- Column names must match allowed_columns exactly (no comments or helper markers as column names).",
    ]
    .join("\n")
}

fn schema_yml_sys_prompt_models_schema_yml() -> String {
    [
        "You are an expert analytics engineer.",
        "Task: update models/schema.yml to add or update dbt model documentation/tests for a small set of gold models.",
        "Requirements:",
        "- Output MUST be valid JSON only.",
        "- Choose EXACTLY ONE patch primitive: replace_file OR replace_range OR replace_list.",
        "- Patch MUST modify ONLY expected_rel_path.",
        "- Do NOT create additional YAML files; use models/schema.yml only.",
        "- Keep output concise: only touch the specified model names; preserve existing content unrelated to those models.",
    ]
    .join("\n")
}

#[derive(Clone)]
pub struct ApplyNextCleanseSchemaBatchTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

#[async_trait]
impl Tool for ApplyNextCleanseSchemaBatchTool {
    fn name(&self) -> &'static str {
        "apply_next_cleanse_schema_batch"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let mut plan = plan::load_cleanse_plan_any(ctx)
            .await
            .ok_or_else(|| "no active cleanse plan found".to_string())?;
        if plan.status != plan::PlanStatus::Approved && plan.status != plan::PlanStatus::Completed {
            return Err(format!(
                "cleanse plan is not approved (status={:?}); return to plan phase",
                plan.status
            ));
        }

        let batch = plan::cleanse_pending_schema_contracts(&plan);
        if batch.is_empty() {
            return Ok(serde_json::json!({
                "ok": true,
                "message": "no pending staging schema contract work (all done)",
                "attempted_dataset_ids": [],
                "succeeded_dataset_ids": [],
                "failed_dataset_ids": [],
            }));
        }

        let instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"))
            .unwrap_or_default();

        mark_in_progress_cleanse_schema(&mut plan, &batch);
        let _ = plan::save_cleanse_plan(ctx, &plan).await;

        let mut succeeded: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        for ds in batch.iter() {
            let Some((_cat, schema, table)) = parse_dataset_id_3(ds) else {
                failed.push(ds.clone());
                errors.push(format!("{ds}: invalid dataset_id (expected <catalog>.<schema>.<table>)"));
                continue;
            };

            let model_name = naming::canonical_staging_model_name(&schema, &table);
            let sql_rel = naming::canonical_staging_rel_path(&schema, &table);
            let yml_rel = format!("models/staging/{}.yml", model_name);

            let sql_key = project_fs::join_storage_key(ctx, &sql_rel);
            let sql_text = match ctx.storage.get_bytes(&sql_key).await {
                Ok(b) => String::from_utf8_lossy(&b).to_string(),
                Err(_) => {
                    failed.push(ds.clone());
                    errors.push(format!("{ds}: missing sibling SQL {sql_rel}"));
                    continue;
                }
            };
            let allowed_cols = match dbt_files::extract_final_select_output_columns(&sql_text) {
                Ok(s) => s.into_iter().collect::<Vec<_>>(),
                Err(e) => {
                    failed.push(ds.clone());
                    errors.push(format!("{ds}: cannot parse allowed output columns from {sql_rel}: {e}"));
                    continue;
                }
            };
            let mut allowed_cols = allowed_cols;
            allowed_cols.sort();
            allowed_cols.dedup();

            let user_payload = serde_json::json!({
                "dataset_id": ds,
                "model_name": model_name,
                "expected_model_sql_path": sql_rel,
                "allowed_columns": allowed_cols,
                "instructions": instructions,
            })
            .to_string();

            let (outcome, _notes) = match crate::data_engineer::patch_protocol::llm_patch_loop_single_file(
                ctx,
                self.datasets.as_ref(),
                schema_yml_sys_prompt_staging(),
                user_payload,
                &yml_rel,
                4,
            )
            .await {
                Ok(v) => v,
                Err(e) => {
                    failed.push(ds.clone());
                    errors.push(format!("{ds}: schema patch failed: {e}"));
                    continue;
                }
            };

            // Validate schema contract against sibling SQL output columns.
            if let Err(e) =
                dbt_files::validate_staging_schema_ymls(ctx, std::slice::from_ref(&outcome)).await
            {
                failed.push(ds.clone());
                errors.push(format!("{ds}: invalid staging schema yml: {e}"));
                continue;
            }

            let yml_key = project_fs::join_storage_key(ctx, &yml_rel);
            if let Err(e) = ctx
                .storage
                .put_bytes(&yml_key, outcome.content.as_bytes(), "text/yaml")
                .await
            {
                failed.push(ds.clone());
                errors.push(format!("{ds}: failed to write {yml_rel}: {e}"));
                continue;
            }

            succeeded.push(ds.clone());
        }

        for ds in succeeded.iter() {
            plan::cleanse_schema_contract_mark_done(&mut plan, ds);
        }
        for ds in failed.iter() {
            plan::cleanse_schema_contract_mark_needs_update(&mut plan, ds);
        }
        update_failure_counters(&mut plan.progress, failed.is_empty());
        let _ = plan::save_cleanse_plan(ctx, &plan).await;

        Ok(serde_json::json!({
            "ok": failed.is_empty(),
            "attempted_dataset_ids": batch,
            "succeeded_dataset_ids": succeeded,
            "failed_dataset_ids": failed,
            "errors": errors,
        }))
    }
}

#[derive(Clone)]
pub struct ApplyNextModelSchemaBatchTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
}

#[async_trait]
impl Tool for ApplyNextModelSchemaBatchTool {
    fn name(&self) -> &'static str {
        "apply_next_model_schema_batch"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let mut plan = plan::load_model_plan_any(ctx)
            .await
            .ok_or_else(|| "no active model plan found".to_string())?;
        if plan.status != plan::PlanStatus::Approved && plan.status != plan::PlanStatus::Completed {
            return Err(format!(
                "model plan is not approved (status={:?}); return to plan phase",
                plan.status
            ));
        }

        let names = plan::model_pending_schema_contracts(&plan);
        if names.is_empty() {
            return Ok(serde_json::json!({
                "ok": true,
                "message": "no pending model schema contract work (all done)",
                "attempted_item_names": [],
                "succeeded_item_names": [],
                "failed_item_names": [],
            }));
        }

        let instructions = extract_string_arg(&args, "instructions")
            .or_else(|| extract_string_arg(&args, "user_instructions"))
            .unwrap_or_default();

        for n in names.iter() {
            plan::model_schema_contract_mark_in_progress(&mut plan, n);
        }
        let _ = plan::save_model_plan(ctx, &plan).await;

        let attempted_names = names.clone();
        let expected_rel = project_files::MODELS_SCHEMA_YML;

        // Provide just the model names + expected SQL rel paths to keep the patch focused.
        let mut models: Vec<Value> = Vec::new();
        for n in names.iter() {
            if let Some(t) = plan.tasks.iter().find(|t| t.name == *n) {
                models.push(serde_json::json!({
                    "name": t.name,
                    "folder": t.folder,
                    "expected_model_path": t.expected_model_path,
                    "goal": t.goal,
                    "inputs": t.inputs,
                    "invariants": t.invariants,
                }));
            } else {
                models.push(serde_json::json!({ "name": n }));
            }
        }
        let user_payload = serde_json::json!({
            "models": models,
            "instructions": instructions,
            "instruction": "Add or update entries under top-level 'models:' for these names only. Keep other models untouched.",
        })
        .to_string();

        let (outcome, _notes) = match crate::data_engineer::patch_protocol::llm_patch_loop_single_file(
            ctx,
            self.datasets.as_ref(),
            schema_yml_sys_prompt_models_schema_yml(),
            user_payload,
            expected_rel,
            4,
        )
        .await {
            Ok(v) => v,
            Err(e) => {
                for n in names.iter() {
                    plan::model_schema_contract_mark_needs_update(&mut plan, n);
                }
                update_failure_counters(&mut plan.progress, false);
                let _ = plan::save_model_plan(ctx, &plan).await;
                return Ok(serde_json::json!({
                    "ok": false,
                    "attempted_item_names": attempted_names.clone(),
                    "succeeded_item_names": [],
                    "failed_item_names": attempted_names,
                    "errors": [format!("models/schema.yml patch failed: {e}")],
                }));
            }
        };

        let key = project_fs::join_storage_key(ctx, expected_rel);
        if let Err(e) = ctx.storage
            .put_bytes(&key, outcome.content.as_bytes(), "text/yaml")
            .await
        {
            for n in names.iter() {
                plan::model_schema_contract_mark_needs_update(&mut plan, n);
            }
            update_failure_counters(&mut plan.progress, false);
            let _ = plan::save_model_plan(ctx, &plan).await;
            return Ok(serde_json::json!({
                "ok": false,
                "attempted_item_names": attempted_names.clone(),
                "succeeded_item_names": [],
                "failed_item_names": attempted_names,
                "errors": [format!("failed to write {}: {}", expected_rel, e)],
            }));
        }

        for n in names.iter() {
            plan::model_schema_contract_mark_done(&mut plan, n);
        }
        update_failure_counters(&mut plan.progress, true);
        let _ = plan::save_model_plan(ctx, &plan).await;

        Ok(serde_json::json!({
            "ok": true,
            "attempted_item_names": attempted_names.clone(),
            "succeeded_item_names": attempted_names,
            "failed_item_names": [],
            "errors": [],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::ChatMessage;
    use react_core::llm::LargeLanguageModel;
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::sync::Arc;
    use std::sync::{Mutex};

    #[derive(Default)]
    struct ScriptedLlm {
        replies: Mutex<Vec<String>>,
    }

    impl LargeLanguageModel for ScriptedLlm {
        fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
            let mut g = self.replies.lock().map_err(|_| "mutex poisoned".to_string())?;
            if g.is_empty() {
                return Err("no more replies".to_string());
            }
            Ok(g.remove(0))
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

    #[tokio::test]
    async fn apply_next_cleanse_schema_batch_writes_canonical_staging_yml() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![serde_json::json!({
                "replace_file": {
                    "path": "models/staging/stg_test_raw_raw_customers.yml",
                    "new_text": "version: 2\n\nmodels:\n  - name: stg_test_raw_raw_customers\n    columns:\n      - name: customer_id_raw\n      - name: email_raw\n"
                }
            }).to_string()]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };

        // Seed a cleanse plan with sql_model done and schema_contract pending.
        let plan_key = plan::new_cleanse_plan_key(&ctx);
        let p = plan::CleansePlan {
            plan_key: plan_key.clone(),
            status: plan::PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_customers".to_string(),
                expected_model_path: Some("models/staging/stg_test_raw_raw_customers.sql".to_string()),
                invariants: vec![],
                status: plan::TaskStatus::InProgress,
                checklist: vec![
                    plan::PlanChecklistItem {
                        checklist_item_id: "sql_model".to_string(),
                        label: "Author staging SQL model".to_string(),
                        details: None,
                        status: plan::ChecklistItemStatus::Done,
                        origin: plan::ChecklistOrigin::Initial,
                        origin_step_idx: None,
                        evidence: vec![],
                    },
                    plan::PlanChecklistItem {
                        checklist_item_id: "schema_contract".to_string(),
                        label: "Define staging schema contract".to_string(),
                        details: None,
                        status: plan::ChecklistItemStatus::Pending,
                        origin: plan::ChecklistOrigin::Initial,
                        origin_step_idx: None,
                        evidence: vec![],
                    },
                ],
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_customers".to_string()]],
            work_groups: vec![],
            progress: plan::PlanProgress::default(),
        };
        plan::save_cleanse_plan(&ctx, &p).await.unwrap();

        // Seed canonical staging SQL with explicit final SELECT list (no '*').
        let sql_rel = "models/staging/stg_test_raw_raw_customers.sql";
        let sql_key = project_fs::join_storage_key(&ctx, sql_rel);
        let sql = "with source as (\n  select * from {{ source('test_raw','raw_customers') }}\n)\nselect\n  customer_id_raw,\n  email_raw\nfrom source\n";
        ctx.storage.put_bytes(&sql_key, sql.as_bytes(), "text/sql").await.unwrap();

        let tool = ApplyNextCleanseSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));

        let yml_rel = "models/staging/stg_test_raw_raw_customers.yml";
        let yml_key = project_fs::join_storage_key(&ctx, yml_rel);
        let got = ctx.storage.get_bytes(&yml_key).await.unwrap();
        let got = String::from_utf8_lossy(&got).to_string();
        assert!(got.contains("stg_test_raw_raw_customers"));
    }

    #[tokio::test]
    async fn apply_next_model_schema_batch_patches_models_schema_yml() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
            replies: Mutex::new(vec![serde_json::json!({
                "replace_file": {
                    "path": "models/schema.yml",
                    "new_text": "version: 2\n\nmodels:\n  - name: dim_customers\n    columns: []\n"
                }
            }).to_string()]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 2,
            thread_id: Some("tid2".to_string()),
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(react_core::agent::DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };

        // Seed a model plan with sql_model done and schema_contract pending.
        let plan_key = plan::new_model_plan_key(&ctx);
        let p = plan::ModelPlan {
            plan_key: plan_key.clone(),
            status: plan::PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![plan::ModelTask {
                name: "dim_customers".to_string(),
                folder: "marts".to_string(),
                goal: "g".to_string(),
                inputs: vec!["stg_test_raw_raw_customers".to_string()],
                expected_model_path: Some("models/marts/dim_customers.sql".to_string()),
                invariants: vec![],
                status: plan::TaskStatus::InProgress,
                checklist: vec![
                    plan::PlanChecklistItem {
                        checklist_item_id: "sql_model".to_string(),
                        label: "Author gold SQL model".to_string(),
                        details: None,
                        status: plan::ChecklistItemStatus::Done,
                        origin: plan::ChecklistOrigin::Initial,
                        origin_step_idx: None,
                        evidence: vec![],
                    },
                    plan::PlanChecklistItem {
                        checklist_item_id: "schema_contract".to_string(),
                        label: "Define schema contract".to_string(),
                        details: None,
                        status: plan::ChecklistItemStatus::Pending,
                        origin: plan::ChecklistOrigin::Initial,
                        origin_step_idx: None,
                        evidence: vec![],
                    },
                ],
            }],
            batches: vec![vec!["dim_customers".to_string()]],
            work_groups: vec![],
            progress: plan::PlanProgress::default(),
        };
        plan::save_model_plan(&ctx, &p).await.unwrap();

        let tool = ApplyNextModelSchemaBatchTool { datasets: None };
        let res = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));

        let key = project_fs::join_storage_key(&ctx, project_files::MODELS_SCHEMA_YML);
        let got = ctx.storage.get_bytes(&key).await.unwrap();
        let got = String::from_utf8_lossy(&got).to_string();
        assert!(got.contains("dim_customers"));
    }
}


