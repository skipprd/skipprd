use super::remediate::active_provider_dialect;
use super::remediate::list_sql_keys_for_scope;
use super::remediate::remediate_dbt_failures_grounded_with_llm;
use super::remediate::RemediationDiff;
use react_core::agent::AgentCtx;
use react_core::providers::{
    CatalogProvider, DatasetCatalogProvider, DbtProvider, DbtValidateArgs, DbtValidateResult,
};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};
use std::sync::Arc;

fn errors_look_like_missing_dbt_utils(errors: &[String]) -> bool {
    let s = errors.join("\n").to_lowercase();
    if !s.contains("dbt_utils") {
        return false;
    }
    s.contains("is undefined")
        || s.contains("could not find macro")
        || (s.contains("macro") && s.contains("not found"))
}

async fn ensure_dbt_utils_package(ctx: &AgentCtx) -> Result<Option<RemediationDiff>, String> {
    // Returns Some(diff) if packages.yml was mutated.
    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string();
    let key = format!("{}/packages.yml", base);
    let existing_opt = ctx
        .storage
        .get_bytes(&key)
        .await
        .ok()
        .map(|b| String::from_utf8_lossy(&b).to_string());
    let existed = existing_opt.is_some();
    let existing = existing_opt.unwrap_or_default();

    let mut root = if existing.trim().is_empty() {
        YamlMapping::new()
    } else {
        let v: YamlValue = serde_yaml::from_str(&existing)
            .map_err(|e| format!("packages.yml parse error: {}", e))?;
        match v {
            YamlValue::Mapping(m) => m,
            _ => return Err("packages.yml must be a YAML mapping at top level".to_string()),
        }
    };

    let packages_seq = match root.get(&YamlValue::String("packages".to_string())) {
        Some(YamlValue::Sequence(seq)) => seq.clone(),
        Some(_) => return Err("packages.yml 'packages' must be a list".to_string()),
        None => Vec::new(),
    };

    let mut has_utils = false;
    for item in packages_seq.iter() {
        let Some(m) = item.as_mapping() else { continue };
        let pkg = m
            .get(&YamlValue::String("package".to_string()))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        if pkg == "dbt-labs/dbt_utils" {
            has_utils = true;
            break;
        }
    }
    if has_utils {
        return Ok(None);
    }

    let mut new_seq = packages_seq;
    let mut entry = YamlMapping::new();
    entry.insert(
        YamlValue::String("package".to_string()),
        YamlValue::String("dbt-labs/dbt_utils".to_string()),
    );
    // Conservative version range compatible with modern dbt (normalized by postprocess).
    entry.insert(
        YamlValue::String("version".to_string()),
        YamlValue::Sequence(vec![
            YamlValue::String(">=1.0.0".to_string()),
            YamlValue::String("<2.0.0".to_string()),
        ]),
    );
    new_seq.push(YamlValue::Mapping(entry));
    root.insert(
        YamlValue::String("packages".to_string()),
        YamlValue::Sequence(new_seq),
    );

    let new_content =
        serde_yaml::to_string(&YamlValue::Mapping(root)).map_err(|e| e.to_string())?;
    let patch_text = crate::data_engineer::project_fs::create_git_patch_text(
        &existing,
        &new_content,
        "packages.yml",
        existed,
    )?;
    let outcome = crate::data_engineer::project_fs::apply_patch(
        ctx,
        None,
        "packages.yml",
        &patch_text,
        None,
        crate::data_engineer::project_fs::PatchApplyKind::UnifiedDiff,
    )
    .await?;
    ctx.storage
        .put_bytes(&outcome.key, outcome.content.as_bytes(), "text/yaml")
        .await?;
    if outcome.base_sha256 == outcome.new_sha256 {
        return Ok(None);
    }
    Ok(Some(RemediationDiff {
        key: outcome.key.clone(),
        rel_path: outcome.rel_path.clone(),
        base_sha256: outcome.base_sha256.clone(),
        new_sha256: outcome.new_sha256.clone(),
        lines_added: outcome.lines_added,
        lines_removed: outcome.lines_removed,
        diff: super::remediate::truncate_diff(&outcome.git_patch, 8_000),
    }))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepairIteration {
    pub iteration: usize,
    pub scanned_models: usize,
    pub rewritten_models: usize,
    pub catalog_refreshed: bool,
    pub llm_changed_files: usize,
    #[serde(default)]
    pub changed_keys: Vec<String>,
    #[serde(default)]
    pub change_diffs: Vec<RemediationDiff>,
    #[serde(default)]
    pub notes: Vec<String>,
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
                    .trim_matches(|c| c == '(' || c == ')' || c == '\"' || c == '\'');
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
    let base = ctx
        .keyspace
        .dbt_prefix(&ctx.scope)
        .trim_end_matches('/')
        .to_string();
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
    args: &DbtValidateArgs,
    max_iterations: usize,
    datasets: Option<&Arc<dyn DatasetCatalogProvider>>,
    catalog: Option<&Arc<dyn CatalogProvider>>,
    dataset_ids: Option<&[String]>,
) -> Result<(DbtValidateResult, RepairReport), String> {
    let dialect = crate::config::resolved_config_from_ctx(ctx)
        .map(active_provider_dialect)
        .unwrap_or_else(|| "Unknown SQL dialect".to_string());

    let mut report = RepairReport {
        dialect: dialect.clone(),
        max_iterations,
        ..Default::default()
    };

    let max_it = max_iterations.max(1).min(25);
    for i in 0..max_it {
        let res = dbt.validate_project(&ctx.scope, args).await?;

        report.iterations_run = i + 1;
        let unresolved_columns =
            crate::data_engineer::dbt_error::extract_unresolved_columns(&res.errors);
        let class = crate::data_engineer::dbt_error::classify(&res.errors);
        let _ = (datasets, catalog, dataset_ids); // reserved for future targeted catalog refresh
        let catalog_refreshed = false;

        // Deterministic packages auto-repair for common missing-macro cases (e.g. dbt_utils).
        // This is safe because packages.yml is normalized/deduped by project_fs postprocess.
        if errors_look_like_missing_dbt_utils(&res.errors) {
            let diff = ensure_dbt_utils_package(ctx).await.ok().flatten();
            let mutated = diff.is_some();
            let diffs: Vec<RemediationDiff> = diff.into_iter().collect();
            report.iterations.push(RepairIteration {
                iteration: i + 1,
                scanned_models: 0,
                rewritten_models: 0,
                catalog_refreshed,
                llm_changed_files: if mutated { 1 } else { 0 },
                changed_keys: diffs.iter().map(|d| d.key.clone()).collect(),
                change_diffs: diffs,
                notes: if mutated {
                    vec!["deterministic: added dbt-labs/dbt_utils to packages.yml".to_string()]
                } else {
                    vec![]
                },
                dbt_ok: res.ok,
                compile_ok: res.compile_ok,
                run_ok: res.run_ok,
                unresolved_columns,
                errors: res.errors.clone(),
            });
            if mutated {
                // Next validate_project should run dbt deps as part of validation.
                if i + 1 == max_it {
                    report.stopped_reason = Some("max_iterations".to_string());
                    return Ok((res, report));
                }
                continue;
            }
            // No mutation possible -> treat as no progress.
            report.stopped_reason = Some("packages_no_progress".to_string());
            return Ok((res, report));
        }

        // Missing sources are grounding failures; do NOT attempt SQL remediation.
        if matches!(
            class,
            crate::data_engineer::dbt_error::DbtErrorClass::MissingSource
        ) {
            report.iterations.push(RepairIteration {
                iteration: i + 1,
                scanned_models: 0,
                rewritten_models: 0,
                catalog_refreshed,
                llm_changed_files: 0,
                changed_keys: vec![],
                change_diffs: vec![],
                notes: vec!["missing dbt source definition; treat as dataset grounding failure (schema.yml vs actual datasets)".to_string()],
                dbt_ok: res.ok,
                compile_ok: res.compile_ok,
                run_ok: res.run_ok,
                unresolved_columns,
                errors: res.errors.clone(),
            });
            report.stopped_reason = Some("missing_source".to_string());
            return Ok((res, report));
        }

        let allow_llm_repair = matches!(
            class,
            crate::data_engineer::dbt_error::DbtErrorClass::SqlFailure
                | crate::data_engineer::dbt_error::DbtErrorClass::SqlOrModel
                | crate::data_engineer::dbt_error::DbtErrorClass::Unknown
        );

        let mut llm_changed_files: usize = 0;
        let mut changed_keys: Vec<String> = Vec::new();
        let mut change_diffs: Vec<RemediationDiff> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        if allow_llm_repair && !res.ok {
            // Scope to failing file(s) when possible; otherwise scan all model SQL keys.
            let rels = extract_sql_rel_paths_from_dbt_errors(&res.errors);
            let mut keys = storage_keys_for_rel_paths(ctx, &rels);
            if keys.is_empty() {
                keys = list_sql_keys_for_scope(ctx).await.unwrap_or_default();
            }

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

            let rem = remediate_dbt_failures_grounded_with_llm(
                ctx,
                &format!("repair_loop_grounded_{phase}"),
                &res.errors,
                &keys,
                datasets,
            )
            .await
            .ok();
            llm_changed_files = rem.as_ref().map(|r| r.changed_files).unwrap_or(0);
            if let Some(r) = rem.as_ref() {
                notes.extend(r.notes.clone());
                change_diffs.extend(r.diffs.clone());
                for ch in r.changes.iter() {
                    if ch.changed && !ch.key.trim().is_empty() {
                        changed_keys.push(ch.key.clone());
                    }
                }
            }
        }

        tracing::info!(
            target: "dbt_repair_loop",
            iteration = i + 1,
            dialect = %dialect,
            error_class = ?class,
            llm_changed_files = llm_changed_files,
            "repair decision (grounded_llm)"
        );

        report.iterations.push(RepairIteration {
            iteration: i + 1,
            scanned_models: 0,
            rewritten_models: 0,
            catalog_refreshed,
            llm_changed_files,
            changed_keys,
            change_diffs,
            notes,
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
        if matches!(
            class,
            crate::data_engineer::dbt_error::DbtErrorClass::WarehouseConfig
        ) {
            report.stopped_reason = Some("warehouse_config".to_string());
            return Ok((res, report));
        }

        // If we made no progress this iteration, stop.
        if !catalog_refreshed && llm_changed_files == 0 {
            report.stopped_reason = Some("llm_no_progress".to_string());
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
    use async_trait::async_trait;
    use react_core::agent::DefaultPolicy;
    use react_core::keyspace::{DefaultKeyspace, Keyspace};
    use react_core::llm::{ChatMessage, LargeLanguageModel};
    use react_core::providers::{DbtValidateArgs, DbtValidateResult};
    use react_core::scope::RequestScope;
    use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};
    use sha2::Digest;
    use std::sync::Mutex;

    fn sha256_hex(s: &str) -> String {
        let mut hasher = sha2::Sha256::new();
        hasher.update(s.as_bytes());
        hex::encode(hasher.finalize())
    }

    struct MockDbt {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl DbtProvider for MockDbt {
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
            _args: &DbtValidateArgs,
        ) -> Result<DbtValidateResult, String> {
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
                    errors: vec![
                        "Runtime Error: Column 'context.session.id' cannot be resolved".to_string(),
                    ],
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
    async fn repair_loop_stops_on_no_progress() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            chat_responses: Mutex::new(vec![
                // Decision: low confidence; no remediation.
                serde_json::json!({"should_remediate": false, "confidence": 0.0, "reason": "not enough information"}).to_string(),
            ]),
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
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };

        struct AlwaysFailDbt;
        #[async_trait]
        impl DbtProvider for AlwaysFailDbt {
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
                _args: &DbtValidateArgs,
            ) -> Result<DbtValidateResult, String> {
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
                select: None,
                exclude: None,
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
        assert_eq!(rep.stopped_reason.as_deref(), Some("llm_no_progress"));
    }

    #[tokio::test]
    async fn repair_loop_stops_on_runtime_run_failure_after_compile() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            // Grounded repair is now always attempted for SQL-ish failures; return no-op changes.
            chat_responses: Mutex::new(vec![
                serde_json::json!({"changes": [], "notes": ["no-op"]}).to_string(),
            ]),
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
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };

        struct CompileOkRunFailDbt;
        #[async_trait]
        impl DbtProvider for CompileOkRunFailDbt {
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
                _args: &DbtValidateArgs,
            ) -> Result<DbtValidateResult, String> {
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
                select: None,
                exclude: None,
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
        assert_eq!(rep.stopped_reason.as_deref(), Some("llm_no_progress"));
    }

    #[tokio::test]
    async fn repair_loop_attempts_unresolved_column_remediation_on_run_failure() {
        std::env::set_var("REACT_LOG_LLM_CALLS", "1");
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        // Seed a staging SQL file that uses struct dereference (will fail if the raw column is literal dotted).
        let base_key = "t/w/p/dbt/models/staging/stg_src_events.sql";
        let old_sql =
            r#"select context.session.id as session_id from {{ source('src','events') }}"#;
        storage
            .put_bytes(base_key, old_sql.as_bytes(), "text/sql")
            .await
            .unwrap();

        let fixed_sql =
            r#"select "context.session.id" as session_id from {{ source('src','events') }}"#;
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            chat_responses: Mutex::new(vec![
                serde_json::json!({
                    "changes": [
                        {"key": base_key, "replace_file": {"new_text": fixed_sql, "expected_sha256": sha256_hex(old_sql)}, "reason": "quote literal dotted column"}
                    ],
                    "notes": ["applied quoted identifier for dotted column"]
                })
                .to_string(),
            ]),
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
            max_steps: 1,
            thread_id: None,
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };
        // Attach a thread store so llm_call steps can be persisted.
        let store = react_core::session::ThreadStore::new(
            storage.clone(),
            scope.clone(),
            Arc::new(DefaultKeyspace::new("b".to_string())),
        );
        let mut ctx = ctx;
        ctx.thread_id = Some("th1".to_string());
        ctx.thread_store = Some(store);

        struct RunFailThenOkDbt {
            calls: Mutex<usize>,
        }
        #[async_trait]
        impl DbtProvider for RunFailThenOkDbt {
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
                _args: &DbtValidateArgs,
            ) -> Result<DbtValidateResult, String> {
                let mut c = self.calls.lock().unwrap();
                *c += 1;
                if *c == 1 {
                    return Ok(DbtValidateResult {
                        ok: false,
                        deps_ok: true,
                        parse_ok: true,
                        compile_ok: true,
                        run_ok: Some(false),
                        uploaded_target_files: 0,
                        errors: vec![format!("Runtime Error: Column 'context.session.id' cannot be resolved (models/staging/stg_src_events.sql)")],
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

        let dbt: Arc<dyn DbtProvider> = Arc::new(RunFailThenOkDbt {
            calls: Mutex::new(0),
        });
        let (_res, rep) = run_repair_loop(
            &ctx,
            &dbt,
            &DbtValidateArgs {
                project_name: "data_engineer".to_string(),
                profiles_dir: None,
                target: "athena".to_string(),
                run: false,
                build: true,
                select: None,
                exclude: None,
            },
            3,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // First iteration should have applied an LLM change rather than stopping early.
        assert!(rep.iterations.len() >= 2);
        assert_eq!(rep.iterations[0].llm_changed_files, 1);
        let bytes = storage.get_bytes(base_key).await.unwrap();
        let got = String::from_utf8_lossy(&bytes);
        assert!(got.contains("\"context.session.id\""));

        // And we should have recorded at least one llm_call step in the persisted thread log.
        let store = ctx.thread_store.as_ref().unwrap();
        let log = store.get("th1").await.unwrap();
        assert!(
            log.steps
                .iter()
                .any(|s| matches!(s, react_core::session::ThreadStep::LlmCall { .. })),
            "expected at least one llm_call step"
        );
    }

    #[tokio::test]
    async fn repair_loop_llm_low_confidence_skips_remediation() {
        std::env::set_var("DBT_REPAIR_LLM_CONFIDENCE_MIN", "0.9");
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        // Seed a SQL file so remediation would have work if it were invoked.
        let ds_id = "AwsDataCatalog.picnic.events";
        let stg_key = format!("t/w/p/dbt/models/{}/stg_events.sql", ds_id);
        storage
            .put_bytes(&stg_key, b"select 1\n", "text/sql")
            .await
            .unwrap();

        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            chat_responses: Mutex::new(vec![
                // Decision: wants remediation but low confidence -> should skip
                serde_json::json!({"should_remediate": true, "confidence": 0.2, "reason": "uncertain"}).to_string(),
            ]),
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
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };

        struct SqlFailureOnceDbt;
        #[async_trait]
        impl DbtProvider for SqlFailureOnceDbt {
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
                _args: &DbtValidateArgs,
            ) -> Result<DbtValidateResult, String> {
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
                select: None,
                exclude: None,
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
        storage
            .put_bytes(&stg_key, b"select 1\n", "text/sql")
            .await
            .unwrap();

        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            chat_responses: Mutex::new(vec![
                // Decision: high confidence
                serde_json::json!({"should_remediate": true, "confidence": 0.9, "reason": "dialect mismatch"}).to_string(),
                // Remediation response (no changes)
                serde_json::json!({"changes": [], "notes": []}).to_string(),
            ]),
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
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };

        struct SqlFailureOnceDbt;
        #[async_trait]
        impl DbtProvider for SqlFailureOnceDbt {
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
                _args: &DbtValidateArgs,
            ) -> Result<DbtValidateResult, String> {
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
                select: None,
                exclude: None,
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

    #[tokio::test]
    async fn repair_loop_auto_adds_dbt_utils_package_on_missing_macro_error() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let llm: Arc<dyn LargeLanguageModel> = Arc::new(MockLlm {
            // Should not be called for deterministic packages fix.
            chat_responses: Mutex::new(vec![]),
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
            max_steps: 1,
            thread_id: None,
            progress_tx: None,
            pre_step_tx: None,
            trace_tx: None,
            agent_name: Some("test".to_string()),
            policy: Arc::new(DefaultPolicy),
            llm,
            storage: storage.clone(),
            scope: scope.clone(),
            keyspace,
            query: None,
            warehouse: Arc::new(react_core::providers::NullWarehouseProvider::default()),
            dbt: None,
            vector: None,
            thread_store: None,
            runtime: Some(minimal_cfg() as Arc<dyn std::any::Any + Send + Sync>),
        };

        struct MissingMacroThenOkDbt {
            calls: Mutex<usize>,
        }
        #[async_trait]
        impl DbtProvider for MissingMacroThenOkDbt {
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
                _args: &DbtValidateArgs,
            ) -> Result<DbtValidateResult, String> {
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
                        errors: vec!["Compilation Error: 'dbt_utils' is undefined".to_string()],
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

        let dbt: Arc<dyn DbtProvider> = Arc::new(MissingMacroThenOkDbt {
            calls: Mutex::new(0),
        });
        let (_res, rep) = run_repair_loop(
            &ctx,
            &dbt,
            &DbtValidateArgs {
                project_name: "data_engineer".to_string(),
                profiles_dir: None,
                target: "athena".to_string(),
                run: false,
                build: false,
                select: None,
                exclude: None,
            },
            3,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // packages.yml should now exist and include dbt_utils.
        let base = ctx
            .keyspace
            .dbt_prefix(&ctx.scope)
            .trim_end_matches('/')
            .to_string();
        let key = format!("{}/packages.yml", base);
        let bytes = storage.get_bytes(&key).await.unwrap();
        let got = String::from_utf8_lossy(&bytes);
        assert!(got.to_lowercase().contains("dbt-labs/dbt_utils"));
        assert!(rep.iterations.len() >= 2);
        assert_eq!(rep.iterations[0].llm_changed_files, 1);
    }
}
