//! Headless runner for `react run`.
//!
//! Design goal: subscribe to the same typed events as WS without using a socket.

use react_core::session::ThreadStore;
use react_suites::SuiteCtx;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use crate::run::event_hub::EventHub;
use crate::ws::api_gen::src::models as api;

#[derive(Clone, Debug)]
pub struct RunOpts {
    pub thread_id: Option<String>,
    pub suite_id: String, // default: data_engineer
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
pub async fn run_headless(ctx: SuiteCtx, opts: RunOpts) -> Result<(i32, String), String> {
    let hub = EventHub::new(4096);
    let mut rx = hub.subscribe();

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
                        api::ServerMessage::Final(r) => {
                            last_thread_id = Some(r.thread_id.clone());
                            if !sent_tid {
                                if let Some(tx) = tid_tx.take() {
                                    let _ = tx.send(r.thread_id.clone());
                                }
                                sent_tid = true;
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
        let any_failed = st.items.values().any(|it| it.status == "failed");
        let code = if any_failed { 1 } else { 0 };
        return Ok((code, thread_id));
    }
    Ok((0, thread_id))
}

pub fn _sink_terminal(_hub: &EventHub) -> broadcast::Receiver<api::ServerMessage> {
    _hub.subscribe()
}

