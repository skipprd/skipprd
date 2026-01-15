use async_trait::async_trait;
use serde_json::Value;

use crate::agent::AgentCtx;
use crate::tools::Tool;
use std::fs;
use std::path::PathBuf;

pub struct DbtValidateTool;

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
        let mut profiles_dir = args
            .get("profiles_dir")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .or_else(|| std::env::var("DBT_PROFILES_DIR").ok());
        let target = args
            .get("target")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .or_else(|| std::env::var("DBT_TARGET").ok())
            .unwrap_or_else(|| "datafusion".to_string());
        let run = args.get("run").and_then(|x| x.as_bool()).unwrap_or(false);
        let build = args.get("build").and_then(|x| x.as_bool()).unwrap_or(false);

        // If profiles_dir not provided, try generating one from resolved config for the active warehouse provider.
        // Keep the tempdir alive for the duration of this call.
        let mut _tmp: Option<tempfile::TempDir> = None;
        if profiles_dir.is_none() {
            if let Some(cfg) = ctx.resolved_config.as_ref() {
                if let Ok(gen) = crate::dbt::profile::generate_profiles_yml(cfg.as_ref()) {
                    let td = tempfile::tempdir().map_err(|e| e.to_string())?;
                    let mut p = PathBuf::from(td.path());
                    p.push("profiles.yml");
                    fs::write(&p, gen.profiles_yml.as_bytes()).map_err(|e| e.to_string())?;
                    profiles_dir = Some(td.path().to_string_lossy().to_string());
                    _tmp = Some(td);
                }
            }
        }

        let res = dbt
            .validate_project(
                &ctx.scope,
                &crate::providers::DbtValidateArgs {
                    project_name: project_name.to_string(),
                    profiles_dir,
                    target,
                    run,
                    build,
                },
            )
            .await?;
        Ok(serde_json::to_value(res)
            .unwrap_or_else(|_| serde_json::json!({"ok": false, "error": "failed to serialize result"})))
    }
}

