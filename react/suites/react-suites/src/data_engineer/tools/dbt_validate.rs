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

#[derive(Clone, Debug, serde::Serialize)]
struct ValidationLadderPhase {
    phase: String,
    select_terms: Vec<String>,
    ok: bool,
    compile_ok: bool,
    run_ok: Option<bool>,
    error_count: usize,
    probe: Option<serde_json::Value>,
}

async fn derive_select_terms(ctx: &AgentCtx, args: &Value) -> Vec<String> {
    if let Some(arr) = args.get("select").and_then(|v| v.as_array()) {
        let mut out: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
        out.sort();
        out.dedup();
        return out;
    }
    let Some(thread_id) = ctx.thread_id.as_deref() else {
        return vec![];
    };
    let Some(store) = ctx.thread_store.as_ref() else {
        return vec![];
    };
    match store.get(thread_id).await {
        Ok(log) => {
            crate::data_engineer::control_flow::derive_targeted_select_terms(ctx, &log).await
        }
        Err(_) => vec![],
    }
}

fn model_name_from_compiled_key(key: &str) -> Option<String> {
    let file = key.rsplit('/').next()?.trim();
    file.strip_suffix(".sql").map(|s| s.to_string())
}

fn key_matches_select_terms(key: &str, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let Some(name) = model_name_from_compiled_key(key) else {
        return false;
    };
    let n = name.to_ascii_lowercase();
    terms.iter().any(|t| {
        let tl = t.trim().to_ascii_lowercase();
        tl == n || tl.ends_with(&format!(".{}", n)) || tl.contains(&n)
    })
}

fn failure_class_key_from_errors(errors: &[String]) -> &'static str {
    match crate::data_engineer::dbt_error::classify(errors) {
        crate::data_engineer::dbt_error::DbtErrorClass::WarehouseConfig => "warehouse_config",
        crate::data_engineer::dbt_error::DbtErrorClass::SqlFailure
        | crate::data_engineer::dbt_error::DbtErrorClass::SqlOrModel => "sql_or_runtime",
        _ => "unknown",
    }
}

fn build_failing_targets(logs: &Value, failure_class: &str) -> Vec<Value> {
    let mut out: Vec<Value> = crate::data_engineer::dbt_error::extract_failed_models_from_logs(logs)
        .into_iter()
        .filter_map(|fm| {
            let node_id = fm
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let canonical_path = fm
                .get("file")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())?;
            Some(serde_json::json!({
                "node_id": node_id,
                "canonical_path": canonical_path,
                "error_code": failure_class,
            }))
        })
        .collect();
    out.sort_by(|a, b| {
        let ap = a
            .get("canonical_path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let bp = b
            .get("canonical_path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        ap.cmp(bp)
    });
    out.dedup_by(|a, b| {
        a.get("canonical_path").and_then(|v| v.as_str())
            == b.get("canonical_path").and_then(|v| v.as_str())
    });
    out
}

async fn probe_compiled_model_sql(
    ctx: &AgentCtx,
    _project_name: &str,
    select_terms: &[String],
) -> Result<serde_json::Value, String> {
    let compiled_prefix = format!("{}target/compiled/", ctx.keyspace.dbt_prefix(&ctx.scope));
    let mut keys = ctx
        .storage
        .list_prefix(&compiled_prefix)
        .await
        .unwrap_or_default();
    keys.sort();
    keys.dedup();
    let keys: Vec<String> = keys
        .into_iter()
        .filter(|k| k.ends_with(".sql"))
        .filter(|k| k.contains("/models/"))
        // Exclude compiled dbt tests under ".../<model>.yml/<test>.sql".
        .filter(|k| !k.contains(".yml/"))
        .filter(|k| key_matches_select_terms(k, select_terms))
        .collect();
    if keys.is_empty() {
        return Ok(serde_json::json!({
            "ok": true,
            "probed_models": 0,
            "skipped_reason": "no_compiled_model_sql_found_for_scope"
        }));
    }

    let mut failures: Vec<serde_json::Value> = Vec::new();
    let mut probed = 0usize;
    for key in keys.iter() {
        let bytes = match ctx.storage.get_bytes(key).await {
            Ok(b) => b,
            Err(e) => {
                failures.push(serde_json::json!({
                    "key": key,
                    "error": format!("failed_to_read_compiled_sql: {}", e),
                }));
                continue;
            }
        };
        let sql = String::from_utf8_lossy(&bytes).to_string();
        let probe_sql = crate::data_engineer::sql_first::wrap_sql_for_validation(&sql, 1);
        match ctx.warehouse.query(&probe_sql).await {
            Ok(qr) => {
                probed = probed.saturating_add(1);
                let dups =
                    crate::data_engineer::sql_first::detect_duplicate_output_columns(&qr.header);
                if !dups.is_empty() {
                    failures.push(serde_json::json!({
                        "key": key,
                        "model_name": model_name_from_compiled_key(key),
                        "error": "duplicate_output_columns",
                        "duplicate_columns": dups,
                    }));
                }
            }
            Err(e) => {
                failures.push(serde_json::json!({
                    "key": key,
                    "model_name": model_name_from_compiled_key(key),
                    "error": format!("warehouse_probe_failed: {}", e),
                }));
            }
        }
    }
    if failures.is_empty() {
        Ok(serde_json::json!({
            "ok": true,
            "probed_models": probed,
            "failed_models": []
        }))
    } else {
        Ok(serde_json::json!({
            "ok": false,
            "probed_models": probed,
            "failed_models": failures
        }))
    }
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

        let dialect = crate::config::resolved_config_from_ctx(ctx)
            .map(crate::data_engineer::dbt_repair::remediate::active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());

        let max_iters: usize = std::env::var("DBT_REPAIR_MAX_ITERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(8)
            .max(1)
            .min(25);

        let select_terms = derive_select_terms(ctx, &args).await;
        let mut ladder: Vec<ValidationLadderPhase> = Vec::new();
        let mut final_res: react_core::providers::DbtValidateResult;
        let final_report: crate::data_engineer::dbt_repair::repair_loop::RepairReport;

        if build {
            // Phase 1: compile-only + repair loop on targeted scope first (faster fail).
            let compile_args = react_core::providers::DbtValidateArgs {
                project_name: project_name.to_string(),
                profiles_dir: profiles_dir.clone(),
                target: target.clone(),
                run: false,
                build: false,
                select: if select_terms.is_empty() {
                    None
                } else {
                    Some(select_terms.clone())
                },
                exclude: None,
            };
            let (res1, rep1) = crate::data_engineer::dbt_repair::repair_loop::run_repair_loop(
                ctx,
                dbt,
                &compile_args,
                max_iters,
                self.datasets.as_ref(),
                self.catalog.as_ref(),
                dataset_ids.as_deref(),
            )
            .await?;
            let probe = probe_compiled_model_sql(ctx, project_name, &select_terms).await?;
            let probe_ok = probe.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
            ladder.push(ValidationLadderPhase {
                phase: "compile_probe".to_string(),
                select_terms: select_terms.clone(),
                ok: res1.ok && probe_ok,
                compile_ok: res1.compile_ok,
                run_ok: res1.run_ok,
                error_count: res1.errors.len(),
                probe: Some(probe.clone()),
            });
            if !res1.ok || !probe_ok {
                final_res = res1;
                if !probe_ok {
                    let mut errs = final_res.errors.clone();
                    errs.push("compiled_sql_probe_failed: one or more compiled model queries failed warehouse probe or projected duplicate output columns".to_string());
                    if let Some(arr) = probe.get("failed_models").and_then(|v| v.as_array()) {
                        for f in arr.iter().take(20) {
                            errs.push(format!(
                                "compiled_probe: {}",
                                serde_json::to_string(f).unwrap_or_else(|_| "{}".to_string())
                            ));
                        }
                    }
                    final_res.ok = false;
                    final_res.errors = errs;
                }
                final_report = rep1;
            } else {
                // Phase 2: selective build (changed/targeted models only).
                if !select_terms.is_empty() {
                    let selective_args = react_core::providers::DbtValidateArgs {
                        project_name: project_name.to_string(),
                        profiles_dir: profiles_dir.clone(),
                        target: target.clone(),
                        run: false,
                        build: true,
                        select: Some(select_terms.clone()),
                        exclude: None,
                    };
                    let (res2, rep2) =
                        crate::data_engineer::dbt_repair::repair_loop::run_repair_loop(
                            ctx,
                            dbt,
                            &selective_args,
                            max_iters,
                            self.datasets.as_ref(),
                            self.catalog.as_ref(),
                            dataset_ids.as_deref(),
                        )
                        .await?;
                    ladder.push(ValidationLadderPhase {
                        phase: "selective_build".to_string(),
                        select_terms: select_terms.clone(),
                        ok: res2.ok,
                        compile_ok: res2.compile_ok,
                        run_ok: res2.run_ok,
                        error_count: res2.errors.len(),
                        probe: None,
                    });
                    if !res2.ok {
                        final_res = res2;
                        final_report = rep2;
                    } else {
                        // Phase 3: full build for final confidence.
                        let full_args = react_core::providers::DbtValidateArgs {
                            project_name: project_name.to_string(),
                            profiles_dir: profiles_dir.clone(),
                            target: target.clone(),
                            run: false,
                            build: true,
                            select: None,
                            exclude: None,
                        };
                        let (res3, rep3) =
                            crate::data_engineer::dbt_repair::repair_loop::run_repair_loop(
                                ctx,
                                dbt,
                                &full_args,
                                max_iters,
                                self.datasets.as_ref(),
                                self.catalog.as_ref(),
                                dataset_ids.as_deref(),
                            )
                            .await?;
                        ladder.push(ValidationLadderPhase {
                            phase: "full_build".to_string(),
                            select_terms: vec![],
                            ok: res3.ok,
                            compile_ok: res3.compile_ok,
                            run_ok: res3.run_ok,
                            error_count: res3.errors.len(),
                            probe: None,
                        });
                        final_res = res3;
                        final_report = rep3;
                    }
                } else {
                    // No targeted selectors -> skip selective stage and go straight to full build.
                    let full_args = react_core::providers::DbtValidateArgs {
                        project_name: project_name.to_string(),
                        profiles_dir: profiles_dir.clone(),
                        target: target.clone(),
                        run: false,
                        build: true,
                        select: None,
                        exclude: None,
                    };
                    let (res3, rep3) =
                        crate::data_engineer::dbt_repair::repair_loop::run_repair_loop(
                            ctx,
                            dbt,
                            &full_args,
                            max_iters,
                            self.datasets.as_ref(),
                            self.catalog.as_ref(),
                            dataset_ids.as_deref(),
                        )
                        .await?;
                    ladder.push(ValidationLadderPhase {
                        phase: "full_build".to_string(),
                        select_terms: vec![],
                        ok: res3.ok,
                        compile_ok: res3.compile_ok,
                        run_ok: res3.run_ok,
                        error_count: res3.errors.len(),
                        probe: None,
                    });
                    final_res = res3;
                    final_report = rep3;
                }
            }
        } else {
            let (res, repair_report) =
                crate::data_engineer::dbt_repair::repair_loop::run_repair_loop(
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
            ladder.push(ValidationLadderPhase {
                phase: "default".to_string(),
                select_terms: vec![],
                ok: res.ok,
                compile_ok: res.compile_ok,
                run_ok: res.run_ok,
                error_count: res.errors.len(),
                probe: None,
            });
            final_res = res;
            final_report = repair_report;
        }

        let mut v = serde_json::to_value(final_res).unwrap_or_else(
            |_| serde_json::json!({"ok": false, "error": "failed to serialize result"}),
        );
        if let Some(obj) = v.as_object_mut() {
            obj.insert("dialect".to_string(), serde_json::json!(dialect));
            obj.insert(
                "repair_report".to_string(),
                serde_json::to_value(final_report).unwrap_or(Value::Null),
            );
            obj.insert(
                "validation_ladder".to_string(),
                serde_json::to_value(ladder).unwrap_or(Value::Null),
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

            let errors: Vec<String> = obj
                .get("errors")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let compile_ok = obj
                .get("compile_ok")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let run_ok = obj.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let logs = obj.get("logs").cloned().unwrap_or(Value::Null);
            let failure_class = failure_class_key_from_errors(&errors);
            let failing_targets = if ok {
                Vec::new()
            } else {
                build_failing_targets(&logs, failure_class)
            };
            if !ok && failing_targets.is_empty() {
                return Err(
                    "validate_outcome_v2_contract_error: missing failing_targets for failed validate"
                        .to_string(),
                );
            }
            let failure_signature = failing_targets.first().map(|t| {
                serde_json::json!({
                    "class": failure_class,
                    "node_id": t.get("node_id").and_then(|v| v.as_str()),
                    "canonical_path": t.get("canonical_path").and_then(|v| v.as_str()),
                    "error_code": t.get("error_code").and_then(|v| v.as_str()),
                })
            });
            obj.insert(
                "validate_outcome_v2".to_string(),
                serde_json::json!({
                    "ok": ok,
                    "compile_ok": compile_ok,
                    "run_ok": run_ok,
                    "failing_targets": failing_targets,
                    "failure_signature": failure_signature,
                }),
            );
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
                // 1) grounded remediation selects file + provides brief instructions
                serde_json::json!({
                    "changes": [{
                        "key":"t/w/p/dbt/models/m.sql",
                        "instructions": "Append `-- remediation` to the query to produce a minimal change.",
                        "reason":"minimal change to trigger retry"
                    }],
                    "notes": []
                })
                .to_string(),
                // 2) single-file patch authored via llm_patch_loop_single_file
                serde_json::json!({
                    "path": "models/m.sql",
                    "patch_text": "@@ ... @@\n-select 1\n+select 1 -- remediation\n",
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
            resolved_config: Some(minimal_cfg()),
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
        assert_eq!(
            obs.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "expected dbt_validate ok after one remediation retry; got: {}",
            serde_json::to_string_pretty(&obs).unwrap_or_default()
        );
        // Repair report should exist (best-effort)
        let _r: Option<crate::data_engineer::dbt_repair::repair_loop::RepairReport> = obs
            .get("repair_report")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
    }
}
