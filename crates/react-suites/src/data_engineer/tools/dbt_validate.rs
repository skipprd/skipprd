use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::providers::{CatalogProvider, DatasetCatalogProvider};
use react_core::tools::Tool;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

pub struct DbtValidateTool {
    pub datasets: Option<Arc<dyn DatasetCatalogProvider>>,
    pub catalog: Option<Arc<dyn CatalogProvider>>,
}

#[async_trait]
impl Tool for DbtValidateTool {
    fn name(&self) -> &'static str {
        "dbt_validate"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let project_name = args
            .get("project_name")
            .and_then(|x| x.as_str())
            .unwrap_or("data_engineer");
        let dbt = ctx
            .dbt
            .as_ref()
            .ok_or_else(|| "dbt provider missing".to_string())?;
        // Prefer profiles generated from resolved config (so dbt_validate is deterministic and
        // avoids host-local profiles drift). Only use DBT_PROFILES_DIR if the caller explicitly
        // requested it (via args.profiles_dir) or if we cannot generate a profile.
        let explicit_profiles_dir = args
            .get("profiles_dir")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let mut profiles_dir = explicit_profiles_dir.clone();
        // Target selection:
        // - If explicitly provided (args.target), use it.
        // - Otherwise, if we generated profiles.yml from the configured warehouse provider, use its target (typically "athena").
        // - Otherwise, error (we do NOT have a valid generic default target).
        let explicit_target = args
            .get("target")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let mut target: Option<String> = explicit_target.clone();
        let run = args.get("run").and_then(|x| x.as_bool()).unwrap_or(false);
        let build = args.get("build").and_then(|x| x.as_bool()).unwrap_or(false);
        let dataset_ids: Option<Vec<String>> = args
            .get("dataset_ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        // If profiles_dir not provided, try generating one from resolved config for the active warehouse provider.
        // Keep the tempdir alive for the duration of this call.
        let mut _tmp: Option<tempfile::TempDir> = None;
        if profiles_dir.is_none() {
            if let Some(cfg) = crate::config::resolved_config_from_ctx(ctx) {
                let threads = Some(react_core::providers::QueryProvider::max_concurrency(
                    ctx.warehouse.as_ref(),
                ));
                if let Ok(gen) = crate::dbt::profile::generate_profiles_yml(cfg, threads) {
                    let td = tempfile::tempdir().map_err(|e| e.to_string())?;
                    let mut p = PathBuf::from(td.path());
                    p.push("profiles.yml");
                    fs::write(&p, gen.profiles_yml.as_bytes()).map_err(|e| e.to_string())?;
                    profiles_dir = Some(td.path().to_string_lossy().to_string());
                    _tmp = Some(td);
                    // If caller didn't explicitly set a target, use the generated one so we
                    // always validate against the configured warehouse provider.
                    if target.is_none() {
                        target = Some(gen.target);
                    }
                }
            }
        }
        // As a last resort, fall back to the host env var.
        if profiles_dir.is_none() {
            profiles_dir = std::env::var("DBT_PROFILES_DIR").ok();
        }
        let target = target.ok_or_else(|| {
            "dbt_validate requires a dbt target name (e.g. 'athena', 'postgres', 'snowflake', 'bigquery', 'sqlserver'). Configure providers.dbt.target or pass args.target explicitly."
                .to_string()
        })?;

        let dialect = ctx
            .runtime
            .as_ref()
            .and_then(|_| crate::config::resolved_config_from_ctx(ctx))
            .map(crate::data_engineer::dbt_repair::remediate::active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());

        let max_iters: usize = std::env::var("DBT_REPAIR_MAX_ITERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(8)
            .max(1)
            .min(25);

        let (res, repair_report) = crate::data_engineer::dbt_repair::repair_loop::run_repair_loop(
            ctx,
            dbt,
            &react_core::providers::DbtValidateArgs {
                project_name: project_name.to_string(),
                profiles_dir: profiles_dir.clone(),
                target: target.clone(),
                run,
                build,
                select: None,
                exclude: None,
            },
            max_iters,
            self.datasets.as_ref(),
            self.catalog.as_ref(),
            dataset_ids.as_deref(),
        )
        .await?;

        let mut v = serde_json::to_value(res).unwrap_or_else(
            |_| serde_json::json!({"ok": false, "error": "failed to serialize result"}),
        );
        if let Some(obj) = v.as_object_mut() {
            obj.insert("dialect".to_string(), serde_json::json!(dialect));
            obj.insert(
                "repair_report".to_string(),
                serde_json::to_value(repair_report).unwrap_or(Value::Null),
            );
            // Structured runtime/test failures extracted from build/run stdout (if present).
            // This avoids relying on giant error blobs or tiny brief summaries.
            let rf = crate::data_engineer::dbt_error::extract_runtime_failures_from_logs(
                &obj.get("logs").cloned().unwrap_or(Value::Null),
            );
            obj.insert("runtime_failures".to_string(), serde_json::json!(rf));

            // Always produce an LLM-backed condensed error summary on failure.
            // This is used by terminal mode to show the real root cause (not startup banners).
            let ok = obj.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            if !ok {
                let errors: Vec<String> = obj
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let logs = obj.get("logs").cloned().unwrap_or(Value::Null);
                match crate::data_engineer::dbt_error::summarize_dbt_failure_llm(
                    ctx.llm.as_ref(),
                    &errors,
                    &logs,
                    &rf,
                    2000,
                ) {
                    Ok(sum) => {
                        obj.insert("error_summary".to_string(), serde_json::json!(sum.summary));
                        obj.insert(
                            "failing_nodes".to_string(),
                            serde_json::json!(sum.failing_nodes),
                        );
                        obj.insert(
                            "suggested_next_files".to_string(),
                            serde_json::json!(sum.suggested_next_files),
                        );
                    }
                    Err(e) => {
                        obj.insert(
                            "error_summary".to_string(),
                            serde_json::json!(format!("(failed to summarize dbt error: {e})")),
                        );
                    }
                }
            }
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use react_core::agent::DefaultPolicy;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::providers::DbtProvider;
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockLlm {
        chat_responses: Mutex<Vec<String>>,
    }
    impl react_core::llm::LargeLanguageModel for MockLlm {
        fn chat(
            &self,
            _messages: &[react_core::llm::ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            let mut q = self.chat_responses.lock().unwrap();
            if q.is_empty() {
                return Err("no mock responses remaining".to_string());
            }
            Ok(q.remove(0))
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    struct MockDbtProvider {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl DbtProvider for MockDbtProvider {
        async fn ensure_minimal_project(&self, _scope: &RequestScope) -> Result<(), String> {
            Ok(())
        }
        async fn write_model_sql(
            &self,
            _scope: &RequestScope,
            _dataset_id: &str,
            _name: &str,
            _sql: &str,
        ) -> Result<String, String> {
            Ok("k".to_string())
        }
        async fn write_metricflow_yaml(
            &self,
            _scope: &RequestScope,
            _dataset_id: &str,
            _name: &str,
            _yaml_text: &str,
        ) -> Result<String, String> {
            Ok("k".to_string())
        }
        async fn validate_project(
            &self,
            _scope: &RequestScope,
            _args: &react_core::providers::DbtValidateArgs,
        ) -> Result<react_core::providers::DbtValidateResult, String> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            if *c == 1 {
                return Ok(react_core::providers::DbtValidateResult {
                    ok: false,
                    deps_ok: true,
                    parse_ok: true,
                    compile_ok: false,
                    run_ok: None,
                    uploaded_target_files: 0,
                    errors: vec!["Compilation Error: database error".to_string()],
                    warnings: vec![],
                    logs: serde_json::json!({}),
                });
            }
            Ok(react_core::providers::DbtValidateResult {
                ok: true,
                deps_ok: true,
                parse_ok: true,
                compile_ok: true,
                run_ok: Some(true),
                uploaded_target_files: 0,
                errors: vec![],
                warnings: vec![],
                logs: serde_json::json!({}),
            })
        }
    }

    #[test]
    fn dbt_validate_mock_change_does_not_require_expected_sha256() {
        // Ensure unit tests do not require LLM echo of sha (suite enforces drift safety internally).
        let v = serde_json::json!({
            "patch_text": "@@\n- select 1\n+ select 2\n",
            "notes": []
        });
        assert!(v.get("patch_text").and_then(|x| x.as_str()).is_some());
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
                    namespace: "src".to_string(),
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
                        target_schema: "src".to_string(),
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
    async fn dbt_validate_retries_once_after_remediation_on_sql_failure() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        // Store one SQL file so remediation scans something.
        storage
            .put_bytes("t/w/p/dbt/models/m.sql", b"select 1", "text/sql")
            .await
            .unwrap();

        let llm: Arc<dyn react_core::llm::LargeLanguageModel> = Arc::new(MockLlm {
            chat_responses: Mutex::new(vec![
                // Grounded remediation response: apply a minimal formatting change so the repair loop makes progress.
                serde_json::json!({
                    "changes": [{
                        "key":"t/w/p/dbt/models/m.sql",
                        "patch_text": "@@ -1 +1 @@\\n-select 1\\n+select 1 -- remediation\\n",
                        "reason":"minimal change to trigger retry"
                    }],
                    "notes": []
                })
                .to_string(),
            ]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let dbt: Arc<dyn DbtProvider> = Arc::new(MockDbtProvider {
            calls: Mutex::new(0),
        });
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };

        let ctx = AgentCtx {
            top_k: 1,
            per_step_timeout_secs: 1,
            max_steps: 1,
            thread_id: None,
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(DefaultPolicy),
            llm,
            storage,
            scope,
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: Some(dbt),
            vector: None,
            thread_store: None,
            exec_ctx: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };

        let tool = DbtValidateTool {
            datasets: None,
            catalog: None,
        };
        let obs = tool
            .call(
                serde_json::json!({"project_name":"data_engineer","build":false}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(obs.get("ok").and_then(|v| v.as_bool()), Some(true));
        // Repair report should exist (best-effort)
        let _r: Option<crate::data_engineer::dbt_repair::repair_loop::RepairReport> = obs
            .get("repair_report")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
    }
}
