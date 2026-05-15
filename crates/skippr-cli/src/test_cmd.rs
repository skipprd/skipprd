//! `skippr test list` / `skippr test run` — materialize pipeline dbt from S3 and invoke dbt like `skippr model`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use react::config::ServeOverrides;
use react_core::keyspace::Keyspace;
use react_core::resolved_config::{ReactResolvedConfig, StorageMode};
use react_core::scope::RequestScope;
use react_core::storage::StorageAdapter;
use react_module_provider_dbt::{DbtProjectProvider, DbtRunnerConfig, DbtRunnerMode};

use crate::run_results_parse::{parse_run_results_json, ParsedDbtRunResult};
use crate::{api_client, auth};
use crate::{
    attach_s3_credentials_provider, config_path, load_cli_execution_config,
    project_root_from_config_path, react_config_from_pipeline_config, react_host, translate,
};

#[derive(Debug, Clone, clap::Subcommand)]
pub enum TestSubcommand {
    /// Print discovered dbt tests as JSON (manifest-derived).
    List(TestListArgs),
    /// Run `dbt test` and emit JSONL `test_result` lines plus `run_complete`.
    Run(TestRunArgs),
}

#[derive(Debug, Clone, clap::Args)]
pub struct TestListArgs {
    #[arg(long)]
    pub pipeline: String,
    /// Output: json or text.
    #[arg(long, default_value = "json")]
    pub output: String,
}

#[derive(Debug, Clone, clap::Args)]
pub struct TestRunArgs {
    #[arg(long)]
    pub pipeline: String,
    /// dbt `--select` expression (repeatable).
    #[arg(long = "select")]
    pub select: Vec<String>,
    /// Output: jsonl, json, or text.
    #[arg(long, default_value = "jsonl")]
    pub output: String,
}

fn is_json_output(output: &str) -> bool {
    output.trim().eq_ignore_ascii_case("json")
}

fn is_jsonl_output(output: &str) -> bool {
    output.trim().eq_ignore_ascii_case("jsonl")
}

fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

fn emit_jsonl(output: &str, line: &serde_json::Value) {
    if is_jsonl_output(output) {
        println!("{}", line.to_string());
    }
}

async fn storage_and_keyspace(
    cfg: &ReactResolvedConfig,
) -> Result<(Arc<dyn StorageAdapter>, Arc<dyn Keyspace>), String> {
    match cfg.storage.mode {
        StorageMode::Local => {
            let root = cfg
                .storage
                .path
                .as_ref()
                .ok_or_else(|| "missing storage.path for local mode".to_string())?;
            let storage: Arc<dyn StorageAdapter> = Arc::new(
                react_module_storage_local::LocalFileStorageAdapter::new(root)
                    .map_err(|e| e.to_string())?,
            );
            let keyspace: Arc<dyn Keyspace> =
                Arc::new(react_core::keyspace::LocalKeyspace::new(root.clone()));
            Ok((storage, keyspace))
        }
        StorageMode::S3 => {
            let bucket = cfg
                .storage
                .bucket
                .clone()
                .ok_or_else(|| "missing storage.bucket for s3 mode".to_string())?;
            let storage: Arc<dyn StorageAdapter> = if let Some(creds) =
                cfg.storage.s3_credentials.as_ref()
            {
                Arc::new(
                    react_module_storage_s3::S3StorageAdapter::from_resolved_credentials(
                        bucket.clone(),
                        creds,
                    )
                    .await,
                )
            } else {
                Arc::new(react_module_storage_s3::S3StorageAdapter::from_env(bucket.clone()).await)
            };
            let keyspace: Arc<dyn Keyspace> =
                Arc::new(react_core::keyspace::DefaultKeyspace::new(bucket));
            Ok((storage, keyspace))
        }
    }
}

fn dbt_provider_for_resolved(
    cfg: &ReactResolvedConfig,
    storage: Arc<dyn StorageAdapter>,
    keyspace: Arc<dyn Keyspace>,
) -> Result<DbtProjectProvider, String> {
    let providers =
        react_suite_data_engineer::de_config::de_config_from_resolved(cfg).ok_or_else(|| {
            "suite_config missing or invalid for data_engineer (cannot run skippr test)".to_string()
        })?;
    if !providers.dbt.enabled {
        return Err(
            "dbt is disabled in resolved configuration (providers.dbt.enabled=false)".to_string(),
        );
    }
    let runner_mode = DbtRunnerMode::parse(&providers.dbt.runner).map_err(|e| e.to_string())?;
    let runner = DbtRunnerConfig {
        mode: runner_mode,
        docker_image: providers.dbt.docker_image.clone(),
        docker_platform: providers.dbt.docker_platform.clone(),
        docker_network: providers.dbt.docker_network.clone(),
        docker_mount_aws_dir: providers.dbt.docker_mount_aws_dir,
    };
    Ok(DbtProjectProvider::new(storage, keyspace, runner))
}

fn pipeline_cache_root(cfg_path: &Path, tenant: &str, pipeline: &str) -> PathBuf {
    let project_root = project_root_from_config_path(cfg_path);
    crate::skippr_dir_from_project_root(&project_root)
        .join(tenant.trim())
        .join("dev")
        .join(pipeline.trim())
        .join("dbt_materialized")
}

async fn prepare_resolved_pipeline(
    explicit_config: &Option<PathBuf>,
    pipeline: &str,
) -> Result<ReactResolvedConfig, String> {
    let engine_cfg = load_cli_execution_config(explicit_config)?;
    crate::validate_pipeline_exists(&engine_cfg, pipeline)?;

    let mut internal_file = react_config_from_pipeline_config(&engine_cfg, pipeline)?;

    let authenticated_with_api_key = std::env::var("SKIPPR_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let creds = if let Ok(api_key) = std::env::var("SKIPPR_API_KEY") {
        if api_key.trim().is_empty() {
            return Err("SKIPPR_API_KEY is set but empty".to_string());
        }
        let base_url = auth::auth_base_url();
        let client = api_client::ApiClient::new(&base_url);
        client
            .exchange_api_key(api_key.trim())
            .await
            .map_err(|e| format!("API key authentication failed: {e}"))?
    } else if let Some(creds) = auth::load_credentials() {
        crate::refresh_user_credentials_or_exit(
            &api_client::ApiClient::new(&auth::auth_base_url()),
            creds,
        )
        .await
    } else {
        return Err(
            "authentication required: run `skippr user login` or set SKIPPR_API_KEY".to_string(),
        );
    };

    let base_url = auth::auth_base_url();
    let tokens = crate::create_token_provider(&creds);
    let client = api_client::ApiClient::authenticated(&base_url, Arc::clone(&tokens));

    crate::ensure_eula_accepted(&client, !authenticated_with_api_key)
        .await
        .map_err(|e| e.to_string())?;

    let initial_balance = match client.get_account().await {
        Ok(account) => {
            let bal = account.balance.balance;
            if bal <= 0.0 {
                return Err("balance is $0.00; add funds to continue".to_string());
            } else if bal < crate::LOW_BALANCE_USD_THRESHOLD {
                eprintln!(
                    "[skippr] WARNING: Low balance (${:.2}). The run may exhaust your balance.",
                    bal
                );
            } else {
                eprintln!("[skippr] balance: ${:.2}", bal);
            }
            bal
        }
        Err(e) => {
            return Err(format!("could not verify account balance ({e})"));
        }
    };

    let srv_creds = client
        .get_credentials()
        .await
        .map_err(|e| format!("failed to fetch server credentials: {e}"))?;
    translate::apply_authenticated_overlay(
        &mut internal_file,
        &srv_creds,
        Arc::clone(&tokens),
        initial_balance,
    )
    .map_err(|e| format!("apply_authenticated_overlay: {e}"))?;

    let mut resolved = react_host::resolve_config(internal_file, ServeOverrides::default())
        .map_err(|e| format!("resolve_config: {e}"))?;
    attach_s3_credentials_provider(&mut resolved, client);
    Ok(resolved)
}

fn write_profiles_dir(cache_root: &Path, yml: &str) -> Result<PathBuf, String> {
    let dir = cache_root.join("dbt_profiles");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let p = dir.join("profiles.yml");
    std::fs::write(&p, yml.as_bytes()).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn tier_env_pairs(
    gen: &react_suite_data_engineer::GeneratedProfiles,
) -> Vec<(&'static str, String)> {
    gen.tier_routing.env_vars()
}

async fn materialize_project(
    dbt: &DbtProjectProvider,
    scope: &RequestScope,
    project_dir: &Path,
) -> Result<usize, String> {
    let (n, _stripped) = dbt
        .materialize_scoped_dbt_tree(scope, "data_engineer", project_dir)
        .await?;
    Ok(n)
}

fn tests_from_manifest(manifest_path: &Path) -> Result<Vec<serde_json::Value>, String> {
    let raw = std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let nodes = v
        .get("nodes")
        .and_then(|n| n.as_object())
        .ok_or_else(|| "manifest.json: missing nodes object".to_string())?;
    let mut tests: Vec<serde_json::Value> = Vec::new();
    for (_k, node) in nodes {
        let resource_type = node
            .get("resource_type")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if resource_type != "test" {
            continue;
        }
        let unique_id = node
            .get("unique_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if unique_id.is_empty() {
            continue;
        }
        let name = node
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let path = node.get("path").cloned();
        let original_file_path = node.get("original_file_path").cloned();
        let column_name = node.get("column_name").cloned();
        let depends_on = node.get("depends_on").cloned();
        tests.push(serde_json::json!({
            "unique_id": unique_id,
            "name": name,
            "resource_type": resource_type,
            "path": path,
            "original_file_path": original_file_path,
            "column": column_name,
            "depends_on": depends_on,
        }));
    }
    tests.sort_by(|a, b| {
        let au = a.get("unique_id").and_then(|x| x.as_str()).unwrap_or("");
        let bu = b.get("unique_id").and_then(|x| x.as_str()).unwrap_or("");
        au.cmp(bu)
    });
    Ok(tests)
}

fn run_dbt_deps_parse(
    dbt: &DbtProjectProvider,
    project_dir: &Path,
    profiles_dir: &Path,
    target: &str,
    tier_env: &[(&'static str, String)],
) -> Result<(), String> {
    let profiles_path = Some(profiles_dir);
    let mut base_env: Vec<(&str, String)> = vec![(
        "DBT_PROFILES_DIR",
        profiles_dir.to_string_lossy().to_string(),
    )];
    base_env.extend(tier_env.iter().cloned());

    let deps = dbt.invoke_dbt_cli(project_dir, profiles_path, &["deps"], &base_env, "deps");
    if !deps.status_ok {
        return Err(format!(
            "dbt deps failed (code {})\nstdout:\n{}\nstderr:\n{}",
            deps.code, deps.stdout, deps.stderr
        ));
    }
    let parse = dbt.invoke_dbt_cli(
        project_dir,
        profiles_path,
        &["parse", "--target", target],
        &base_env,
        "parse",
    );
    if !parse.status_ok {
        return Err(format!(
            "dbt parse failed (code {})\nstdout:\n{}\nstderr:\n{}",
            parse.code, parse.stdout, parse.stderr
        ));
    }
    Ok(())
}

pub async fn cmd_test_list(
    _log: Option<String>,
    explicit_config: &Option<PathBuf>,
    args: TestListArgs,
) -> Result<(), String> {
    let resolved = prepare_resolved_pipeline(explicit_config, &args.pipeline).await?;
    let cfg_path = config_path(explicit_config);
    let tenant = resolved.scope.tenant.to_string();
    let cache_root = pipeline_cache_root(&cfg_path, &tenant, &args.pipeline);
    let project_dir = cache_root.join("data_engineer");
    std::fs::create_dir_all(&project_dir).map_err(|e| e.to_string())?;

    let scope =
        RequestScope::parse(&tenant, "dev", &args.pipeline).map_err(|e| format!("scope: {e}"))?;
    let (storage, keyspace) = storage_and_keyspace(&resolved).await?;
    let dbt = dbt_provider_for_resolved(&resolved, storage, keyspace)?;

    let n = materialize_project(&dbt, &scope, &project_dir).await?;
    eprintln!(
        "[skippr] materialized {n} files under {}",
        project_dir.display()
    );

    let gen = react_suite_data_engineer::skippr_cli_generate_dbt_profiles_yml(&resolved, None)?;
    let profiles_dir = write_profiles_dir(&cache_root, &gen.profiles_yml)?;
    let target = gen.target.as_str();
    let tier_env = tier_env_pairs(&gen);

    let _ = std::fs::remove_dir_all(project_dir.join("target"));
    run_dbt_deps_parse(&dbt, &project_dir, &profiles_dir, target, &tier_env)?;

    let manifest_path = project_dir.join("target").join("manifest.json");
    let tests = tests_from_manifest(&manifest_path)?;
    let project_root = project_dir.canonicalize().unwrap_or(project_dir.clone());
    let doc = serde_json::json!({
        "pipeline": args.pipeline,
        "tenant": tenant,
        "project_root": project_root.to_string_lossy(),
        "tests": tests,
    });
    if is_json_output(&args.output) {
        print_json(&doc);
    } else if !is_jsonl_output(&args.output) {
        println!(
            "{} test node(s) — project {}",
            tests.len(),
            project_root.display()
        );
        for t in &tests {
            if let Some(uid) = t.get("unique_id").and_then(|x| x.as_str()) {
                println!("  {}", uid);
            }
        }
    }
    Ok(())
}

fn map_dbt_status(s: &str) -> &'static str {
    match s {
        "pass" => "pass",
        "success" => "pass",
        "fail" | "failed" => "fail",
        "error" | "runtime error" => "error",
        "skipped" | "skip" => "skipped",
        _ => "error",
    }
}

fn summarize_run_results(rows: &[ParsedDbtRunResult]) -> (bool, usize, usize) {
    let mut fail = 0usize;
    let mut err = 0usize;
    for r in rows {
        let m = map_dbt_status(r.status.as_str());
        if m == "fail" {
            fail += 1;
        } else if m == "error" {
            err += 1;
        }
    }
    let ok = fail == 0 && err == 0;
    (ok, fail, err)
}

pub async fn cmd_test_run(
    _log: Option<String>,
    explicit_config: &Option<PathBuf>,
    args: TestRunArgs,
) -> Result<(), String> {
    let resolved = prepare_resolved_pipeline(explicit_config, &args.pipeline).await?;
    let cfg_path = config_path(explicit_config);
    let tenant = resolved.scope.tenant.to_string();
    let cache_root = pipeline_cache_root(&cfg_path, &tenant, &args.pipeline);
    let project_dir = cache_root.join("data_engineer");
    std::fs::create_dir_all(&project_dir).map_err(|e| e.to_string())?;

    let scope =
        RequestScope::parse(&tenant, "dev", &args.pipeline).map_err(|e| format!("scope: {e}"))?;
    let (storage, keyspace) = storage_and_keyspace(&resolved).await?;
    let dbt = dbt_provider_for_resolved(&resolved, storage, keyspace)?;

    let n = materialize_project(&dbt, &scope, &project_dir).await?;
    eprintln!(
        "[skippr] materialized {n} files under {}",
        project_dir.display()
    );

    let gen = react_suite_data_engineer::skippr_cli_generate_dbt_profiles_yml(&resolved, None)?;
    let profiles_dir = write_profiles_dir(&cache_root, &gen.profiles_yml)?;
    let target = gen.target.clone();
    let tier_env = tier_env_pairs(&gen);
    let profiles_pd = profiles_dir.as_path();
    let mut base_env: Vec<(&str, String)> = vec![(
        "DBT_PROFILES_DIR",
        profiles_pd.to_string_lossy().to_string(),
    )];
    base_env.extend(tier_env.iter().cloned());

    let _ = std::fs::remove_dir_all(project_dir.join("target"));
    run_dbt_deps_parse(&dbt, &project_dir, profiles_pd, target.as_str(), &tier_env)?;

    let mut argv: Vec<String> = vec!["test".into(), "--target".into(), target.clone()];
    for s in &args.select {
        if !s.trim().is_empty() {
            argv.push("--select".into());
            argv.push(s.trim().to_string());
        }
    }
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let test_out = dbt.invoke_dbt_cli(
        &project_dir,
        Some(profiles_pd),
        &argv_refs,
        &base_env,
        "test",
    );

    let run_results_path = project_dir.join("target").join("run_results.json");
    let rr_path_abs = run_results_path
        .canonicalize()
        .unwrap_or(run_results_path.clone());

    let parsed: Vec<ParsedDbtRunResult> = if run_results_path.is_file() {
        let txt = std::fs::read_to_string(&run_results_path).unwrap_or_default();
        parse_run_results_json(&txt).unwrap_or_default()
    } else {
        Vec::new()
    };

    if is_jsonl_output(&args.output) {
        if !test_out.status_ok {
            emit_jsonl(
                &args.output,
                &serde_json::json!({
                    "event": "test_error",
                    "message": format!("dbt test exited with code {}", test_out.code),
                    "stdout": test_out.stdout,
                    "stderr": test_out.stderr,
                }),
            );
        }
        for row in &parsed {
            emit_jsonl(
                &args.output,
                &serde_json::json!({
                    "event": "test_result",
                    "unique_id": row.unique_id,
                    "status": map_dbt_status(row.status.as_str()),
                    "message": row.message,
                    "failures": row.failures,
                    "compiled_path": row.compiled_path,
                    "path": row.path,
                }),
            );
        }
        emit_jsonl(
            &args.output,
            &serde_json::json!({
                "event": "run_complete",
                "run_results": rr_path_abs.to_string_lossy(),
                "dbt_exit_code": test_out.code,
            }),
        );
    } else if is_json_output(&args.output) {
        let (summary_ok, fails, errs) = summarize_run_results(&parsed);
        let doc = serde_json::json!({
            "pipeline": args.pipeline,
            "dbt_ok": test_out.status_ok,
            "dbt_code": test_out.code,
            "run_results": rr_path_abs.to_string_lossy(),
            "summary_ok": summary_ok && test_out.status_ok,
            "failure_count": fails,
            "error_count": errs,
            "results": parsed,
            "stdout": test_out.stdout,
            "stderr": test_out.stderr,
        });
        print_json(&doc);
    } else {
        if !test_out.stdout.trim().is_empty() {
            print!("{}", test_out.stdout);
        }
        if !test_out.stderr.trim().is_empty() {
            eprint!("{}", test_out.stderr);
        }
        eprintln!(
            "[skippr] run_results: {} (dbt exit {})",
            rr_path_abs.display(),
            test_out.code
        );
    }

    let (summary_ok, fails, errs) = summarize_run_results(&parsed);
    let exit_bad = !test_out.status_ok || !summary_ok || fails > 0 || errs > 0;
    if exit_bad {
        return Err("one or more dbt tests failed or dbt exited with an error".to_string());
    }
    Ok(())
}

/// Used by unit tests: build the list JSON shape from a manifest file on disk.
#[cfg(test)]
pub fn tests_document_from_manifest_file(
    pipeline: &str,
    tenant: &str,
    project_root: &Path,
    manifest_path: &Path,
) -> Result<serde_json::Value, String> {
    let tests = tests_from_manifest(manifest_path)?;
    Ok(serde_json::json!({
        "pipeline": pipeline,
        "tenant": tenant,
        "project_root": project_root.to_string_lossy(),
        "tests": tests,
    }))
}

/// Stable ordering helper for tests (exported for Rust tests).
#[cfg(test)]
pub fn sort_tests_json_tests_array(doc: &mut serde_json::Value) {
    let Some(tests) = doc.get_mut("tests").and_then(|t| t.as_array_mut()) else {
        return;
    };
    tests.sort_by(|a, b| {
        let au = a.get("unique_id").and_then(|x| x.as_str()).unwrap_or("");
        let bu = b.get("unique_id").and_then(|x| x.as_str()).unwrap_or("");
        au.cmp(bu)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_json_shape_from_minimal_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("manifest.json");
        let raw = r#"{
          "nodes": {
            "test.pkg.a": {
              "resource_type": "test",
              "unique_id": "test.pkg.a",
              "name": "a",
              "path": "schema.yml",
              "original_file_path": "models/schema.yml",
              "column_name": "id",
              "depends_on": { "nodes": ["model.pkg.m"] }
            },
            "model.pkg.m": { "resource_type": "model", "name": "m" }
          }
        }"#;
        std::fs::write(&manifest, raw).unwrap();
        let mut doc =
            tests_document_from_manifest_file("pipe1", "t1", dir.path(), &manifest).unwrap();
        sort_tests_json_tests_array(&mut doc);
        let tests = doc["tests"].as_array().unwrap();
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0]["unique_id"], "test.pkg.a");
        assert_eq!(tests[0]["column"], "id");
    }
}
