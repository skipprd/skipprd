use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::io::Write;
use crate::qa::agent::AgentCtx;
use super::Tool;
use tracing::info;

pub struct DbtValidateTool;

#[async_trait]
impl Tool for DbtValidateTool {
	fn name(&self) -> &'static str { "dbt_validate" }
	async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
		let project_name = args.get("project_name").and_then(|x| x.as_str()).unwrap_or("skippr_model");
		let s3_prefix_opt = args.get("s3_prefix").and_then(|x| x.as_str()).map(|s| s.to_string());
		if s3_prefix_opt.is_none() {
			return Ok(serde_json::json!({"ok": false, "error": "s3_prefix is required. Upload the project to S3 and pass s3_prefix to validate."}));
		}
		let profiles_dir = args.get("profiles_dir").and_then(|x| x.as_str()).map(|s| s.to_string())
			.or_else(|| std::env::var("DBT_PROFILES_DIR").ok());
		let target = args.get("target").and_then(|x| x.as_str()).map(|s| s.to_string())
			.or_else(|| std::env::var("DBT_TARGET").ok())
			.unwrap_or_else(|| "datafusion".to_string());
			let run = args.get("run").and_then(|x| x.as_bool()).unwrap_or(false);
			let build = args.get("build").and_then(|x| x.as_bool()).unwrap_or(false);
		// Stage project under a temp dir
		let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
		let root = tmp.path().join(project_name);
		std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
		// Populate temp project from S3 prefix (required)
		if let Some(pref) = s3_prefix_opt.as_ref() {
			// Pull project from S3 prefix
			let bucket = crate::helpers::configuration::Config::get_skippr_s3_bucket();
			let client = crate::helpers::s3::get_s3_client().await;
			let mut token: Option<String> = None;
			let prefix = if pref.ends_with('/') { pref.clone() } else { format!("{}/", pref) };
			let mut file_count = 0usize;
			loop {
				let mut req = client.list_objects_v2()
					.bucket(&bucket)
					.prefix(&prefix)
					.max_keys(1000);
				if let Some(t) = token.as_ref() { req = req.continuation_token(t); }
				let resp = match req.send().await {
					Ok(r) => r,
					Err(e) => return Ok(serde_json::json!({"ok": false, "error": format!("list_objects_v2 failed for s3://{}/{}: {:?}", bucket, prefix, e)})),
				};
				for obj in resp.contents() {
					if let Some(key) = obj.key() {
						if key.ends_with('/') { continue; }
						// derive relative path under root
						let rel = key.strip_prefix(&prefix).unwrap_or(key);
						let dest = root.join(rel);
						if let Some(parent) = dest.parent() { let _ = std::fs::create_dir_all(parent); }
						match crate::helpers::s3::get_bytes(key).await {
							Ok(bytes) => { let _ = write_file(&dest, &bytes); file_count += 1; }
							Err(e) => return Ok(serde_json::json!({"ok": false, "error": format!("get_bytes failed for '{}': {:?}", key, e)})),
						}
					}
				}
				if resp.next_continuation_token().is_none() { break; }
				token = resp.next_continuation_token().map(|s| s.to_string());
			}
			info!("dbt_validate: fetched {} file(s) from s3://{}/{}", file_count, bucket, prefix);
			// Ensure minimal project file if missing
			let proj = root.join("dbt_project.yml");
			if !proj.exists() {
				let y = format!("name: {}\nversion: '1.0'\nprofile: '{}'\nmodel-paths: ['models']\ntarget-path: 'target'\n", project_name, project_name);
				write_file(&proj, y.as_bytes())?;
			}
		} else {
			// unreachable due to earlier none-check
			return Ok(serde_json::json!({"ok": false, "error": "s3_prefix is required"}));
		}
		// Build command env
		let mut envs: Vec<(&str, String)> = Vec::new();
		if let Some(pd) = profiles_dir.as_ref() { envs.push(("DBT_PROFILES_DIR", pd.clone())); }
		// deps → parse → compile
		let deps_res = run_cmd("dbt", &["deps"], &root, &envs);
		info!("dbt deps finished code={} ok={}", deps_res.code, deps_res.status_ok);
		let parse_res = run_cmd("dbt", &["parse"], &root, &envs);
		info!("dbt parse finished code={} ok={}", parse_res.code, parse_res.status_ok);
		let compile_res = run_cmd("dbt", &["compile", "--target", &target], &root, &envs);
		info!("dbt compile finished code={} ok={}", compile_res.code, compile_res.status_ok);
		// Optional: build or run (build preferred)
		let run_or_build_res = if build {
			let r = run_cmd("dbt", &["build", "--target", &target], &root, &envs);
			info!("dbt build finished code={} ok={}", r.code, r.status_ok);
			Some(r)
		} else if run {
			let r = run_cmd("dbt", &["run", "--target", &target], &root, &envs);
			info!("dbt run finished code={} ok={}", r.code, r.status_ok);
			Some(r)
		} else {
			None
		};
		let ok = deps_res.status_ok && parse_res.status_ok && compile_res.status_ok
			&& run_or_build_res.as_ref().map(|o| o.status_ok).unwrap_or(true);
		// Collect error strings
		let mut errs_vec: Vec<String> = Vec::new();
		for e in combine_errors(&deps_res, &parse_res) { errs_vec.push(e); }
		for e in combine_errors(&compile_res, run_or_build_res.as_ref().unwrap_or(&CmdOut{status_ok:true,code:0,stdout:String::new(),stderr:String::new()})) { errs_vec.push(e); }
		let out = serde_json::json!({
			"ok": ok,
			"deps_ok": deps_res.status_ok,
			"parse_ok": parse_res.status_ok,
			"compile_ok": compile_res.status_ok,
			"run_ok": run_or_build_res.as_ref().map(|o| o.status_ok),
			"errors": errs_vec,
			"warnings": [],
			"logs": {
				"deps": { "code": deps_res.code, "stdout": deps_res.stdout, "stderr": deps_res.stderr },
				"parse": { "code": parse_res.code, "stdout": parse_res.stdout, "stderr": parse_res.stderr },
				"compile": { "code": compile_res.code, "stdout": compile_res.stdout, "stderr": compile_res.stderr },
				"run_or_build": run_or_build_res.as_ref().map(|r| serde_json::json!({
					"code": r.code, "stdout": r.stdout, "stderr": r.stderr
				})),
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


