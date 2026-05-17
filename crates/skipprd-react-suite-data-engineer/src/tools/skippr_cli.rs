use async_trait::async_trait;
use serde_json::Value;
use std::process::Command;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct SkipprCliTool;

fn configured_skippr() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "skippr".to_string())
}

fn config_args() -> Vec<String> {
    std::env::var("SKIPPR_CONFIG_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| vec!["--config".to_string(), s])
        .unwrap_or_default()
}

fn run_skippr(args: Vec<String>) -> Result<Value, String> {
    let output = Command::new(configured_skippr())
        .args(&args)
        .output()
        .map_err(|e| format!("failed to run skippr: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let parsed = serde_json::from_str::<Value>(&stdout)
        .ok()
        .map(|value| compact_skippr_json(&args, value));
    let stdout_text = if parsed.is_some() {
        String::new()
    } else {
        stdout.chars().take(12000).collect::<String>()
    };
    Ok(serde_json::json!({
        "ok": output.status.success(),
        "status": output.status.code(),
        "args": args,
        "json": parsed,
        "stdout": stdout_text,
        "stderr": stderr.chars().take(12000).collect::<String>(),
    }))
}

fn compact_skippr_json(args: &[String], value: Value) -> Value {
    if args.ends_with(&[
        "user".to_string(),
        "--output".to_string(),
        "json".to_string(),
        "account".to_string(),
    ]) {
        return compact_account_json(value);
    }
    value
}

fn compact_account_json(value: Value) -> Value {
    let Some(account) = value.get("account") else {
        return value;
    };
    serde_json::json!({
        "account": {
            "balance": account.get("balance").cloned(),
            "profile": account.get("profile").cloned(),
            "monthly_cost_est": account.get("monthly_cost_est").cloned(),
            "daily_costs_est": account.get("daily_costs_est").cloned(),
            "recent_usage_count": account
                .get("recent_usage")
                .and_then(|v| v.as_array())
                .map(|rows| rows.len())
                .unwrap_or(0),
        }
    })
}

#[async_trait]
impl Tool for SkipprCliTool {
    fn name(&self) -> &'static str {
        "skippr_cli"
    }

    async fn call(&self, args: Value, _ctx: &AgentCtx) -> Result<Value, String> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let mut cli_args = config_args();
        match command {
            "user" => {
                let action = if action.is_empty() { "account" } else { action };
                match action {
                    "account" | "list-api-keys" => {
                        cli_args.extend(
                            ["user", "--output", "json", action]
                                .into_iter()
                                .map(str::to_string),
                        );
                    }
                    _ => {
                        return Err(
                            "skippr_cli user action must be account or list-api-keys".to_string()
                        )
                    }
                }
            }
            "doctor" => {
                cli_args.extend(
                    ["doctor", "--output", "json"]
                        .into_iter()
                        .map(str::to_string),
                );
            }
            "test" => {
                let pipeline = args
                    .get("pipeline")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "skippr_cli test requires pipeline".to_string())?;
                let action = if action.is_empty() { "list" } else { action };
                match action {
                    "list" => cli_args.extend(
                        ["test", "list", "--pipeline", pipeline, "--output", "json"]
                            .into_iter()
                            .map(str::to_string),
                    ),
                    "run" => {
                        cli_args.extend(
                            ["test", "run", "--pipeline", pipeline, "--output", "json"]
                                .into_iter()
                                .map(str::to_string),
                        );
                        if let Some(selects) = args.get("select").and_then(|v| v.as_array()) {
                            for select in selects.iter().filter_map(|v| v.as_str()) {
                                if !select.trim().is_empty() {
                                    cli_args.push("--select".to_string());
                                    cli_args.push(select.trim().to_string());
                                }
                            }
                        }
                    }
                    _ => return Err("skippr_cli test action must be list or run".to_string()),
                }
            }
            "connect" => {
                cli_args.extend(["connect", "--help"].into_iter().map(str::to_string));
            }
            _ => {
                return Err(
                    "skippr_cli command must be one of: user, doctor, test, connect".to_string(),
                )
            }
        }
        run_skippr(cli_args)
    }
}
