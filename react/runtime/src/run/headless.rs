//! Headless runner for `react run`.
//!
//! Design goal: subscribe to the same typed events as WS without using a socket.

use react_core::session::{ThreadEvent, ThreadEventStatus, ThreadItemStatus, ThreadStore};
use react_core::suite::{SuiteCtx, SuiteRegistry};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use crate::run::event_hub::EventHub;
use crate::ws::api_gen::src::models as api;

fn classify_error(summary: &str) -> &'static str {
    let s = summary.to_ascii_lowercase();
    if s.contains("timed out reading response")
        || s.contains("timed out")
        || s.contains("network error")
    {
        "network_timeout"
    } else if s.contains("max_output_tokens") || s.contains("output truncated") {
        "token_truncation"
    } else if s.contains("too many consecutive batch failures") || s.contains("batch_locked") {
        "batch_locked"
    } else if s.contains("invalid staging model sql") {
        "invalid_staging_sql"
    } else if s.contains("nosuchkey") || s.contains("no such key") || s.contains("not found") {
        "missing_artifact"
    } else {
        "unknown"
    }
}

fn summarize_failure_state(
    st: &react_core::session::ThreadState,
    events: &[ThreadEvent],
) -> Option<String> {
    let last_failed_event = events
        .iter()
        .rev()
        .find(|e| e.status == Some(ThreadEventStatus::Failed));
    let mut detail = String::new();
    let mut phase = None::<String>;
    let mut tool = None::<String>;
    if let Some(ev) = last_failed_event {
        phase = ev.phase.clone();
        tool = ev.clean_name.clone().or_else(|| ev.name.clone());
        if let Some(err) = ev
            .error
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            detail = err.to_string();
        }
    }
    if detail.is_empty() {
        if let Some(it) = st
            .items
            .values()
            .find(|it| it.status == ThreadItemStatus::Failed)
        {
            if let Some(err) = it
                .last_error
                .as_ref()
                .map(|e| e.summary.trim())
                .filter(|s| !s.is_empty())
            {
                detail = err.to_string();
            }
        }
    }
    if detail.is_empty() {
        return None;
    }
    let class = classify_error(&detail);
    let mut out = format!("failure summary: class={}", class);
    if let Some(p) = phase.filter(|p| !p.trim().is_empty()) {
        out.push_str(&format!(" phase={}", p));
    }
    if let Some(t) = tool.filter(|t| !t.trim().is_empty()) {
        out.push_str(&format!(" tool={}", t));
    }
    out.push_str(&format!(" detail={}", detail.replace('\n', " | ")));
    Some(out)
}

#[derive(Clone, Debug)]
pub struct RunOpts {
    pub thread_id: Option<String>,
    pub suite_id: String,
    pub agent: String,    // default: agent
    /// Optional channel to publish the thread_id as soon as it is observed.
    pub thread_id_tx: Option<mpsc::UnboundedSender<String>>,
}

/// Run a headless thread execution and return the final exit code.
///
/// Exit codes:
/// - 0: no failed items in materialized thread_state
/// - 1: at least one failed item
/// - 2: no final state could be determined
pub async fn run_headless(ctx: SuiteCtx, opts: RunOpts, registry: SuiteRegistry) -> Result<(i32, String), String> {
    let hub = EventHub::new(4096);
    let mut rx = hub.subscribe();
    let plain_progress = std::env::var("REACT_PLAIN_PROGRESS")
        .ok()
        .filter(|v| !v.trim().is_empty() && v != "0" && v.to_ascii_lowercase() != "false")
        .is_some();

    // Capture the thread id from events (for `new`) and detect completion.
    let mut tid_tx = opts.thread_id_tx.clone();
    let capture = tokio::spawn(async move {
        let mut last_thread_id: Option<String> = None;
        let mut saw_final: bool = false;
        let mut sent_tid: bool = false;
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    match msg {
                        api::ServerMessage::Phase(r) => {
                            if plain_progress {
                                let from = r.from_phase.unwrap_or_else(|| "-".to_string());
                                if from == r.phase {
                                    let reason = r
                                        .reason_code
                                        .map(|x| format!("{:?}", x))
                                        .unwrap_or_else(|| "checkpoint".to_string());
                                    println!(
                                        "phase checkpoint {} [{}] ({})",
                                        r.phase, reason, r.ts
                                    );
                                } else {
                                    println!("phase {} -> {} ({})", from, r.phase, r.ts);
                                }
                            }
                        }
                        api::ServerMessage::ToolStart(r) => {
                            if plain_progress {
                                let label = r.clean_name.unwrap_or(r.name);
                                println!("tool start: {}", label);
                            }
                        }
                        api::ServerMessage::ToolEnd(r) => {
                            if plain_progress {
                                let label = r.clean_name.unwrap_or(r.name);
                                if matches!(r.status, api::ToolEventStatus::Failed) {
                                    let err = r
                                        .error
                                        .as_deref()
                                        .map(|s| s.trim())
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or("tool failed");
                                    // Keep this single-line and CI-friendly.
                                    println!("tool end: {} (Failed) — {}", label, err);
                                } else {
                                    println!("tool end: {} ({:?})", label, r.status);
                                }
                            }
                        }
                        api::ServerMessage::Final(r) => {
                            last_thread_id = Some(r.thread_id.clone());
                            if !sent_tid {
                                if let Some(tx) = tid_tx.take() {
                                    let _ = tx.send(r.thread_id.clone());
                                }
                                sent_tid = true;
                            }
                            if plain_progress {
                                println!("final thread: {}", r.thread_id);
                            }
                            saw_final = true;
                            break;
                        }
                        api::ServerMessage::AwaitUser(r) => {
                            last_thread_id = Some(r.thread_id.clone());
                            if !sent_tid {
                                if let Some(tx) = tid_tx.take() {
                                    let _ = tx.send(r.thread_id.clone());
                                }
                                sent_tid = true;
                            }
                        }
                        api::ServerMessage::AwaitApproval(r) => {
                            last_thread_id = Some(r.thread_id.clone());
                            if !sent_tid {
                                if let Some(tx) = tid_tx.take() {
                                    let _ = tx.send(r.thread_id.clone());
                                }
                                sent_tid = true;
                            }
                        }
                        api::ServerMessage::ThreadState(r) => {
                            last_thread_id = Some(r.thread_id.clone());
                            if !sent_tid {
                                if let Some(tx) = tid_tx.take() {
                                    let _ = tx.send(r.thread_id.clone());
                                }
                                sent_tid = true;
                            }
                        }
                        _ => {}
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
        (last_thread_id, saw_final)
    });

    let requested_tid = opts.thread_id.clone();
    let tid = crate::ws::server::run_headless_with_hub(
        ctx.clone(),
        opts.thread_id,
        opts.suite_id,
        opts.agent,
        hub.clone(),
        registry,
    )
    .await?;

    // Closing the hub allows the capture task to finish even if final wasn't observed.
    drop(hub);
    let (captured_tid, saw_final) = capture
        .await
        .map_err(|e| format!("internal: capture task failed: {e}"))?;

    let thread_id = requested_tid
        .or_else(|| captured_tid)
        .unwrap_or_else(|| tid.clone());

    // Determine exit code from durable, materialized state.
    let store = ThreadStore::new(ctx.storage.clone(), ctx.scope.clone(), ctx.keyspace.clone());
    let st = store.get_thread_state(&thread_id).await.ok();
    if st.is_none() && !saw_final {
        return Ok((2, thread_id));
    }
    if let Some(st) = st {
        let any_failed = st
            .items
            .values()
            .any(|it| it.status == ThreadItemStatus::Failed);
        if any_failed && plain_progress {
            let timeline_events = store
                .get_thread_timeline(&thread_id)
                .await
                .map(|t| t.events)
                .unwrap_or_default();
            if let Some(line) = summarize_failure_state(&st, &timeline_events) {
                println!("{}", line);
            }
        }
        let code = if any_failed { 1 } else { 0 };
        return Ok((code, thread_id));
    }
    Ok((0, thread_id))
}

pub fn _sink_terminal(_hub: &EventHub) -> broadcast::Receiver<api::ServerMessage> {
    _hub.subscribe()
}
