//! `skippr dbt compile-sql` / `skippr dbt run` — compile SQL or run models for a pipeline.

use std::path::{Path, PathBuf};

use react_module_provider_dbt::DbtProjectProvider;
use react_suite_data_engineer::PipelineName;
use serde::Serialize;

use crate::run_results_parse::{parse_run_results_json, ParsedDbtRunResult};
use crate::test_cmd::{self, OpenDbtProject};
use crate::{config_path, project_root_from_config_path};

#[derive(Debug, Clone, clap::Subcommand)]
pub enum DbtSubcommand {
    /// Compile a dbt SQL resource and return compiled SQL (plus model/test metadata).
    CompileSql(CompileSqlArgs),
    /// Run `dbt run` for a pipeline (materialize from cloud storage or use `--project-dir`).
    Run(DbtRunArgs),
}

#[derive(Debug, Clone, clap::Args)]
pub struct DbtRunArgs {
    #[arg(long)]
    pub pipeline: PipelineName,
    /// dbt `--select` expression (repeatable).
    #[arg(long = "select")]
    pub select: Vec<String>,
    /// dbt profile target (defaults to generated profiles target).
    #[arg(long)]
    pub target: Option<String>,
    /// Use a local dbt project directory instead of materializing from cloud storage.
    #[arg(long)]
    pub project_dir: Option<PathBuf>,
    /// Output: jsonl, json, or text.
    #[arg(long, default_value = "jsonl")]
    pub output: String,
}

#[derive(Debug, Clone, clap::Args)]
pub struct CompileSqlArgs {
    #[arg(long)]
    pub pipeline: PipelineName,
    /// Absolute or workspace-relative path to a dbt `.sql` resource (under `models/`, `snapshots/`, or `analyses/`).
    #[arg(long)]
    pub file: PathBuf,
    /// Only run `dbt parse` (no `dbt compile`). Faster probe for test metadata.
    #[arg(long, default_value_t = false)]
    pub parse_only: bool,
    /// Output: json or text.
    #[arg(long, default_value = "json")]
    pub output: String,
}

#[derive(Debug, Serialize)]
struct CompileSqlResponse<'a> {
    ok: bool,
    pipeline: &'a str,
    rel_path: String,
    project_dir: String,
    model_name: Option<String>,
    model_unique_id: Option<String>,
    test_select: Option<String>,
    has_tests: bool,
    compiled_sql: Option<String>,
    compiled_path: Option<String>,
    dbt_select: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compile: Option<serde_json::Value>,
}

fn is_json_output(output: &str) -> bool {
    output.trim().eq_ignore_ascii_case("json")
}

fn is_jsonl_output(output: &str) -> bool {
    output.trim().eq_ignore_ascii_case("jsonl")
}

fn emit_jsonl(output: &str, line: &serde_json::Value) {
    if is_jsonl_output(output) {
        println!("{line}");
    }
}

fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

fn is_dbt_sql_resource_rel(rel: &str) -> bool {
    rel.starts_with("models/") || rel.starts_with("snapshots/") || rel.starts_with("analyses/")
}

/// Walk ancestors for `dbt_project.yml` and return `(project_root, rel_path)` when `file` is a dbt SQL resource.
pub fn find_dbt_project_and_rel(file: &Path) -> Result<(PathBuf, String), String> {
    let file = file
        .canonicalize()
        .map_err(|e| format!("file not found: {e}"))?;
    let mut dir = file
        .parent()
        .ok_or_else(|| "file has no parent directory".to_string())?;
    loop {
        if dir.join("dbt_project.yml").is_file() {
            let rel = file
                .strip_prefix(dir)
                .map_err(|_| "file is outside dbt project root".to_string())?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if !is_dbt_sql_resource_rel(&rel_str) {
                return Err(format!(
                    "not a dbt SQL resource (expected models/, snapshots/, or analyses/): {rel_str}"
                ));
            }
            return Ok((dir.to_path_buf(), rel_str));
        }
        dir = dir
            .parent()
            .ok_or_else(|| "no dbt_project.yml found in parent directories".to_string())?;
    }
}

fn read_dbt_project_name(project_dir: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(project_dir.join("dbt_project.yml"))
        .map_err(|e| format!("read dbt_project.yml: {e}"))?;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("name:") {
            let name = trimmed
                .strip_prefix("name:")
                .map(|s| s.trim().trim_matches('"').trim_matches('\''))
                .unwrap_or("")
                .to_string();
            if !name.is_empty() {
                return Ok(name);
            }
        }
    }
    Err("dbt_project.yml: missing name:".to_string())
}

type ModelManifestInfo = (Option<String>, Option<String>, Option<String>, bool);

#[allow(clippy::type_complexity)]
fn model_and_tests_from_manifest(
    manifest_path: &Path,
    rel_path: &str,
) -> Result<ModelManifestInfo, String> {
    let raw = std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let nodes = v
        .get("nodes")
        .and_then(|n| n.as_object())
        .ok_or_else(|| "manifest.json: missing nodes object".to_string())?;

    let mut model_unique_id: Option<String> = None;
    let mut model_name: Option<String> = None;
    for (_k, node) in nodes {
        if node.get("resource_type").and_then(|x| x.as_str()) != Some("model") {
            continue;
        }
        let original = node
            .get("original_file_path")
            .and_then(|x| x.as_str())
            .or_else(|| node.get("path").and_then(|x| x.as_str()))
            .unwrap_or("");
        if original.replace('\\', "/") != rel_path {
            continue;
        }
        model_unique_id = node
            .get("unique_id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        model_name = node
            .get("name")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        break;
    }

    let Some(model_uid) = model_unique_id.clone() else {
        return Ok((model_name, model_unique_id, None, false));
    };

    let mut test_count = 0usize;
    for (_k, node) in nodes {
        if node.get("resource_type").and_then(|x| x.as_str()) != Some("test") {
            continue;
        }
        let depends = node
            .get("depends_on")
            .and_then(|d| d.get("nodes"))
            .and_then(|n| n.as_array());
        let Some(deps) = depends else {
            continue;
        };
        if deps
            .iter()
            .filter_map(|x| x.as_str())
            .any(|id| id == model_uid)
        {
            test_count += 1;
        }
    }

    let test_select = model_name.clone();
    Ok((model_name, model_unique_id, test_select, test_count > 0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedDbtLayout {
    Canonical,
}

impl ManagedDbtLayout {
    fn label(self) -> &'static str {
        match self {
            ManagedDbtLayout::Canonical => "<project>/<pipeline>/dbt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedDbtProject {
    pipeline: String,
    layout: ManagedDbtLayout,
}

fn managed_dbt_project_from_root(
    project_root: &Path,
    dbt_root: &Path,
) -> Option<ManagedDbtProject> {
    if dbt_root.file_name().and_then(|s| s.to_str()) != Some("dbt") {
        return None;
    }
    let pipeline_dir = dbt_root.parent()?;
    if pipeline_dir.parent() == Some(project_root) {
        let pipeline = pipeline_dir.file_name()?.to_str()?.trim();
        if !pipeline.is_empty() {
            return Some(ManagedDbtProject {
                pipeline: pipeline.to_string(),
                layout: ManagedDbtLayout::Canonical,
            });
        }
    }

    None
}

fn non_canonical_managed_dbt_root(
    project_root: &Path,
    dbt_root: &Path,
) -> Option<(String, PathBuf)> {
    let non_canonical_parent = dbt_root.parent()?;
    if non_canonical_parent.file_name().and_then(|s| s.to_str()) == Some("dbt")
        && non_canonical_parent.parent() == Some(project_root)
    {
        let pipeline = dbt_root.file_name()?.to_str()?.trim();
        if !pipeline.is_empty() {
            return Some((
                pipeline.to_string(),
                project_root.join(pipeline).join("dbt"),
            ));
        }
    }
    None
}

fn validate_managed_dbt_project_pipeline(
    explicit_config: &Option<PathBuf>,
    requested_pipeline: &str,
    dbt_root: &Path,
) -> Result<(), String> {
    let cfg_path = config_path(explicit_config);
    let project_root = project_root_from_config_path(&cfg_path);
    let project_root = project_root.canonicalize().unwrap_or(project_root);
    if let Some((_pipeline, canonical_root)) =
        non_canonical_managed_dbt_root(&project_root, dbt_root)
    {
        return Err(format!(
            "dbt project path must use managed layout {}; expected {}",
            ManagedDbtLayout::Canonical.label(),
            canonical_root.display()
        ));
    }
    let Some(managed) = managed_dbt_project_from_root(&project_root, dbt_root) else {
        return Ok(());
    };
    if managed.pipeline == requested_pipeline.trim() {
        return Ok(());
    }
    Err(format!(
        "dbt project path implies pipeline '{}' via managed layout {}, but --pipeline was '{}'",
        managed.pipeline,
        managed.layout.label(),
        requested_pipeline.trim()
    ))
}

fn resolve_project_dir(
    explicit_config: &Option<PathBuf>,
    pipeline: &str,
    file: &Path,
    dbt_root: &Path,
) -> PathBuf {
    let cfg_path = config_path(explicit_config);
    let project_root = project_root_from_config_path(&cfg_path);
    let local_direct = project_root.join(pipeline.trim()).join("dbt");
    if file.starts_with(&local_direct) && local_direct.join("dbt_project.yml").is_file() {
        return local_direct;
    }
    if dbt_root.join("dbt_project.yml").is_file() {
        return dbt_root.to_path_buf();
    }
    local_direct
}

fn read_compiled_sql(
    project_dir: &Path,
    project_name: &str,
    rel_path: &str,
) -> Result<(String, PathBuf), String> {
    let compiled_path = project_dir
        .join("target")
        .join("compiled")
        .join(project_name)
        .join(rel_path);
    if !compiled_path.is_file() {
        return Err(format!(
            "compiled SQL not found at {}",
            compiled_path.display()
        ));
    }
    let sql = std::fs::read_to_string(&compiled_path).map_err(|e| e.to_string())?;
    Ok((sql, compiled_path))
}

fn run_dbt_compile(
    dbt: &DbtProjectProvider,
    project_dir: &Path,
    profiles_dir: &Path,
    target: &str,
    tier_env: &[(&str, String)],
    dbt_select: &str,
) -> Result<serde_json::Value, String> {
    let profiles_path = Some(profiles_dir);
    let mut base_env: Vec<(&str, String)> = vec![(
        "DBT_PROFILES_DIR",
        profiles_dir.to_string_lossy().to_string(),
    )];
    base_env.extend(tier_env.iter().cloned());

    let compile = dbt.invoke_dbt_cli(
        project_dir,
        profiles_path,
        &["compile", "--target", target, "--select", dbt_select],
        &base_env,
        "compile",
    );
    let body = serde_json::json!({
        "code": compile.code,
        "status_ok": compile.status_ok,
        "stdout": compile.stdout,
        "stderr": compile.stderr,
    });
    if !compile.status_ok {
        return Err(format!(
            "dbt compile failed (code {})\nstdout:\n{}\nstderr:\n{}",
            compile.code, compile.stdout, compile.stderr
        ));
    }
    Ok(body)
}

pub async fn cmd_dbt_compile_sql(
    _log: Option<String>,
    explicit_config: &Option<PathBuf>,
    args: CompileSqlArgs,
) -> Result<(), String> {
    let file = if args.file.is_absolute() {
        args.file.clone()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(&args.file)
    };

    let (dbt_root, rel_path) = find_dbt_project_and_rel(&file)?;
    validate_managed_dbt_project_pipeline(explicit_config, &args.pipeline, &dbt_root)?;
    let dbt_select = format!("path:{rel_path}");

    let session =
        test_cmd::open_materialized_dbt_project(explicit_config, &args.pipeline, true).await?;
    let mut project_dir = resolve_project_dir(explicit_config, &args.pipeline, &file, &dbt_root);

    let use_local = project_dir.join("dbt_project.yml").is_file() && file.starts_with(&project_dir);
    if !use_local {
        project_dir = session.project_dir.clone();
    }

    let profiles_dir = session.profiles_dir.clone();
    let target = session.target.as_str();
    let tier_env = session.tier_env.as_slice();

    if use_local {
        test_cmd::run_dbt_deps_parse_on_project(
            &session.dbt,
            &project_dir,
            &profiles_dir,
            target,
            tier_env,
        )?;
    }

    let manifest_path = project_dir.join("target").join("manifest.json");
    let (model_name, model_unique_id, test_select, has_tests) =
        model_and_tests_from_manifest(&manifest_path, &rel_path)
            .unwrap_or((None, None, None, false));

    let mut compile_meta: Option<serde_json::Value> = None;
    let mut compiled_sql: Option<String> = None;
    let mut compiled_path: Option<String> = None;
    let mut error: Option<String> = None;

    if !args.parse_only {
        match run_dbt_compile(
            &session.dbt,
            &project_dir,
            &profiles_dir,
            target,
            tier_env,
            &dbt_select,
        ) {
            Ok(meta) => compile_meta = Some(meta),
            Err(e) => error = Some(e),
        }

        if error.is_none() {
            match read_compiled_sql(
                &project_dir,
                &read_dbt_project_name(&project_dir)?,
                &rel_path,
            ) {
                Ok((sql, path)) => {
                    compiled_sql = Some(sql);
                    compiled_path = Some(path.to_string_lossy().to_string());
                }
                Err(e) => error = Some(e),
            }
        }
    }

    let ok = error.is_none();
    let err_message = error.clone();
    let resp = CompileSqlResponse {
        ok,
        pipeline: &args.pipeline,
        rel_path,
        project_dir: project_dir.to_string_lossy().to_string(),
        model_name,
        model_unique_id,
        test_select,
        has_tests,
        compiled_sql: compiled_sql.clone(),
        compiled_path: compiled_path.clone(),
        dbt_select,
        error,
        compile: compile_meta,
    };
    let value = serde_json::to_value(&resp).map_err(|e| e.to_string())?;
    if is_json_output(&args.output) {
        print_json(&value);
    } else if !ok {
        return Err(err_message.unwrap_or_else(|| "dbt compile-sql failed".to_string()));
    } else {
        println!(
            "compiled {} (tests: {})",
            compiled_path.as_deref().unwrap_or("?"),
            has_tests
        );
        if let Some(sql) = &compiled_sql {
            println!("{sql}");
        }
    }
    if !ok {
        return Err(err_message.unwrap_or_else(|| "dbt compile-sql failed".to_string()));
    }
    Ok(())
}

fn map_dbt_run_status(s: &str) -> &'static str {
    match s {
        "success" => "success",
        "pass" => "success",
        "fail" | "failed" | "error" | "runtime error" => "error",
        "skipped" | "skip" => "skipped",
        _ => "error",
    }
}

fn summarize_model_run(rows: &[ParsedDbtRunResult]) -> (bool, usize, usize) {
    let mut err = 0usize;
    let mut skip = 0usize;
    for r in rows {
        match map_dbt_run_status(r.status.as_str()) {
            "error" => err += 1,
            "skipped" => skip += 1,
            _ => {}
        }
    }
    let ok = err == 0;
    (ok, err, skip)
}

/// Open a dbt project for `skippr dbt run`: local `--project-dir` or cloud materialization.
pub async fn open_dbt_project_for_run(
    explicit_config: &Option<PathBuf>,
    pipeline: &str,
    project_dir_override: Option<&Path>,
    clear_target: bool,
) -> Result<OpenDbtProject, String> {
    if let Some(dir) = project_dir_override {
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("project-dir not found: {e}"))?;
        if !dir.join("dbt_project.yml").is_file() {
            return Err(format!(
                "project-dir missing dbt_project.yml: {}",
                dir.display()
            ));
        }
        let mut session =
            test_cmd::open_materialized_dbt_project(explicit_config, pipeline, false).await?;
        if clear_target {
            let _ = std::fs::remove_dir_all(dir.join("target"));
        }
        test_cmd::run_dbt_deps_parse_on_project(
            &session.dbt,
            &dir,
            &session.profiles_dir,
            session.target.as_str(),
            &session.tier_env,
        )?;
        session.project_dir = dir;
        return Ok(session);
    }
    test_cmd::open_materialized_dbt_project(explicit_config, pipeline, clear_target).await
}

pub async fn cmd_dbt_run(
    _log: Option<String>,
    explicit_config: &Option<PathBuf>,
    args: DbtRunArgs,
) -> Result<(), String> {
    let project_override = args.project_dir.as_deref();
    let session =
        open_dbt_project_for_run(explicit_config, &args.pipeline, project_override, true).await?;
    let project_dir = &session.project_dir;
    let target = args
        .target
        .as_deref()
        .unwrap_or(session.target.as_str())
        .to_string();
    let tier_env = session.tier_env.as_slice();
    let profiles_pd = session.profiles_dir.as_path();
    let mut base_env: Vec<(&str, String)> = vec![(
        "DBT_PROFILES_DIR",
        profiles_pd.to_string_lossy().to_string(),
    )];
    base_env.extend(tier_env.iter().cloned());

    let mut argv: Vec<String> = vec!["run".into(), "--target".into(), target.clone()];
    for s in &args.select {
        if !s.trim().is_empty() {
            argv.push("--select".into());
            argv.push(s.trim().to_string());
        }
    }
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let run_out =
        session
            .dbt
            .invoke_dbt_cli(project_dir, Some(profiles_pd), &argv_refs, &base_env, "run");

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
        if !run_out.status_ok {
            emit_jsonl(
                &args.output,
                &serde_json::json!({
                    "event": "run_error",
                    "message": format!("dbt run exited with code {}", run_out.code),
                    "stdout": run_out.stdout,
                    "stderr": run_out.stderr,
                }),
            );
        }
        for row in &parsed {
            emit_jsonl(
                &args.output,
                &serde_json::json!({
                    "event": "model_result",
                    "unique_id": row.unique_id,
                    "status": map_dbt_run_status(row.status.as_str()),
                    "message": row.message,
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
                "dbt_exit_code": run_out.code,
            }),
        );
    } else if is_json_output(&args.output) {
        let (summary_ok, errs, skipped) = summarize_model_run(&parsed);
        let doc = serde_json::json!({
            "pipeline": args.pipeline,
            "dbt_ok": run_out.status_ok,
            "dbt_code": run_out.code,
            "run_results": rr_path_abs.to_string_lossy(),
            "summary_ok": summary_ok && run_out.status_ok,
            "error_count": errs,
            "skipped_count": skipped,
            "results": parsed,
            "stdout": run_out.stdout,
            "stderr": run_out.stderr,
        });
        print_json(&doc);
    } else {
        if !run_out.stdout.trim().is_empty() {
            print!("{}", run_out.stdout);
        }
        if !run_out.stderr.trim().is_empty() {
            eprint!("{}", run_out.stderr);
        }
        eprintln!(
            "[skippr] run_results: {} (dbt exit {})",
            rr_path_abs.display(),
            run_out.code
        );
    }

    let (summary_ok, errs, _skipped) = summarize_model_run(&parsed);
    let exit_bad = !run_out.status_ok || !summary_ok || errs > 0;
    if exit_bad {
        return Err("one or more dbt models failed or dbt exited with an error".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbt_resource_rel_requires_models_prefix() {
        assert!(is_dbt_sql_resource_rel("models/staging/orders.sql"));
        assert!(is_dbt_sql_resource_rel("snapshots/orders_snapshot.sql"));
        assert!(!is_dbt_sql_resource_rel("macros/foo.sql"));
    }

    #[test]
    fn managed_dbt_project_detects_canonical_pipeline() {
        let project_root = PathBuf::from("/tmp/skippr_project");
        let dbt_root = project_root.join("bike_hire").join("dbt");

        let managed = managed_dbt_project_from_root(&project_root, &dbt_root).unwrap();

        assert_eq!(managed.pipeline, "bike_hire");
        assert_eq!(managed.layout, ManagedDbtLayout::Canonical);
    }

    #[test]
    fn managed_dbt_project_rejects_canonical_pipeline_mismatch() {
        let project_root = PathBuf::from("/tmp/skippr_project");
        let config = Some(project_root.join("skippr.yml"));
        let dbt_root = project_root.join("bike_hire").join("dbt");

        let err = validate_managed_dbt_project_pipeline(&config, "bank", &dbt_root).unwrap_err();

        assert!(err.contains("implies pipeline 'bike_hire'"));
        assert!(err.contains("--pipeline was 'bank'"));
    }

    #[test]
    fn managed_dbt_project_rejects_non_canonical_layout() {
        let project_root = PathBuf::from("/tmp/skippr_project");
        let config = Some(project_root.join("skippr.yml"));
        let dbt_root = project_root.join("dbt").join("bike_hire");

        let err = validate_managed_dbt_project_pipeline(&config, "bank", &dbt_root).unwrap_err();

        assert!(err.contains("must use managed layout <project>/<pipeline>/dbt"));
        assert!(err.contains("/tmp/skippr_project/bike_hire/dbt"));
    }

    #[test]
    fn managed_dbt_project_allows_non_managed_root() {
        let project_root = PathBuf::from("/tmp/skippr_project");
        let config = Some(project_root.join("skippr.yml"));
        let dbt_root = PathBuf::from("/tmp/external_dbt_project");

        validate_managed_dbt_project_pipeline(&config, "bank", &dbt_root).unwrap();
    }
}
