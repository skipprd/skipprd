use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{io::BufRead, process::Stdio};

use crate::adapters::storage::StorageAdapter;
use crate::providers::{Keyspace, RequestScope};

use react_core::providers::{DbtProvider, DbtValidateArgs, DbtValidateResult};

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

fn redact_docker_args_for_log(args: &[String]) -> String {
    // IMPORTANT: docker args can include `-e KEY=VALUE` where VALUE may be a secret.
    // We redact values after '=' for any token passed to `-e`, and fully redact known AWS secrets.
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut next_is_env = false;
    for a in args.iter() {
        if next_is_env {
            next_is_env = false;
            if let Some((k, _v)) = a.split_once('=') {
                out.push(format!("{}=***", k));
            } else {
                out.push("***".to_string());
            }
            continue;
        }
        if a == "-e" || a == "--env" {
            out.push(a.clone());
            next_is_env = true;
            continue;
        }
        // Also redact inline KEY=VALUE tokens for common AWS secrets if present.
        if let Some((k, _v)) = a.split_once('=') {
            let k_uc = k.to_ascii_uppercase();
            if k_uc.contains("AWS_SECRET_ACCESS_KEY") || k_uc.contains("AWS_SESSION_TOKEN") || k_uc.contains("AWS_ACCESS_KEY_ID") {
                out.push(format!("{}=***", k));
                continue;
            }
        }
        out.push(a.clone());
    }
    out.join(" ")
}

fn run_cmd_labeled(cmd: &str, args: &[&str], cwd: &Path, envs: &[(&str, String)], label: &str) -> CmdOut {
    let started = std::time::Instant::now();
    tracing::info!(
        target: "dbt",
        phase = %label,
        runner = "host",
        cmd = %cmd,
        args = %args.join(" "),
        "starting"
    );
    let mut c = std::process::Command::new(cmd);
    c.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs.iter() {
        c.env(k, v);
    }

    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            return CmdOut { status_ok: false, code: -1, stdout: String::new(), stderr: format!("spawn error: {}", e) };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (tx, rx) = std::sync::mpsc::channel::<(bool, String)>(); // (is_stderr, line)
    if let Some(out) = stdout {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let br = std::io::BufReader::new(out);
            for line in br.lines().flatten() {
                let _ = tx.send((false, line));
            }
        });
    }
    if let Some(err) = stderr {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let br = std::io::BufReader::new(err);
            for line in br.lines().flatten() {
                let _ = tx.send((true, line));
            }
        });
    }
    drop(tx);

    let mut out_buf = String::new();
    let mut err_buf = String::new();
    for (is_err, line) in rx {
        if is_err {
            tracing::info!(target: "dbt", phase = %label, runner = "host", stream = "stderr", "{}", line);
            err_buf.push_str(&line);
            err_buf.push('\n');
        } else {
            tracing::info!(target: "dbt", phase = %label, runner = "host", stream = "stdout", "{}", line);
            out_buf.push_str(&line);
            out_buf.push('\n');
        }
    }

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            return CmdOut { status_ok: false, code: -1, stdout: out_buf, stderr: format!("wait error: {}", e) };
        }
    };
    let code = status.code().unwrap_or(-1);
    let ok = status.success();
    tracing::info!(
        target: "dbt",
        phase = %label,
        runner = "host",
        exit_code = code,
        ok = ok,
        duration_ms = started.elapsed().as_millis() as u64,
        "finished"
    );
    CmdOut { status_ok: ok, code, stdout: out_buf, stderr: err_buf }
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
    run_cmd_docker_labeled(runner, project_dir, profiles_dir, dbt_args, envs, "dbt")
}

fn run_cmd_docker_labeled(
    runner: &DbtRunnerConfig,
    project_dir: &Path,
    profiles_dir: Option<&Path>,
    dbt_args: &[&str],
    envs: &[(&str, String)],
    label: &str,
) -> CmdOut {
    let started = std::time::Instant::now();
    let args = match build_docker_run_args(runner, project_dir, profiles_dir, dbt_args, envs) {
        Ok(v) => v,
        Err(e) => {
            return CmdOut { status_ok: false, code: -1, stdout: String::new(), stderr: e };
        }
    };

    tracing::info!(
        target: "dbt",
        phase = %label,
        runner = "docker",
        cmd = "docker",
        args = %redact_docker_args_for_log(&args),
        "starting"
    );

    let mut c = std::process::Command::new("docker");
    c.args(&args)
        .current_dir(project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            return CmdOut { status_ok: false, code: -1, stdout: String::new(), stderr: format!("spawn error: {}", e) };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (tx, rx) = std::sync::mpsc::channel::<(bool, String)>(); // (is_stderr, line)
    if let Some(out) = stdout {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let br = std::io::BufReader::new(out);
            for line in br.lines().flatten() {
                let _ = tx.send((false, line));
            }
        });
    }
    if let Some(err) = stderr {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let br = std::io::BufReader::new(err);
            for line in br.lines().flatten() {
                let _ = tx.send((true, line));
            }
        });
    }
    drop(tx);

    let mut out_buf = String::new();
    let mut err_buf = String::new();
    for (is_err, line) in rx {
        if is_err {
            tracing::info!(target: "dbt", phase = %label, runner = "docker", stream = "stderr", "{}", line);
            err_buf.push_str(&line);
            err_buf.push('\n');
        } else {
            tracing::info!(target: "dbt", phase = %label, runner = "docker", stream = "stdout", "{}", line);
            out_buf.push_str(&line);
            out_buf.push('\n');
        }
    }

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            return CmdOut { status_ok: false, code: -1, stdout: out_buf, stderr: format!("wait error: {}", e) };
        }
    };
    let code = status.code().unwrap_or(-1);
    let ok = status.success();
    tracing::info!(
        target: "dbt",
        phase = %label,
        runner = "docker",
        exit_code = code,
        ok = ok,
        duration_ms = started.elapsed().as_millis() as u64,
        "finished"
    );
    CmdOut { status_ok: ok, code, stdout: out_buf, stderr: err_buf }
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
            // Back-compat / self-heal: if an existing dbt_project.yml is invalid for dbt-core (e.g. has a top-level
            // `depends_on` key), rewrite it to a minimal valid project file. Otherwise, leave it intact.
            if let Ok(bytes) = self.storage.get_bytes(&project_key).await {
                let text = String::from_utf8_lossy(&bytes).to_string();
                if let Ok(yv) = serde_yaml::from_str::<serde_yaml::Value>(&text) {
                    if let Some(map) = yv.as_mapping() {
                        let has_depends_on = map
                            .keys()
                            .any(|k| k.as_str().map(|s| s == "depends_on").unwrap_or(false));
                        if !has_depends_on {
                            return Ok(());
                        }
                    } else {
                        // Non-mapping YAML: treat as invalid and rewrite.
                    }
                } else {
                    // Unparseable YAML: rewrite.
                }
            }
            // Fall through and rewrite below.
        }
        let name = format!("{}_project", scope.project_id.replace('/', "_"));
        let y = format!(
            "name: {name}\nversion: '1.0'\nprofile: '{project_id}'\nmodel-paths: ['models']\nseed-paths: ['seeds']\nmacro-paths: ['macros']\ntarget-path: 'target'\n\nmodels:\n  {name}:\n    # Suffix strategy: dbt materializes schemas as <DBT_TARGET_SCHEMA>_<suffix>.\n    # Default all models into GOLD by setting their custom schema name to the gold suffix.\n    +schema: \"{{{{ env_var('DBT_GOLD_SUFFIX', 'warehouse') }}}}\"\n    # Force staging models under models/staging into SILVER.\n    staging:\n      +schema: \"{{{{ env_var('DBT_SILVER_SUFFIX', 'silver') }}}}\"\n",
            name = name,
            project_id = scope.project_id
        );
        self.storage.put_bytes(&project_key, y.as_bytes(), "text/yaml").await?;
        Ok(())
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

        let profiles_dir = args
            .profiles_dir
            .clone()
            .or_else(|| std::env::var("DBT_PROFILES_DIR").ok());
        let target = if args.target.is_empty() {
            return Err("dbt validate requires a non-empty target (e.g. 'athena'); no default target exists".to_string());
        } else {
            args.target.clone()
        };

        let run = args.run;
        let build = args.build;
        let select_terms = args.select.as_ref().filter(|v| !v.is_empty());
        let exclude_terms = args.exclude.as_ref().filter(|v| !v.is_empty());

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
            // Important: profile name must match the generated `profiles.yml` entry, which is scope.project_id.
            let y = format!(
                "name: {}\nversion: '1.0'\nprofile: '{}'\nmodel-paths: ['models']\ntarget-path: 'target'\n\nmodels:\n  {}:\n    # Suffix strategy: dbt materializes schemas as <DBT_TARGET_SCHEMA>_<suffix>.\n    +schema: \"{{{{ env_var('DBT_GOLD_SUFFIX', 'warehouse') }}}}\"\n    staging:\n      +schema: \"{{{{ env_var('DBT_SILVER_SUFFIX', 'silver') }}}}\"\n",
                project_name,
                scope.project_id
                ,
                project_name
            );
            write_file(&proj, y.as_bytes())?;
        }

        // Sanitize dbt_project.yml for dbt-core strict schema:
        // - Remove invalid top-level keys like `depends_on` (seen in some templates).
        // - Force `profile:` to match scope.project_id so it aligns with generated profiles.yml.
        // - Persist the sanitized version back to storage so future runs are clean.
        {
            let project_key = self.keyspace.dbt_project_key(scope);
            let raw = std::fs::read_to_string(&proj).unwrap_or_default();
            let mut changed = false;
            let mut v: serde_yaml::Value = match serde_yaml::from_str(&raw) {
                Ok(v) => v,
                Err(_) => {
                    changed = true;
                    serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
                }
            };
            if !matches!(v, serde_yaml::Value::Mapping(_)) {
                v = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
                changed = true;
            }
            let map = v.as_mapping_mut().unwrap();
            // Remove invalid top-level depends_on
            let dep_key = serde_yaml::Value::String("depends_on".to_string());
            if map.remove(&dep_key).is_some() {
                changed = true;
            }
            // Force profile to scope.project_id
            let prof_key = serde_yaml::Value::String("profile".to_string());
            let desired_profile = serde_yaml::Value::String(scope.project_id.clone());
            if map.get(&prof_key) != Some(&desired_profile) {
                map.insert(prof_key, desired_profile);
                changed = true;
            }
            if changed {
                let new_text = serde_yaml::to_string(&v).unwrap_or_else(|_| raw.clone());
                let _ = write_file(&proj, new_text.as_bytes());
                let _ = self
                    .storage
                    .put_bytes(&project_key, new_text.as_bytes(), "text/yaml")
                    .await;
            }
        }

        let mut envs: Vec<(&str, String)> = Vec::new();
        if let Some(pd) = profiles_dir.as_ref() { envs.push(("DBT_PROFILES_DIR", pd.clone())); }

        let use_docker = self.runner.mode.to_lowercase() == "docker";
        let profiles_path = profiles_dir.as_ref().map(|s| Path::new(s));

        // Hard fail if dbt CLI itself is broken on this runner (do NOT attempt to repair).
        // This catches host-level Python/env issues early (e.g. import errors) before we try deps/parse/compile.
        let version_res = if use_docker {
            run_cmd_docker_labeled(&self.runner, &root, profiles_path, &["--version"], &envs, "version")
        } else {
            run_cmd_labeled("dbt", &["--version"], &root, &envs, "version")
        };
        if !version_res.status_ok {
            return Err(format!(
                "dbt environment check failed (dbt --version). This is a host/runner issue; do not attempt auto-repair.\n\nstdout:\n{}\n\nstderr:\n{}",
                version_res.stdout.trim(),
                version_res.stderr.trim()
            ));
        }

        let deps_res = if use_docker {
            run_cmd_docker_labeled(&self.runner, &root, profiles_path, &["deps"], &envs, "deps")
        } else {
            run_cmd_labeled("dbt", &["deps"], &root, &envs, "deps")
        };
        let parse_res = if use_docker {
            run_cmd_docker_labeled(&self.runner, &root, profiles_path, &["parse"], &envs, "parse")
        } else {
            run_cmd_labeled("dbt", &["parse"], &root, &envs, "parse")
        };
        let compile_res = {
            let mut argv: Vec<String> = vec!["compile".to_string(), "--target".to_string(), target.clone()];
            if let Some(sel) = select_terms {
                for t in sel.iter() {
                    argv.push("--select".to_string());
                    argv.push(t.clone());
                }
            }
            if let Some(ex) = exclude_terms {
                for t in ex.iter() {
                    argv.push("--exclude".to_string());
                    argv.push(t.clone());
                }
            }
            let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            if use_docker {
                run_cmd_docker_labeled(&self.runner, &root, profiles_path, &argv_refs, &envs, "compile")
            } else {
                run_cmd_labeled("dbt", &argv_refs, &root, &envs, "compile")
            }
        };
        let run_or_build_res = if build {
            Some({
                let mut argv: Vec<String> = vec!["build".to_string(), "--target".to_string(), target.clone()];
                if let Some(sel) = select_terms {
                    for t in sel.iter() {
                        argv.push("--select".to_string());
                        argv.push(t.clone());
                    }
                }
                if let Some(ex) = exclude_terms {
                    for t in ex.iter() {
                        argv.push("--exclude".to_string());
                        argv.push(t.clone());
                    }
                }
                let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
                if use_docker {
                    run_cmd_docker_labeled(&self.runner, &root, profiles_path, &argv_refs, &envs, "build")
                } else {
                    run_cmd_labeled("dbt", &argv_refs, &root, &envs, "build")
                }
            })
        } else if run {
            Some({
                let mut argv: Vec<String> = vec!["run".to_string(), "--target".to_string(), target.clone()];
                if let Some(sel) = select_terms {
                    for t in sel.iter() {
                        argv.push("--select".to_string());
                        argv.push(t.clone());
                    }
                }
                if let Some(ex) = exclude_terms {
                    for t in ex.iter() {
                        argv.push("--exclude".to_string());
                        argv.push(t.clone());
                    }
                }
                let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
                if use_docker {
                    run_cmd_docker_labeled(&self.runner, &root, profiles_path, &argv_refs, &envs, "run")
                } else {
                    run_cmd_labeled("dbt", &argv_refs, &root, &envs, "run")
                }
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

