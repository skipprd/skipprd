use async_trait::async_trait;
use serde_json::Value;
use std::process::Command;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct SkipprCliTool;

pub(crate) fn configured_skippr() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "skippr".to_string())
}

pub(crate) fn config_args() -> Vec<String> {
    std::env::var("SKIPPR_CONFIG_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| vec!["--config".to_string(), s])
        .unwrap_or_default()
}

pub(crate) fn ide_chat_surface_enabled() -> bool {
    std::env::var("SKIPPR_EXECUTION_SURFACE")
        .map(|v| v == "ide_chat")
        .unwrap_or(false)
}

pub(crate) fn ide_model_bridge_request(args: &Value) -> Result<Value, String> {
    let pipeline = args
        .get("pipeline")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "model_subagent requires pipeline".to_string())?;
    Ok(serde_json::json!({
        "ok": true,
        "ide_model_run_requested": true,
        "pipeline": pipeline,
        "dbt_output_path": args.get("dbt_output_path").and_then(|v| v.as_str()),
        "no_resume": args.get("no_resume").and_then(|v| v.as_bool()).unwrap_or(false),
    }))
}

fn normalized_tool_args(args: &Value) -> &Value {
    match args.get("args") {
        Some(nested) if nested.is_object() => nested,
        _ => args,
    }
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

pub(crate) fn build_model_args(args: &Value) -> Result<Vec<String>, String> {
    let pipeline = args
        .get("pipeline")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "model_subagent requires pipeline".to_string())?;
    let mut cli_args = config_args();
    cli_args.extend(
        ["model", "--pipeline", pipeline, "--output", "jsonl"]
            .into_iter()
            .map(str::to_string),
    );
    if let Some(dbt_output_path) = args
        .get("dbt_output_path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cli_args.push("--dbt-output-path".to_string());
        cli_args.push(dbt_output_path.to_string());
    }
    if args
        .get("no_resume")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        cli_args.push("--no-resume".to_string());
    }
    Ok(cli_args)
}

pub(crate) fn run_skippr_model(args: Vec<String>) -> Result<Value, String> {
    let output = Command::new(configured_skippr())
        .args(&args)
        .output()
        .map_err(|e| format!("failed to run skippr model: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect();
    let terminal_event = events.iter().rev().find(|event| {
        event
            .get("event")
            .and_then(|v| v.as_str())
            .map(|event| matches!(event, "model_complete" | "model_error"))
            .unwrap_or(false)
    });
    let failure_summary = terminal_event
        .and_then(|event| event.get("failure_summary"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            events.iter().rev().find_map(|event| {
                event
                    .get("error")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
            })
        });
    let changed_files = events
        .iter()
        .filter(|event| event.get("event").and_then(|v| v.as_str()) == Some("model_file_changed"))
        .filter_map(|event| event.get("changed_files").cloned())
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "ok": output.status.success(),
        "status": output.status.code(),
        "args": args,
        "event_count": events.len(),
        "terminal_event": terminal_event.cloned(),
        "failure_summary": failure_summary,
        "changed_files": changed_files,
        "events_tail": events.iter().rev().take(40).cloned().collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),
        "stdout_tail": stdout.chars().rev().take(12000).collect::<String>().chars().rev().collect::<String>(),
        "stderr_tail": stderr.chars().rev().take(12000).collect::<String>().chars().rev().collect::<String>(),
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
        let args = normalized_tool_args(&args);
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
            "config" => {
                let action = if action.is_empty() { "show" } else { action };
                if action != "show" {
                    return Err("skippr_cli config action must be show".to_string());
                }
                cli_args.extend(
                    ["config", "show", "--output", "json"]
                        .into_iter()
                        .map(str::to_string),
                );
            }
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
            "lineage" => {
                let action = if action.is_empty() { "graph" } else { action };
                if action != "graph" {
                    return Err("skippr_cli lineage action must be graph".to_string());
                }
                cli_args.extend(
                    ["lineage", "graph", "--output", "json"]
                        .into_iter()
                        .map(str::to_string),
                );
                if let Some(pipeline) = args
                    .get("pipeline")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    cli_args.push("--pipeline".to_string());
                    cli_args.push(pipeline.to_string());
                }
                if let Some(field_node_id) = args
                    .get("field_node_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    cli_args.push("--field-node-id".to_string());
                    cli_args.push(field_node_id.to_string());
                }
                if let Some(direction) = args
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    cli_args.push("--direction".to_string());
                    cli_args.push(direction.to_string());
                }
            }
            "query" => {
                let pipeline = args
                    .get("pipeline")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "skippr_cli query requires pipeline".to_string())?;
                let sql = args
                    .get("sql")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "skippr_cli query requires sql".to_string())?;
                cli_args.extend(
                    [
                        "query",
                        "--pipeline",
                        pipeline,
                        "--sql",
                        sql,
                        "--output",
                        "json",
                    ]
                    .into_iter()
                    .map(str::to_string),
                );
            }
            "connect" => {
                cli_args.extend(["connect", "--help"].into_iter().map(str::to_string));
            }
            "model" => {
                if ide_chat_surface_enabled() {
                    return ide_model_bridge_request(&args);
                }
                return run_skippr_model(build_model_args(&args)?);
            }
            _ => return Err(
                "skippr_cli command must be one of: config, user, doctor, test, lineage, query, connect, model"
                    .to_string(),
            ),
        }
        run_skippr(cli_args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalized_tool_args_accepts_nested_args_object() {
        let input = json!({
            "args": {
                "command": "query",
                "pipeline": "bike_hire",
                "sql": "select count(*) from analytics.raw.bikes"
            }
        });
        let args = normalized_tool_args(&input);

        assert_eq!(args["command"], "query");
        assert_eq!(args["pipeline"], "bike_hire");
        assert_eq!(args["sql"], "select count(*) from analytics.raw.bikes");
    }

    #[test]
    fn normalized_tool_args_preserves_top_level_args() {
        let input = json!({
            "command": "query",
            "pipeline": "bike_hire",
            "sql": "select count(*) from analytics.raw.bikes"
        });
        let args = normalized_tool_args(&input);

        assert_eq!(args["command"], "query");
        assert_eq!(args["pipeline"], "bike_hire");
    }
}
