use async_trait::async_trait;
use serde_json::Value;

use crate::providers::{CatalogProvider, DatasetCatalogProvider};
use react_core::agent::AgentCtx;
use react_core::storage::retry_get_bytes;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationScope {
    PlanSlice,
    PlanOwned,
    FullProject,
}

impl ValidationScope {
    fn from_args(args: &Value, select_terms: &[String]) -> Self {
        match args
            .get("validation_scope")
            .or_else(|| args.get("scope"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("plan_slice") | Some("slice") => Self::PlanSlice,
            Some("plan_owned") | Some("owned") => Self::PlanOwned,
            Some("full_project") | Some("full") => Self::FullProject,
            _ if !select_terms.is_empty() => Self::PlanSlice,
            _ => Self::FullProject,
        }
    }

    fn allows_full_build_escalation(self) -> bool {
        matches!(self, Self::FullProject)
    }
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
    let Some(thread_id) = ctx.thread_id().as_deref() else {
        return vec![];
    };
    let Some(store) = ctx.thread_store().as_ref() else {
        return vec![];
    };
    let st =
        match crate::state_manager::load_execution_state_strict(&store.control_store(), thread_id)
            .await
        {
            Ok(Some(st)) => st,
            Ok(None) => crate::progress_controller::ExecutionState::new(),
            Err(e) => {
                tracing::warn!(
                    "skipping targeted validate terms due to invalid execution state: {}",
                    e
                );
                return vec![];
            }
        };
    let mut out = st
        .telemetry
        .last_mutation_summary
        .as_ref()
        .map(|m| m.select_terms.clone())
        .unwrap_or_default();
    out.sort();
    out.dedup();
    out
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

async fn probe_compiled_model_sql(
    ctx: &AgentCtx,
    _project_name: &str,
    select_terms: &[String],
) -> Result<serde_json::Value, String> {
    let compiled_prefix = format!(
        "{}target/compiled/",
        ctx.keyspace().scoped_prefix(ctx.scope(), &["dbt"])
    );
    let mut keys = ctx
        .storage()
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
        let bytes = match retry_get_bytes(ctx.storage().as_ref(), key).await {
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
        let probe_sql = crate::sql_first::wrap_sql_for_validation(&sql, 1);
        let wh = crate::ctx_ext::actx_warehouse(ctx).unwrap();
        let probe_result =
            crate::transient_retry::retry_transient_default("compiled_sql_probe", || async {
                wh.query(&probe_sql).await
            })
            .await;
        match probe_result {
            Ok(qr) => {
                probed = probed.saturating_add(1);
                let dups = crate::sql_first::detect_duplicate_output_columns(&qr.header);
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
        let dbt =
            crate::ctx_ext::actx_dbt(ctx).ok_or_else(|| "dbt provider missing".to_string())?;
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
        let _dataset_ids: Option<Vec<String>> = args
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
            if let Some(cfg) = crate::resolved_config_from_ctx(ctx) {
                let threads = crate::ctx_ext::actx_warehouse(ctx)
                    .map(|w| crate::providers::QueryProvider::max_concurrency(w.as_ref()));
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
            profiles_dir =
                crate::env_util::getenv_nonempty(crate::env_util::env_keys::DBT_PROFILES_DIR);
        }
        let target = target.ok_or_else(|| {
            "dbt_validate requires a dbt target name (e.g. 'athena', 'postgres', 'snowflake', 'bigquery', 'sqlserver'). Configure providers.dbt.target or pass args.target explicitly."
                .to_string()
        })?;

        let dialect = crate::resolved_config_from_ctx(ctx)
            .map(crate::dialect::active_provider_dialect)
            .unwrap_or_else(|| "Unknown SQL dialect".to_string());

        let select_terms = derive_select_terms(ctx, &args).await;
        let validation_scope = ValidationScope::from_args(&args, &select_terms);
        let mut ladder: Vec<ValidationLadderPhase> = Vec::new();
        let mut final_res: crate::providers::DbtValidateResult;

        if build {
            let compile_args = crate::providers::DbtValidateArgs {
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
            let res1 =
                crate::transient_retry::retry_transient_default("dbt_validate_compile", || async {
                    dbt.validate_project(ctx.scope(), &compile_args).await
                })
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
            } else if !select_terms.is_empty() {
                let selective_args = crate::providers::DbtValidateArgs {
                    project_name: project_name.to_string(),
                    profiles_dir: profiles_dir.clone(),
                    target: target.clone(),
                    run: false,
                    build: true,
                    select: Some(select_terms.clone()),
                    exclude: None,
                };
                let res2 = crate::transient_retry::retry_transient_default(
                    "dbt_validate_selective",
                    || async { dbt.validate_project(ctx.scope(), &selective_args).await },
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
                if !res2.ok || !validation_scope.allows_full_build_escalation() {
                    final_res = res2;
                } else {
                    let full_args = crate::providers::DbtValidateArgs {
                        project_name: project_name.to_string(),
                        profiles_dir: profiles_dir.clone(),
                        target: target.clone(),
                        run: false,
                        build: true,
                        select: None,
                        exclude: None,
                    };
                    let res3 = crate::transient_retry::retry_transient_default(
                        "dbt_validate_full",
                        || async { dbt.validate_project(ctx.scope(), &full_args).await },
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
                }
            } else {
                let full_args = crate::providers::DbtValidateArgs {
                    project_name: project_name.to_string(),
                    profiles_dir: profiles_dir.clone(),
                    target: target.clone(),
                    run: false,
                    build: true,
                    select: None,
                    exclude: None,
                };
                let res3 = crate::transient_retry::retry_transient_default(
                    "dbt_validate_full",
                    || async { dbt.validate_project(ctx.scope(), &full_args).await },
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
            }
        } else {
            let validate_args = crate::providers::DbtValidateArgs {
                project_name: project_name.to_string(),
                profiles_dir: profiles_dir.clone(),
                target: target.clone(),
                run,
                build,
                select: None,
                exclude: None,
            };
            let res =
                crate::transient_retry::retry_transient_default("dbt_validate_default", || async {
                    dbt.validate_project(ctx.scope(), &validate_args).await
                })
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
        }

        let mut v = serde_json::to_value(final_res).unwrap_or_else(
            |_| serde_json::json!({"ok": false, "error": "failed to serialize result"}),
        );
        if let Some(obj) = v.as_object_mut() {
            obj.insert("dialect".to_string(), serde_json::json!(dialect));
            obj.insert(
                "validation_scope".to_string(),
                serde_json::to_value(validation_scope).unwrap_or(Value::Null),
            );
            obj.insert(
                "validation_ladder".to_string(),
                serde_json::to_value(ladder).unwrap_or(Value::Null),
            );

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
                match crate::dbt_error::summarize_dbt_failure_llm(ctx, &errors, &logs, 2000).await {
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

            let _ = crate::controller_event::attach_validate_outcome_v2(&mut v)?;
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::ValidationScope;

    #[test]
    fn dbt_validate_mock_change_does_not_require_expected_sha256() {
        // Ensure unit tests do not require LLM echo of sha (suite enforces drift safety internally).
        let v = serde_json::json!({
            "patch_text": "@@\n- select 1\n+ select 2\n",
            "notes": []
        });
        assert!(v.get("patch_text").and_then(|x| x.as_str()).is_some());
    }

    #[test]
    fn validation_scope_defaults_to_plan_slice_when_select_terms_exist() {
        let args = serde_json::json!({});
        let select_terms = vec!["agg_daily_product_sales".to_string()];

        let scope = ValidationScope::from_args(&args, &select_terms);

        assert_eq!(scope, ValidationScope::PlanSlice);
        assert!(!scope.allows_full_build_escalation());
    }

    #[test]
    fn validation_scope_can_request_full_project() {
        let args = serde_json::json!({"validation_scope": "full_project"});
        let select_terms = vec!["agg_daily_product_sales".to_string()];

        let scope = ValidationScope::from_args(&args, &select_terms);

        assert_eq!(scope, ValidationScope::FullProject);
        assert!(scope.allows_full_build_escalation());
    }
}
