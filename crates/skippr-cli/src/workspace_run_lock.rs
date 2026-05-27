//! Workspace-wide exclusive lock for discover / sync / model via circles-auth API.

use crate::api_client::{ApiClient, ApiError};
use reqwest::StatusCode;
use std::future::Future;
use std::time::Duration;
use tokio::task::JoinHandle;

#[allow(dead_code)]
const HEAVY_COMMANDS: &[&str] = &["discover", "sync", "sync-once", "sync-all-once", "model"];

#[allow(dead_code)]
pub fn is_heavy_command(command: &str) -> bool {
    HEAVY_COMMANDS.contains(&command.trim())
}

pub fn sync_api_command(once: bool) -> &'static str {
    if once {
        "sync-once"
    } else {
        "sync"
    }
}

/// CI uses API keys against auth.skippr.io; workspace run-lock routes are not yet on API Gateway.
fn skip_workspace_run_lock() -> bool {
    if matches!(
        std::env::var("SKIPPR_SKIP_WORKSPACE_RUN_LOCK").ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    ) {
        return true;
    }
    std::env::var("GITHUB_ACTIONS").ok().as_deref() == Some("true")
        && std::env::var("SKIPPR_API_KEY")
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
}

struct ActiveLock {
    client: ApiClient,
    workspace: String,
    run_id: String,
    version: i64,
}

/// Run `f` while holding the workspace heavy lock using an existing API client.
pub async fn with_heavy_run_lock_client<T, F, Fut>(
    client: ApiClient,
    workspace: &str,
    command: &str,
    pipeline: Option<&str>,
    f: F,
) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    if skip_workspace_run_lock() {
        return f().await;
    }
    let lock = match acquire_heavy_lock_with_client(&client, workspace, command, pipeline).await {
        Ok(l) => l,
        Err(msg) => {
            eprintln!("[skippr] ERROR: {msg}");
            std::process::exit(1);
        }
    };
    let heartbeat = spawn_heartbeat(
        lock.client.clone(),
        lock.workspace.clone(),
        lock.run_id.clone(),
        lock.version,
    );
    let result = f().await;
    heartbeat.abort();
    let status = if std::thread::panicking() {
        "failed"
    } else {
        "completed"
    };
    if let Err(e) = lock.complete(status).await {
        eprintln!("[skippr] warning: failed to release run lock: {e}");
    }
    result
}

/// Run `f` while holding the workspace heavy lock; releases on completion.
pub async fn with_heavy_run_lock<T, F, Fut>(
    workspace: &str,
    command: &str,
    pipeline: Option<&str>,
    f: F,
) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    if skip_workspace_run_lock() {
        return f().await;
    }
    let client = crate::authenticated_api_client().await;
    let lock = match acquire_heavy_lock_with_client(&client, workspace, command, pipeline).await {
        Ok(l) => l,
        Err(msg) => {
            eprintln!("[skippr] ERROR: {msg}");
            std::process::exit(1);
        }
    };
    let heartbeat = spawn_heartbeat(
        lock.client.clone(),
        lock.workspace.clone(),
        lock.run_id.clone(),
        lock.version,
    );
    let result = f().await;
    heartbeat.abort();
    let status = if std::thread::panicking() {
        "failed"
    } else {
        "completed"
    };
    if let Err(e) = lock.complete(status).await {
        eprintln!("[skippr] warning: failed to release run lock: {e}");
    }
    result
}

#[allow(dead_code)]
pub async fn is_cancel_requested(workspace: &str, run_id: &str) -> bool {
    let client = crate::authenticated_api_client().await;
    let Ok(resp) = client.get_run_lock(workspace).await else {
        return false;
    };
    resp.lock
        .map(|l| l.run_id == run_id && l.cancel_requested)
        .unwrap_or(false)
}

async fn acquire_heavy_lock_with_client(
    client: &ApiClient,
    workspace: &str,
    command: &str,
    pipeline: Option<&str>,
) -> Result<ActiveLock, String> {
    if let (Ok(run_id), Ok(version)) = (
        std::env::var("SKIPPR_RUN_ID"),
        std::env::var("SKIPPR_RUN_VERSION"),
    ) {
        if !run_id.trim().is_empty() {
            let version: i64 = version
                .trim()
                .parse()
                .map_err(|_| "SKIPPR_RUN_VERSION must be an integer".to_string())?;
            return Ok(ActiveLock {
                client: client.clone(),
                workspace: workspace.to_string(),
                run_id: run_id.trim().to_string(),
                version,
            });
        }
    }

    let resp = client
        .acquire_run_lock(workspace, command, pipeline, None)
        .await
        .map_err(format_lock_error)?;
    Ok(ActiveLock {
        client: client.clone(),
        workspace: workspace.to_string(),
        run_id: resp.run_id,
        version: resp.version,
    })
}

impl ActiveLock {
    async fn complete(self, status: &str) -> Result<(), String> {
        self.client
            .complete_run_lock(&self.workspace, &self.run_id, self.version, status)
            .await
            .map_err(|e| e.to_string())
    }
}

fn spawn_heartbeat(
    client: ApiClient,
    workspace: String,
    run_id: String,
    mut version: i64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.tick().await;
        loop {
            interval.tick().await;
            match client
                .heartbeat_run_lock(&workspace, &run_id, version)
                .await
            {
                Ok(resp) => version = resp.version,
                Err(e) => {
                    eprintln!("[skippr] run lock heartbeat failed: {e}");
                    break;
                }
            }
        }
    })
}

fn format_lock_error(err: ApiError) -> String {
    if err.status == Some(StatusCode::CONFLICT)
        || err.body.contains("activeRunId")
        || err.body.contains("active_run_id")
    {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&err.body) {
            let cmd = v
                .get("activeCommand")
                .or_else(|| v.get("active_command"))
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");
            let pipe = v
                .get("activePipeline")
                .or_else(|| v.get("active_pipeline"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let run = v
                .get("activeRunId")
                .or_else(|| v.get("active_run_id"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            return format!(
                "workspace already running {cmd}{} (run {run}). Cancel or wait for it to finish.",
                if pipe.is_empty() {
                    String::new()
                } else {
                    format!(" on pipeline '{pipe}'")
                }
            );
        }
    }
    format!("{}: {}", err.context, err.body)
}
