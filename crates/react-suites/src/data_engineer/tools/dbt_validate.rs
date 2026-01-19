use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use react_core::providers::{CatalogProvider, DatasetCatalogProvider};

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
                if let Ok(gen) = crate::dbt::profile::generate_profiles_yml(cfg) {
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
            "dbt_validate requires a target derived from the configured warehouse provider. Enable/configure providers.athena (and providers.athena.result_s3) or pass args.target explicitly."
                .to_string()
        })?;

        let dialect = ctx
            .runtime
            .as_ref()
            .and_then(|_| crate::config::resolved_config_from_ctx(ctx))
            .map(crate::dbt::remediate::active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());

        let max_iters: usize = std::env::var("DBT_REPAIR_MAX_ITERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(8)
            .max(1)
            .min(25);

        let (res, repair_report) = crate::dbt::repair_loop::run_repair_loop(
            ctx,
            dbt,
            &react_core::providers::DbtValidateArgs {
                project_name: project_name.to_string(),
                profiles_dir: profiles_dir.clone(),
                target: target.clone(),
                run,
                build,
            },
            max_iters,
            self.datasets.as_ref(),
            self.catalog.as_ref(),
            dataset_ids.as_deref(),
        )
        .await?;

        let mut v = serde_json::to_value(res)
            .unwrap_or_else(|_| serde_json::json!({"ok": false, "error": "failed to serialize result"}));
        if let Some(obj) = v.as_object_mut() {
            obj.insert("dialect".to_string(), serde_json::json!(dialect));
            obj.insert("repair_report".to_string(), serde_json::to_value(repair_report).unwrap_or(Value::Null));
            // Structured runtime/test failures extracted from build/run stdout (if present).
            // This avoids relying on giant error blobs or tiny brief summaries.
            let rf = crate::data_engineer::dbt_error::extract_runtime_failures_from_logs(
                &obj.get("logs").cloned().unwrap_or(Value::Null),
            );
            obj.insert("runtime_failures".to_string(), serde_json::json!(rf));
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::storage::InMemoryStorageAdapter;
    use react_core::agent::DefaultPolicy;
    use crate::dbt::remediate::RemediationReport;
    use react_core::providers::{DbtProvider, Keyspace, RequestScope};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockLlm {
        chat_responses: Mutex<Vec<String>>,
    }
    impl react_core::llm::LargeLanguageModel for MockLlm {
        fn chat(&self, _messages: &[react_core::llm::ChatMessage]) -> Result<String, String> {
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
        async fn write_model_sql(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _sql: &str) -> Result<String, String> {
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

    fn minimal_cfg() -> Arc<crate::config::ReactResolvedConfig> {
        Arc::new(crate::config::ReactResolvedConfig {
            server: crate::config::ServerResolved { port: 1 },
            storage: crate::config::StorageResolved { bucket: "b".to_string() },
            scope: RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() },
            llm: crate::config::LlmResolved::default(),
            providers: crate::config::ProvidersResolved {
                athena: crate::config::AthenaResolved {
                    enabled: true,
                    workgroup: None,
                    region: None,
                    result_s3: Some("s3://x/".to_string()),
                    source_schema: Some("src".to_string()),
                    target_catalog: "AwsDataCatalog".to_string(),
                    silver_schema: Some("src_silver".to_string()),
                    gold_schema: Some("src_warehouse".to_string()),
                    discovery_cache_ttl_secs: 120,
                },
                catalog: crate::config::CatalogResolved { enabled: false, refresh_secs: 60, max_concurrency: 8 },
                dbt: crate::config::DbtResolved {
                    enabled: true,
                    profiles_dir: None,
                    target: Some("athena".to_string()),
                    naming: crate::config::DbtNamingResolved {
                        target_schema: Some("src".to_string()),
                        silver_suffix: Some("silver".to_string()),
                        gold_suffix: Some("warehouse".to_string()),
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
        let storage: Arc<dyn crate::adapters::storage::StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        // Store one SQL file so remediation scans something.
        storage
            .put_bytes("t/w/p/dbt/models/m.sql", b"select 1", "text/sql")
            .await
            .unwrap();

        let llm: Arc<dyn react_core::llm::LargeLanguageModel> = Arc::new(MockLlm {
            chat_responses: Mutex::new(vec![
                // Decision: confident dialect/syntax issue -> run remediation
                serde_json::json!({"should_remediate": true, "confidence": 0.95, "reason": "dialect/syntax mismatch likely"}).to_string(),
                // Remediation response
                serde_json::json!({
                    "changes": [{"key":"t/w/p/dbt/models/m.sql","new_content":"select 1","reason":"noop"}],
                    "notes": []
                }).to_string(),
            ]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(react_core::providers::keyspace::DefaultKeyspace::new("b".to_string()));
        let dbt: Arc<dyn DbtProvider> = Arc::new(MockDbtProvider { calls: Mutex::new(0) });
        let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };

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
            dbt: Some(dbt),
            vector: None,
            thread_store: None,
            resolved_config: Some(minimal_cfg()),
        };

        let tool = DbtValidateTool { datasets: None, catalog: None };
        let obs = tool
            .call(serde_json::json!({"project_name":"data_engineer","build":false}), &ctx)
            .await
            .unwrap();
        assert_eq!(obs.get("ok").and_then(|v| v.as_bool()), Some(true));
        // Repair report should exist (best-effort)
        let _r: Option<crate::dbt::repair_loop::RepairReport> = obs
            .get("repair_report")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
    }
}

