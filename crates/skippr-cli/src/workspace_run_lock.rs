//! Workspace-wide exclusive lock for discover / sync / model via circles-auth API.

use crate::api_client::{ApiClient, ApiError};
use reqwest::StatusCode;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
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

struct ActiveLock {
    client: ApiClient,
    workspace: String,
    run_id: String,
    /// Shared with the heartbeat task so complete uses the latest server version.
    version: Arc<Mutex<i64>>,
}

/// Ensures the workspace heavy lock is released exactly once (normal return, panic, or signal).
struct RunLockGuard {
    lock: Option<ActiveLock>,
    heartbeat: Option<JoinHandle<()>>,
    released: Arc<AtomicBool>,
    shutdown_listener: Option<JoinHandle<()>>,
}

impl RunLockGuard {
    fn new(lock: ActiveLock, heartbeat: JoinHandle<()>) -> Self {
        let released = Arc::new(AtomicBool::new(false));
        let shutdown_listener =
            spawn_shutdown_listener(Arc::clone(&released), lock.clone_for_signal());
        Self {
            lock: Some(lock),
            heartbeat: Some(heartbeat),
            released,
            shutdown_listener,
        }
    }

    async fn release(&mut self, status: &str) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(listener) = self.shutdown_listener.take() {
            listener.abort();
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        if let Some(lock) = self.lock.take() {
            if let Err(e) = lock.complete(status).await {
                eprintln!("[skippr] warning: failed to release run lock: {e}");
            }
        }
    }
}

impl Drop for RunLockGuard {
    fn drop(&mut self) {
        if self.released.load(Ordering::SeqCst) {
            return;
        }
        if let Some(listener) = self.shutdown_listener.take() {
            listener.abort();
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        let Some(lock) = self.lock.take() else {
            return;
        };
        let status = if std::thread::panicking() {
            "failed"
        } else {
            "failed"
        };
        if let Err(e) = block_on_complete(lock, status) {
            eprintln!("[skippr] warning: failed to release run lock on drop: {e}");
        }
    }
}

impl ActiveLock {
    fn clone_for_signal(&self) -> ActiveLock {
        ActiveLock {
            client: self.client.clone(),
            workspace: self.workspace.clone(),
            run_id: self.run_id.clone(),
            version: Arc::clone(&self.version),
        }
    }

    async fn complete(self, status: &str) -> Result<(), String> {
        let version = *self.version.lock().await;
        self.client
            .complete_run_lock(&self.workspace, &self.run_id, version, status)
            .await
            .map_err(|e| e.to_string())
    }
}

fn block_on_complete(lock: ActiveLock, status: &str) -> Result<(), String> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return handle.block_on(lock.complete(status));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(lock.complete(status))
}

fn spawn_shutdown_listener(released: Arc<AtomicBool>, lock: ActiveLock) -> Option<JoinHandle<()>> {
    #[cfg(unix)]
    {
        Some(tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut int = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(_) => return,
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            if released.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Err(e) = lock.complete("failed").await {
                eprintln!("[skippr] warning: failed to release run lock on shutdown signal: {e}");
            }
            std::process::exit(137);
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = (released, lock);
        None
    }
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
        Arc::clone(&lock.version),
    );
    let mut guard = RunLockGuard::new(lock, heartbeat);
    let result = f().await;
    let status = if std::thread::panicking() {
        "failed"
    } else {
        "completed"
    };
    guard.release(status).await;
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
    let client = crate::authenticated_api_client().await;
    with_heavy_run_lock_client(client, workspace, command, pipeline, f).await
}

/// Best-effort release after an abnormal process exit (e.g. chaos SIGKILL before signal handler).
/// Used by CI harnesses when retrying after an allowed non-zero exit.
pub async fn release_workspace_heavy_lock_best_effort(workspace: &str) {
    let client = crate::authenticated_api_client().await;
    let Ok(resp) = client.get_run_lock(workspace).await else {
        return;
    };
    let Some(lock) = resp.lock else {
        return;
    };
    if lock.status != "running" {
        return;
    }
    if let Err(e) = client
        .complete_run_lock(workspace, &lock.run_id, lock.version, "failed")
        .await
    {
        eprintln!("[skippr] warning: could not release stale workspace run lock: {e}");
        return;
    }
    eprintln!(
        "[skippr] released stale workspace run lock for '{}' (run {})",
        workspace, lock.run_id
    );
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
                version: Arc::new(Mutex::new(version)),
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
        version: Arc::new(Mutex::new(resp.version)),
    })
}

fn spawn_heartbeat(
    client: ApiClient,
    workspace: String,
    run_id: String,
    version: Arc<Mutex<i64>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.tick().await;
        loop {
            interval.tick().await;
            let current = *version.lock().await;
            match client
                .heartbeat_run_lock(&workspace, &run_id, current)
                .await
            {
                Ok(resp) => *version.lock().await = resp.version,
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
