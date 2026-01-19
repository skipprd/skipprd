use crate::agent::AgentCtx;
use crate::dbt::remediate::active_provider_dialect;
use crate::dbt::remediate::list_sql_keys_for_scope;
use crate::dbt::remediate::remediate_dbt_sql_keys_with_llm;
use crate::providers::{CatalogProvider, DatasetCatalogProvider, DbtProvider};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepairIteration {
    pub iteration: usize,
    pub scanned_models: usize,
    pub rewritten_models: usize,
    pub catalog_refreshed: bool,
    pub llm_changed_files: usize,
    pub dbt_ok: bool,
    pub compile_ok: bool,
    pub run_ok: Option<bool>,
    #[serde(default)]
    pub unresolved_columns: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RepairReport {
    pub dialect: String,
    pub max_iterations: usize,
    pub iterations_run: usize,
    #[serde(default)]
    pub iterations: Vec<RepairIteration>,
    #[serde(default)]
    pub stopped_reason: Option<String>,
}

fn extract_sql_rel_paths_from_dbt_errors(errors: &[String]) -> Vec<String> {
    // Best-effort: dbt often reports file paths like:
    //   Model 'model.x.y' (models/staging/foo.sql) ...
    // We only consider paths under models/ and ending in .sql.
    let mut out: Vec<String> = Vec::new();
    let joined = errors.join("\n");
    for line in joined.lines() {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        let mut start_idx = 0usize;
        while let Some(pos) = s[start_idx..].find("models/") {
            let abs = start_idx + pos;
            if let Some(end_rel) = s[abs..].find(".sql") {
                let end = abs + end_rel + ".sql".len();
                let rel = s[abs..end]
                    .trim()
                    .trim_matches(|c| c == '(' || c == ')' || c == '"' || c == '\'');
                if rel.starts_with("models/") && rel.ends_with(".sql") {
                    out.push(rel.to_string());
                }
                start_idx = end;
            } else {
                break;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn storage_keys_for_rel_paths(ctx: &AgentCtx, rels: &[String]) -> Vec<String> {
    let base = ctx.keyspace.dbt_prefix(&ctx.scope).trim_end_matches('/').to_string();
    let mut out: Vec<String> = Vec::new();
    for r in rels {
        let rel = r.trim_start_matches('/').to_string();
        if rel.starts_with("models/") && rel.ends_with(".sql") {
            out.push(format!("{}/{}", base, rel));
        }
    }
    out.sort();
    out.dedup();
    out
}

pub async fn run_repair_loop(
    ctx: &AgentCtx,
    dbt: &Arc<dyn DbtProvider>,
    args: &crate::providers::DbtValidateArgs,
    max_iterations: usize,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    catalog: Option<&Arc<dyn CatalogProvider>>,
    dataset_ids: Option<&[String]>,
) -> Result<(crate::providers::DbtValidateResult, RepairReport), String> {
    let dialect = ctx
        .resolved_config
        .as_ref()
        .map(|c| active_provider_dialect(c.as_ref()))
        .unwrap_or_else(|| "Unknown SQL dialect".to_string());

    let mut report = RepairReport { dialect: dialect.clone(), max_iterations, ..Default::default() };

    let max_it = max_iterations.max(1).min(25);
    for i in 0..max_it {
        let res = dbt.validate_project(&ctx.scope, args).await?;

        report.iterations_run = i + 1;
        let unresolved_columns = crate::suites::data_engineer_suite::dbt_error::extract_unresolved_columns(&res.errors);
        let class = crate::suites::data_engineer_suite::dbt_error::classify(&res.errors);
        let _ = (datasets, catalog, dataset_ids); // reserved for future targeted catalog refresh
        let catalog_refreshed = false;

        // Early-stop: runtime/run/build failures after successful compile are not good candidates
        // for SQL dialect remediation. Surface the dbt result directly.
        if (args.run || args.build) && res.compile_ok && matches!(res.run_ok, Some(false)) {
            tracing::info!(
                target: "dbt_repair_loop",
                iteration = i + 1,
                dialect = %dialect,
                "stopping early: run/build failed after successful compile (no SQL repair)"
            );
            report.iterations.push(RepairIteration {
                iteration: i + 1,
                scanned_models: 0,
                rewritten_models: 0,
                catalog_refreshed,
                llm_changed_files: 0,
                dbt_ok: res.ok,
                compile_ok: res.compile_ok,
                run_ok: res.run_ok,
                unresolved_columns,
                errors: res.errors.clone(),
            });
            report.stopped_reason = Some("run_failed_no_repair".to_string());
            return Ok((res, report));
        }

        // LLM remediation is expensive and risky: only run if the LLM itself reports high confidence
        // that the errors are due to dialect/syntax incompatibility for the configured provider.
        let min_conf: f32 = std::env::var("DBT_REPAIR_LLM_CONFIDENCE_MIN")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(0.85)
            .max(0.0)
            .min(1.0);
        let mut llm_changed_files: usize = 0;
        let mut llm_decision_reason: Option<String> = None;
        let mut llm_decision_conf: Option<f32> = None;
        let mut llm_decision_should: Option<bool> = None;

        // Skip LLM remediation for clearly non-SQL classes.
        let allow_llm_check = matches!(
            class,
            crate::suites::data_engineer_suite::dbt_error::DbtErrorClass::SqlFailure
                | crate::suites::data_engineer_suite::dbt_error::DbtErrorClass::SqlOrModel
                | crate::suites::data_engineer_suite::dbt_error::DbtErrorClass::Unknown
        );

        if allow_llm_check {
            let brief = crate::suites::data_engineer_suite::dbt_error::compact_brief(&res.errors, 6, 900);
            let phase = if args.build {
                "build"
            } else if args.run {
                "run"
            } else if !res.compile_ok {
                "compile"
            } else if !res.parse_ok {
                "parse"
            } else {
                "validate"
            };

            if let Ok(dec) = crate::dbt::remediate::llm_should_remediate_sql(ctx, &dialect, phase, &brief) {
                llm_decision_should = Some(dec.should_remediate);
                llm_decision_conf = Some(dec.confidence);
                llm_decision_reason = Some(dec.reason.clone());
                if dec.should_remediate && dec.confidence >= min_conf {
                    // Best-effort remediation; scope to failing file(s) when possible.
                    let rels = extract_sql_rel_paths_from_dbt_errors(&res.errors);
                    let mut keys = storage_keys_for_rel_paths(ctx, &rels);
                    if keys.is_empty() {
                        keys = list_sql_keys_for_scope(ctx).await.unwrap_or_default();
                    }
                    let llm_report = remediate_dbt_sql_keys_with_llm(ctx, "repair_loop", &keys).await.ok();
                    llm_changed_files = llm_report.as_ref().map(|r| r.changed_files).unwrap_or(0);
                }
            }
        }

        tracing::info!(
            target: "dbt_repair_loop",
            iteration = i + 1,
            dialect = %dialect,
            error_class = ?class,
            llm_should_remediate = llm_decision_should,
            llm_confidence = llm_decision_conf,
            llm_min_confidence = min_conf,
            llm_changed_files = llm_changed_files,
            "repair decision (llm_reason={})",
            llm_decision_reason.as_deref().unwrap_or("")
        );

        report.iterations.push(RepairIteration {
            iteration: i + 1,
            scanned_models: 0,
            rewritten_models: 0,
            catalog_refreshed,
            llm_changed_files,
            dbt_ok: res.ok,
            compile_ok: res.compile_ok,
            run_ok: res.run_ok,
            unresolved_columns,
            errors: res.errors.clone(),
        });

        if res.ok {
            report.stopped_reason = Some("dbt ok".to_string());
            return Ok((res, report));
        }

        // Stop on non-remediable warehouse config errors.
        if matches!(class, crate::suites::data_engineer_suite::dbt_error::DbtErrorClass::WarehouseConfig) {
            report.stopped_reason = Some("warehouse_config".to_string());
            return Ok((res, report));
        }

        // If we made no progress this iteration, stop.
        if !catalog_refreshed && llm_changed_files == 0 {
            report.stopped_reason = Some("no_progress".to_string());
            return Ok((res, report));
        }

        // Continue looping; caller is “eager” and we’re bounded by max_it.
        if i + 1 == max_it {
            report.stopped_reason = Some("max_iterations".to_string());
            return Ok((res, report));
        }
    }
    Err("repair loop fell through unexpectedly".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::storage::InMemoryStorageAdapter;
    use crate::adapters::storage::StorageAdapter;
    use crate::llm::{ChatMessage, LargeLanguageModel};
    use crate::agent::DefaultPolicy;
    use crate::providers::{DbtValidateArgs, DbtValidateResult, Keyspace, RequestScope};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockDbt {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl DbtProvider for MockDbt {
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

        async fn validate_project(&self, _scope: &RequestScope, _args: &DbtValidateArgs) -> Result<DbtValidateResult, String> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            if *c == 1 {
                return Ok(DbtValidateResult {
                    ok: false,
                    deps_ok: true,
                    parse_ok: true,
                    compile_ok: false,
                    run_ok: None,
                    uploaded_target_files: 0,
                    errors: vec!["Runtime Error: Column 'context.session.id' cannot be resolved".to_string()],
                    warnings: vec![],
                    logs: serde_json::json!({}),
                });
            }
            Ok(DbtValidateResult {
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

    #[derive(Default)]
    struct MockLlm {
        // Queue of responses to return from chat() (decision first, then remediation, etc.).
        chat_responses: Mutex<Vec<String>>,
    }

    impl LargeLanguageModel for MockLlm {
        fn chat(&self, _messages: &[ChatMessage]) -> Result<String, String> {
            let mut q = self.chat_responses.lock().unwrap();
            if q.is_empty() {
                return Err("no mock responses remaining".to_string());
            }
            Ok(q.remove(0))
        }

        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Err("not implemented".to_string())
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
    async fn repair_loop_stops_on_no_progress() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            chat_responses: Mutex::new(vec![
                // Decision: low confidence; no remediation.
                serde_json::json!({"should_remediate": false, "confidence": 0.0, "reason": "not enough information"}).to_string(),
            ]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(crate::providers::keyspace::DefaultKeyspace::new("b".to_string()));
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
            scope: scope.clone(),
            keyspace,
            query: None,
            dbt: None,
            vector: None,
            thread_store: None,
            resolved_config: Some(minimal_cfg()),
        };

        struct AlwaysFailDbt;
        #[async_trait]
        impl DbtProvider for AlwaysFailDbt {
            async fn ensure_minimal_project(&self, _scope: &RequestScope) -> Result<(), String> { Ok(()) }
            async fn write_model_sql(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _sql: &str) -> Result<String, String> { Ok("k".to_string()) }
            async fn write_metricflow_yaml(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _yaml_text: &str) -> Result<String, String> { Ok("k".to_string()) }
            async fn validate_project(&self, _scope: &RequestScope, _args: &DbtValidateArgs) -> Result<DbtValidateResult, String> {
                Ok(DbtValidateResult {
                    ok: false,
                    deps_ok: true,
                    parse_ok: true,
                    compile_ok: false,
                    run_ok: None,
                    uploaded_target_files: 0,
                    errors: vec!["Compilation Error: something".to_string()],
                    warnings: vec![],
                    logs: serde_json::json!({}),
                })
            }
        }

        let dbt: Arc<dyn DbtProvider> = Arc::new(AlwaysFailDbt);
        let (res, rep) = run_repair_loop(
            &ctx,
            &dbt,
            &DbtValidateArgs {
                project_name: "data_engineer".to_string(),
                profiles_dir: None,
                target: "athena".to_string(),
                run: false,
                build: false,
            },
            5,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(!res.ok);
        assert_eq!(rep.iterations_run, 1);
        assert_eq!(rep.stopped_reason.as_deref(), Some("no_progress"));
    }

    #[tokio::test]
    async fn repair_loop_stops_on_runtime_run_failure_after_compile() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            // Should not be called.
            chat_responses: Mutex::new(vec![]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(crate::providers::keyspace::DefaultKeyspace::new("b".to_string()));
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
            scope: scope.clone(),
            keyspace,
            query: None,
            dbt: None,
            vector: None,
            thread_store: None,
            resolved_config: Some(minimal_cfg()),
        };

        struct CompileOkRunFailDbt;
        #[async_trait]
        impl DbtProvider for CompileOkRunFailDbt {
            async fn ensure_minimal_project(&self, _scope: &RequestScope) -> Result<(), String> { Ok(()) }
            async fn write_model_sql(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _sql: &str) -> Result<String, String> { Ok("k".to_string()) }
            async fn write_metricflow_yaml(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _yaml_text: &str) -> Result<String, String> { Ok("k".to_string()) }
            async fn validate_project(&self, _scope: &RequestScope, _args: &DbtValidateArgs) -> Result<DbtValidateResult, String> {
                Ok(DbtValidateResult {
                    ok: false,
                    deps_ok: true,
                    parse_ok: true,
                    compile_ok: true,
                    run_ok: Some(false),
                    uploaded_target_files: 0,
                    errors: vec!["Database Error: something during run".to_string()],
                    warnings: vec![],
                    logs: serde_json::json!({}),
                })
            }
        }

        let dbt: Arc<dyn DbtProvider> = Arc::new(CompileOkRunFailDbt);
        let (res, rep) = run_repair_loop(
            &ctx,
            &dbt,
            &DbtValidateArgs {
                project_name: "data_engineer".to_string(),
                profiles_dir: None,
                target: "athena".to_string(),
                run: false,
                build: true,
            },
            5,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(!res.ok);
        assert_eq!(rep.iterations_run, 1);
        assert_eq!(rep.stopped_reason.as_deref(), Some("run_failed_no_repair"));
    }

    #[tokio::test]
    async fn repair_loop_llm_low_confidence_skips_remediation() {
        std::env::set_var("DBT_REPAIR_LLM_CONFIDENCE_MIN", "0.9");
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        // Seed a SQL file so remediation would have work if it were invoked.
        let ds_id = "AwsDataCatalog.picnic.events";
        let stg_key = format!("t/w/p/dbt/models/{}/stg_events.sql", ds_id);
        storage.put_bytes(&stg_key, b"select 1\n", "text/sql").await.unwrap();

        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            chat_responses: Mutex::new(vec![
                // Decision: wants remediation but low confidence -> should skip
                serde_json::json!({"should_remediate": true, "confidence": 0.2, "reason": "uncertain"}).to_string(),
            ]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(crate::providers::keyspace::DefaultKeyspace::new("b".to_string()));
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
            scope: scope.clone(),
            keyspace,
            query: None,
            dbt: None,
            vector: None,
            thread_store: None,
            resolved_config: Some(minimal_cfg()),
        };

        struct SqlFailureOnceDbt;
        #[async_trait]
        impl DbtProvider for SqlFailureOnceDbt {
            async fn ensure_minimal_project(&self, _scope: &RequestScope) -> Result<(), String> { Ok(()) }
            async fn write_model_sql(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _sql: &str) -> Result<String, String> { Ok("k".to_string()) }
            async fn write_metricflow_yaml(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _yaml_text: &str) -> Result<String, String> { Ok("k".to_string()) }
            async fn validate_project(&self, _scope: &RequestScope, _args: &DbtValidateArgs) -> Result<DbtValidateResult, String> {
                Ok(DbtValidateResult {
                    ok: false,
                    deps_ok: true,
                    parse_ok: true,
                    compile_ok: false,
                    run_ok: None,
                    uploaded_target_files: 0,
                    errors: vec!["Compilation Error: mismatched input".to_string()],
                    warnings: vec![],
                    logs: serde_json::json!({}),
                })
            }
        }

        let dbt: Arc<dyn DbtProvider> = Arc::new(SqlFailureOnceDbt);
        let (_res, rep) = run_repair_loop(
            &ctx,
            &dbt,
            &DbtValidateArgs {
                project_name: "data_engineer".to_string(),
                profiles_dir: None,
                target: "athena".to_string(),
                run: false,
                build: false,
            },
            1,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(rep.iterations_run, 1);
        assert_eq!(rep.iterations[0].llm_changed_files, 0);
    }

    #[tokio::test]
    async fn repair_loop_llm_high_confidence_runs_remediation() {
        std::env::set_var("DBT_REPAIR_LLM_CONFIDENCE_MIN", "0.5");
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        // Seed a SQL file so remediation scans something.
        let ds_id = "AwsDataCatalog.picnic.events";
        let stg_key = format!("t/w/p/dbt/models/{}/stg_events.sql", ds_id);
        storage.put_bytes(&stg_key, b"select 1\n", "text/sql").await.unwrap();

        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            chat_responses: Mutex::new(vec![
                // Decision: high confidence
                serde_json::json!({"should_remediate": true, "confidence": 0.9, "reason": "dialect mismatch"}).to_string(),
                // Remediation response (no changes)
                serde_json::json!({"changes": [], "notes": []}).to_string(),
            ]),
        });
        let keyspace: Arc<dyn Keyspace> = Arc::new(crate::providers::keyspace::DefaultKeyspace::new("b".to_string()));
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
            scope: scope.clone(),
            keyspace,
            query: None,
            dbt: None,
            vector: None,
            thread_store: None,
            resolved_config: Some(minimal_cfg()),
        };

        struct SqlFailureOnceDbt;
        #[async_trait]
        impl DbtProvider for SqlFailureOnceDbt {
            async fn ensure_minimal_project(&self, _scope: &RequestScope) -> Result<(), String> { Ok(()) }
            async fn write_model_sql(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _sql: &str) -> Result<String, String> { Ok("k".to_string()) }
            async fn write_metricflow_yaml(&self, _scope: &RequestScope, _dataset_id: &str, _name: &str, _yaml_text: &str) -> Result<String, String> { Ok("k".to_string()) }
            async fn validate_project(&self, _scope: &RequestScope, _args: &DbtValidateArgs) -> Result<DbtValidateResult, String> {
                Ok(DbtValidateResult {
                    ok: false,
                    deps_ok: true,
                    parse_ok: true,
                    compile_ok: false,
                    run_ok: None,
                    uploaded_target_files: 0,
                    errors: vec!["Compilation Error: syntax error".to_string()],
                    warnings: vec![],
                    logs: serde_json::json!({}),
                })
            }
        }

        let dbt: Arc<dyn DbtProvider> = Arc::new(SqlFailureOnceDbt);
        let (_res, rep) = run_repair_loop(
            &ctx,
            &dbt,
            &DbtValidateArgs {
                project_name: "data_engineer".to_string(),
                profiles_dir: None,
                target: "athena".to_string(),
                run: false,
                build: false,
            },
            1,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(rep.iterations_run, 1);
        // Remediation ran but made no changes; still should not crash.
        assert_eq!(rep.iterations[0].llm_changed_files, 0);
    }
}

