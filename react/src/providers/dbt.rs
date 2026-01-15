use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::adapters::storage::StorageAdapter;
use crate::providers::{Keyspace, RequestScope};

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
    async fn ensure_minimal_project(&self, scope: &RequestScope) -> Result<(), String>;

    async fn scaffold_full_project(
        &self,
        scope: &RequestScope,
        dataset_ids: &[String],
    ) -> Result<Vec<String>, String>;

    async fn write_model_sql(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        name: &str,
        sql: &str,
    ) -> Result<String, String>;

    async fn write_metricflow_yaml(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        name: &str,
        yaml_text: &str,
    ) -> Result<String, String>;

    async fn validate_project(
        &self,
        scope: &RequestScope,
        args: &DbtValidateArgs,
    ) -> Result<DbtValidateResult, String>;
}

#[derive(Clone)]
pub struct DbtProjectProvider {
    pub storage: Arc<dyn StorageAdapter>,
    pub keyspace: Arc<dyn Keyspace>,
    /// How DBT commands should be executed (host vs docker).
    pub runner: DbtRunnerConfig,
}

impl DbtProjectProvider {
    pub fn new(storage: Arc<dyn StorageAdapter>, keyspace: Arc<dyn Keyspace>, runner: DbtRunnerConfig) -> Self {
        Self { storage, keyspace, runner }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DbtRunnerConfig {
    /// Runner mode: "host" (default) or "docker".
    pub mode: String,
    /// Docker image to use when mode=="docker" (pinned strongly recommended).
    pub docker_image: Option<String>,
    /// Optional docker platform (e.g. "linux/amd64").
    pub docker_platform: Option<String>,
    /// Optional docker network (e.g. "host" or a named network).
    pub docker_network: Option<String>,
    /// If true, mount ~/.aws into the container at /root/.aws (useful for AWS_PROFILE flows).
    pub docker_mount_aws_dir: bool,
}

fn parse_dataset_id(dataset_id: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = dataset_id.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string(), parts[2].to_string()))
}

fn encode_key_component(s: &str) -> String {
    // Same encoding strategy as Keyspace: keep a conservative safe set, percent-encode the rest.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        let safe = c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'; // allow dots
        if safe {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn make_sources_yaml(dataset_ids: &[String]) -> String {
    let mut out = String::new();
    out.push_str("version: 2\n\n");
    out.push_str("sources:\n");
    let mut by_db: std::collections::BTreeMap<String, Vec<(String, String)>> = std::collections::BTreeMap::new();
    for ds in dataset_ids {
        if let Some((_cat, db, table)) = parse_dataset_id(ds) {
            by_db.entry(db).or_default().push((table, ds.clone()));
        }
    }
    for (db, tables) in by_db {
        out.push_str(&format!("  - name: {}\n", db));
        out.push_str(&format!("    schema: {}\n", db));
        out.push_str("    tables:\n");
        for (table, ds_id) in tables {
            out.push_str(&format!("      - name: {}\n", table));
            out.push_str(&format!("        meta:\n          dataset_id: \"{}\"\n", ds_id));
        }
    }
    out
}

fn make_staging_model_sql(dataset_id: &str) -> String {
    let (_cat, db, table) = parse_dataset_id(dataset_id).unwrap_or(("AwsDataCatalog".into(), "default".into(), dataset_id.to_string()));
    format!(
        r#"{{{{ config(materialized="view") }}}}

select *
from {db}.{table}
"#,
        db = db,
        table = table
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

fn build_docker_run_args(
    runner: &DbtRunnerConfig,
    project_dir: &Path,
    profiles_dir: Option<&Path>,
    dbt_args: &[&str],
    envs: &[(&str, String)],
) -> Result<Vec<String>, String> {
    let image = runner
        .docker_image
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "dbt runner is docker but providers.dbt.docker_image is not set".to_string())?;

    let proj = project_dir.canonicalize().unwrap_or_else(|_| project_dir.to_path_buf());
    let proj_s = proj.to_string_lossy().to_string();

    let mut args: Vec<String> = Vec::new();
    args.push("run".to_string());
    args.push("--rm".to_string());
    if let Some(p) = runner.docker_platform.as_ref().filter(|s| !s.trim().is_empty()) {
        args.push("--platform".to_string());
        args.push(p.trim().to_string());
    }
    if let Some(n) = runner.docker_network.as_ref().filter(|s| !s.trim().is_empty()) {
        args.push("--network".to_string());
        args.push(n.trim().to_string());
    }

    // Mount project at /project.
    args.push("-v".to_string());
    args.push(format!("{}:/project", proj_s));
    args.push("-w".to_string());
    args.push("/project".to_string());

    // Mount profiles at /profiles and force DBT_PROFILES_DIR to that path.
    if let Some(pd) = profiles_dir {
        let pd = pd.canonicalize().unwrap_or_else(|_| pd.to_path_buf());
        args.push("-v".to_string());
        args.push(format!("{}:/profiles", pd.to_string_lossy()));
        args.push("-e".to_string());
        args.push("DBT_PROFILES_DIR=/profiles".to_string());
    }

    // Pass through envs requested by caller.
    for (k, v) in envs.iter() {
        args.push("-e".to_string());
        args.push(format!("{}={}", k, v));
    }

    // Pass through common AWS env vars if present. This avoids baking secrets into files.
    for k in [
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_SDK_LOAD_CONFIG",
    ] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                args.push("-e".to_string());
                args.push(format!("{}={}", k, v));
            }
        }
    }

    // Optional: mount ~/.aws for profile-based auth chains.
    if runner.docker_mount_aws_dir {
        if let Ok(home) = std::env::var("HOME") {
            let aws_dir = Path::new(&home).join(".aws");
            if aws_dir.exists() {
                args.push("-v".to_string());
                args.push(format!("{}:/root/.aws:ro", aws_dir.to_string_lossy()));
            }
        }
    }

    // Image + command.
    args.push("--entrypoint".to_string());
    args.push("dbt".to_string());
    args.push(image);
    for a in dbt_args.iter() {
        args.push(a.to_string());
    }
    Ok(args)
}

fn run_cmd_docker(
    runner: &DbtRunnerConfig,
    project_dir: &Path,
    profiles_dir: Option<&Path>,
    dbt_args: &[&str],
    envs: &[(&str, String)],
) -> CmdOut {
    let args = match build_docker_run_args(runner, project_dir, profiles_dir, dbt_args, envs) {
        Ok(v) => v,
        Err(e) => {
            return CmdOut { status_ok: false, code: -1, stdout: String::new(), stderr: e };
        }
    };

    let mut c = std::process::Command::new("docker");
    c.args(&args);
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

impl DbtProjectProvider {
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
impl DbtProvider for DbtProjectProvider {
    async fn ensure_minimal_project(&self, scope: &RequestScope) -> Result<(), String> {
        let project_key = self.keyspace.dbt_project_key(scope);
        if self.storage.head_etag(&project_key).await?.is_some() {
            return Ok(());
        }
        let name = format!("{}_project", scope.project_id.replace('/', "_"));
        let y = format!(
            "name: {name}\nversion: '1.0'\nprofile: '{project_id}'\nmodel-paths: ['models']\nseed-paths: ['seeds']\nmacro-paths: ['macros']\ntarget-path: 'target'\n",
            name = name,
            project_id = scope.project_id
        );
        self.storage.put_bytes(&project_key, y.as_bytes(), "text/yaml").await?;
        Ok(())
    }

    async fn scaffold_full_project(
        &self,
        scope: &RequestScope,
        dataset_ids: &[String],
    ) -> Result<Vec<String>, String> {
        self.ensure_minimal_project(scope).await?;
        let mut written: Vec<String> = Vec::new();

        let sources_yaml = make_sources_yaml(dataset_ids);
        let sources_key = format!("{}models/schema.yml", self.keyspace.dbt_prefix(scope));
        self.storage.put_bytes(&sources_key, sources_yaml.as_bytes(), "text/yaml").await?;
        written.push(sources_key);

        for ds in dataset_ids {
            let dir = encode_key_component(ds);
            let model_name = format!("stg_{}", dir.replace('.', "_"));
            let sql_text = make_staging_model_sql(ds);
            let key = format!("{}models/{}/{}.sql", self.keyspace.dbt_prefix(scope), dir, model_name);
            self.storage.put_bytes(&key, sql_text.as_bytes(), "text/sql").await?;
            written.push(key);
        }
        Ok(written)
    }

    async fn write_model_sql(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        name: &str,
        sql: &str,
    ) -> Result<String, String> {
        let dir = encode_key_component(dataset_id);
        let key = format!("{}models/{}/{}.sql", self.keyspace.dbt_prefix(scope), dir, name);
        self.storage.put_bytes(&key, sql.as_bytes(), "text/sql").await?;
        Ok(key)
    }

    async fn write_metricflow_yaml(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        name: &str,
        yaml_text: &str,
    ) -> Result<String, String> {
        let dir = encode_key_component(dataset_id);
        let key = format!("{}metrics/{}/{}.yaml", self.keyspace.dbt_prefix(scope), dir, name);
        self.storage.put_bytes(&key, yaml_text.as_bytes(), "text/yaml").await?;
        Ok(key)
    }

    async fn validate_project(
        &self,
        scope: &RequestScope,
        args: &DbtValidateArgs,
    ) -> Result<DbtValidateResult, String> {
        let project_name = if args.project_name.is_empty() { "data_engineer".to_string() } else { args.project_name.clone() };
        let s3_prefix_base = {
            let pref = self.keyspace.dbt_prefix(scope);
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

        let use_docker = self.runner.mode.to_lowercase() == "docker";
        let profiles_path = profiles_dir.as_ref().map(|s| Path::new(s));
        let deps_res = if use_docker {
            run_cmd_docker(&self.runner, &root, profiles_path, &["deps"], &envs)
        } else {
            run_cmd("dbt", &["deps"], &root, &envs)
        };
        let parse_res = if use_docker {
            run_cmd_docker(&self.runner, &root, profiles_path, &["parse"], &envs)
        } else {
            run_cmd("dbt", &["parse"], &root, &envs)
        };
        let compile_res = if use_docker {
            run_cmd_docker(&self.runner, &root, profiles_path, &["compile", "--target", &target], &envs)
        } else {
            run_cmd("dbt", &["compile", "--target", &target], &root, &envs)
        };
        let run_or_build_res = if build {
            Some(if use_docker {
                run_cmd_docker(&self.runner, &root, profiles_path, &["build", "--target", &target], &envs)
            } else {
                run_cmd("dbt", &["build", "--target", &target], &root, &envs)
            })
        } else if run {
            Some(if use_docker {
                run_cmd_docker(&self.runner, &root, profiles_path, &["run", "--target", &target], &envs)
            } else {
                run_cmd("dbt", &["run", "--target", &target], &root, &envs)
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_args_require_image() {
        let runner = DbtRunnerConfig { mode: "docker".to_string(), docker_image: None, ..Default::default() };
        let tmp = tempfile::tempdir().unwrap();
        let err = build_docker_run_args(&runner, tmp.path(), None, &["deps"], &[]).unwrap_err();
        assert!(err.to_lowercase().contains("docker_image"));
    }

    #[test]
    fn docker_args_include_project_mount_and_workdir() {
        let runner = DbtRunnerConfig {
            mode: "docker".to_string(),
            docker_image: Some("ghcr.io/dbt-labs/dbt-athena:1.8.3".to_string()),
            docker_platform: Some("linux/amd64".to_string()),
            docker_network: Some("host".to_string()),
            docker_mount_aws_dir: false,
        };
        let tmp = tempfile::tempdir().unwrap();
        let args = build_docker_run_args(&runner, tmp.path(), None, &["deps"], &[]).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("--rm"));
        assert!(joined.contains("--platform linux/amd64"));
        assert!(joined.contains("--network host"));
        assert!(joined.contains(":/project"));
        assert!(joined.contains("-w /project"));
        assert!(joined.contains("ghcr.io/dbt-labs/dbt-athena:1.8.3"));
    }
}

