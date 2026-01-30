use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use tokio::task::JoinSet;
use tracing::info;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

use crate::data_engineer::dbt_repair::remediate::active_provider_dialect;
use crate::data_engineer::{naming, patch_protocol};

fn emit_trace(ctx: &AgentCtx, line: impl Into<String>) {
    if let Some(tx) = ctx.trace_tx.as_ref() {
        let _ = tx.send(line.into());
    }
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

fn contains_unsupported_sql_for_provider(provider: &str, sql: &str) -> Option<&'static str> {
    // Keep this intentionally conservative: only block known, repeat offender functions.
    let p = provider.trim().to_lowercase();
    if p == "athena" || p == "trino" {
        let s = sql.to_ascii_lowercase();
        if s.contains("initcap(") {
            return Some("initcap() is not supported on Athena/Trino; remove it (avoid title-casing strings).");
        }
    }
    None
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

fn build_gold_sys_prompt(provider: &str, dialect: &str, max_items: usize) -> String {
    format!(
        "You are an expert analytics engineer.\n\
         Task: author dbt GOLD mart model(s) for a warehouse project.\n\
         Provider: {provider}\n\
         Dialect: {dialect}\n\
         Output MUST be valid JSON only.\n\
         You MUST choose EXACTLY ONE patch primitive to modify the provided model_path.\n\
         Use structured primitives only (replace_file / replace_range / replace_list).\n\
         Output schema:\n\
         {{\n\
           \"notes\": [\"...\"],\n\
           \"replace_file\": {{\"new_text\":\"...\"}} | null,\n\
           \"replace_range\": {{\"start_line\":1,\"end_line\":1,\"new_text\":\"...\"}} | null,\n\
           \"replace_list\": {{\"edits\":[{{\"start_line\":1,\"end_line\":1,\"new_text\":\"...\"}}]}} | null\n\
         }}\n\
         (Exactly ONE of replace_file/replace_range/replace_list must be provided; the others must be null.)\n\
         \n\
         CRITICAL gold rules:\n\
         - You MUST write a SELECT-based dbt model.\n\
         - Gold models MUST ONLY read from silver/staging models using ref('stg_*').\n\
         - Gold models MUST NOT call source() anywhere.\n\
         - Prefer minimal, stable columns for business use; do not invent fields.\n\
         - CRITICAL: Do NOT select or reference any column not present in inputs[].schema_columns for that input.\n\
           If you need a field that does not exist in silver, put it in notes and do NOT guess.\n\
         - Use provided inputs[].schema_columns (from the warehouse/catalog) as ground truth for available columns + types.\n\
         - IMPORTANT time handling (consistency):\n\
           - If an input column is already typed as timestamp/date/timestamptz/datetime, use it directly; do NOT re-cast it to the same type.\n\
           - Do NOT narrow time zones: never cast timestamptz -> timestamp.\n\
           - If you need parsed timestamps but the input only has string-ish fields, do NOT try_cast in gold; instead note that silver should add a cleaned timestamp column.\n\
         - Dialect/provider compatibility:\n\
           - If Provider is athena (Trino SQL), DO NOT use initcap() (it is not registered).\n\
         - Batch throughput: you will be asked to create up to {max_items} models per call.\n\
         - Do NOT include a dbt config block; the suite injects schema/alias deterministically.\n\
         Patch rules:\n\
         - The patch MUST modify ONLY the provided model_path.\n\
         \n"
    )
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
            .map(|cfg| if cfg.providers.athena.enabled { "athena" } else { "unknown" })
            .unwrap_or("unknown");
        let sys = build_gold_sys_prompt(provider_name, &dialect, max_items);

        let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
        let query = ctx.query.clone();
        let (target_catalog, silver_db) = crate::config::resolved_config_from_ctx(ctx)
            .map(|cfg| {
                let cat = cfg.providers.athena.target_catalog.clone();
                let base_schema = cfg.providers.dbt.naming.target_schema.clone();
                let silver_suffix = cfg.providers.dbt.naming.silver_suffix.clone();
                let db = if base_schema.trim().is_empty() {
                    // Fallback: do not guess; leave empty so we skip schema probing.
                    "".to_string()
                } else {
                    format!("{}_{}", base_schema.trim(), silver_suffix.trim())
                };
                (cat, db)
            })
            .unwrap_or_else(|| ("".to_string(), "".to_string()));

        let mut written: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut succeeded_item_names: Vec<String> = Vec::new();

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

            let folder = normalize_folder(it.folder.as_deref());
            let rel_path = gold_model_rel_path(&folder, name);

            // Load the inputs to ground the LLM in actual silver SQL.
            let max_fetch_concurrency = 3usize;
            let storage = ctx.storage.clone();
            let query2 = query.clone();
            let target_catalog2 = target_catalog.clone();
            let silver_db2 = silver_db.clone();
            let mut set: JoinSet<(usize, String, String, String, String, Vec<(String, String)>)> = JoinSet::new();
            // (idx, input, rel_path, content, derived_relation_fqn, schema_cols)
            let mut fetched: Vec<(usize, String, String, String, String, Vec<(String, String)>)> = Vec::new();
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
                let target_catalog3 = target_catalog2.clone();
                let silver_db3 = silver_db2.clone();
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
                    let derived_fqn = if !target_catalog3.trim().is_empty() && !silver_db3.trim().is_empty() && !alias.trim().is_empty() {
                        format!("{}.{}.{}", target_catalog3, silver_db3, alias)
                    } else {
                        "".to_string()
                    };
                    let schema_cols = if let (Some(q), true) = (query3.as_ref(), !derived_fqn.is_empty()) {
                        q.schema(&derived_fqn).await.unwrap_or_default()
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

            let user = serde_json::json!({
                "model_name": name,
                "model_path": rel_path,
                "goal": goal,
                "instructions": it.instructions,
                "inputs": input_blocks,
                "existing_model_sql": ctx
                    .storage
                    .get_bytes(&format!("{}/{}", base, rel_path))
                    .await
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).to_string())
                    .unwrap_or_default()
            })
            .to_string();

            let (outcome, llm_notes) = match patch_protocol::llm_patch_loop_single_file(
                ctx,
                None,
                sys.clone(),
                user,
                &rel_path,
                4,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("{name}: patch authoring/apply failed: {e}"));
                    continue;
                }
            };

            if naming::contains_source_call(&outcome.content) {
                errors.push(format!(
                    "{name}: invalid gold SQL after patch apply: contains source(). Gold must only read from silver via ref('stg_*')."
                ));
                continue;
            }
            if !naming::contains_ref_call(&outcome.content) {
                errors.push(format!(
                    "{name}: invalid gold SQL after patch apply: must reference at least one silver/staging model via ref('stg_*')."
                ));
                continue;
            }
            if let Some(msg) = contains_unsupported_sql_for_provider(provider_name, &outcome.content) {
                errors.push(format!("{name}: unsupported SQL for provider '{provider_name}': {msg}"));
                continue;
            }
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

            for n in llm_notes {
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
            "errors": errors,
            "succeeded_item_names": succeeded_item_names
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

    #[derive(Default)]
    struct MockLlm {
        resp: String,
    }

    impl LargeLanguageModel for MockLlm {
        fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
            Ok(self.resp.clone())
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    fn minimal_cfg() -> Arc<crate::config::ReactResolvedConfig> {
        Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved { bucket: "b".to_string() },
            scope: RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                athena: crate::config::AthenaResolved {
                    enabled: true,
                    workgroup: "wg".to_string(),
                    region: "eu-west-1".to_string(),
                    result_s3: "s3://x/".to_string(),
                    target_catalog: "AwsDataCatalog".to_string(),
                    source_schema: "test_raw".to_string(),
                    discovery_cache_ttl_secs: 120,
                },
                catalog: crate::config::CatalogResolved { enabled: false, refresh_secs: 60, max_concurrency: 8 },
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
        let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
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
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        }
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
                "replace_file": { "path": "models/marts/fct_orders.sql", "new_text": "select * from {{ ref('stg_test_raw_raw_orders') }}" },
                "notes": []
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

        assert!(out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
        let written = out
            .get("written_keys")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(written.len(), 1);
        let key = written[0].as_str().unwrap_or("").to_string();
        let bytes = storage.get_bytes(&key).await.expect("written file exists");
        let content = String::from_utf8_lossy(&bytes).to_string();
        assert!(content.contains("config(schema=\"warehouse\""));
        assert!(content.contains("alias=\"fct_orders\""));
        assert!(content.to_ascii_lowercase().contains("ref('stg_test_raw_raw_orders')"));
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
                "replace_file": { "path": "models/marts/fct_orders.sql", "new_text": "select * from {{ source('test_raw','raw_orders') }}" },
                "notes": []
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

        assert!(!out.get("ok").and_then(|v| v.as_bool()).unwrap_or(true));
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
            .put_bytes(&stg_orders_key, "select 1 as order_id".as_bytes(), "text/sql")
            .await
            .expect("seed orders staging");
        storage
            .put_bytes(&stg_users_key, "select 1 as user_id".as_bytes(), "text/sql")
            .await
            .expect("seed users staging");

        let llm = Arc::new(MockLlm {
            resp: serde_json::json!({
                "replace_file": { "path": "models/marts/fct_orders.sql", "new_text": "select * from {{ ref('stg_test_raw_raw_orders') }}" },
                "notes": ["ok"]
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
}

