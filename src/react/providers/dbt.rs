use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::adapters::storage::StorageAdapter;
use crate::react::providers::{Keyspace, RequestScope};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbtValidateArgs {
    pub project_name: String,
    pub profiles_dir: Option<String>,
    pub target: String,
    pub run: bool,
    pub build: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbtValidateResult {
    pub ok: bool,
    pub deps_ok: bool,
    pub parse_ok: bool,
    pub compile_ok: bool,
    pub run_ok: Option<bool>,
    pub uploaded_target_files: usize,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub logs: serde_json::Value,
}

#[async_trait]
pub trait DbtProvider: Send + Sync {
    async fn ensure_minimal_project(&self, scope: &RequestScope, pipeline: &str) -> Result<(), String>;

    async fn scaffold_full_project(
        &self,
        scope: &RequestScope,
        pipeline: &str,
        namespaces: &[String],
    ) -> Result<Vec<String>, String>;

    async fn write_model_sql(
        &self,
        scope: &RequestScope,
        pipeline: &str,
        namespace: &str,
        name: &str,
        sql: &str,
    ) -> Result<String, String>;

    async fn write_metricflow_yaml(
        &self,
        scope: &RequestScope,
        pipeline: &str,
        namespace: &str,
        name: &str,
        yaml_text: &str,
    ) -> Result<String, String>;

    async fn validate_project(
        &self,
        scope: &RequestScope,
        pipeline: &str,
        args: &DbtValidateArgs,
    ) -> Result<DbtValidateResult, String>;
}

#[derive(Clone)]
pub struct SkipprDbtProvider {
    pub storage: Arc<dyn StorageAdapter>,
    pub keyspace: Arc<dyn Keyspace>,
}

impl SkipprDbtProvider {
    pub fn new(storage: Arc<dyn StorageAdapter>, keyspace: Arc<dyn Keyspace>) -> Self {
        Self { storage, keyspace }
    }
}

fn make_sources_yaml(pipeline: &str, namespaces: &[String]) -> String {
    let mut out = String::new();
    out.push_str("version: 2\n\n");
    out.push_str("sources:\n");
    out.push_str(&format!("  - name: {}\n", pipeline));
    out.push_str("    tables:\n");
    for ns in namespaces {
        out.push_str(&format!("      - name: {}\n", ns));
    }
    out
}

fn make_staging_model_sql(pipeline: &str, namespace: &str) -> String {
    format!(
        r#"{{{{ config(materialized="view") }}}}

select *
from {{{{ source('{pipeline}','{namespace}') }}}}
"#,
        pipeline = pipeline,
        namespace = namespace
    )
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())
}

#[derive(Clone, Debug)]
struct CmdOut {
    status_ok: bool,
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_cmd(cmd: &str, args: &[&str], cwd: &Path, envs: &[(&str, String)]) -> CmdOut {
    let mut c = std::process::Command::new(cmd);
    c.args(args).current_dir(cwd);
    for (k, v) in envs.iter() {
        c.env(k, v);
    }
    match c.output() {
        Ok(o) => {
            let code = o.status.code().unwrap_or(-1);
            let ok = o.status.success();
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            CmdOut { status_ok: ok, code, stdout, stderr }
        }
        Err(e) => CmdOut { status_ok: false, code: -1, stdout: String::new(), stderr: format!("spawn error: {}", e) },
    }
}

fn combine_errors(a: &CmdOut, b: &CmdOut) -> Vec<String> {
    let mut v = Vec::new();
    if !a.status_ok {
        if !a.stderr.trim().is_empty() { v.push(a.stderr.trim().to_string()); }
        if !a.stdout.trim().is_empty() { v.push(a.stdout.trim().to_string()); }
    }
    if !b.status_ok {
        if !b.stderr.trim().is_empty() { v.push(b.stderr.trim().to_string()); }
        if !b.stdout.trim().is_empty() { v.push(b.stdout.trim().to_string()); }
    }
    v
}

impl SkipprDbtProvider {
    async fn upload_dir_to_storage(&self, local_dir: &Path, prefix: &str) -> Result<usize, String> {
        if !local_dir.is_dir() {
            return Ok(0);
        }
        let mut stack: Vec<PathBuf> = vec![local_dir.to_path_buf()];
        let mut uploaded: usize = 0;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path.strip_prefix(local_dir).map_err(|e| e.to_string())?;
                let mut key = String::from(prefix);
                let rel_s = rel.to_string_lossy().replace('\\', "/");
                key.push_str(&rel_s);
                let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                let content_type = match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
                    "sql" => "text/sql",
                    "json" => "application/json",
                    "yml" | "yaml" => "text/yaml",
                    "txt" => "text/plain",
                    _ => "application/octet-stream",
                };
                self.storage.put_bytes(&key, &bytes, content_type).await?;
                uploaded += 1;
            }
        }
        Ok(uploaded)
    }
}

#[async_trait]
impl DbtProvider for SkipprDbtProvider {
    async fn ensure_minimal_project(&self, scope: &RequestScope, pipeline: &str) -> Result<(), String> {
        let project_key = self.keyspace.dbt_project_key(scope, pipeline);
        if self.storage.head_etag(&project_key).await?.is_some() {
            return Ok(());
        }
        let name = format!("{}_project", pipeline.replace('/', "_"));
        let y = format!(
            "name: {name}\nversion: '1.0'\nprofile: '{pipeline}'\nmodel-paths: ['models']\nseed-paths: ['seeds']\nmacro-paths: ['macros']\ntarget-path: 'target'\n",
            name = name,
            pipeline = pipeline
        );
        self.storage.put_bytes(&project_key, y.as_bytes(), "text/yaml").await?;
        Ok(())
    }

    async fn scaffold_full_project(
        &self,
        scope: &RequestScope,
        pipeline: &str,
        namespaces: &[String],
    ) -> Result<Vec<String>, String> {
        self.ensure_minimal_project(scope, pipeline).await?;
        let mut written: Vec<String> = Vec::new();

        let sources_yaml = make_sources_yaml(pipeline, namespaces);
        let sources_key = format!("{}models/schema.yml", self.keyspace.dbt_prefix(scope, pipeline));
        self.storage.put_bytes(&sources_key, sources_yaml.as_bytes(), "text/yaml").await?;
        written.push(sources_key);

        for ns in namespaces {
            let model_name = format!("stg_{}", ns);
            let sql_text = make_staging_model_sql(pipeline, ns);
            let key = format!("{}models/{}/{}.sql", self.keyspace.dbt_prefix(scope, pipeline), ns, model_name);
            self.storage.put_bytes(&key, sql_text.as_bytes(), "text/sql").await?;
            written.push(key);
        }
        Ok(written)
    }

    async fn write_model_sql(
        &self,
        scope: &RequestScope,
        pipeline: &str,
        namespace: &str,
        name: &str,
        sql: &str,
    ) -> Result<String, String> {
        let key = format!("{}models/{}/{}.sql", self.keyspace.dbt_prefix(scope, pipeline), namespace, name);
        self.storage.put_bytes(&key, sql.as_bytes(), "text/sql").await?;
        Ok(key)
    }

    async fn write_metricflow_yaml(
        &self,
        scope: &RequestScope,
        pipeline: &str,
        namespace: &str,
        name: &str,
        yaml_text: &str,
    ) -> Result<String, String> {
        let key = format!("{}metrics/{}/{}.yaml", self.keyspace.dbt_prefix(scope, pipeline), namespace, name);
        self.storage.put_bytes(&key, yaml_text.as_bytes(), "text/yaml").await?;
        Ok(key)
    }

    async fn validate_project(
        &self,
        scope: &RequestScope,
        pipeline: &str,
        args: &DbtValidateArgs,
    ) -> Result<DbtValidateResult, String> {
        let project_name = if args.project_name.is_empty() { "skippr_model".to_string() } else { args.project_name.clone() };
        let s3_prefix_base = {
            let pref = self.keyspace.dbt_prefix(scope, pipeline);
            if pref.ends_with('/') { pref } else { format!("{}/", pref) }
        };

        let profiles_dir = args.profiles_dir.clone().or_else(|| std::env::var("DBT_PROFILES_DIR").ok());
        let target = if args.target.is_empty() {
            std::env::var("DBT_TARGET").ok().unwrap_or_else(|| "datafusion".to_string())
        } else {
            args.target.clone()
        };

        let run = args.run;
        let build = args.build;

        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let root = tmp.path().join(project_name.clone());
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;

        // Populate temp project from storage prefix
        let keys = self.storage.list_prefix(&s3_prefix_base).await?;
        let mut file_count = 0usize;
        for key in keys {
            if key.ends_with('/') { continue; }
            let rel = key.strip_prefix(&s3_prefix_base).unwrap_or(&key);
            let dest = root.join(rel);
            if let Some(parent) = dest.parent() { let _ = std::fs::create_dir_all(parent); }
            let bytes = self.storage.get_bytes(&key).await?;
            write_file(&dest, &bytes)?;
            file_count += 1;
        }

        // Ensure minimal project file if missing
        let proj = root.join("dbt_project.yml");
        if !proj.exists() {
            let y = format!("name: {}\nversion: '1.0'\nprofile: '{}'\nmodel-paths: ['models']\ntarget-path: 'target'\n", project_name, project_name);
            write_file(&proj, y.as_bytes())?;
        }

        let mut envs: Vec<(&str, String)> = Vec::new();
        if let Some(pd) = profiles_dir.as_ref() { envs.push(("DBT_PROFILES_DIR", pd.clone())); }

        let deps_res = run_cmd("dbt", &["deps"], &root, &envs);
        let parse_res = run_cmd("dbt", &["parse"], &root, &envs);
        let compile_res = run_cmd("dbt", &["compile", "--target", &target], &root, &envs);
        let run_or_build_res = if build {
            Some(run_cmd("dbt", &["build", "--target", &target], &root, &envs))
        } else if run {
            Some(run_cmd("dbt", &["run", "--target", &target], &root, &envs))
        } else {
            None
        };

        let ok = deps_res.status_ok
            && parse_res.status_ok
            && compile_res.status_ok
            && run_or_build_res.as_ref().map(|o| o.status_ok).unwrap_or(true);

        let mut uploaded_files: usize = 0;
        if compile_res.status_ok || run_or_build_res.as_ref().map(|r| r.status_ok).unwrap_or(false) {
            let local_target = root.join("target");
            if local_target.exists() {
                let target_prefix = format!("{}target/", s3_prefix_base);
                uploaded_files = self.upload_dir_to_storage(&local_target, &target_prefix).await.unwrap_or(0);
            }
        }

        let mut errs_vec: Vec<String> = Vec::new();
        for e in combine_errors(&deps_res, &parse_res) { errs_vec.push(e); }
        let empty = CmdOut { status_ok: true, code: 0, stdout: String::new(), stderr: String::new() };
        for e in combine_errors(&compile_res, run_or_build_res.as_ref().unwrap_or(&empty)) { errs_vec.push(e); }

        Ok(DbtValidateResult {
            ok,
            deps_ok: deps_res.status_ok,
            parse_ok: parse_res.status_ok,
            compile_ok: compile_res.status_ok,
            run_ok: run_or_build_res.as_ref().map(|o| o.status_ok),
            uploaded_target_files: uploaded_files,
            errors: errs_vec,
            warnings: vec![],
            logs: serde_json::json!({
                "deps": { "code": deps_res.code, "stdout": deps_res.stdout, "stderr": deps_res.stderr },
                "parse": { "code": parse_res.code, "stdout": parse_res.stdout, "stderr": parse_res.stderr },
                "compile": { "code": compile_res.code, "stdout": compile_res.stdout, "stderr": compile_res.stderr },
                "run_or_build": run_or_build_res.as_ref().map(|r| serde_json::json!({ "code": r.code, "stdout": r.stdout, "stderr": r.stderr })),
                "fetched_files": file_count,
            }),
        })
    }
}

