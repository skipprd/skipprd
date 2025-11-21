use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::io::Write;
use crate::qa::agent::AgentCtx;
use super::Tool;

pub struct DbtValidateTool;

#[async_trait]
impl Tool for DbtValidateTool {
	fn name(&self) -> &'static str { "dbt_validate" }
	async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
		let project_name = args.get("project_name").and_then(|x| x.as_str()).unwrap_or("skippr_model");
		let files = args.get("files").and_then(|x| x.as_array()).cloned().unwrap_or_default();
		if files.is_empty() {
			return Ok(serde_json::json!({"ok": false, "error": "files array required"}));
		}
		let profiles_dir = args.get("profiles_dir").and_then(|x| x.as_str()).map(|s| s.to_string())
			.or_else(|| std::env::var("DBT_PROFILES_DIR").ok());
		let target = args.get("target").and_then(|x| x.as_str()).map(|s| s.to_string())
			.or_else(|| std::env::var("DBT_TARGET").ok())
			.unwrap_or_else(|| "datafusion".to_string());
		// Stage project under a temp dir
		let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
		let root = tmp.path().join(project_name);
		std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
		// Ensure minimal dbt_project.yml if not provided
		let mut has_project_yml = false;
		for f in files.iter() {
			if f.get("path").and_then(|x| x.as_str()).unwrap_or("").ends_with("dbt_project.yml") { has_project_yml = true; break; }
		}
		if !has_project_yml {
			let y = format!("name: {}\nversion: '1.0'\nprofile: '{}'\nmodel-paths: ['models']\ntarget-path: 'target'\n", project_name, project_name);
			write_file(&root.join("dbt_project.yml"), y.as_bytes())?;
		}
		// Write provided files
		for f in files.iter() {
			let path = f.get("path").and_then(|x| x.as_str()).ok_or_else(|| "file.path required".to_string())?;
			let content = f.get("content").and_then(|x| x.as_str()).ok_or_else(|| "file.content required".to_string())?;
			let full = root.join(path);
			if let Some(parent) = full.parent() { std::fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
			write_file(&full, content.as_bytes())?;
		}
		// Build command env
		let mut envs: Vec<(&str, String)> = Vec::new();
		if let Some(pd) = profiles_dir.as_ref() { envs.push(("DBT_PROFILES_DIR", pd.clone())); }
		// Run dbt parse
		let parse_res = run_cmd("dbt", &["parse"], &root, &envs);
		// Run dbt compile with target
		let compile_res = run_cmd("dbt", &["compile", "--target", &target], &root, &envs);
		let ok = parse_res.status_ok && compile_res.status_ok;
		let out = serde_json::json!({
			"ok": ok,
			"parse_ok": parse_res.status_ok,
			"compile_ok": compile_res.status_ok,
			"errors": combine_errors(&parse_res, &compile_res),
			"warnings": [],
			"logs": {
				"parse": { "code": parse_res.code, "stdout": parse_res.stdout, "stderr": parse_res.stderr },
				"compile": { "code": compile_res.code, "stdout": compile_res.stdout, "stderr": compile_res.stderr },
			}
		});
		Ok(out)
	}
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
	let mut f = std::fs::File::create(path).map_err(|e| e.to_string())?;
	f.write_all(bytes).map_err(|e| e.to_string())
}

struct CmdOut {
	status_ok: bool,
	code: i32,
	stdout: String,
	stderr: String,
}

fn run_cmd(cmd: &str, args: &[&str], cwd: &Path, envs: &[(&str, String)]) -> CmdOut {
	let mut c = std::process::Command::new(cmd);
	c.args(args).current_dir(cwd);
	for (k, v) in envs.iter() { c.env(k, v); }
	match c.output() {
		Ok(o) => {
			let code = o.status.code().unwrap_or(-1);
			let ok = o.status.success();
			let stdout = String::from_utf8_lossy(&o.stdout).to_string();
			let stderr = String::from_utf8_lossy(&o.stderr).to_string();
			CmdOut { status_ok: ok, code, stdout, stderr }
		}
		Err(e) => CmdOut { status_ok: false, code: -1, stdout: String::new(), stderr: format!("spawn error: {}", e) }
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


