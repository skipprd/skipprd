use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
// Agent loop is invoked through suites; WS server doesn't call Agent directly.
// Removed unused tool imports; flows handle registry/tool selection
use crate::models as m;
use crate::run::event_hub::EventHub;
use crate::ws::api_gen::src::models as api;
use crate::ws::terminal::{self, TerminalEvent, TerminalSink};
use chrono::Utc;
use react_core::session::{
    Observation, ThreadItemError as CoreThreadItemError, ThreadItemState as CoreThreadItemState,
    ThreadLog, ThreadState as CoreThreadState, ThreadStep, ThreadStore, ToolObservation,
};
use react_suites::data_engineer::plan as de_plan;
use react_suites::registry::SuiteRegistry;
use react_suites::SuiteCtx;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use uuid::Uuid;

// Steering prompts removed for model agent; model runs eagerly without awaiting user choice.

fn final_display_text_from_payload(payload: &Value) -> String {
    if let Some(s) = payload.get("answer").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(s) = payload.get("text").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Ok(s) = serde_json::to_string(payload) {
        return s;
    }
    String::new()
}

fn ws_final_result_from_typed_final(
    kind: &str,
    payload: &Value,
    display: &Option<String>,
) -> api::FinalResult {
    match kind {
        "ask" => {
            let answer = payload
                .get("answer")
                .and_then(|x| x.as_str())
                .or_else(|| display.as_deref())
                .unwrap_or("")
                .to_string();
            let sql = payload
                .get("sql")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if answer.trim().is_empty() || sql.trim().is_empty() {
                // Ask payload must include answer+sql; degrade to generic.
                return ws_final_result_from_typed_final("generic", payload, display);
            }
            let mut p = api::AskFinalPayload::new(answer, sql);
            if let Some(v) = payload.get("data").cloned() {
                if let Ok(d) = serde_json::from_value::<api::AskFinalPayloadData>(v) {
                    p.data = Some(d);
                }
            }
            if let Some(v) = payload.get("chart").cloned() {
                if let Ok(c) = serde_json::from_value::<api::AskFinalPayloadChart>(v) {
                    p.chart = Some(c);
                }
            }
            api::FinalResult::Ask(api::AskFinalResult::new(
                api::ask_final_result::Kind::Ask,
                p,
            ))
        }
        "kb" => {
            let answer = payload
                .get("answer")
                .and_then(|x| x.as_str())
                .or_else(|| display.as_deref())
                .unwrap_or("")
                .to_string();
            if answer.trim().is_empty() {
                return ws_final_result_from_typed_final("generic", payload, display);
            }
            let p = api::KbFinalPayload::new(answer);
            api::FinalResult::Kb(api::KbFinalResult::new(api::kb_final_result::Kind::Kb, p))
        }
        _ => {
            // WS schema only knows ask/kb/generic for now; map all other kinds to generic.
            let txt = payload
                .get("text")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .or_else(|| display.clone())
                .unwrap_or_else(|| final_display_text_from_payload(payload));
            let p = api::GenericFinalPayload::new(txt);
            api::FinalResult::Generic(api::GenericFinalResult::new(
                api::generic_final_result::Kind::Generic,
                p,
            ))
        }
    }
}

fn ws_thread_state_snapshot_from_core(
    st: &CoreThreadState,
    reg: &SuiteRegistry,
) -> api::ThreadStateSnapshot {
    fn elapsed_ms_since(start_ts: &str) -> Option<i64> {
        let start = chrono::DateTime::parse_from_rfc3339(start_ts).ok()?;
        let now = chrono::Utc::now();
        let delta = now.signed_duration_since(start.with_timezone(&chrono::Utc));
        let ms = delta.num_milliseconds();
        Some(ms.max(0))
    }

    let mut items: Vec<api::ThreadStateItem> = Vec::new();
    for (item_id, it) in st.items.iter() {
        let mut wi = api::ThreadStateItem::new(item_id.clone(), it.kind.clone(), it.status.clone());
        wi.started_at = it.started_at.clone();
        wi.finished_at = it.finished_at.clone();
        wi.runtime_ms = it.runtime_ms.map(|n| n as i64);
        // Render-time runtime for in-flight items: now - started_at.
        if wi.runtime_ms.is_none() {
            if let (Some(ref started), None) = (&wi.started_at, &wi.finished_at) {
                wi.runtime_ms = elapsed_ms_since(started);
            }
        }
        wi.outputs = it.outputs.as_ref().and_then(|v| v.as_object()).map(|obj| {
            let mut out: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            for (k, vv) in obj.iter() {
                out.insert(k.clone(), vv.clone());
            }
            out
        });
        if let Some(ref e) = it.last_error {
            let mut we = api::ThreadStateItemError::new(e.summary.clone());
            we.tool_step_idx = e.tool_step_idx.map(|n| n as i32);
            we.step_ts = e.step_ts.clone();
            wi.last_error = Some(we);
        }
        items.push(wi);
    }
    items.sort_by(|a, b| a.item_id.cmp(&b.item_id));

    // Render-time total runtime: persisted completed-phase total + current phase elapsed (if any).
    let mut total_runtime_ms: i64 = st.total_runtime_ms as i64;
    if let Some(ref cur) = st.current_phase {
        let cur_id = format!("phase:{}", cur);
        if let Some(cur_item) = st.items.get(&cur_id) {
            if cur_item.finished_at.is_none() {
                if let Some(ref started) = cur_item.started_at {
                    total_runtime_ms += elapsed_ms_since(started).unwrap_or(0);
                }
            }
        }
    }

    // Suite-owned phase ordering + completed phases derived from current phase.
    let suite_id = st.suite_id.as_deref().unwrap_or("");
    let agent_type = st.agent_type.as_deref().unwrap_or("ask");
    let phases: Vec<String> = reg
        .get(suite_id)
        .map(|s| s.phase_order(agent_type))
        .unwrap_or_default();
    let current_phase = st
        .current_phase
        .clone()
        .unwrap_or_else(|| "preflight".to_string());
    let completed_phases = derive_completed_phases(&phases, &current_phase);

    // Durable, bounded timeline events (post reconnect tool timeline).
    fn map_ctx(c: &react_core::session::ExecutionContext) -> api::ExecutionContext {
        let mut out = api::ExecutionContext::new();
        out.plan_kind = c.plan_kind.clone();
        out.plan_key = c.plan_key.clone();
        out.workgroup_id = c.workgroup_id.clone();
        out.task_id = c.task_id.clone();
        out.checklist_item_id = c.checklist_item_id.clone();
        out
    }
    let events: Vec<api::ThreadEvent> = st
        .events
        .iter()
        .map(|ev| {
            let kind = match ev.event_kind.as_str() {
                "tool_start" => api::ThreadEventKind::ToolStart,
                "tool_end" => api::ThreadEventKind::ToolEnd,
                "llm_start" => api::ThreadEventKind::LlmStart,
                "llm_end" => api::ThreadEventKind::LlmEnd,
                _ => api::ThreadEventKind::ToolEnd,
            };
            let mut out = api::ThreadEvent::new(ev.step_idx as i32, kind, ev.ts.clone());
            out.tool_id = ev.tool_id.clone();
            out.name = ev.name.clone();
            out.clean_name = ev.clean_name.clone();
            out.runtime_ms = ev.runtime_ms.map(|n| n as i64);
            out.error = ev.error.clone();
            out.call_id = ev.call_id.map(|n| n as i32);
            out.model = ev.model.clone();
            out.phase = ev.phase.clone();
            out.status = ev.status.as_deref().map(|s| match s {
                "running" => api::ToolEventStatus::Running,
                "ok" => api::ToolEventStatus::Ok,
                "failed" => api::ToolEventStatus::Failed,
                _ => api::ToolEventStatus::Running,
            });
            out.payload = ev.payload.as_ref().and_then(|v| v.as_object()).map(|obj| {
                let mut hm: std::collections::HashMap<String, serde_json::Value> =
                    std::collections::HashMap::new();
                for (k, vv) in obj.iter() {
                    hm.insert(k.clone(), vv.clone());
                }
                hm
            });
            out.ctx = ev.ctx.as_ref().map(map_ctx);
            out
        })
        .collect();

    let mut snap = api::ThreadStateSnapshot::new(
        st.thread_state_schema_version as i32,
        st.thread_id.clone(),
        st.last_materialized_step_count as i32,
        total_runtime_ms.max(0),
        phases,
        completed_phases,
        events,
        items,
    );
    snap.suite_id = st.suite_id.clone();
    snap.agent_type = st.agent_type.clone();
    snap.current_phase = st.current_phase.clone();
    if !st.plan_summaries.is_empty() {
        let mut hm: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        for (k, v) in st.plan_summaries.iter() {
            hm.insert(k.clone(), v.clone());
        }
        snap.plan_summaries = Some(hm);
    }
    snap
}

async fn upsert_thread_state_from_plans(
    store: &ThreadStore,
    thread_id: &str,
    cleanse: &Option<api::PlanSnapshot>,
    model: &Option<api::PlanSnapshot>,
) {
    let mut st = match store.get_thread_state(thread_id).await {
        Ok(s) => s,
        Err(_) => return,
    };

    fn summarize_plan(p: &api::PlanSnapshot) -> serde_json::Value {
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for t in p.tasks.iter() {
            let st = match t {
                api::PlanTask::Cleanse(c) => c.status,
                api::PlanTask::Model(m) => m.status,
            };
            let k = format!("{:?}", st).to_lowercase();
            *counts.entry(k).or_insert(0) += 1;
        }
        serde_json::json!({
            "planKey": p.plan_key,
            "status": format!("{:?}", p.status).to_lowercase(),
            "taskCounts": counts,
        })
    }

    fn map_task_status_to_item_status(s: api::PlanTaskStatus) -> String {
        match s {
            api::PlanTaskStatus::Pending => "queued",
            api::PlanTaskStatus::InProgress => "running",
            api::PlanTaskStatus::Done => "ok",
            api::PlanTaskStatus::Blocked => "blocked",
            api::PlanTaskStatus::NeedsUpdate => "blocked",
        }
        .to_string()
    }

    fn task_error_from_checklist(items: &[api::PlanChecklistItem]) -> Option<String> {
        // Prefer the most actionable remaining work: needs_update > blocked.
        let mut pick: Option<&api::PlanChecklistItem> = None;
        for it in items.iter() {
            match it.status {
                api::PlanChecklistItemStatus::NeedsUpdate => {
                    pick = Some(it);
                    break;
                }
                api::PlanChecklistItemStatus::Blocked => {
                    if pick.is_none() {
                        pick = Some(it);
                    }
                }
                _ => {}
            }
        }
        let Some(it) = pick else { return None };
        let details = it.details.as_deref().unwrap_or("").trim();
        if details.is_empty() {
            Some(it.label.clone())
        } else {
            Some(format!("{}: {}", it.label, details))
        }
    }

    for (kind, plan_opt) in [("cleanse", cleanse), ("model", model)] {
        if let Some(p) = plan_opt {
            st.plan_summaries
                .insert(kind.to_string(), summarize_plan(p));

            for t in p.tasks.iter() {
                let (task_id, status, checklist, outputs) = match t {
                    api::PlanTask::Cleanse(c) => (
                        c.task_id.clone(),
                        c.status,
                        c.checklist.clone(),
                        serde_json::json!({
                            "expected_model_path": c.expected_model_path,
                        }),
                    ),
                    api::PlanTask::Model(mo) => (
                        mo.task_id.clone(),
                        mo.status,
                        mo.checklist.clone(),
                        serde_json::json!({
                            "expected_model_path": mo.expected_model_path,
                            "folder": mo.folder,
                            "inputs": mo.inputs,
                        }),
                    ),
                };
                let item_id = format!("task:data_engineer:{}:{}", p.plan_key, task_id);
                let ent = st
                    .items
                    .entry(item_id)
                    .or_insert_with(|| CoreThreadItemState {
                        kind: "task".to_string(),
                        status: "queued".to_string(),
                        started_at: None,
                        finished_at: None,
                        runtime_ms: None,
                        last_error: None,
                        outputs: None,
                    });
                ent.kind = "task".to_string();
                ent.status = map_task_status_to_item_status(status);
                ent.outputs = Some(outputs);
                if let Some(err) = task_error_from_checklist(&checklist) {
                    ent.last_error = Some(CoreThreadItemError {
                        summary: err,
                        tool_step_idx: None,
                        step_ts: None,
                    });
                }
            }
        }
    }

    let _ = store.put_thread_state(thread_id, &st).await;
}

fn env_bool(key: &str, default: bool) -> bool {
    let dv = if default { "1" } else { "0" };
    match std::env::var(key).unwrap_or_else(|_| dv.to_string()).trim() {
        "1" | "true" | "TRUE" | "yes" | "YES" => true,
        "0" | "false" | "FALSE" | "no" | "NO" => false,
        _ => default,
    }
}

fn ws_log_in(txt: &str) {
    // Full WS payloads can be enormous (prompts, plan snapshots, etc). Default to compact logs.
    if env_bool("WS_LOG_BODIES", false) {
        // Only log full bodies when TRACE is enabled.
        tracing::trace!("WS <- {}", txt);
        return;
    }
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    if let Ok(v) = serde_json::from_str::<Value>(txt) {
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("unknown");
        let cid = v.get("cid").and_then(|x| x.as_str());
        let thread_id = v.get("thread_id").and_then(|x| x.as_str());
        tracing::debug!(
            "WS <- type={} cid={} thread_id={}",
            typ,
            cid.unwrap_or("-"),
            thread_id.unwrap_or("-")
        );
    } else {
        tracing::debug!("WS <- (non-json) bytes={}", txt.len());
    }
}

fn ws_log_out(txt: &str) {
    // Full WS payloads can be enormous (thread_state, plans). Default to compact logs.
    if env_bool("WS_LOG_BODIES", false) {
        // Only log full bodies when TRACE is enabled.
        tracing::trace!("WS -> {}", txt);
        return;
    }
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    if let Ok(v) = serde_json::from_str::<Value>(txt) {
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("unknown");
        let seq = v.get("seq").and_then(|x| x.as_i64());
        let cid = v.get("cid").and_then(|x| x.as_str());
        let for_cid = v.get("for_cid").and_then(|x| x.as_str());
        let thread_id = v.get("thread_id").and_then(|x| x.as_str());
        // Extremely frequent in headless mode; keep it at DEBUG.
        tracing::debug!(
            "WS -> type={} seq={} cid={} for_cid={} thread_id={}",
            typ,
            seq.map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string()),
            cid.unwrap_or("-"),
            for_cid.unwrap_or("-"),
            thread_id.unwrap_or("-")
        );
    } else {
        tracing::debug!("WS -> (non-json) bytes={}", txt.len());
    }
}

/// Start the WebSocket server with an injected suite registry and context.
///
/// This is the preferred entrypoint for keeping `react` runtime generic: callers
/// decide how to build configuration, storage roots, credentials, etc.
pub async fn start_with_ctx(port: u16, suite_ctx: SuiteCtx) -> Result<(), String> {
    let reg = Arc::new(react_suites::default_registry());
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).await.map_err(|e| e.to_string())?;
    tracing::info!("WebSocket server listening on ws://{}", addr);
    if let Some(t) = terminal::sink() {
        t.emit(TerminalEvent::Info(format!(
            "WS server listening on ws://{}",
            addr
        )));
    }
    loop {
        let (stream, _sockaddr) = listener.accept().await.map_err(|e| e.to_string())?;
        let reg = reg.clone();
        let suite_ctx = suite_ctx.clone();
        tokio::spawn(async move {
            if let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await {
                let (mut write, mut read) = ws_stream.split();
                let mut state = ConnState::new(reg, suite_ctx, None);
                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(Message::Text(txt)) => {
                            ws_log_in(&txt);
                            // Fast-path new/open to stream initial frames immediately
                            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                if let Some(t) = v.get("type").and_then(|x| x.as_str()) {
                                    if t == "new" {
                                        if let Err(e) =
                                            process_new(&v, &mut state, &mut write).await
                                        {
                                            let cid_guess = v
                                                .get("cid")
                                                .and_then(|x| x.as_str())
                                                .map(|s| s.to_string());
                                            let mut err = api::ErrorResponse::new(
                                                1,
                                                m::error_response::Type::Error,
                                                now_iso(),
                                                e.clone(),
                                            );
                                            err.code = Some("invalid_request".to_string());
                                            err.cid = cid_guess;
                                            let s = serde_json::to_string(&err).unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
                                            ws_log_out(&s);
                                            let _ = write.send(Message::Text(s)).await;
                                        }
                                        continue;
                                    } else if t == "open" {
                                        if let Err(e) =
                                            process_open(&v, &mut state, &mut write).await
                                        {
                                            let cid_guess = v
                                                .get("cid")
                                                .and_then(|x| x.as_str())
                                                .map(|s| s.to_string());
                                            let mut err = api::ErrorResponse::new(
                                                1,
                                                m::error_response::Type::Error,
                                                now_iso(),
                                                e.clone(),
                                            );
                                            err.code = Some("invalid_request".to_string());
                                            err.cid = cid_guess;
                                            let s = serde_json::to_string(&err).unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
                                            ws_log_out(&s);
                                            let _ = write.send(Message::Text(s)).await;
                                        }
                                        continue;
                                    } else if t == "user" {
                                        if let Err(e) =
                                            process_user(&v, &mut state, &mut write).await
                                        {
                                            let cid_guess = v
                                                .get("cid")
                                                .and_then(|x| x.as_str())
                                                .map(|s| s.to_string());
                                            let mut err = api::ErrorResponse::new(
                                                1,
                                                m::error_response::Type::Error,
                                                now_iso(),
                                                e.clone(),
                                            );
                                            err.code = Some("invalid_request".to_string());
                                            err.cid = cid_guess;
                                            let s = serde_json::to_string(&err).unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
                                            ws_log_out(&s);
                                            let _ = write.send(Message::Text(s)).await;
                                        }
                                        continue;
                                    } else if t == "approve" {
                                        if let Err(e) =
                                            process_approve(&v, &mut state, &mut write).await
                                        {
                                            let cid_guess = v
                                                .get("cid")
                                                .and_then(|x| x.as_str())
                                                .map(|s| s.to_string());
                                            let mut err = api::ErrorResponse::new(
                                                1,
                                                m::error_response::Type::Error,
                                                now_iso(),
                                                e.clone(),
                                            );
                                            err.code = Some("invalid_request".to_string());
                                            err.cid = cid_guess;
                                            let s = serde_json::to_string(&err).unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
                                            ws_log_out(&s);
                                            let _ = write.send(Message::Text(s)).await;
                                        }
                                        continue;
                                    } else if t == "reject" {
                                        if let Err(e) =
                                            process_reject(&v, &mut state, &mut write).await
                                        {
                                            let cid_guess = v
                                                .get("cid")
                                                .and_then(|x| x.as_str())
                                                .map(|s| s.to_string());
                                            let mut err = api::ErrorResponse::new(
                                                1,
                                                m::error_response::Type::Error,
                                                now_iso(),
                                                e.clone(),
                                            );
                                            err.code = Some("invalid_request".to_string());
                                            err.cid = cid_guess;
                                            let s = serde_json::to_string(&err).unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
                                            ws_log_out(&s);
                                            let _ = write.send(Message::Text(s)).await;
                                        }
                                        continue;
                                    }
                                }
                            }
                            // Other types: handle and send after processing
                            match handle_message(&txt, &mut state).await {
                                Ok(frames) => {
                                    for f in frames {
                                        if let Some(t) = state.term() {
                                            t.emit(TerminalEvent::RawJson(f.clone()));
                                        }
                                        ws_log_out(&f);
                                        let _ = write.send(Message::Text(f)).await;
                                    }
                                }
                                Err(e) => {
                                    let cid_guess =
                                        serde_json::from_str::<Value>(&txt).ok().and_then(|vv| {
                                            vv.get("cid")
                                                .and_then(|x| x.as_str())
                                                .map(|s| s.to_string())
                                        });
                                    let mut err = api::ErrorResponse::new(
                                        1,
                                        m::error_response::Type::Error,
                                        now_iso(),
                                        e.clone(),
                                    );
                                    err.code = Some("invalid_request".to_string());
                                    err.cid = cid_guess;
                                    let s = serde_json::to_string(&err)
										.unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
                                    ws_log_out(&s);
                                    let _ = write.send(Message::Text(s)).await;
                                }
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        _ => {}
                    }
                }
            }
        });
    }
}

async fn handle_message(text: &str, state: &mut ConnState) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let typ = v
        .get("type")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "missing field `type`".to_string())?;
    let mut out: Vec<String> = Vec::new();
    match typ {
        "list" => {
            let _req: api::ListRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let store = state.thread_store();
            let ids = store.list().await;
            let mut threads: Vec<api::ListResponseThreadsInner> = Vec::new();
            for tid in ids {
                let mut item = api::ListResponseThreadsInner::new(tid.clone());
                if let Ok(log) = store.get(&tid).await {
                    let (suite_id, agent_type) = derive_thread_context(&log);
                    // last_activity
                    item.last_activity = log.steps.last().map(|s| s.ts().to_string());
                    item.title = log.title.clone();
                    item.suite_id = Some(suite_id);
                    item.agent_type = Some(agent_type);
                    // compute preview from last user or final
                    let mut preview: Option<String> = None;
                    for step in log.steps.iter().rev() {
                        match step {
                            ThreadStep::User { text, .. } => {
                                preview = Some(text.to_string());
                                break;
                            }
                            ThreadStep::Final {
                                payload, display, ..
                            } => {
                                preview =
                                    Some(display.clone().unwrap_or_else(|| {
                                        final_display_text_from_payload(payload)
                                    }));
                                break;
                            }
                            _ => {}
                        }
                    }
                    item.last_message_preview = preview;
                    // unread count based on seen map vs assistant messages count
                    let seen = state.seen.get(&tid).copied().unwrap_or(0);
                    let (assistant_seq_max, assistant_after_seen) =
                        compute_unread_for_log(&log, seen);
                    let unread = assistant_after_seen;
                    item.unread_count = Some(unread);
                    // ensure thread_seq map is at least assistant_seq_max
                    let entry = state.thread_seq.entry(tid.clone()).or_insert(0);
                    if *entry < assistant_seq_max {
                        *entry = assistant_seq_max;
                    }
                }
                threads.push(item);
            }
            let resp = api::ListResponse::new(
                1,
                m::list_response::Type::List,
                now_iso(),
                state.next_seq(),
                threads,
            );
            out.push(serde_json::to_string(&resp).unwrap_or_else(|_| {
                "{\"type\":\"error\",\"error\":\"serialization error\"}".to_string()
            }));
            state.buffer_last(&out[out.len() - 1]);
        }
        "suites" => {
            let _req: api::SuitesRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let suites = build_suites_catalog(&state.reg);
            let mut resp = api::SuitesResponse::new(
                1,
                m::suites_response::Type::Suites,
                now_iso(),
                state.next_seq(),
                suites,
            );
            resp.default_suite_id = Some("data_engineer".to_string());
            let s = serde_json::to_string(&resp).unwrap();
            state.buffer_last(&s);
            out.push(s);
        }
        "new" => {
            let req: api::NewRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let cid = req.cid.clone();
            let question = req.question.clone().unwrap_or_default();
            let thread_id = Uuid::new_v4().to_string();
            let suite_id = req.suite_id.clone();
            let agent = normalize_agent_new(req.agent_type)?;
            state
                .current_suite
                .insert(thread_id.clone(), suite_id.clone());
            state.current_agent.insert(thread_id.clone(), agent.clone());
            // Persist initial suite/agent selection so it survives reconnects
            {
                let store = state.thread_store();
                let _ = store
                    .append_step(
                        &thread_id,
                        ThreadStep::SwitchSuite {
                            from: None,
                            to: suite_id.clone(),
                            observation: Observation::ok(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: agent.clone(),
                        },
                    )
                    .await;
                let _ = store
                    .append_step(
                        &thread_id,
                        ThreadStep::SwitchAgent {
                            from: None,
                            to: agent.clone(),
                            observation: Observation::ok(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: agent.clone(),
                        },
                    )
                    .await;
            }
            // Emit a real preflight phase step so UIs get an event during "preflight" (not just a default label).
            {
                let store = state.thread_store();
                if let Ok(Some((step_idx, ts))) =
                    ensure_preflight_phase_step(&store, &thread_id, &agent, Some(&suite_id)).await
                {
                    let runs = vec![api::PhaseRun::new(ts.clone())];
                    let mut ev = api::PhaseResponse::new(
                        1,
                        api::phase_response::Type::Phase,
                        now_iso(),
                        state.next_seq(),
                        thread_id.clone(),
                        step_idx as i32,
                        "preflight".to_string(),
                        ts,
                        runs,
                        0,
                    );
                    ev.for_cid = Some(cid.clone());
                    ev.reason_code = Some("preflight_start".to_string());
                    let s = serde_json::to_string(&api::ServerMessage::Phase(ev)).unwrap();
                    state.buffer_last(&s);
                    out.push(s);
                }
            }
            // ok
            let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
            ok.cid = Some(cid.clone());
            out.push(serde_json::to_string(&api::ServerMessage::Ok(ok)).unwrap());
            // thread_assigned
            let ta = api::ServerMessage::ThreadAssigned(api::ThreadAssignedResponse::new(
                1,
                m::thread_assigned_response::Type::ThreadAssigned,
                now_iso(),
                state.next_seq(),
                cid.clone(),
                thread_id.clone(),
            ));
            let ta_s = serde_json::to_string(&ta).unwrap();
            state.buffer_last(&ta_s);
            out.push(ta_s);
            // If a question was provided, record it and run the suite. Otherwise, this is "create thread".
            if !question.trim().is_empty() {
                {
                    let store = state.thread_store();
                    let _ = store
                        .append_step(
                            &thread_id,
                            ThreadStep::User {
                                text: question.clone(),
                                observation: Observation::ok(),
                                ts: chrono::Utc::now().to_rfc3339(),
                                agent: agent.clone(),
                            },
                        )
                        .await;
                    let _ = store
                        .set_title_if_absent(&thread_id, &truncate_title(&question, 64))
                        .await;
                }
                // run agent
                let frames = run_agent_and_frames(
                    &thread_id,
                    &question,
                    &suite_id,
                    &agent,
                    &state.reg,
                    &state.suite_ctx,
                )
                .await?;
                for f in frames {
                    match f {
                        AgentFrame::Review { text, meta } => {
                            let tseq = state.next_thread_seq(&thread_id);
                            let mut rr = api::ReviewResponse::new(
                                1,
                                m::review_response::Type::Review,
                                now_iso(),
                                state.next_seq(),
                                thread_id.clone(),
                                tseq,
                                text.clone(),
                            );
                            if let Some(v) = meta {
                                if let Some(obj) = v.as_object() {
                                    let mut hm: std::collections::HashMap<
                                        String,
                                        serde_json::Value,
                                    > = std::collections::HashMap::new();
                                    for (k, vv) in obj.iter() {
                                        hm.insert(k.clone(), vv.clone());
                                    }
                                    if !hm.is_empty() {
                                        rr.meta = Some(hm);
                                    }
                                }
                            }
                            let resp = api::ServerMessage::Review(rr.clone());
                            let s = serde_json::to_string(&resp).unwrap();
                            state.buffer_last(&s);
                            out.push(s);
                            {
                                let store = state.thread_store();
                                let meta_val: Option<Value> =
                                    rr.meta.as_ref().and_then(|m| serde_json::to_value(m).ok());
                                let _ = store
                                    .append_step(
                                        &thread_id,
                                        ThreadStep::ReviewResponse {
                                            text: text.clone(),
                                            meta: meta_val,
                                            observation: Observation::ok(),
                                            ts: chrono::Utc::now().to_rfc3339(),
                                            agent: agent.clone(),
                                        },
                                    )
                                    .await;
                            }
                        }
                        AgentFrame::Final {
                            kind,
                            payload,
                            display,
                        } => {
                            let display_text = display
                                .clone()
                                .unwrap_or_else(|| final_display_text_from_payload(&payload));
                            // finalize title once using concise summary
                            {
                                let store = state.thread_store();
                                let title = synthesize_title(
                                    &state.suite_ctx.llm,
                                    &question,
                                    &display_text,
                                )
                                .await;
                                let _ = store.finalize_title(&thread_id, &title).await;
                            }
                            // Persist final so reconnect/history can observe completion.
                            {
                                let store = state.thread_store();
                                append_step_if_new(
                                    &store,
                                    &thread_id,
                                    ThreadStep::Final {
                                        kind: kind.clone(),
                                        payload: payload.clone(),
                                        display: display.clone(),
                                        observation: Observation::ok(),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: agent.clone(),
                                    },
                                )
                                .await;
                            }
                            let tseq = state.next_thread_seq(&thread_id);
                            let final_result =
                                ws_final_result_from_typed_final(&kind, &payload, &display);
                            let resp = api::ServerMessage::Final(api::FinalResponse::new(
                                1,
                                m::final_response::Type::Final,
                                now_iso(),
                                state.next_seq(),
                                thread_id.clone(),
                                tseq,
                                final_result,
                            ));
                            let s = serde_json::to_string(&resp).unwrap();
                            state.buffer_last(&s);
                            out.push(s);
                            // Log entire thread on final
                            {
                                let store = state.thread_store();
                                if let Ok(log) = store.get(&thread_id).await {
                                    if let Ok(pretty) = serde_json::to_string_pretty(&log) {
                                        tracing::info!("{}", pretty);
                                    }
                                }
                            }
                        }
                        AgentFrame::AwaitUser { prompt } => {
                            let tseq = state.next_thread_seq(&thread_id);
                            let resp = api::ServerMessage::AwaitUser(api::AwaitUserResponse::new(
                                1,
                                m::await_user_response::Type::AwaitUser,
                                now_iso(),
                                state.next_seq(),
                                thread_id.clone(),
                                tseq,
                                prompt.clone(),
                            ));
                            let s = serde_json::to_string(&resp).unwrap();
                            state.buffer_last(&s);
                            out.push(s);
                            // persist gate in thread
                            {
                                let store = state.thread_store();
                                append_step_if_new(
                                    &store,
                                    &thread_id,
                                    ThreadStep::AskUser {
                                        prompt,
                                        observation: Observation::ok(),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: agent.clone(),
                                    },
                                )
                                .await;
                            }
                        }
                        AgentFrame::AwaitApproval { prompt } => {
                            let tseq = state.next_thread_seq(&thread_id);
                            let resp =
                                api::ServerMessage::AwaitApproval(api::AwaitApprovalResponse::new(
                                    1,
                                    m::await_approval_response::Type::AwaitApproval,
                                    now_iso(),
                                    state.next_seq(),
                                    thread_id.clone(),
                                    tseq,
                                    prompt.clone(),
                                ));
                            let s = serde_json::to_string(&resp).unwrap();
                            state.buffer_last(&s);
                            out.push(s);
                            // Persist gate so reconnect/history does not stall.
                            {
                                let store = state.thread_store();
                                append_step_if_new(
                                    &store,
                                    &thread_id,
                                    ThreadStep::AskApproval {
                                        prompt,
                                        observation: Observation::ok(),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: agent.clone(),
                                    },
                                )
                                .await;
                            }
                        }
                    }
                }
            }
        }
        "open" => {
            let req: api::OpenRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let cid = req.cid.clone();
            let thread_id = req.thread_id.clone();
            if thread_id.is_empty() {
                return Err("thread_id required".into());
            }
            if uuid::Uuid::parse_str(&thread_id).is_err() {
                return Err("invalid thread_id".into());
            }
            let question = req
                .question
                .clone()
                .unwrap_or_else(|| "Continue.".to_string());
            let requested_suite = req.suite_id.clone();
            let requested_agent = normalize_agent_open(req.agent_type)?;

            // Derive current suite/agent from persisted thread log (durable across reconnects)
            let store = state.thread_store();
            let log = store.get(&thread_id).await?;
            let (current_suite, current_agent) = derive_thread_context(&log);
            // Track suite per-thread (explicit client selection) and persist switch if changed
            if current_suite != requested_suite {
                let _ = store
                    .append_step(
                        &thread_id,
                        ThreadStep::SwitchSuite {
                            from: Some(current_suite.clone()),
                            to: requested_suite.clone(),
                            observation: Observation::ok(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: requested_agent.clone(),
                        },
                    )
                    .await;
            }
            state
                .current_suite
                .insert(thread_id.clone(), requested_suite.clone());

            if current_agent != requested_agent {
                // append switch_agent step
                let _ = store
                    .append_step(
                        &thread_id,
                        ThreadStep::SwitchAgent {
                            from: Some(current_agent.clone()),
                            to: requested_agent.clone(),
                            observation: Observation::ok(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            agent: requested_agent.clone(),
                        },
                    )
                    .await;
                state
                    .current_agent
                    .insert(thread_id.clone(), requested_agent.clone());
            }
            // Ensure we always persist the explicitly requested agent for this thread
            state
                .current_agent
                .insert(thread_id.clone(), requested_agent.clone());
            // ok
            let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
            ok.cid = Some(cid.clone());
            out.push(serde_json::to_string(&api::ServerMessage::Ok(ok)).unwrap());
            // Ensure preflight is visible as a first-class phase event before we do any work.
            let suite_id = state
                .current_suite
                .get(&thread_id)
                .cloned()
                .unwrap_or_else(|| requested_suite.clone());
            let agent = state
                .current_agent
                .get(&thread_id)
                .cloned()
                .unwrap_or_else(|| requested_agent.clone());
            {
                let store = state.thread_store();
                if let Ok(Some((step_idx, ts))) =
                    ensure_preflight_phase_step(&store, &thread_id, &agent, Some(&suite_id)).await
                {
                    let runs = vec![api::PhaseRun::new(ts.clone())];
                    let mut ev = api::PhaseResponse::new(
                        1,
                        api::phase_response::Type::Phase,
                        now_iso(),
                        state.next_seq(),
                        thread_id.clone(),
                        step_idx as i32,
                        "preflight".to_string(),
                        ts,
                        runs,
                        0,
                    );
                    ev.for_cid = Some(cid.clone());
                    ev.reason_code = Some("preflight_start".to_string());
                    let s = serde_json::to_string(&api::ServerMessage::Phase(ev)).unwrap();
                    state.buffer_last(&s);
                    out.push(s);
                }
            }
            // run agent
            let frames = run_agent_and_frames(
                &thread_id,
                &question,
                &suite_id,
                &agent,
                &state.reg,
                &state.suite_ctx,
            )
            .await?;
            for f in frames {
                match f {
                    AgentFrame::Review { text, meta } => {
                        let tseq = state.next_thread_seq(&thread_id);
                        let mut rr = api::ReviewResponse::new(
                            1,
                            m::review_response::Type::Review,
                            now_iso(),
                            state.next_seq(),
                            thread_id.clone(),
                            tseq,
                            text.clone(),
                        );
                        if let Some(v) = meta {
                            if let Some(obj) = v.as_object() {
                                let mut hm: std::collections::HashMap<String, serde_json::Value> =
                                    std::collections::HashMap::new();
                                for (k, vv) in obj.iter() {
                                    hm.insert(k.clone(), vv.clone());
                                }
                                if !hm.is_empty() {
                                    rr.meta = Some(hm);
                                }
                            }
                        }
                        let resp = api::ServerMessage::Review(rr.clone());
                        let s = serde_json::to_string(&resp).unwrap();
                        state.buffer_last(&s);
                        out.push(s);
                        {
                            let store = state.thread_store();
                            let meta_val: Option<Value> =
                                rr.meta.as_ref().and_then(|m| serde_json::to_value(m).ok());
                            let _ = store
                                .append_step(
                                    &thread_id,
                                    ThreadStep::ReviewResponse {
                                        text: text.clone(),
                                        meta: meta_val,
                                        observation: Observation::ok(),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: agent.clone(),
                                    },
                                )
                                .await;
                        }
                    }
                    AgentFrame::Final {
                        kind,
                        payload,
                        display,
                    } => {
                        let display_text = display
                            .clone()
                            .unwrap_or_else(|| final_display_text_from_payload(&payload));
                        let tseq = state.next_thread_seq(&thread_id);
                        let final_result =
                            ws_final_result_from_typed_final(&kind, &payload, &display);
                        let resp = api::ServerMessage::Final(api::FinalResponse::new(
                            1,
                            m::final_response::Type::Final,
                            now_iso(),
                            state.next_seq(),
                            thread_id.clone(),
                            tseq,
                            final_result,
                        ));
                        let s = serde_json::to_string(&resp).unwrap();
                        state.buffer_last(&s);
                        out.push(s);
                        // Log entire thread on final
                        {
                            let store = state.thread_store();
                            if let Ok(log) = store.get(&thread_id).await {
                                if let Ok(pretty) = serde_json::to_string_pretty(&log) {
                                    tracing::info!("{}", pretty);
                                }
                            }
                        }
                    }
                    AgentFrame::AwaitUser { prompt } => {
                        let tseq = state.next_thread_seq(&thread_id);
                        let resp = api::ServerMessage::AwaitUser(api::AwaitUserResponse::new(
                            1,
                            m::await_user_response::Type::AwaitUser,
                            now_iso(),
                            state.next_seq(),
                            thread_id.clone(),
                            tseq,
                            prompt.clone(),
                        ));
                        let s = serde_json::to_string(&resp).unwrap();
                        state.buffer_last(&s);
                        out.push(s);
                        // persist gate in thread
                        {
                            let store = state.thread_store();
                            let _ = store
                                .append_step(
                                    &thread_id,
                                    ThreadStep::AskUser {
                                        prompt,
                                        observation: Observation::ok(),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: agent.clone(),
                                    },
                                )
                                .await;
                        }
                    }
                    AgentFrame::AwaitApproval { prompt } => {
                        let tseq = state.next_thread_seq(&thread_id);
                        let resp =
                            api::ServerMessage::AwaitApproval(api::AwaitApprovalResponse::new(
                                1,
                                m::await_approval_response::Type::AwaitApproval,
                                now_iso(),
                                state.next_seq(),
                                thread_id.clone(),
                                tseq,
                                prompt,
                            ));
                        let s = serde_json::to_string(&resp).unwrap();
                        state.buffer_last(&s);
                        out.push(s);
                    }
                }
            }
        }
        "user" => {
            let req: api::UserRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let thread_id = req.thread_id.clone();
            let text = req.text.clone();
            if thread_id.is_empty() || text.trim().is_empty() {
                return Err("thread_id and text required".into());
            }
            if uuid::Uuid::parse_str(&thread_id).is_err() {
                return Err("invalid thread_id".into());
            }
            // Ensure durable suite/agent state is available after reconnect
            if !state.current_suite.contains_key(&thread_id)
                || !state.current_agent.contains_key(&thread_id)
            {
                let store = state.thread_store();
                let log = store.get(&thread_id).await?;
                let (suite_id, agent_type) = derive_thread_context(&log);
                state.current_suite.insert(thread_id.clone(), suite_id);
                state.current_agent.insert(thread_id.clone(), agent_type);
            }
            let store = state.thread_store();
            let agent_label = state
                .current_agent
                .get(&thread_id)
                .cloned()
                .unwrap_or_else(|| "ask".to_string());
            let _ = store
                .append_step(
                    &thread_id,
                    ThreadStep::User {
                        text: text.clone(),
                        observation: Observation::ok(),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: agent_label.clone(),
                    },
                )
                .await;
            // ack
            // we don't increment thread_seq on user ack
            let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
            ok.cid = Some(req.cid.clone());
            out.push(serde_json::to_string(&api::ServerMessage::Ok(ok)).unwrap());
            // Steering gates removed: user messages no longer drive model/metric type or existing/new choices.
            // NOTE: For production WS, `type:"user"` is fast-pathed via `process_user` so we can stream progress.
            // This fallback keeps behavior for direct `handle_message` callers but does not stream progress.
            let suite_id = state
                .current_suite
                .get(&thread_id)
                .cloned()
                .ok_or_else(|| "suite_id missing for thread".to_string())?;
            let agent = state
                .current_agent
                .get(&thread_id)
                .cloned()
                .ok_or_else(|| "agent_type missing for thread".to_string())?;
            // run agent for this thread using the user text
            tracing::info!(
                "user auto-resume (non-streaming fallback): thread_id={} agent={}",
                thread_id,
                agent
            );
            let frames = run_user_and_frames(
                &thread_id,
                &text,
                &suite_id,
                &agent,
                &state.reg,
                &state.suite_ctx,
            )
            .await?;
            for f in frames {
                match f {
                    AgentFrame::Review { text, meta } => {
                        let tseq = state.next_thread_seq(&thread_id);
                        let mut rr = api::ReviewResponse::new(
                            1,
                            m::review_response::Type::Review,
                            now_iso(),
                            state.next_seq(),
                            thread_id.clone(),
                            tseq,
                            text.clone(),
                        );
                        if let Some(v) = meta {
                            if let Some(obj) = v.as_object() {
                                let mut hm: std::collections::HashMap<String, serde_json::Value> =
                                    std::collections::HashMap::new();
                                for (k, vv) in obj.iter() {
                                    hm.insert(k.clone(), vv.clone());
                                }
                                if !hm.is_empty() {
                                    rr.meta = Some(hm);
                                }
                            }
                        }
                        let resp = api::ServerMessage::Review(rr.clone());
                        let s = serde_json::to_string(&resp).unwrap();
                        state.buffer_last(&s);
                        out.push(s);
                        {
                            let store = state.thread_store();
                            let agent_label = state
                                .current_agent
                                .get(&thread_id)
                                .cloned()
                                .unwrap_or_else(|| "ask".to_string());
                            let meta_val: Option<Value> =
                                rr.meta.as_ref().and_then(|m| serde_json::to_value(m).ok());
                            let _ = store
                                .append_step(
                                    &thread_id,
                                    ThreadStep::ReviewResponse {
                                        text: text.clone(),
                                        meta: meta_val,
                                        observation: Observation::ok(),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: agent_label,
                                    },
                                )
                                .await;
                        }
                    }
                    AgentFrame::Final {
                        kind,
                        payload,
                        display,
                    } => {
                        let display_text = display
                            .clone()
                            .unwrap_or_else(|| final_display_text_from_payload(&payload));
                        let tseq = state.next_thread_seq(&thread_id);
                        let final_result =
                            ws_final_result_from_typed_final(&kind, &payload, &display);
                        let resp = api::ServerMessage::Final(api::FinalResponse::new(
                            1,
                            m::final_response::Type::Final,
                            now_iso(),
                            state.next_seq(),
                            thread_id.clone(),
                            tseq,
                            final_result,
                        ));
                        let s = serde_json::to_string(&resp).unwrap();
                        state.buffer_last(&s);
                        out.push(s);
                        // Log entire thread on final (best-effort)
                        if let Ok(log) = store.get(&thread_id).await {
                            if let Ok(pretty) = serde_json::to_string_pretty(&log) {
                                tracing::info!("{}", pretty);
                            }
                        }
                    }
                    AgentFrame::AwaitUser { prompt } => {
                        let tseq = state.next_thread_seq(&thread_id);
                        let resp = api::ServerMessage::AwaitUser(api::AwaitUserResponse::new(
                            1,
                            m::await_user_response::Type::AwaitUser,
                            now_iso(),
                            state.next_seq(),
                            thread_id.clone(),
                            tseq,
                            prompt.clone(),
                        ));
                        let s = serde_json::to_string(&resp).unwrap();
                        state.buffer_last(&s);
                        out.push(s);
                    }
                    AgentFrame::AwaitApproval { prompt } => {
                        let tseq = state.next_thread_seq(&thread_id);
                        let resp =
                            api::ServerMessage::AwaitApproval(api::AwaitApprovalResponse::new(
                                1,
                                m::await_approval_response::Type::AwaitApproval,
                                now_iso(),
                                state.next_seq(),
                                thread_id.clone(),
                                tseq,
                                prompt,
                            ));
                        let s = serde_json::to_string(&resp).unwrap();
                        state.buffer_last(&s);
                        out.push(s);
                    }
                }
            }
        }
        "history" => {
            let req: api::HistoryRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let thread_id = req.thread_id.clone();
            if thread_id.is_empty() {
                return Err("thread_id required".into());
            }
            if uuid::Uuid::parse_str(&thread_id).is_err() {
                return Err("invalid thread_id".into());
            }
            let store = state.thread_store();
            let (messages, next_before) =
                build_history(&store, &thread_id, req.before_thread_seq, req.limit).await?;
            let mut resp = api::HistoryResponse::new(
                1,
                m::history_response::Type::History,
                now_iso(),
                state.next_seq(),
                thread_id.clone(),
                messages,
            );
            let log = store.get(&thread_id).await?;
            let (suite_id, agent_type) = derive_thread_context(&log);
            resp.title = log.title;
            resp.suite_id = Some(suite_id);
            resp.agent_type = Some(agent_type);
            resp.next_before_thread_seq = next_before;
            let outm = api::ServerMessage::History(resp);
            let s = serde_json::to_string(&outm).unwrap();
            state.buffer_last(&s);
            out.push(s);
        }
        "seen" => {
            let req: api::SeenRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let thread_id = req.thread_id.clone();
            state.seen.insert(thread_id.clone(), req.up_to_thread_seq);
            // compute unread now
            let store = state.thread_store();
            let mut unread = 0;
            let log = store.get(&thread_id).await?;
            let (_max_assistant, after_seen) = compute_unread_for_log(&log, req.up_to_thread_seq);
            unread = after_seen;
            let resp = api::ServerMessage::Unread(api::UnreadResponse::new(
                1,
                m::unread_response::Type::Unread,
                now_iso(),
                state.next_seq(),
                thread_id.clone(),
                unread,
            ));
            let s = serde_json::to_string(&resp).unwrap();
            state.buffer_last(&s);
            out.push(s);
        }
        "plans" => {
            let req: api::PlansRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let thread_id = req.thread_id.clone();
            if thread_id.is_empty() {
                return Err("thread_id required".into());
            }
            if uuid::Uuid::parse_str(&thread_id).is_err() {
                return Err("invalid thread_id".into());
            }

            let (cleanse, model) = load_latest_plans(&state.suite_ctx, &thread_id).await;
            let mut resp = api::PlansResponse::new(
                1,
                m::plans_response::Type::Plans,
                now_iso(),
                state.next_seq(),
                thread_id.clone(),
            );
            resp.for_cid = Some(req.cid.clone());
            resp.cleanse = cleanse;
            resp.model = model;
            if let Some(t) = state.term() {
                t.emit(TerminalEvent::Plans {
                    thread_id: thread_id.clone(),
                    cleanse: resp.cleanse.clone(),
                    model: resp.model.clone(),
                });
            }
            let s = serde_json::to_string(&resp).unwrap();
            state.buffer_last(&s);
            out.push(s);
        }
        "thread_state" => {
            let req: api::ThreadStateRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let thread_id = req.thread_id.clone();
            if thread_id.is_empty() {
                return Err("thread_id required".into());
            }
            if uuid::Uuid::parse_str(&thread_id).is_err() {
                return Err("invalid thread_id".into());
            }

            let store = state.thread_store();
            let st = store.get_thread_state(&thread_id).await?;
            let snap = ws_thread_state_snapshot_from_core(&st, state.reg.as_ref());
            if let Some(t) = state.term() {
                t.emit(TerminalEvent::ThreadState(snap.clone()));
            }
            let mut resp = api::ThreadStateResponse::new(
                1,
                m::thread_state_response::Type::ThreadState,
                now_iso(),
                state.next_seq(),
                thread_id.clone(),
                snap,
            );
            resp.for_cid = Some(req.cid.clone());
            let s = serde_json::to_string(&resp).unwrap();
            state.buffer_last(&s);
            out.push(s);
        }
        "delete" => {
            let req: api::DeleteRequest =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            let cid = req.cid.clone();
            let thread_id = req.thread_id.clone();
            if thread_id.is_empty() {
                return Err("thread_id required".into());
            }
            if uuid::Uuid::parse_str(&thread_id).is_err() {
                return Err("invalid thread_id".into());
            }
            // delete thread json
            {
                let store = state.thread_store();
                let _ = store.delete(&thread_id).await;
            }
            // best-effort vector cleanup (optional provider)
            if let Some(vs) = state.suite_ctx.vector.as_ref() {
                let _ = vs
                    .delete_thread_embeddings(&state.suite_ctx.scope, &thread_id)
                    .await;
            }
            // respond ok
            let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
            ok.cid = Some(cid.clone());
            out.push(serde_json::to_string(&api::ServerMessage::Ok(ok)).unwrap());
        }
        _ => {
            return Err("unknown type".to_string());
        }
    }
    Ok(out)
}

/// Headless runner for `react run`.
///
/// It reuses the WS request handlers + streaming loop, but sends frames into a
/// "null" sink (no socket) and emits the same typed `api::ServerMessage` events
/// to the provided `EventHub`.
pub async fn run_headless_with_hub(
    suite_ctx: SuiteCtx,
    thread_id: Option<String>,
    suite_id: String,
    agent: String,
    hub: EventHub,
) -> Result<String, String> {
    use serde_json::json;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    let reg = Arc::new(react_suites::default_registry());
    let mut state = ConnState::new(reg, suite_ctx, Some(hub));
    #[derive(Clone, Default)]
    struct Capture {
        thread_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }
    impl Capture {
        fn get(&self) -> Option<String> {
            self.thread_id.lock().ok().and_then(|g| g.clone())
        }
        fn capture(&self, s: &str) {
            if let Ok(v) = serde_json::from_str::<Value>(s) {
                if v.get("type").and_then(|x| x.as_str()) == Some("thread_assigned") {
                    if let Some(tid) = v.get("thread_id").and_then(|x| x.as_str()) {
                        if let Ok(mut g) = self.thread_id.lock() {
                            *g = Some(tid.to_string());
                        }
                    }
                }
            }
        }
    }
    struct CaptureSink {
        cap: Capture,
        _recent: VecDeque<String>,
    }
    impl CaptureSink {
        fn new(cap: Capture) -> Self {
            Self {
                cap,
                _recent: VecDeque::new(),
            }
        }
    }
    impl futures::sink::Sink<Message> for CaptureSink {
        type Error = String;
        fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            if let Message::Text(s) = item {
                self.cap.capture(&s);
                self._recent.push_back(s);
                while self._recent.len() > 32 {
                    self._recent.pop_front();
                }
            }
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }
    let cap = Capture::default();
    let mut write = CaptureSink::new(cap.clone());

    if let Some(tid) = thread_id {
        if tid.trim().is_empty() {
            return Err("thread_id required".into());
        }
        if uuid::Uuid::parse_str(&tid).is_err() {
            return Err("invalid thread_id".into());
        }
        // Enforce "must exist" semantics with a clear error message.
        let store = state.thread_store();
        if store.get(&tid).await.is_err() {
            return Err(format!("thread does not exist: {}", tid));
        }

        let v = json!({
            "v": 1,
            "type": "open",
            "cid": "headless",
            "thread_id": tid,
            "suiteId": suite_id,
            "agentType": agent,
            "question": "continue"
        });
        let tid = v
            .get("thread_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        process_open(&v, &mut state, &mut write).await?;
        return Ok(tid);
    }

    // Create a new thread with an initial "go" prompt.
    let v = json!({
        "v": 1,
        "type": "new",
        "cid": "headless",
        "suiteId": suite_id,
        "agentType": agent,
        "question": "go"
    });
    process_new(&v, &mut state, &mut write).await?;

    cap.get()
        .ok_or_else(|| "internal: failed to capture thread_id from new thread".to_string())
}

fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

async fn append_step_if_new(store: &ThreadStore, thread_id: &str, step: ThreadStep) {
    // Best-effort idempotency: if the last persisted step is equivalent, skip.
    let is_dup = match store
        .get(thread_id)
        .await
        .ok()
        .and_then(|l| l.steps.last().cloned())
    {
        Some(ThreadStep::AskUser { prompt: p1, .. }) => {
            matches!(&step, ThreadStep::AskUser { prompt: p2, .. } if p1 == *p2)
        }
        Some(ThreadStep::AskApproval { prompt: p1, .. }) => {
            matches!(&step, ThreadStep::AskApproval { prompt: p2, .. } if p1 == *p2)
        }
        Some(ThreadStep::Final {
            kind: k1,
            payload: pl1,
            display: d1,
            ..
        }) => matches!(
            &step,
            ThreadStep::Final { kind: k2, payload: pl2, display: d2, .. }
                if k1 == *k2 && pl1 == *pl2 && d1 == *d2
        ),
        _ => false,
    };
    if is_dup {
        return;
    }
    let _ = store.append_step(thread_id, step).await;
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| {
            let vv = v.trim().to_lowercase();
            vv == "1" || vv == "true" || vv == "yes"
        })
        .unwrap_or(false)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    match s.char_indices().take_while(|(i, _)| *i < max).last() {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.chars().take(max).collect(),
    }
}

fn summarize_step(step: &ThreadStep) -> String {
    let agent = match step {
        ThreadStep::SwitchSuite { agent, .. }
        | ThreadStep::SwitchAgent { agent, .. }
        | ThreadStep::User { agent, .. }
        | ThreadStep::ToolStart { agent, .. }
        | ThreadStep::ToolEnd { agent, .. }
        | ThreadStep::LlmStart { agent, .. }
        | ThreadStep::LlmEnd { agent, .. }
        | ThreadStep::LlmCall { agent, .. }
        | ThreadStep::Phase { agent, .. }
        | ThreadStep::GuardBlock { agent, .. }
        | ThreadStep::ArtifactFocus { agent, .. }
        | ThreadStep::ArtifactSaved { agent, .. }
        | ThreadStep::AskUser { agent, .. }
        | ThreadStep::AskApproval { agent, .. }
        | ThreadStep::ReviewResponse { agent, .. }
        | ThreadStep::Final { agent, .. } => agent.as_str(),
    };
    let raw = serde_json::to_string(step).unwrap_or_else(|_| "{\"type\":\"unknown\"}".to_string());
    let raw = truncate_str(&raw, 220);
    format!("ts={} agent={} step={}", step.ts(), agent, raw)
}

async fn ensure_preflight_phase_step(
    store: &ThreadStore,
    thread_id: &str,
    agent: &str,
    suite_id: Option<&str>,
) -> Result<Option<(usize, String)>, String> {
    let log = store.get(thread_id).await?;
    let already_has_phase = log
        .steps
        .iter()
        .any(|s| matches!(s, ThreadStep::Phase { .. }));
    if already_has_phase {
        return Ok(None);
    }
    let step_idx = log.steps.len();
    let ts = chrono::Utc::now().to_rfc3339();
    let mut detail = serde_json::Map::new();
    detail.insert("phase".to_string(), serde_json::json!("preflight"));
    if let Some(s) = suite_id {
        if !s.trim().is_empty() {
            detail.insert("suite_id".to_string(), serde_json::json!(s));
        }
    }
    if !agent.trim().is_empty() {
        detail.insert("agent".to_string(), serde_json::json!(agent));
    }
    let _ = store
        .append_step(
            thread_id,
            ThreadStep::Phase {
                phase: "preflight".to_string(),
                from_phase: None,
                reason_code: Some("preflight_start".to_string()),
                reason_detail: Some(serde_json::Value::Object(detail)),
                observation: Observation::ok(),
                ts: ts.clone(),
                agent: agent.to_string(),
            },
        )
        .await;
    Ok(Some((step_idx, ts)))
}

fn duration_ms(start_ts: &str, end_ts: &str) -> Option<i64> {
    let start = chrono::DateTime::parse_from_rfc3339(start_ts).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(end_ts).ok()?;
    let delta = end.signed_duration_since(start);
    Some(delta.num_milliseconds().max(0))
}

fn phase_runs_from_steps(
    steps: &[ThreadStep],
) -> std::collections::HashMap<String, Vec<api::PhaseRun>> {
    let mut runs: std::collections::HashMap<String, Vec<api::PhaseRun>> =
        std::collections::HashMap::new();
    for step in steps.iter() {
        let ThreadStep::Phase {
            phase,
            from_phase,
            ts,
            ..
        } = step
        else {
            continue;
        };
        let ts = ts.clone();

        // Close the phase we are leaving (if we have an open run recorded).
        if let Some(prev) = from_phase
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if let Some(v) = runs.get_mut(prev) {
                if let Some(last) = v.last_mut() {
                    if last.ended_at.is_none() {
                        last.ended_at = Some(ts.clone());
                        last.runtime_ms = duration_ms(&last.started_at, &ts);
                    }
                }
            }
        }

        let ph = phase.trim();
        if ph.is_empty() {
            continue;
        }
        let v = runs.entry(ph.to_string()).or_default();
        // Ensure at most one in-flight run per phase even if logs are imperfect.
        if let Some(last) = v.last_mut() {
            if last.ended_at.is_none() {
                last.ended_at = Some(ts.clone());
                last.runtime_ms = duration_ms(&last.started_at, &ts);
            }
        }
        v.push(api::PhaseRun::new(ts));
    }
    runs
}

fn total_completed_runtime_ms(runs: &[api::PhaseRun]) -> i64 {
    runs.iter().filter_map(|r| r.runtime_ms).sum()
}

fn derive_current_phase_from_steps(steps: &[ThreadStep]) -> String {
    for step in steps.iter().rev() {
        if let ThreadStep::Phase { phase, .. } = step {
            let t = phase.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    "preflight".to_string()
}

fn phase_at_step_idx(steps: &[ThreadStep], idx: usize) -> Option<String> {
    if steps.is_empty() {
        return None;
    }
    let end = idx.min(steps.len().saturating_sub(1));
    Some(derive_current_phase_from_steps(&steps[..=end]))
}

fn derive_completed_phases(order: &[String], current: &str) -> Vec<String> {
    let mut completed: Vec<String> = Vec::new();
    if order.is_empty() {
        return completed;
    }
    let mut idx: Option<usize> = None;
    for (i, p) in order.iter().enumerate() {
        if p == current {
            idx = Some(i);
            break;
        }
    }
    let upto = idx.unwrap_or(0);
    for p in order.iter().take(upto) {
        completed.push(p.clone());
    }
    completed
}

async fn log_thread_steps_if_enabled(store: &ThreadStore, thread_id: &str, reason: &str) {
    if !env_truthy("REACT_LOG_THREAD_STEPS") {
        return;
    }
    match store.get(thread_id).await {
        Ok(log) => {
            tracing::info!(
                "THREAD_LOG {} thread_id={} steps={} title={:?} finalized={}",
                reason,
                thread_id,
                log.steps.len(),
                log.title,
                log.title_finalized
            );
            // Print last N steps (most useful when debugging long loops).
            let n = 40usize;
            let start = log.steps.len().saturating_sub(n);
            for (i, step) in log.steps.iter().enumerate().skip(start) {
                tracing::info!(
                    "THREAD_STEP {} #{} {}",
                    thread_id,
                    i + 1,
                    summarize_step(step)
                );
            }
            // Optional full JSON dump (very verbose).
            if env_truthy("REACT_LOG_THREAD_JSON") {
                if let Ok(pretty) = serde_json::to_string_pretty(&log) {
                    tracing::info!("THREAD_JSON {} {}", thread_id, pretty);
                }
            }
        }
        Err(e) => tracing::info!(
            "THREAD_LOG {} thread_id={} (failed to load: {})",
            reason,
            thread_id,
            e
        ),
    }
}

fn truncate_title(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    match s.char_indices().take_while(|(i, _)| *i < max_chars).last() {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.chars().take(max_chars).collect(),
    }
}

async fn synthesize_title(llm: &react_core::llm::DynLlm, question: &str, answer: &str) -> String {
    // Try LLM to produce a concise title (<= 8 words), else fallback to truncated question
    let prompt = format!(
		"Create a very short, descriptive chat title (≤ 8 words).\nRules: plain text only, no quotes, no punctuation beyond spaces, title case.\nQuestion: {}\nAnswer: {}\nTitle:",
		question, answer
	);
    let out = tokio::task::spawn_blocking({
        let llm2 = llm.clone();
        let p = prompt.clone();
        move || {
            llm2.chat(&[crate::llm::ChatMessage {
                role: "user".into(),
                content: p,
            }])
        }
    })
    .await;
    if let Ok(Ok(text)) = out {
        let t = text.trim();
        if !t.is_empty() {
            // Normalize whitespace and cap length
            let norm = t.split_whitespace().collect::<Vec<_>>().join(" ");
            return truncate_title(&norm, 64);
        }
    }
    truncate_title(question, 64)
}

struct ConnState {
    seq: i32,
    sent: VecDeque<(i32, String)>,
    thread_seq: HashMap<String, i32>,
    seen: HashMap<String, i32>,
    current_suite: HashMap<String, String>,
    current_agent: HashMap<String, String>,
    reg: Arc<SuiteRegistry>,
    suite_ctx: SuiteCtx,
    terminal: Option<TerminalSink>,
    hub: Option<EventHub>,
}

async fn load_last_run_sql_async(thread_id: &str) -> (Vec<String>, Vec<Vec<String>>) {
    // NOTE: This helper is currently unused. Keep it as a stub to avoid coupling
    // to the thread store from this generic WS module.
    let _ = thread_id;
    (Vec::new(), Vec::new())
}

async fn synthesize_summary(
    question: &str,
    agent_answer: &str,
    sql_opt: &Option<String>,
    header: &[String],
    rows: &[Vec<String>],
) -> Option<String> {
    // NOTE: Currently unused. Kept as a stub to avoid coupling WS core to LLM bootstrap/config.
    let _ = (question, agent_answer, sql_opt, header, rows);
    None
}

impl ConnState {
    fn new(reg: Arc<SuiteRegistry>, suite_ctx: SuiteCtx, hub: Option<EventHub>) -> Self {
        Self {
            seq: 0,
            sent: VecDeque::new(),
            thread_seq: HashMap::new(),
            seen: HashMap::new(),
            current_suite: HashMap::new(),
            current_agent: HashMap::new(),
            reg,
            suite_ctx,
            terminal: terminal::sink().cloned(),
            hub,
        }
    }
    fn term(&self) -> Option<&TerminalSink> {
        self.terminal.as_ref()
    }
    fn hub(&self) -> Option<&EventHub> {
        self.hub.as_ref()
    }
    fn thread_store(&self) -> ThreadStore {
        ThreadStore::new(
            self.suite_ctx.storage.clone(),
            self.suite_ctx.scope.clone(),
            self.suite_ctx.keyspace.clone(),
        )
    }
    fn next_seq(&mut self) -> i32 {
        self.seq += 1;
        self.seq
    }
    fn next_thread_seq(&mut self, thread_id: &str) -> i32 {
        let entry = self.thread_seq.entry(thread_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }
    fn buffer_last(&mut self, json: &str) {
        // try to extract seq
        if let Ok(v) = serde_json::from_str::<Value>(json) {
            if let Some(seq) = v.get("seq").and_then(|x| x.as_i64()) {
                self.sent.push_back((seq as i32, json.to_string()));
                while self.sent.len() > 500 {
                    self.sent.pop_front();
                }
            }
        }
    }
}

fn derive_thread_context(log: &ThreadLog) -> (String, String) {
    // Defaults for back-compat threads with no recorded context.
    let mut suite_id = "data_engineer".to_string();
    let mut agent_type = "ask".to_string();
    for step in log.steps.iter() {
        match step {
            ThreadStep::SwitchSuite { to, .. } => {
                if !to.trim().is_empty() {
                    suite_id = to.to_string();
                }
            }
            ThreadStep::SwitchAgent { to, .. } => {
                if !to.trim().is_empty() {
                    agent_type = to.to_string();
                }
            }
            _ => {}
        }
    }
    (suite_id, agent_type)
}

fn build_suites_catalog(reg: &SuiteRegistry) -> Vec<api::SuitesResponseSuitesInner> {
    let mut out: Vec<api::SuitesResponseSuitesInner> = Vec::new();
    for id in reg.list_ids() {
        match id {
            "data_engineer" => {
                let mut s = api::SuitesResponseSuitesInner::new(
                    "data_engineer".to_string(),
                    vec!["ask".to_string(), "agent".to_string(), "review".to_string()],
                );
                s.label = Some("Data Engineer".to_string());
                s.default_agent_type = Some("ask".to_string());
                out.push(s);
            }
            "kb" => {
                let mut s =
                    api::SuitesResponseSuitesInner::new("kb".to_string(), vec!["kb".to_string()]);
                s.label = Some("KB".to_string());
                s.default_agent_type = Some("kb".to_string());
                out.push(s);
            }
            _ => {
                let mut s =
                    api::SuitesResponseSuitesInner::new(id.to_string(), vec!["ask".to_string()]);
                s.default_agent_type = Some("ask".to_string());
                out.push(s);
            }
        };
    }
    out
}

fn map_plan_status(s: de_plan::PlanStatus) -> api::PlanStatus {
    match s {
        de_plan::PlanStatus::Draft => api::PlanStatus::Draft,
        de_plan::PlanStatus::Approved => api::PlanStatus::Approved,
        de_plan::PlanStatus::Completed => api::PlanStatus::Completed,
        de_plan::PlanStatus::Cancelled => api::PlanStatus::Cancelled,
    }
}

fn map_task_status(s: de_plan::TaskStatus) -> api::PlanTaskStatus {
    match s {
        de_plan::TaskStatus::Pending => api::PlanTaskStatus::Pending,
        de_plan::TaskStatus::InProgress => api::PlanTaskStatus::InProgress,
        de_plan::TaskStatus::Done => api::PlanTaskStatus::Done,
        de_plan::TaskStatus::Blocked => api::PlanTaskStatus::Blocked,
        de_plan::TaskStatus::NeedsUpdate => api::PlanTaskStatus::NeedsUpdate,
    }
}

fn map_checklist_item_status(s: de_plan::ChecklistItemStatus) -> api::PlanChecklistItemStatus {
    match s {
        de_plan::ChecklistItemStatus::Pending => api::PlanChecklistItemStatus::Pending,
        de_plan::ChecklistItemStatus::InProgress => api::PlanChecklistItemStatus::InProgress,
        de_plan::ChecklistItemStatus::Done => api::PlanChecklistItemStatus::Done,
        de_plan::ChecklistItemStatus::Blocked => api::PlanChecklistItemStatus::Blocked,
        de_plan::ChecklistItemStatus::NeedsUpdate => api::PlanChecklistItemStatus::NeedsUpdate,
    }
}

fn map_checklist_origin(s: de_plan::ChecklistOrigin) -> api::PlanChecklistOrigin {
    match s {
        de_plan::ChecklistOrigin::Initial => api::PlanChecklistOrigin::Initial,
        de_plan::ChecklistOrigin::ReviewActionable => api::PlanChecklistOrigin::ReviewActionable,
    }
}

fn map_checklist_evidence(e: de_plan::ChecklistEvidence) -> api::PlanChecklistEvidence {
    let mut out = api::PlanChecklistEvidence::new(e.kind, e.step_idx as i32);
    out.tool_name = e.tool_name;
    out.tool_id = e.tool_id;
    out.ts = e.ts;
    out
}

fn map_checklist_item(it: de_plan::PlanChecklistItem) -> api::PlanChecklistItem {
    let mut out = api::PlanChecklistItem::new(
        it.checklist_item_id,
        it.label,
        map_checklist_item_status(it.status),
        map_checklist_origin(it.origin),
    );
    out.details = it.details;
    out.origin_step_idx = it.origin_step_idx.map(|x| x as i32);
    if !it.evidence.is_empty() {
        out.evidence = Some(it.evidence.into_iter().map(map_checklist_evidence).collect());
    }
    out
}

fn map_work_group_kind(k: de_plan::WorkGroupKind) -> api::PlanWorkGroupKind {
    match k {
        de_plan::WorkGroupKind::AuthorSql => api::PlanWorkGroupKind::AuthorSql,
        de_plan::WorkGroupKind::AuthorSchema => api::PlanWorkGroupKind::AuthorSchema,
        de_plan::WorkGroupKind::Validate => api::PlanWorkGroupKind::Validate,
    }
}

fn map_work_group(wg: de_plan::PlanWorkGroup) -> api::PlanWorkGroup {
    let items = wg
        .items
        .into_iter()
        .map(|it| api::PlanWorkGroupItemRef::new(it.task_id, it.checklist_item_id))
        .collect::<Vec<_>>();
    let mut out = api::PlanWorkGroup::new(wg.group_id, wg.label, map_work_group_kind(wg.kind), items);
    out.depends_on_group_ids = wg.depends_on_group_ids;
    out
}

fn map_project_snapshot(v: serde_json::Value) -> Option<HashMap<String, serde_json::Value>> {
    let obj = v.as_object()?;
    let mut out: HashMap<String, serde_json::Value> = HashMap::new();
    for (k, vv) in obj.iter() {
        out.insert(k.clone(), vv.clone());
    }
    Some(out)
}

fn plan_parse_error_snapshot(
    plan_kind: api::plan_snapshot::PlanKind,
    plan_key: String,
    err: String,
) -> api::PlanSnapshot {
    let mut item = api::PlanChecklistItem::new(
        "parse_error".to_string(),
        "Plan failed to deserialize".to_string(),
        api::PlanChecklistItemStatus::NeedsUpdate,
        api::PlanChecklistOrigin::Initial,
    );
    item.details = Some(err.clone());

    let task = match plan_kind {
        api::plan_snapshot::PlanKind::Cleanse => {
            let snap = api::CleanseTaskSnapshot::new(
                "parse_error".to_string(),
                "parse_error".to_string(),
                api::PlanTaskStatus::NeedsUpdate,
                vec![item],
            );
            api::PlanTask::Cleanse(snap)
        }
        api::plan_snapshot::PlanKind::Model => {
            let snap = api::ModelTaskSnapshot::new(
                "parse_error".to_string(),
                "parse_error".to_string(),
                api::PlanTaskStatus::NeedsUpdate,
                vec![item],
            );
            api::PlanTask::Model(snap)
        }
    };

    let mut snap = api::PlanSnapshot::new(
        plan_kind,
        plan_key,
        api::PlanStatus::Cancelled,
        vec![task],
        vec![],
    );
    let mut ps: HashMap<String, serde_json::Value> = HashMap::new();
    ps.insert("parse_error".to_string(), serde_json::json!(err));
    snap.project_snapshot = Some(ps);
    snap
}

// load_active_plan_snapshot removed (hard cutover to `plans` + `thread_state`).

async fn load_latest_plans(
    ctx: &SuiteCtx,
    thread_id: &str,
) -> (Option<api::PlanSnapshot>, Option<api::PlanSnapshot>) {
    // Plans are stored alongside other top-level resources (threads/, dbt/, etc),
    // NOT under dbt/.
    let base = ctx
        .keyspace
        .threads_prefix(&ctx.scope)
        .trim_end_matches("/threads")
        .trim_end_matches('/')
        .to_string();
    let pref = format!("{}/plans/{}/", base, thread_id.trim());
    let mut keys = ctx.storage.list_prefix(&pref).await.unwrap_or_default();
    keys.sort();

    let mut cleanse_active: Option<api::PlanSnapshot> = None;
    let mut newest_terminal_cleanse: Option<de_plan::CleansePlan> = None;
    for k in keys.iter().filter(|k| k.ends_with("_cleanse.json")) {
        if let Ok(bytes) = ctx.storage.get_bytes(k).await {
            match serde_json::from_slice::<de_plan::CleansePlan>(&bytes) {
                Ok(mut p) => {
                    if p.plan_key.trim().is_empty() {
                        p.plan_key = k.to_string();
                    }
                    if !p.status.is_terminal() {
                        let project_snapshot = p.project_snapshot;
                        let plan_key = p.plan_key;
                        let status = p.status;
                        let work_groups = p
                            .work_groups
                            .into_iter()
                            .map(map_work_group)
                            .collect::<Vec<_>>();
                        let tasks = p
                            .tasks
                            .into_iter()
                            .map(|t| {
                                let checklist = t
                                    .checklist
                                    .into_iter()
                                    .map(map_checklist_item)
                                    .collect::<Vec<_>>();
                                let mut snap = api::CleanseTaskSnapshot::new(
                                    t.dataset_id.clone(),
                                    t.dataset_id.clone(),
                                    map_task_status(t.status),
                                    checklist,
                                );
                                snap.expected_model_path = t.expected_model_path;
                                if !t.invariants.is_empty() {
                                    snap.invariants = Some(t.invariants);
                                }
                                api::PlanTask::Cleanse(snap)
                            })
                            .collect::<Vec<_>>();
                        let mut snap = api::PlanSnapshot::new(
                            api::plan_snapshot::PlanKind::Cleanse,
                            plan_key,
                            map_plan_status(status),
                            tasks,
                            work_groups,
                        );
                        snap.project_snapshot = map_project_snapshot(project_snapshot);
                        cleanse_active = Some(snap);
                        break;
                    } else {
                        newest_terminal_cleanse = Some(p);
                    }
                }
                Err(e) => {
                    // Hard cutover: surface a parse error snapshot so the UI doesn't silently drop plans.
                    cleanse_active = Some(plan_parse_error_snapshot(
                        api::plan_snapshot::PlanKind::Cleanse,
                        k.to_string(),
                        format!("failed to parse cleanse plan JSON at {}: {}", k, e),
                    ));
                    break;
                }
            }
        }
    }

    let cleanse = if let Some(s) = cleanse_active {
        Some(s)
    } else if let Some(p) = newest_terminal_cleanse {
        let project_snapshot = p.project_snapshot;
        let plan_key = p.plan_key;
        let status = p.status;
        let work_groups = p
            .work_groups
            .into_iter()
            .map(map_work_group)
            .collect::<Vec<_>>();
        let tasks = p
            .tasks
            .into_iter()
            .map(|t| {
                let checklist =
                    t.checklist.into_iter().map(map_checklist_item).collect::<Vec<_>>();
                let mut snap = api::CleanseTaskSnapshot::new(
                    t.dataset_id.clone(),
                    t.dataset_id.clone(),
                    map_task_status(t.status),
                    checklist,
                );
                snap.expected_model_path = t.expected_model_path;
                if !t.invariants.is_empty() {
                    snap.invariants = Some(t.invariants);
                }
                api::PlanTask::Cleanse(snap)
            })
            .collect::<Vec<_>>();
        let mut snap = api::PlanSnapshot::new(
            api::plan_snapshot::PlanKind::Cleanse,
            plan_key,
            map_plan_status(status),
            tasks,
            work_groups,
        );
        snap.project_snapshot = map_project_snapshot(project_snapshot);
        Some(snap)
    } else {
        None
    };

    let mut model_active: Option<api::PlanSnapshot> = None;
    let mut newest_terminal_model: Option<de_plan::ModelPlan> = None;
    for k in keys.iter().filter(|k| k.ends_with("_model.json")) {
        if let Ok(bytes) = ctx.storage.get_bytes(k).await {
            match serde_json::from_slice::<de_plan::ModelPlan>(&bytes) {
                Ok(mut p) => {
                    if p.plan_key.trim().is_empty() {
                        p.plan_key = k.to_string();
                    }
                    if !p.status.is_terminal() {
                        let project_snapshot = p.project_snapshot;
                        let plan_key = p.plan_key;
                        let status = p.status;
                        let work_groups = p
                            .work_groups
                            .into_iter()
                            .map(map_work_group)
                            .collect::<Vec<_>>();
                        let tasks = p
                            .tasks
                            .into_iter()
                            .map(|t| {
                                let checklist = t
                                    .checklist
                                    .into_iter()
                                    .map(map_checklist_item)
                                    .collect::<Vec<_>>();
                                let mut snap = api::ModelTaskSnapshot::new(
                                    t.name.clone(),
                                    t.name.clone(),
                                    map_task_status(t.status),
                                    checklist,
                                );
                                if !t.folder.trim().is_empty() {
                                    snap.folder = Some(t.folder);
                                }
                                if !t.goal.trim().is_empty() {
                                    snap.goal = Some(t.goal);
                                }
                                if !t.inputs.is_empty() {
                                    snap.inputs = Some(t.inputs);
                                }
                                snap.expected_model_path = t.expected_model_path;
                                if !t.invariants.is_empty() {
                                    snap.invariants = Some(t.invariants);
                                }
                                api::PlanTask::Model(snap)
                            })
                            .collect::<Vec<_>>();
                        let mut snap = api::PlanSnapshot::new(
                            api::plan_snapshot::PlanKind::Model,
                            plan_key,
                            map_plan_status(status),
                            tasks,
                            work_groups,
                        );
                        snap.project_snapshot = map_project_snapshot(project_snapshot);
                        model_active = Some(snap);
                        break;
                    } else {
                        newest_terminal_model = Some(p);
                    }
                }
                Err(e) => {
                    model_active = Some(plan_parse_error_snapshot(
                        api::plan_snapshot::PlanKind::Model,
                        k.to_string(),
                        format!("failed to parse model plan JSON at {}: {}", k, e),
                    ));
                    break;
                }
            }
        }
    }

    let model = if let Some(s) = model_active {
        Some(s)
    } else if let Some(p) = newest_terminal_model {
        let project_snapshot = p.project_snapshot;
        let plan_key = p.plan_key;
        let status = p.status;
        let work_groups = p
            .work_groups
            .into_iter()
            .map(map_work_group)
            .collect::<Vec<_>>();
        let tasks = p
            .tasks
            .into_iter()
            .map(|t| {
                let checklist =
                    t.checklist.into_iter().map(map_checklist_item).collect::<Vec<_>>();
                let mut snap = api::ModelTaskSnapshot::new(
                    t.name.clone(),
                    t.name.clone(),
                    map_task_status(t.status),
                    checklist,
                );
                if !t.folder.trim().is_empty() {
                    snap.folder = Some(t.folder);
                }
                if !t.goal.trim().is_empty() {
                    snap.goal = Some(t.goal);
                }
                if !t.inputs.is_empty() {
                    snap.inputs = Some(t.inputs);
                }
                snap.expected_model_path = t.expected_model_path;
                if !t.invariants.is_empty() {
                    snap.invariants = Some(t.invariants);
                }
                api::PlanTask::Model(snap)
            })
            .collect::<Vec<_>>();
        let mut snap = api::PlanSnapshot::new(
            api::plan_snapshot::PlanKind::Model,
            plan_key,
            map_plan_status(status),
            tasks,
            work_groups,
        );
        snap.project_snapshot = map_project_snapshot(project_snapshot);
        Some(snap)
    } else {
        None
    };

    (cleanse, model)
}

// Plan diff/change frames removed (hard cutover to thread_state + plans).

async fn process_new(
    v: &Value,
    state: &mut ConnState,
    write: &mut (impl SinkExt<Message> + Unpin),
) -> Result<(), String> {
    let req: api::NewRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
    let cid = req.cid.clone();
    let question = req.question.clone().unwrap_or_default();
    let thread_id = Uuid::new_v4().to_string();
    let suite_id = req.suite_id.clone();
    let agent = normalize_agent_new(req.agent_type)?;
    state
        .current_suite
        .insert(thread_id.clone(), suite_id.clone());
    state.current_agent.insert(thread_id.clone(), agent.clone());
    // Persist initial suite/agent selection so it survives reconnects
    {
        let store = state.thread_store();
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::SwitchSuite {
                    from: None,
                    to: suite_id.clone(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: agent.clone(),
                },
            )
            .await;
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::SwitchAgent {
                    from: None,
                    to: agent.clone(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: agent.clone(),
                },
            )
            .await;
    }
    // ok
    let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
    ok.cid = Some(cid.clone());
    {
        let s = serde_json::to_string(&ok).unwrap();
        ws_log_out(&s);
        let _ = write.send(Message::Text(s)).await;
    }
    // thread_assigned
    let ta = api::ThreadAssignedResponse::new(
        1,
        m::thread_assigned_response::Type::ThreadAssigned,
        now_iso(),
        state.next_seq(),
        cid.clone(),
        thread_id.clone(),
    );
    {
        let s = serde_json::to_string(&ta).unwrap();
        state.buffer_last(&s);
        ws_log_out(&s);
        let _ = write.send(Message::Text(s)).await;
    }
    // agent with periodic updates
    // If a question was provided, record it and run the suite. Otherwise, this is "create thread".
    if !question.trim().is_empty() {
        {
            let store = state.thread_store();
            let _ = store
                .append_step(
                    &thread_id,
                    ThreadStep::User {
                        text: question.clone(),
                        observation: Observation::ok(),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: agent.clone(),
                    },
                )
                .await;
            let _ = store
                .set_title_if_absent(&thread_id, &truncate_title(&question, 64))
                .await;
        }
        run_suite_and_stream(
            &thread_id,
            &question,
            &suite_id,
            &agent,
            &cid,
            SuiteRunKind::New,
            state,
            write,
        )
        .await?;
    }
    Ok(())
}

async fn process_open(
    v: &Value,
    state: &mut ConnState,
    write: &mut (impl SinkExt<Message> + Unpin),
) -> Result<(), String> {
    let req: api::OpenRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
    let cid = req.cid.clone();
    let thread_id = req.thread_id.clone();
    if thread_id.is_empty() {
        return Err("thread_id required".into());
    }
    if uuid::Uuid::parse_str(&thread_id).is_err() {
        return Err("invalid thread_id".into());
    }
    let question = req
        .question
        .clone()
        .unwrap_or_else(|| "Continue.".to_string());
    let requested_suite = req.suite_id.clone();
    let requested_agent = normalize_agent_open(req.agent_type)?;

    // Derive current suite/agent from persisted thread log (durable across reconnects)
    let store = state.thread_store();
    let log = store.get(&thread_id).await?;
    let (current_suite, current_agent) = derive_thread_context(&log);
    if current_suite != requested_suite {
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::SwitchSuite {
                    from: Some(current_suite.clone()),
                    to: requested_suite.clone(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: requested_agent.clone(),
                },
            )
            .await;
    }
    state
        .current_suite
        .insert(thread_id.clone(), requested_suite.clone());

    if current_agent != requested_agent {
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::SwitchAgent {
                    from: Some(current_agent.clone()),
                    to: requested_agent.clone(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: requested_agent.clone(),
                },
            )
            .await;
        state
            .current_agent
            .insert(thread_id.clone(), requested_agent.clone());
    }
    // Ensure we always persist the explicitly requested agent for this thread
    state
        .current_agent
        .insert(thread_id.clone(), requested_agent.clone());
    // ok
    let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
    ok.cid = Some(cid.clone());
    {
        let s = serde_json::to_string(&ok).unwrap();
        ws_log_out(&s);
        let _ = write.send(Message::Text(s)).await;
    }

    // Strongly-consistent materialized state snapshot (durable across reloads).
    {
        let store = state.thread_store();
        let (cleanse, model) = load_latest_plans(&state.suite_ctx, &thread_id).await;
        upsert_thread_state_from_plans(&store, &thread_id, &cleanse, &model).await;
        if let Ok(st) = store.get_thread_state(&thread_id).await {
            let snap = ws_thread_state_snapshot_from_core(&st, state.reg.as_ref());
            let mut resp = api::ThreadStateResponse::new(
                1,
                m::thread_state_response::Type::ThreadState,
                now_iso(),
                state.next_seq(),
                thread_id.clone(),
                snap,
            );
            resp.for_cid = Some(cid.clone());
            let s = serde_json::to_string(&resp).unwrap();
            state.buffer_last(&s);
            ws_log_out(&s);
            let _ = write.send(Message::Text(s)).await;
        }
    }
    // agent with periodic updates
    let suite_id = state
        .current_suite
        .get(&thread_id)
        .cloned()
        .unwrap_or_else(|| requested_suite.clone());
    let agent = state
        .current_agent
        .get(&thread_id)
        .cloned()
        .unwrap_or_else(|| requested_agent.clone());
    // if user supplied a prompt on open, record it
    if !question.trim().is_empty() {
        let store = state.thread_store();
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::User {
                    text: question.clone(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: agent.clone(),
                },
            )
            .await;
        let _ = store
            .set_title_if_absent(&thread_id, &truncate_title(&question, 64))
            .await;
    }
    run_suite_and_stream(
        &thread_id,
        &question,
        &suite_id,
        &agent,
        &cid,
        SuiteRunKind::Open,
        state,
        write,
    )
    .await
}

async fn process_user(
    v: &Value,
    state: &mut ConnState,
    write: &mut (impl SinkExt<Message> + Unpin),
) -> Result<(), String> {
    let req: api::UserRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
    let cid = req.cid.clone();
    let thread_id = req.thread_id.clone();
    let text = req.text.clone();
    if thread_id.is_empty() || text.trim().is_empty() {
        return Err("thread_id and text required".into());
    }
    if uuid::Uuid::parse_str(&thread_id).is_err() {
        return Err("invalid thread_id".into());
    }

    // Reconnect-safe: derive suite/agent from persisted history if not present in connection state
    if !state.current_suite.contains_key(&thread_id)
        || !state.current_agent.contains_key(&thread_id)
    {
        let store = state.thread_store();
        let log = store.get(&thread_id).await?;
        let (suite_id, agent_type) = derive_thread_context(&log);
        state.current_suite.insert(thread_id.clone(), suite_id);
        state.current_agent.insert(thread_id.clone(), agent_type);
    }
    let suite_id = state
        .current_suite
        .get(&thread_id)
        .cloned()
        .ok_or_else(|| "suite_id missing for thread".to_string())?;
    let agent = state
        .current_agent
        .get(&thread_id)
        .cloned()
        .ok_or_else(|| "agent_type missing for thread".to_string())?;

    // ack
    let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
    ok.cid = Some(cid.clone());
    {
        let s = serde_json::to_string(&ok).unwrap();
        ws_log_out(&s);
        let _ = write.send(Message::Text(s)).await;
    }

    // record user message
    {
        let store = state.thread_store();
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::User {
                    text: text.clone(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: agent.clone(),
                },
            )
            .await;
    }
    run_suite_and_stream(
        &thread_id,
        &text,
        &suite_id,
        &agent,
        &cid,
        SuiteRunKind::User,
        state,
        write,
    )
    .await
}

async fn process_approve(
    v: &Value,
    state: &mut ConnState,
    write: &mut (impl SinkExt<Message> + Unpin),
) -> Result<(), String> {
    let cid = v
        .get("cid")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "cid required".to_string())?
        .to_string();
    let thread_id = v
        .get("thread_id")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "thread_id required".to_string())?
        .to_string();
    if thread_id.is_empty() {
        return Err("thread_id required".into());
    }
    if uuid::Uuid::parse_str(&thread_id).is_err() {
        return Err("invalid thread_id".into());
    }
    // Reconnect-safe: derive suite/agent from persisted history if not present in connection state
    if !state.current_suite.contains_key(&thread_id)
        || !state.current_agent.contains_key(&thread_id)
    {
        let store = state.thread_store();
        let log = store.get(&thread_id).await?;
        let (suite_id, agent_type) = derive_thread_context(&log);
        state.current_suite.insert(thread_id.clone(), suite_id);
        state.current_agent.insert(thread_id.clone(), agent_type);
    }
    let suite_id = state
        .current_suite
        .get(&thread_id)
        .cloned()
        .ok_or_else(|| "suite_id missing for thread".to_string())?;
    let agent = state
        .current_agent
        .get(&thread_id)
        .cloned()
        .ok_or_else(|| "agent_type missing for thread".to_string())?;
    // ack
    let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
    ok.cid = Some(cid.clone());
    {
        let s = serde_json::to_string(&ok).unwrap();
        ws_log_out(&s);
        let _ = write.send(Message::Text(s)).await;
    }
    // append user=approve step
    {
        let store = state.thread_store();
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::User {
                    text: "approve".to_string(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: agent.clone(),
                },
            )
            .await;
    }
    tracing::info!("approve: thread_id={} agent={}", thread_id, agent);
    run_suite_and_stream(
        &thread_id,
        "Continue.",
        &suite_id,
        &agent,
        &cid,
        SuiteRunKind::User,
        state,
        write,
    )
    .await
}

async fn process_reject(
    v: &Value,
    state: &mut ConnState,
    write: &mut (impl SinkExt<Message> + Unpin),
) -> Result<(), String> {
    let cid = v
        .get("cid")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "cid required".to_string())?
        .to_string();
    let thread_id = v
        .get("thread_id")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "thread_id required".to_string())?
        .to_string();
    if thread_id.is_empty() {
        return Err("thread_id required".into());
    }
    if uuid::Uuid::parse_str(&thread_id).is_err() {
        return Err("invalid thread_id".into());
    }
    // Reconnect-safe: derive suite/agent from persisted history if not present in connection state
    if !state.current_suite.contains_key(&thread_id)
        || !state.current_agent.contains_key(&thread_id)
    {
        let store = state.thread_store();
        let log = store.get(&thread_id).await?;
        let (suite_id, agent_type) = derive_thread_context(&log);
        state.current_suite.insert(thread_id.clone(), suite_id);
        state.current_agent.insert(thread_id.clone(), agent_type);
    }
    let suite_id = state
        .current_suite
        .get(&thread_id)
        .cloned()
        .ok_or_else(|| "suite_id missing for thread".to_string())?;
    let agent = state
        .current_agent
        .get(&thread_id)
        .cloned()
        .ok_or_else(|| "agent_type missing for thread".to_string())?;
    // ack
    let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
    ok.cid = Some(cid.clone());
    {
        let s = serde_json::to_string(&ok).unwrap();
        ws_log_out(&s);
        let _ = write.send(Message::Text(s)).await;
    }
    // append user=reject step
    {
        let store = state.thread_store();
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::User {
                    text: "reject".to_string(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: agent.clone(),
                },
            )
            .await;
    }
    tracing::info!("reject: thread_id={} agent={}", thread_id, agent);
    run_suite_and_stream(
        &thread_id,
        "Continue.",
        &suite_id,
        &agent,
        &cid,
        SuiteRunKind::User,
        state,
        write,
    )
    .await
}
// normalize_agent removed (unused)

fn normalize_agent_new(a: api::new_request::AgentType) -> Result<String, String> {
    match a {
		api::new_request::AgentType::Ask => Ok("ask".to_string()),
		api::new_request::AgentType::Agent => Ok("agent".to_string()),
		api::new_request::AgentType::Review => Ok("review".to_string()),
		api::new_request::AgentType::Kb => Ok("kb".to_string()),
		// No legacy modes: remove cleanse/model from WS-facing agent types.
		api::new_request::AgentType::Cleanse | api::new_request::AgentType::Model => Err(
			"agent_type 'cleanse'/'model' is no longer supported; use agent_type='agent' and let phases handle cleanse vs model."
				.to_string(),
		),
	}
}

fn normalize_agent_open(a: api::open_request::AgentType) -> Result<String, String> {
    match a {
		api::open_request::AgentType::Ask => Ok("ask".to_string()),
		api::open_request::AgentType::Agent => Ok("agent".to_string()),
		api::open_request::AgentType::Review => Ok("review".to_string()),
		api::open_request::AgentType::Kb => Ok("kb".to_string()),
		api::open_request::AgentType::Cleanse | api::open_request::AgentType::Model => Err(
			"agent_type 'cleanse'/'model' is no longer supported; use agent_type='agent' and let phases handle cleanse vs model."
				.to_string(),
		),
	}
}
async fn run_suite_and_stream(
    thread_id: &str,
    question: &str,
    suite_id: &str,
    agent: &str,
    cid: &str,
    kind: SuiteRunKind,
    state: &mut ConnState,
    write: &mut (impl SinkExt<Message> + Unpin),
) -> Result<(), String> {
    run_agent_with_processing_suite(
        thread_id, question, suite_id, agent, cid, kind, state, write,
    )
    .await
}
fn compact_schema_columns(fields: &[arrow::datatypes::FieldRef]) -> Vec<(String, String)> {
    use arrow::datatypes::{DataType, Field};
    fn walk(prefix: &str, f: &Field, depth: usize, out: &mut Vec<(String, String)>) {
        let name = if prefix.is_empty() {
            f.name().to_string()
        } else {
            format!("{}.{}", prefix, f.name())
        };
        match f.data_type() {
            DataType::Struct(inner) if depth < 2 => {
                for child in inner.iter() {
                    walk(&name, child.as_ref(), depth + 1, out);
                }
            }
            dt => {
                out.push((name, format!("{:?}", dt)));
            }
        }
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for f in fields {
        walk("", f.as_ref(), 0, &mut out);
    }
    out
}

// Trace streaming removed (hard cutover).
enum AgentFrame {
    Final {
        kind: String,
        payload: serde_json::Value,
        display: Option<String>,
    },
    Review {
        text: String,
        meta: Option<serde_json::Value>,
    },
    AwaitUser {
        prompt: String,
    },
    AwaitApproval {
        prompt: String,
    },
}

#[derive(Clone, Copy, Debug)]
enum SuiteRunKind {
    New,
    Open,
    User,
}

async fn run_agent_with_processing_suite(
    thread_id: &str,
    question: &str,
    suite_id: &str,
    agent: &str,
    cid: &str,
    kind: SuiteRunKind,
    state: &mut ConnState,
    write: &mut (impl SinkExt<Message> + Unpin),
) -> Result<(), String> {
    let headless = env_bool("REACT_HEADLESS", false) || terminal::enabled();
    let go_text = std::env::var("REACT_HEADLESS_GO")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "go!".to_string());
    let auto_approve = if headless {
        env_bool("REACT_HEADLESS_AUTO_APPROVE", true)
    } else {
        env_bool("REACT_HEADLESS_AUTO_APPROVE", false)
    };

    let mut sctx2 = state.suite_ctx.clone();

    // Preflight is often the implicit "no phase yet" state; emit it explicitly so the UI sees activity.
    {
        let store = state.thread_store();
        if let Ok(Some((step_idx, ts))) =
            ensure_preflight_phase_step(&store, thread_id, agent, Some(suite_id)).await
        {
            let runs = vec![api::PhaseRun::new(ts.clone())];
            let mut ev = api::PhaseResponse::new(
                1,
                api::phase_response::Type::Phase,
                now_iso(),
                state.next_seq(),
                thread_id.to_string(),
                step_idx as i32,
                "preflight".to_string(),
                ts,
                runs,
                0,
            );
            ev.for_cid = Some(cid.to_string());
            ev.reason_code = Some("preflight_start".to_string());
            let s = serde_json::to_string(&api::ServerMessage::Phase(ev)).unwrap();
            state.buffer_last(&s);
            ws_log_out(&s);
            let _ = write.send(Message::Text(s)).await;
        }
    }

    let suite = state
        .reg
        .get(suite_id)
        .ok_or_else(|| format!("invalid suite_id '{}'", suite_id))?
        .clone();
    let spawn_task = |run_kind: SuiteRunKind, q: String| {
        let suite2 = suite.clone();
        let sctx3 = sctx2.clone();
        let thread_id_s = thread_id.to_string();
        let agent_s = agent.to_string();
        tokio::spawn(async move {
            let tid_scope = thread_id_s.clone();
            crate::llm::thread_ctx::scope_thread_id(&tid_scope, async move {
                match run_kind {
                    SuiteRunKind::New => suite2
                        .handle_new(&thread_id_s, &q, &agent_s, &sctx3)
                        .await,
                    SuiteRunKind::Open => suite2
                        .handle_open(&thread_id_s, &q, &agent_s, &sctx3)
                        .await,
                    SuiteRunKind::User => suite2
                        .handle_user(&thread_id_s, &q, &agent_s, &sctx3)
                        .await,
                }
            })
            .await
        })
    };

    let mut agent_task = spawn_task(kind, question.to_string());
    let mut auto_turns: usize = 0;

    // Periodically refresh plan summaries/tasks into thread_state.
    let mut plan_tick = tokio::time::interval(std::time::Duration::from_millis(800));
    plan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_cleanse_plan_fp: Option<String> = None;
    let mut last_model_plan_fp: Option<String> = None;

    // Emit tool_start/tool_end events from persisted thread log steps.
    let mut last_emitted_step_idx: usize = match state.thread_store().get(thread_id).await {
        Ok(log) => log.steps.len(),
        Err(_) => 0,
    };
    let mut tool_tick = tokio::time::interval(std::time::Duration::from_millis(200));
    tool_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Emit strongly-consistent materialized thread_state while running.
    let mut last_state_sent: Option<api::ThreadStateSnapshot> = None;
    let mut state_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    state_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    async fn emit_ws(
        state: &mut ConnState,
        write: &mut (impl SinkExt<Message> + Unpin),
        msg: api::ServerMessage,
    ) {
        if let Some(hub) = state.hub() {
            hub.emit(msg.clone());
        }
        // Best-effort: update terminal from typed messages (no JSON parsing).
        if let Some(t) = state.term() {
            match &msg {
                api::ServerMessage::ThreadState(r) => t.emit(TerminalEvent::ThreadState(r.state.clone())),
                api::ServerMessage::Plans(r) => t.emit(TerminalEvent::Plans {
                    thread_id: r.thread_id.clone(),
                    cleanse: r.cleanse.clone(),
                    model: r.model.clone(),
                }),
                api::ServerMessage::PlansChanged(r) => t.emit(TerminalEvent::PlansChanged(r.clone())),
                api::ServerMessage::Phase(r) => t.emit(TerminalEvent::Phase(r.clone())),
                api::ServerMessage::ToolStart(r) => t.emit(TerminalEvent::ToolStart(r.clone())),
                api::ServerMessage::ToolEnd(r) => t.emit(TerminalEvent::ToolEnd(r.clone())),
                api::ServerMessage::LlmStart(r) => t.emit(TerminalEvent::LlmStart(r.clone())),
                api::ServerMessage::LlmEnd(r) => t.emit(TerminalEvent::LlmEnd(r.clone())),
                _ => {}
            }
        }
        if let Ok(s) = serde_json::to_string(&msg) {
            state.buffer_last(&s);
            ws_log_out(&s);
            let _ = write.send(Message::Text(s)).await;
        }
    }

    loop {
        tokio::select! {
            _ = plan_tick.tick() => {
                let store = state.thread_store();
                let (cleanse, model) = load_latest_plans(&state.suite_ctx, thread_id).await;
                // Emit a lightweight "plans changed" notification so UIs can fetch `plans`.
                {
                    let fp = |p: &Option<api::PlanSnapshot>| -> Option<String> {
                        p.as_ref().and_then(|pp| serde_json::to_string(pp).ok())
                    };
                    let new_cleanse_fp = fp(&cleanse);
                    let new_model_fp = fp(&model);
                    let mut changed: Vec<api::plans_changed_response::Changed> = Vec::new();
                    if new_cleanse_fp != last_cleanse_plan_fp {
                        changed.push(api::plans_changed_response::Changed::Cleanse);
                        last_cleanse_plan_fp = new_cleanse_fp;
                    }
                    if new_model_fp != last_model_plan_fp {
                        changed.push(api::plans_changed_response::Changed::Model);
                        last_model_plan_fp = new_model_fp;
                    }
                    if !changed.is_empty() {
                        let mut ev = api::PlansChangedResponse::new(
                            1,
                            api::plans_changed_response::Type::PlansChanged,
                            now_iso(),
                            state.next_seq(),
                            thread_id.to_string(),
                            changed,
                        );
                        ev.for_cid = Some(cid.to_string());
                        ev.cleanse_plan_key = cleanse.as_ref().map(|p| p.plan_key.clone());
                        ev.model_plan_key = model.as_ref().map(|p| p.plan_key.clone());
                        if let Some(t) = state.term() {
                            t.emit(TerminalEvent::PlansChanged(ev.clone()));
                        }
                        emit_ws(state, write, api::ServerMessage::PlansChanged(ev)).await;

                        // Also emit full plan snapshots so terminal/headless can render hierarchy
                        // without making an explicit `plans` request.
                        let mut pr = api::PlansResponse::new(
                            1,
                            api::plans_response::Type::Plans,
                            now_iso(),
                            state.next_seq(),
                            thread_id.to_string(),
                        );
                        pr.for_cid = Some(cid.to_string());
                        pr.cleanse = cleanse.clone();
                        pr.model = model.clone();
                        emit_ws(state, write, api::ServerMessage::Plans(pr)).await;
                    }
                }
                upsert_thread_state_from_plans(&store, thread_id, &cleanse, &model).await;
            }
            _ = tool_tick.tick() => {
                let store = state.thread_store();
                let log = match store.get(thread_id).await {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                if last_emitted_step_idx >= log.steps.len() {
                    continue;
                }

                fn payload_map(v: &Option<serde_json::Value>) -> Option<std::collections::HashMap<String, serde_json::Value>> {
                    let obj = v.as_ref()?.as_object()?;
                    let mut out: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
                    for (k, vv) in obj.iter() {
                        out.insert(k.clone(), vv.clone());
                    }
                    Some(out)
                }

                for i in last_emitted_step_idx..log.steps.len() {
                    match &log.steps[i] {
                        ThreadStep::Phase { phase, from_phase, reason_code, reason_detail, ts, .. } => {
                            let runs_map = phase_runs_from_steps(&log.steps[..=i]);
                            let runs = runs_map.get(phase).cloned().unwrap_or_default();
                            let total_runtime_ms = total_completed_runtime_ms(&runs);
                            let (from_runs, from_total) =
                                match from_phase.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                                    Some(fp) => {
                                        let rr = runs_map.get(fp).cloned().unwrap_or_default();
                                        let tt = total_completed_runtime_ms(&rr);
                                        (Some(rr), Some(tt))
                                    }
                                    None => (None, None),
                                };

                            let mut ev = api::PhaseResponse::new(
                                1,
                                api::phase_response::Type::Phase,
                                now_iso(),
                                state.next_seq(),
                                thread_id.to_string(),
                                i as i32,
                                phase.clone(),
                                ts.clone(),
                                runs,
                                total_runtime_ms,
                            );
                            ev.for_cid = Some(cid.to_string());
                            ev.from_phase = from_phase.clone();
                            ev.reason_code = reason_code.clone();
                            ev.from_phase_runs = from_runs;
                            ev.from_phase_total_runtime_ms = from_total;
                            ev.reason_detail = reason_detail.as_ref().and_then(|v| v.as_object()).map(|obj| {
                                let mut hm: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
                                for (k, vv) in obj.iter() {
                                    hm.insert(k.clone(), vv.clone());
                                }
                                hm
                            });
                            if let Some(t) = state.term() {
                                t.emit(TerminalEvent::Phase(ev.clone()));
                            }
                            emit_ws(state, write, api::ServerMessage::Phase(ev)).await;
                        }
                        ThreadStep::ToolStart { tool_id, name, clean_name, status, payload, ctx, .. } => {
                            let st = match status.as_str() {
                                "running" => api::ToolEventStatus::Running,
                                "ok" => api::ToolEventStatus::Ok,
                                "failed" => api::ToolEventStatus::Failed,
                                _ => api::ToolEventStatus::Running,
                            };
                            let mut ev = api::ToolStartResponse::new(
                                1,
                                api::tool_start_response::Type::ToolStart,
                                now_iso(),
                                state.next_seq(),
                                thread_id.to_string(),
                                tool_id.clone(),
                                name.clone(),
                                st,
                            );
                            ev.for_cid = Some(cid.to_string());
                            if !clean_name.trim().is_empty() {
                                ev.clean_name = Some(clean_name.clone());
                            }
                            ev.phase = phase_at_step_idx(&log.steps, i);
                            ev.payload = payload_map(payload);
                            ev.ctx = ctx.as_ref().map(|c| {
                                let mut out = api::ExecutionContext::new();
                                out.plan_kind = c.plan_kind.clone();
                                out.plan_key = c.plan_key.clone();
                                out.workgroup_id = c.workgroup_id.clone();
                                out.task_id = c.task_id.clone();
                                out.checklist_item_id = c.checklist_item_id.clone();
                                out
                            });
                            if let Some(t) = state.term() {
                                t.emit(TerminalEvent::ToolStart(ev.clone()));
                            }
                            emit_ws(state, write, api::ServerMessage::ToolStart(ev)).await;
                        }
                        ThreadStep::ToolEnd { tool_id, name, clean_name, status, payload, ctx, observation, .. } => {
                            let st = match status.as_str() {
                                "running" => api::ToolEventStatus::Running,
                                "ok" => api::ToolEventStatus::Ok,
                                "failed" => api::ToolEventStatus::Failed,
                                _ => if observation.ok { api::ToolEventStatus::Ok } else { api::ToolEventStatus::Failed },
                            };
                            let mut ev = api::ToolEndResponse::new(
                                1,
                                api::tool_end_response::Type::ToolEnd,
                                now_iso(),
                                state.next_seq(),
                                thread_id.to_string(),
                                tool_id.clone(),
                                name.clone(),
                                st,
                            );
                            ev.for_cid = Some(cid.to_string());
                            if !clean_name.trim().is_empty() {
                                ev.clean_name = Some(clean_name.clone());
                            }
                            ev.phase = phase_at_step_idx(&log.steps, i);
                            ev.payload = payload_map(payload);
                            if !observation.ok {
                                ev.error = observation.errors.first().cloned();
                            }
                            ev.ctx = ctx.as_ref().map(|c| {
                                let mut out = api::ExecutionContext::new();
                                out.plan_kind = c.plan_kind.clone();
                                out.plan_key = c.plan_key.clone();
                                out.workgroup_id = c.workgroup_id.clone();
                                out.task_id = c.task_id.clone();
                                out.checklist_item_id = c.checklist_item_id.clone();
                                out
                            });
                            if let Some(t) = state.term() {
                                t.emit(TerminalEvent::ToolEnd(ev.clone()));
                            }
                            emit_ws(state, write, api::ServerMessage::ToolEnd(ev)).await;
                        }
                        ThreadStep::LlmStart { call_id, phase, model, ctx, .. } => {
                            let mut ev = api::LlmStartResponse::new(
                                1,
                                api::llm_start_response::Type::LlmStart,
                                now_iso(),
                                state.next_seq(),
                                thread_id.to_string(),
                                *call_id as i32,
                                phase.clone(),
                            );
                            ev.for_cid = Some(cid.to_string());
                            ev.model = model.clone();
                            ev.ctx = ctx.as_ref().map(|c| {
                                let mut out = api::ExecutionContext::new();
                                out.plan_kind = c.plan_kind.clone();
                                out.plan_key = c.plan_key.clone();
                                out.workgroup_id = c.workgroup_id.clone();
                                out.task_id = c.task_id.clone();
                                out.checklist_item_id = c.checklist_item_id.clone();
                                out
                            });
                            if let Some(t) = state.term() {
                                t.emit(TerminalEvent::LlmStart(ev.clone()));
                            }
                            emit_ws(state, write, api::ServerMessage::LlmStart(ev)).await;
                        }
                        ThreadStep::LlmEnd { call_id, phase, model, status, error, ctx, .. } => {
                            let mut ev = api::LlmEndResponse::new(
                                1,
                                api::llm_end_response::Type::LlmEnd,
                                now_iso(),
                                state.next_seq(),
                                thread_id.to_string(),
                                *call_id as i32,
                                phase.clone(),
                                if status == "failed" { api::llm_end_response::Status::Failed } else { api::llm_end_response::Status::Ok },
                            );
                            ev.for_cid = Some(cid.to_string());
                            ev.model = model.clone();
                            ev.error = error.clone();
                            ev.ctx = ctx.as_ref().map(|c| {
                                let mut out = api::ExecutionContext::new();
                                out.plan_kind = c.plan_kind.clone();
                                out.plan_key = c.plan_key.clone();
                                out.workgroup_id = c.workgroup_id.clone();
                                out.task_id = c.task_id.clone();
                                out.checklist_item_id = c.checklist_item_id.clone();
                                out
                            });
                            if let Some(t) = state.term() {
                                t.emit(TerminalEvent::LlmEnd(ev.clone()));
                            }
                            emit_ws(state, write, api::ServerMessage::LlmEnd(ev)).await;
                        }
                        _ => {}
                    }
                }
                last_emitted_step_idx = log.steps.len();
            }
            _ = state_tick.tick() => {
                let store = state.thread_store();
                let st = match store.get_thread_state(thread_id).await {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let snap = ws_thread_state_snapshot_from_core(&st, state.reg.as_ref());
                if last_state_sent.as_ref() == Some(&snap) {
                    continue;
                }
                last_state_sent = Some(snap.clone());
                if let Some(t) = state.term() {
                    t.emit(TerminalEvent::ThreadState(snap.clone()));
                }
                let mut resp = api::ThreadStateResponse::new(
                    1,
                    m::thread_state_response::Type::ThreadState,
                    now_iso(),
                    state.next_seq(),
                    thread_id.to_string(),
                    snap,
                );
                resp.for_cid = Some(cid.to_string());
                emit_ws(state, write, api::ServerMessage::ThreadState(resp)).await;
            }
            res = &mut agent_task => {
                let frames = match res {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => return Err(e),
                    Err(e) => return Err(format!("agent task failed: {}", e)),
                };
                let convert = |ff: react_suites::FlowFrame| -> AgentFrame {
                    match ff {
                        react_suites::FlowFrame::Final { kind, payload, display } => AgentFrame::Final { kind, payload, display },
                        react_suites::FlowFrame::Review { text, meta } => AgentFrame::Review { text, meta },
                        react_suites::FlowFrame::AwaitUser { prompt } => AgentFrame::AwaitUser { prompt },
                        react_suites::FlowFrame::AwaitApproval { prompt } => AgentFrame::AwaitApproval { prompt },
                    }
                };
                let frames = frames.into_iter().map(convert).collect::<Vec<_>>();
                // Continue with normal WS frame emission below.
                let frames = frames;

                let mut rerun: Option<(SuiteRunKind, String)> = None;
                for f in frames {
                    match f {
                        AgentFrame::Review { text, meta } => {
                            let tseq = state.next_thread_seq(thread_id);
                            let mut resp = api::ReviewResponse::new(
                                1,
                                m::review_response::Type::Review,
                                now_iso(),
                                state.next_seq(),
                                thread_id.to_string(),
                                tseq,
                                text.clone(),
                            );
                            // Convert meta value to ReviewResponse's map type (best-effort).
                            if let Some(v) = meta {
                                if let Some(obj) = v.as_object() {
                                    let mut hm: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
                                    for (k, vv) in obj.iter() {
                                        hm.insert(k.clone(), vv.clone());
                                    }
                                    if !hm.is_empty() {
                                        resp.meta = Some(hm);
                                    }
                                }
                            }
                            // Persist meta before moving `resp` into the event sink.
                            let meta_json = resp
                                .meta
                                .as_ref()
                                .and_then(|m| serde_json::to_value(m).ok());
                            emit_ws(state, write, api::ServerMessage::Review(resp)).await;

                            // Persist as its own step so history can show reviewer output.
                            {
                                let store = state.thread_store();
                                let _ = store
                                    .append_step(
                                        thread_id,
                                        ThreadStep::ReviewResponse {
                                            text: text.clone(),
                                            meta: meta_json,
                                            observation: Observation::ok(),
                                            ts: chrono::Utc::now().to_rfc3339(),
                                            agent: agent.to_string(),
                                        }
                                    )
                                    .await;
                            }
                            // Review is non-terminal; continue emitting subsequent frames.
                            continue;
                        }
                        AgentFrame::Final { kind, payload, display } => {
                            // Persist final so it’s durable across reconnects.
                            {
                                let store = state.thread_store();
                                append_step_if_new(
                                    &store,
                                    thread_id,
                                    ThreadStep::Final {
                                        kind: kind.clone(),
                                        payload: payload.clone(),
                                        display: display.clone(),
                                        observation: Observation::ok(),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: agent.to_string(),
                                    },
                                )
                                .await;
                            }
                            let tseq = state.next_thread_seq(thread_id);
                            let final_result = ws_final_result_from_typed_final(&kind, &payload, &display);
                            let resp = api::FinalResponse::new(
                                1,
                                m::final_response::Type::Final,
                                now_iso(),
                                state.next_seq(),
                                thread_id.to_string(),
                                tseq,
                                final_result,
                            );
                            emit_ws(state, write, api::ServerMessage::Final(resp)).await;

                            // Debug: print persisted thread steps
                            {
                                let store = state.thread_store();
                                log_thread_steps_if_enabled(&store, thread_id, "final").await;
                            }
                            return Ok(());
                        }
                        AgentFrame::AwaitUser { prompt } => {
                            // Persist gate so the thread doesn't look like it's still running.
                            {
                                let store = state.thread_store();
                                append_step_if_new(
                                    &store,
                                    thread_id,
                                    ThreadStep::AskUser {
                                        prompt: prompt.clone(),
                                        observation: Observation::ok(),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: agent.to_string(),
                                    },
                                )
                                .await;
                            }
                            if headless && auto_turns < 32 {
                                auto_turns += 1;
                                // Auto-answer with "go!" (headless terminal mode).
                                {
                                    let store = state.thread_store();
                                    let _ = store
                                        .append_step(
                                            thread_id,
                                            ThreadStep::User {
                                                text: go_text.clone(),
                                                observation: Observation::ok(),
                                                ts: chrono::Utc::now().to_rfc3339(),
                                                agent: agent.to_string(),
                                            },
                                        )
                                        .await;
                                }
                                if let Some(t) = state.term() {
                                    t.emit(TerminalEvent::Info(format!(
                                        "headless: auto user='{}' (prompt='{}')",
                                        go_text,
                                        prompt
                                    )));
                                }
                                rerun = Some((SuiteRunKind::User, go_text.clone()));
                                break;
                            }
                            let tseq = state.next_thread_seq(thread_id);
                            let resp = api::AwaitUserResponse::new(
                                1,
                                m::await_user_response::Type::AwaitUser,
                                now_iso(),
                                state.next_seq(),
                                thread_id.to_string(),
                                tseq,
                                prompt,
                            );
                            emit_ws(state, write, api::ServerMessage::AwaitUser(resp)).await;
                            {
                                let store = state.thread_store();
                                log_thread_steps_if_enabled(&store, thread_id, "await_user").await;
                            }
                            return Ok(());
                        }
                        AgentFrame::AwaitApproval { prompt } => {
                            // Persist gate so the thread doesn't look like it's still running.
                            {
                                let store = state.thread_store();
                                append_step_if_new(
                                    &store,
                                    thread_id,
                                    ThreadStep::AskApproval {
                                        prompt: prompt.clone(),
                                        observation: Observation::ok(),
                                        ts: chrono::Utc::now().to_rfc3339(),
                                        agent: agent.to_string(),
                                    },
                                )
                                .await;
                            }
                            if headless && auto_approve && auto_turns < 32 {
                                auto_turns += 1;
                                // Auto-approve (best-effort) to keep headless runs moving.
                                {
                                    let store = state.thread_store();
                                    let _ = store
                                        .append_step(
                                            thread_id,
                                            ThreadStep::User {
                                                text: "approve".to_string(),
                                                observation: Observation::ok(),
                                                ts: chrono::Utc::now().to_rfc3339(),
                                                agent: agent.to_string(),
                                            },
                                        )
                                        .await;
                                }
                                if let Some(t) = state.term() {
                                    t.emit(TerminalEvent::Info(format!(
                                        "headless: auto approve (prompt='{}')",
                                        prompt
                                    )));
                                }
                                rerun = Some((SuiteRunKind::User, "Continue.".to_string()));
                                break;
                            }
                            let tseq = state.next_thread_seq(thread_id);
                            let resp = api::AwaitApprovalResponse::new(
                                1,
                                m::await_approval_response::Type::AwaitApproval,
                                now_iso(),
                                state.next_seq(),
                                thread_id.to_string(),
                                tseq,
                                prompt,
                            );
                            emit_ws(state, write, api::ServerMessage::AwaitApproval(resp)).await;
                            {
                                let store = state.thread_store();
                                log_thread_steps_if_enabled(&store, thread_id, "await_approval").await;
                            }
                            return Ok(());
                        }
                    }
                }
                if let Some((k2, q2)) = rerun {
                    agent_task = spawn_task(k2, q2);
                    continue;
                }
                return Ok(());
            }
        }
    }
}

async fn run_agent_and_frames(
    thread_id: &str,
    question: &str,
    suite_id: &str,
    agent: &str,
    reg: &SuiteRegistry,
    sctx: &SuiteCtx,
) -> Result<Vec<AgentFrame>, String> {
    // Delegate to suites
    let convert = |ff: react_suites::FlowFrame| -> AgentFrame {
        match ff {
            react_suites::FlowFrame::Final {
                kind,
                payload,
                display,
            } => AgentFrame::Final {
                kind,
                payload,
                display,
            },
            react_suites::FlowFrame::Review { text, meta } => AgentFrame::Review { text, meta },
            react_suites::FlowFrame::AwaitUser { prompt } => AgentFrame::AwaitUser { prompt },
            react_suites::FlowFrame::AwaitApproval { prompt } => {
                AgentFrame::AwaitApproval { prompt }
            }
        }
    };
    let suite = reg
        .get(suite_id)
        .ok_or_else(|| format!("invalid suite_id '{}'", suite_id))?;
    let frames = suite
        .handle_open(thread_id, question, agent, sctx)
        .await?
        .into_iter()
        .map(convert)
        .collect();
    Ok(frames)
}

async fn run_user_and_frames(
    thread_id: &str,
    text: &str,
    suite_id: &str,
    agent: &str,
    reg: &SuiteRegistry,
    sctx: &SuiteCtx,
) -> Result<Vec<AgentFrame>, String> {
    let convert = |ff: react_suites::FlowFrame| -> AgentFrame {
        match ff {
            react_suites::FlowFrame::Final {
                kind,
                payload,
                display,
            } => AgentFrame::Final {
                kind,
                payload,
                display,
            },
            react_suites::FlowFrame::Review { text, meta } => AgentFrame::Review { text, meta },
            react_suites::FlowFrame::AwaitUser { prompt } => AgentFrame::AwaitUser { prompt },
            react_suites::FlowFrame::AwaitApproval { prompt } => {
                AgentFrame::AwaitApproval { prompt }
            }
        }
    };
    let suite = reg
        .get(suite_id)
        .ok_or_else(|| format!("invalid suite_id '{}'", suite_id))?;
    let frames = suite
        .handle_user(thread_id, text, agent, sctx)
        .await?
        .into_iter()
        .map(convert)
        .collect();
    Ok(frames)
}

fn chunk_text(s: &str, max_chunk: usize) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    for w in s.split_whitespace() {
        if buf.is_empty() {
            buf.push_str(w);
        } else if buf.len() + 1 + w.len() <= max_chunk {
            buf.push(' ');
            buf.push_str(w);
        } else {
            out.push(buf.clone());
            buf.clear();
            buf.push_str(w);
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

fn compute_unread_for_log(log: &ThreadLog, seen_seq: i32) -> (i32, i32) {
    let mut tseq: i32 = 0;
    let mut assistant_count_after_seen: i32 = 0;
    let mut last_assistant_seq: i32 = 0;
    for step in log.steps.iter() {
        match step {
            ThreadStep::User { .. } => {
                tseq += 1;
            }
            ThreadStep::Final { .. }
            | ThreadStep::AskUser { .. }
            | ThreadStep::AskApproval { .. }
            | ThreadStep::ReviewResponse { .. } => {
                tseq += 1;
                if tseq > seen_seq {
                    assistant_count_after_seen += 1;
                }
                last_assistant_seq = tseq;
            }
            _ => {}
        }
    }
    (last_assistant_seq, assistant_count_after_seen)
}

async fn build_history(
    store: &ThreadStore,
    thread_id: &str,
    before: Option<i32>,
    limit_opt: Option<i32>,
) -> Result<(Vec<api::HistoryResponseMessagesInner>, Option<i32>), String> {
    let mut msgs: Vec<api::HistoryResponseMessagesInner> = Vec::new();
    let mut next_before: Option<i32> = None;
    let limit = limit_opt.unwrap_or(50).max(1);
    let log = store.get(thread_id).await?;
    let mut tseq: i32 = 0;
    let mut all_msgs: Vec<api::HistoryResponseMessagesInner> = Vec::new();
    for step in log.steps.iter() {
        match step {
            ThreadStep::User { text, .. } => {
                tseq += 1;
                all_msgs.push(api::HistoryResponseMessagesInner {
                    thread_seq: tseq,
                    role: m::history_response_messages_inner::Role::User,
                    content: text.to_string(),
                    created_at: step.ts().to_string(),
                });
            }
            ThreadStep::Final {
                payload, display, ..
            } => {
                tseq += 1;
                let content = display
                    .clone()
                    .or_else(|| {
                        payload
                            .get("text")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                    })
                    .or_else(|| {
                        payload
                            .get("answer")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| final_display_text_from_payload(payload));
                all_msgs.push(api::HistoryResponseMessagesInner {
                    thread_seq: tseq,
                    role: m::history_response_messages_inner::Role::Assistant,
                    content,
                    created_at: step.ts().to_string(),
                });
            }
            ThreadStep::ReviewResponse { text, .. } => {
                tseq += 1;
                all_msgs.push(api::HistoryResponseMessagesInner {
                    thread_seq: tseq,
                    role: m::history_response_messages_inner::Role::Assistant,
                    content: text.to_string(),
                    created_at: step.ts().to_string(),
                });
            }
            ThreadStep::AskUser { prompt, .. } => {
                tseq += 1;
                all_msgs.push(api::HistoryResponseMessagesInner {
                    thread_seq: tseq,
                    role: m::history_response_messages_inner::Role::Assistant,
                    content: prompt.to_string(),
                    created_at: step.ts().to_string(),
                });
            }
            ThreadStep::AskApproval { prompt, .. } => {
                tseq += 1;
                all_msgs.push(api::HistoryResponseMessagesInner {
                    thread_seq: tseq,
                    role: m::history_response_messages_inner::Role::Assistant,
                    content: prompt.to_string(),
                    created_at: step.ts().to_string(),
                });
            }
            _ => {}
        }
    }
    // apply before and limit
    let mut filtered: Vec<api::HistoryResponseMessagesInner> = if let Some(b) = before {
        all_msgs.into_iter().filter(|m| m.thread_seq < b).collect()
    } else {
        all_msgs
    };
    let total = filtered.len() as i32;
    if total > limit {
        let start = (total - limit) as usize;
        let trimmed = filtered.split_off(start);
        let first_seq = trimmed.first().map(|m| m.thread_seq).unwrap_or(0);
        next_before = if first_seq > 1 { Some(first_seq) } else { None };
        msgs = trimmed;
    } else {
        msgs = filtered;
        next_before = None;
    }
    Ok((msgs, next_before))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{DefaultKeyspace, RequestScope};
    use async_trait::async_trait;
    use futures_util::sink::Sink;
    use react_core::keyspace::Keyspace;
    use react_core::llm::NullModel;
    use react_core::providers::NullSecretsProvider;
    use react_core::storage::InMemoryStorageAdapter;
    use serde_json::json;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use std::time::Duration;

    #[test]
    fn thread_state_snapshot_maps_ctx_from_core_event_field() {
        let mut core = CoreThreadState::default();
        core.thread_state_schema_version = 1;
        core.thread_id = "tid".to_string();
        core.suite_id = Some("data_engineer".to_string());
        core.agent_type = Some("agent".to_string());
        core.current_phase = Some("preflight".to_string());
        core.events = vec![react_core::session::ThreadEvent {
            step_idx: 0,
            event_kind: "tool_start".to_string(),
            ts: "t".to_string(),
            ctx: Some(react_core::session::ExecutionContext {
                plan_kind: Some("cleanse".to_string()),
                plan_key: Some("p1".to_string()),
                workgroup_id: Some("wg1".to_string()),
                task_id: Some("task1".to_string()),
                checklist_item_id: Some("sql_model".to_string()),
            }),
            ..Default::default()
        }];
        let reg = react_suites::registry::SuiteRegistry::new();
        let snap = ws_thread_state_snapshot_from_core(&core, &reg);
        let ctx = snap.events[0].ctx.as_ref().expect("ctx");
        assert_eq!(ctx.plan_kind.as_deref(), Some("cleanse"));
        assert_eq!(ctx.plan_key.as_deref(), Some("p1"));
        assert_eq!(ctx.workgroup_id.as_deref(), Some("wg1"));
        assert_eq!(ctx.task_id.as_deref(), Some("task1"));
        assert_eq!(ctx.checklist_item_id.as_deref(), Some("sql_model"));
    }

    #[derive(Clone, Default)]
    struct CollectSink {
        out: Arc<Mutex<Vec<String>>>,
    }

    impl Sink<Message> for CollectSink {
        type Error = std::convert::Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            if let Message::Text(s) = item {
                self.out.lock().unwrap().push(s);
            }
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    struct StubDataEngineerSuite;

    #[async_trait]
    impl react_suites::suite::Suite for StubDataEngineerSuite {
        fn id(&self) -> &'static str {
            "data_engineer"
        }

        fn phase_order(&self, _agent_type: &str) -> Vec<String> {
            vec!["preflight".to_string(), "done".to_string()]
        }

        async fn handle_new(
            &self,
            _thread_id: &str,
            _question: &str,
            _agent_type: &str,
            _ctx: &SuiteCtx,
        ) -> Result<Vec<react_suites::FlowFrame>, String> {
            Ok(vec![react_suites::FlowFrame::Final {
                kind: "ask".to_string(),
                payload: serde_json::json!({"answer":"ok","sql":"SELECT 1"}),
                display: Some("ok".to_string()),
            }])
        }

        async fn handle_open(
            &self,
            _thread_id: &str,
            _question: &str,
            _agent_type: &str,
            _ctx: &SuiteCtx,
        ) -> Result<Vec<react_suites::FlowFrame>, String> {
            Ok(vec![react_suites::FlowFrame::Final {
                kind: "ask".to_string(),
                payload: serde_json::json!({"answer":"ok","sql":"SELECT 1"}),
                display: Some("ok".to_string()),
            }])
        }

        async fn handle_user(
            &self,
            thread_id: &str,
            _text: &str,
            _agent_type: &str,
            ctx: &SuiteCtx,
        ) -> Result<Vec<react_suites::FlowFrame>, String> {
            // Emit a tool_start/tool_end pair into the durable thread log so WS can stream tool events.
            let store =
                ThreadStore::new(ctx.storage.clone(), ctx.scope.clone(), ctx.keyspace.clone());
            let tool_id = "t1".to_string();
            let _ = store
                .append_step(
                    thread_id,
                    ThreadStep::ToolStart {
                        tool_id: tool_id.clone(),
                        name: "dbt_files".to_string(),
                        clean_name: "dbt_files patch".to_string(),
                        args: serde_json::json!({"op":"patch"}),
                        status: "running".to_string(),
                        payload: Some(serde_json::json!({"hint":"starting"})),
                        ctx: None,
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: "agent".to_string(),
                    },
                )
                .await;
            let _ = store
                .append_step(
                    thread_id,
                    ThreadStep::ToolEnd {
                        tool_id: tool_id.clone(),
                        name: "dbt_files".to_string(),
                        clean_name: "dbt_files patch".to_string(),
                        args: serde_json::json!({"op":"patch"}),
                        status: "ok".to_string(),
                        payload: Some(serde_json::json!({"written_keys": []})),
                        ctx: None,
                        observation: ToolObservation::normalize(serde_json::json!({"ok": true})),
                        ts: chrono::Utc::now().to_rfc3339(),
                        agent: "agent".to_string(),
                    },
                )
                .await;

            // Sleep long enough for WS ticks (tool/state) to emit at least once.
            tokio::time::sleep(Duration::from_millis(650)).await;
            Ok(vec![react_suites::FlowFrame::Final {
                kind: "ask".to_string(),
                payload: serde_json::json!({"answer":"ok","sql":"SELECT 1"}),
                display: Some("ok".to_string()),
            }])
        }
    }

    struct StubAwaitApprovalSuite;

    #[async_trait]
    impl react_suites::suite::Suite for StubAwaitApprovalSuite {
        fn id(&self) -> &'static str {
            "data_engineer"
        }

        fn phase_order(&self, _agent_type: &str) -> Vec<String> {
            vec!["preflight".to_string(), "done".to_string()]
        }

        async fn handle_new(
            &self,
            _thread_id: &str,
            _question: &str,
            _agent_type: &str,
            _ctx: &SuiteCtx,
        ) -> Result<Vec<react_suites::FlowFrame>, String> {
            Ok(vec![react_suites::FlowFrame::AwaitApproval {
                prompt: "approve?".to_string(),
            }])
        }

        async fn handle_open(
            &self,
            _thread_id: &str,
            _question: &str,
            _agent_type: &str,
            _ctx: &SuiteCtx,
        ) -> Result<Vec<react_suites::FlowFrame>, String> {
            Ok(vec![react_suites::FlowFrame::AwaitApproval {
                prompt: "approve?".to_string(),
            }])
        }

        async fn handle_user(
            &self,
            _thread_id: &str,
            _text: &str,
            _agent_type: &str,
            _ctx: &SuiteCtx,
        ) -> Result<Vec<react_suites::FlowFrame>, String> {
            Ok(vec![react_suites::FlowFrame::AwaitApproval {
                prompt: "approve?".to_string(),
            }])
        }
    }

    #[test]
    fn normalize_agent_includes_agent_and_review() {
        assert_eq!(
            normalize_agent_new(api::new_request::AgentType::Agent).unwrap(),
            "agent"
        );
        assert_eq!(
            normalize_agent_new(api::new_request::AgentType::Review).unwrap(),
            "review"
        );
        assert_eq!(
            normalize_agent_open(api::open_request::AgentType::Agent).unwrap(),
            "agent"
        );
        assert_eq!(
            normalize_agent_open(api::open_request::AgentType::Review).unwrap(),
            "review"
        );
        assert!(normalize_agent_new(api::new_request::AgentType::Cleanse).is_err());
        assert!(normalize_agent_new(api::new_request::AgentType::Model).is_err());
        assert!(normalize_agent_open(api::open_request::AgentType::Cleanse).is_err());
        assert!(normalize_agent_open(api::open_request::AgentType::Model).is_err());
    }

    #[test]
    fn derive_thread_context_ignores_step_agent_labels() {
        let log = ThreadLog {
            steps: vec![
                ThreadStep::SwitchSuite {
                    from: None,
                    to: "data_engineer".to_string(),
                    observation: Observation::ok(),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
                ThreadStep::SwitchAgent {
                    from: None,
                    to: "agent".to_string(),
                    observation: Observation::ok(),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
                // Inner phase/tool steps may record agent labels like "cleanse" — these must NOT
                // override the user-selected agent_type derived from switch_agent.
                ThreadStep::ToolEnd {
                    tool_id: "t".to_string(),
                    name: "sql_schema".to_string(),
                    clean_name: "List tables".to_string(),
                    args: json!({}),
                    status: "ok".to_string(),
                    payload: None,
                    ctx: None,
                    observation: ToolObservation::normalize(json!({"ok": true})),
                    ts: "t".to_string(),
                    agent: "cleanse".to_string(),
                },
            ],
            ..Default::default()
        };
        let (_suite, agent_type) = derive_thread_context(&log);
        assert_eq!(agent_type, "agent");
    }

    #[test]
    fn unread_counts_include_review_response() {
        let log = ThreadLog {
            steps: vec![
                ThreadStep::User {
                    text: "hi".to_string(),
                    observation: Observation::ok(),
                    ts: "t".to_string(),
                    agent: "ask".to_string(),
                },
                ThreadStep::ReviewResponse {
                    text: "review text".to_string(),
                    meta: None,
                    observation: Observation::ok(),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
                ThreadStep::Final {
                    kind: "generic".to_string(),
                    payload: serde_json::json!({ "text": "done" }),
                    display: Some("done".to_string()),
                    observation: Observation::ok(),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
            ],
            ..Default::default()
        };
        // No seen messages yet -> both assistant messages should count as unread.
        let (_max_seq, unread) = compute_unread_for_log(&log, 0);
        assert_eq!(unread, 2);
    }

    #[tokio::test]
    async fn history_includes_review_response_as_assistant_message() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let store = ThreadStore::new(storage, scope, keyspace);

        let tid = "thread1";
        let _ = store
            .append_step(
                tid,
                ThreadStep::User {
                    text: "start".to_string(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "ask".to_string(),
                },
            )
            .await;

        let _ = store
            .append_step(
                tid,
                ThreadStep::ReviewResponse {
                    text: "review text".to_string(),
                    meta: None,
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "agent".to_string(),
                },
            )
            .await;

        let (msgs, _next) = build_history(&store, tid, None, Some(50)).await.unwrap();
        assert!(msgs.iter().any(
            |m| m.role == m::history_response_messages_inner::Role::Assistant
                && m.content == "review text"
        ));
    }

    #[tokio::test]
    async fn suites_request_requires_cid_and_v() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage,
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope,
            keyspace,
        );
        let reg = Arc::new(react_suites::default_registry());
        let mut state = ConnState::new(reg, suite_ctx, None);

        // Missing required fields should fail strict parsing.
        let bad = json!({"type":"suites"}).to_string();
        assert!(handle_message(&bad, &mut state).await.is_err());
    }

    #[tokio::test]
    async fn delete_request_requires_cid_and_v() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage,
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope,
            keyspace,
        );
        let reg = Arc::new(react_suites::default_registry());
        let mut state = ConnState::new(reg, suite_ctx, None);

        let bad = json!({"type":"delete","thread_id":"not-a-uuid"}).to_string();
        assert!(handle_message(&bad, &mut state).await.is_err());
    }

    #[tokio::test]
    async fn headless_run_requires_existing_thread_when_thread_id_provided() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage,
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope,
            keyspace,
        );
        let missing = uuid::Uuid::new_v4().to_string();
        let hub = EventHub::new(256);
        let err = run_headless_with_hub(
            suite_ctx,
            Some(missing.clone()),
            "data_engineer".to_string(),
            "agent".to_string(),
            hub,
        )
        .await
        .unwrap_err();
        assert!(err.contains("thread does not exist"));
        assert!(err.contains(&missing));
    }

    #[tokio::test]
    async fn plans_request_returns_latest_cleanse_and_model_when_present() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage.clone(),
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope.clone(),
            keyspace.clone(),
        );
        let reg = Arc::new(react_suites::default_registry());
        let mut state = ConnState::new(reg, suite_ctx.clone(), None);

        let thread_id = uuid::Uuid::new_v4().to_string();
        let base = keyspace
            .threads_prefix(&scope)
            .trim_end_matches("/threads")
            .trim_end_matches('/')
            .to_string();

        let cleanse_key = format!("{}/plans/{}/20260126T000000Z_cleanse.json", base, thread_id);
        let cleanse = de_plan::CleansePlan {
            plan_key: cleanse_key.clone(),
            status: de_plan::PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![],
            batches: vec![],
            work_groups: vec![],
            progress: de_plan::PlanProgress::default(),
        };
        suite_ctx
            .storage
            .put_bytes(
                &cleanse_key,
                &serde_json::to_vec_pretty(&cleanse).unwrap(),
                "application/json",
            )
            .await
            .unwrap();

        let model_key = format!("{}/plans/{}/20260126T000000Z_model.json", base, thread_id);
        let model = de_plan::ModelPlan {
            plan_key: model_key.clone(),
            status: de_plan::PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![],
            batches: vec![],
            work_groups: vec![],
            progress: de_plan::PlanProgress::default(),
        };
        suite_ctx
            .storage
            .put_bytes(
                &model_key,
                &serde_json::to_vec_pretty(&model).unwrap(),
                "application/json",
            )
            .await
            .unwrap();

        let msg = json!({"v":1,"type":"plans","cid":"c1","thread_id":thread_id}).to_string();
        let frames = handle_message(&msg, &mut state).await.unwrap();
        assert_eq!(frames.len(), 1);
        let resp: api::PlansResponse = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(resp.for_cid.as_deref(), Some("c1"));
        assert!(resp.cleanse.is_some());
        assert!(resp.model.is_some());
    }

    #[tokio::test]
    async fn plans_request_includes_checklist_and_omits_notes() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage.clone(),
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope.clone(),
            keyspace.clone(),
        );
        let reg = Arc::new(react_suites::default_registry());
        let mut state = ConnState::new(reg, suite_ctx.clone(), None);

        let thread_id = uuid::Uuid::new_v4().to_string();
        let base = keyspace
            .threads_prefix(&scope)
            .trim_end_matches("/threads")
            .trim_end_matches('/')
            .to_string();

        let cleanse_key = format!("{}/plans/{}/20260126T000000Z_cleanse.json", base, thread_id);
        let cleanse = de_plan::CleansePlan {
            plan_key: cleanse_key.clone(),
            status: de_plan::PlanStatus::Approved,
            project_snapshot: serde_json::json!({}),
            tasks: vec![de_plan::CleanseTask {
                dataset_id: "AwsDataCatalog.test_raw.raw_orders".to_string(),
                expected_model_path: Some("models/staging/stg_test_raw_raw_orders.sql".to_string()),
                invariants: vec![],
                status: de_plan::TaskStatus::Pending,
                checklist: vec![de_plan::PlanChecklistItem {
                    checklist_item_id: "sql_model".to_string(),
                    label: "Author staging SQL".to_string(),
                    details: None,
                    status: de_plan::ChecklistItemStatus::Pending,
                    origin: de_plan::ChecklistOrigin::Initial,
                    origin_step_idx: None,
                    evidence: vec![],
                }],
            }],
            batches: vec![vec!["AwsDataCatalog.test_raw.raw_orders".to_string()]],
            work_groups: vec![],
            progress: de_plan::PlanProgress::default(),
        };
        suite_ctx
            .storage
            .put_bytes(
                &cleanse_key,
                &serde_json::to_vec_pretty(&cleanse).unwrap(),
                "application/json",
            )
            .await
            .unwrap();

        let msg = json!({"v":1,"type":"plans","cid":"c1","thread_id":thread_id}).to_string();
        let frames = handle_message(&msg, &mut state).await.unwrap();
        assert_eq!(frames.len(), 1);

        let v: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        let cleanse_snap = v.get("cleanse").expect("cleanse");
        let tasks = cleanse_snap.get("tasks").and_then(|x| x.as_array()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].get("taskKind").and_then(|x| x.as_str()),
            Some("cleanse")
        );
        let cl = tasks[0].get("checklist").and_then(|x| x.as_array()).unwrap();
        assert_eq!(
            cl[0].get("checklistItemId").and_then(|x| x.as_str()),
            Some("sql_model")
        );
        assert!(tasks[0].get("notes").is_none());
    }

    #[tokio::test]
    async fn plans_request_surfaces_parse_error_snapshot_for_corrupt_plan_json() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage.clone(),
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope.clone(),
            keyspace.clone(),
        );
        let reg = Arc::new(react_suites::default_registry());
        let mut state = ConnState::new(reg, suite_ctx.clone(), None);

        let thread_id = uuid::Uuid::new_v4().to_string();
        let base = keyspace
            .threads_prefix(&scope)
            .trim_end_matches("/threads")
            .trim_end_matches('/')
            .to_string();
        let cleanse_key = format!("{}/plans/{}/20260126T000000Z_cleanse.json", base, thread_id);
        suite_ctx
            .storage
            .put_bytes(&cleanse_key, b"{not valid json", "application/json")
            .await
            .unwrap();

        let msg = json!({"v":1,"type":"plans","cid":"c1","thread_id":thread_id}).to_string();
        let frames = handle_message(&msg, &mut state).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        let cleanse_snap = v.get("cleanse").expect("cleanse");
        assert_eq!(
            cleanse_snap.get("status").and_then(|x| x.as_str()),
            Some("cancelled")
        );
        let tasks = cleanse_snap.get("tasks").and_then(|x| x.as_array()).unwrap();
        let cl = tasks[0].get("checklist").and_then(|x| x.as_array()).unwrap();
        assert_eq!(
            cl[0].get("checklistItemId").and_then(|x| x.as_str()),
            Some("parse_error")
        );
        let ps = cleanse_snap
            .get("projectSnapshot")
            .and_then(|x| x.as_object())
            .expect("projectSnapshot");
        assert!(ps.get("parse_error").is_some());
    }

    #[tokio::test]
    async fn open_emits_thread_state_frame() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage.clone(),
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope.clone(),
            keyspace.clone(),
        );
        let mut reg = react_suites::registry::SuiteRegistry::new();
        reg.register(StubDataEngineerSuite);
        let reg = Arc::new(reg);
        let mut state = ConnState::new(reg, suite_ctx.clone(), None);

        // Seed empty thread log so process_open can load it.
        let thread_id = uuid::Uuid::new_v4().to_string();
        let store = ThreadStore::new(storage.clone(), scope.clone(), keyspace.clone());
        // minimal steps so derive_thread_context has something (optional)
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::SwitchSuite {
                    from: None,
                    to: "data_engineer".to_string(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "agent".to_string(),
                },
            )
            .await;

        let msg = json!({"v":1,"type":"open","cid":"c1","thread_id":thread_id,"suiteId":"data_engineer","agentType":"agent","question":"Continue."});
        let mut sink = CollectSink::default();
        process_open(&msg, &mut state, &mut sink).await.unwrap();

        let frames = sink.out.lock().unwrap().clone();
        assert!(frames.iter().any(|s| {
            serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t == "thread_state")
                })
                .unwrap_or(false)
        }));
    }

    #[tokio::test]
    async fn open_persists_final_step_in_thread_log() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage.clone(),
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope.clone(),
            keyspace.clone(),
        );
        let mut reg = react_suites::registry::SuiteRegistry::new();
        reg.register(StubDataEngineerSuite);
        let reg = Arc::new(reg);
        let mut state = ConnState::new(reg, suite_ctx.clone(), None);

        let thread_id = uuid::Uuid::new_v4().to_string();
        let store = ThreadStore::new(storage.clone(), scope.clone(), keyspace.clone());
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::SwitchSuite {
                    from: None,
                    to: "data_engineer".to_string(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "agent".to_string(),
                },
            )
            .await;

        let msg = json!({"v":1,"type":"open","cid":"c1","thread_id":thread_id,"suiteId":"data_engineer","agentType":"agent","question":"Continue."});
        let mut sink = CollectSink::default();
        process_open(&msg, &mut state, &mut sink).await.unwrap();

        let log = store.get(&thread_id).await.unwrap();
        assert!(matches!(log.steps.last(), Some(ThreadStep::Final { .. })));
    }

    #[tokio::test]
    async fn open_persists_await_approval_step_in_thread_log() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage.clone(),
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope.clone(),
            keyspace.clone(),
        );
        let mut reg = react_suites::registry::SuiteRegistry::new();
        reg.register(StubAwaitApprovalSuite);
        let reg = Arc::new(reg);
        let mut state = ConnState::new(reg, suite_ctx.clone(), None);

        let thread_id = uuid::Uuid::new_v4().to_string();
        let store = ThreadStore::new(storage.clone(), scope.clone(), keyspace.clone());
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::SwitchSuite {
                    from: None,
                    to: "data_engineer".to_string(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "agent".to_string(),
                },
            )
            .await;

        let msg = json!({"v":1,"type":"open","cid":"c1","thread_id":thread_id,"suiteId":"data_engineer","agentType":"agent","question":"Continue."});
        let mut sink = CollectSink::default();
        process_open(&msg, &mut state, &mut sink).await.unwrap();

        let log = store.get(&thread_id).await.unwrap();
        assert!(matches!(
            log.steps.last(),
            Some(ThreadStep::AskApproval { .. })
        ));
    }

    #[tokio::test]
    async fn user_request_streams_thread_state_and_tool_events() {
        let storage = Arc::new(InMemoryStorageAdapter::default());
        let scope = RequestScope {
            tenant: "t".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
        };
        let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
        let suite_ctx = SuiteCtx::new(
            storage.clone(),
            Arc::new(NullSecretsProvider::default()),
            Arc::new(NullModel::new()),
            scope.clone(),
            keyspace.clone(),
        );

        // Seed a thread with durable suite/agent selection so process_user can derive context.
        let thread_id = uuid::Uuid::new_v4().to_string();
        let store = ThreadStore::new(storage.clone(), scope.clone(), keyspace.clone());
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::SwitchSuite {
                    from: None,
                    to: "data_engineer".to_string(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "agent".to_string(),
                },
            )
            .await;
        let _ = store
            .append_step(
                &thread_id,
                ThreadStep::SwitchAgent {
                    from: None,
                    to: "agent".to_string(),
                    observation: Observation::ok(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    agent: "agent".to_string(),
                },
            )
            .await;

        let mut reg = react_suites::registry::SuiteRegistry::new();
        reg.register(StubDataEngineerSuite);
        let reg = Arc::new(reg);
        let mut state = ConnState::new(reg, suite_ctx, None);

        let msg = json!({"v":1,"type":"user","cid":"c1","thread_id":thread_id,"text":"continue"});
        let mut sink = CollectSink::default();
        process_user(&msg, &mut state, &mut sink).await.unwrap();

        let frames = sink.out.lock().unwrap().clone();
        assert!(!frames.is_empty());
        let mut saw_thread_state = false;
        let mut saw_tool_start = false;
        let mut saw_tool_end = false;
        let mut saw_tool_phase = false;
        for s in frames {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "thread_state" => saw_thread_state = true,
                    "tool_start" => saw_tool_start = true,
                    "tool_end" => saw_tool_end = true,
                    _ => {}
                }
                if matches!(
                    v.get("type").and_then(|t| t.as_str()),
                    Some("tool_start" | "tool_end")
                ) {
                    if v.get("phase")
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .trim()
                        .is_empty()
                        == false
                    {
                        saw_tool_phase = true;
                    }
                }
            }
        }
        assert!(saw_thread_state);
        assert!(saw_tool_start);
        assert!(saw_tool_end);
        assert!(saw_tool_phase);
    }
}
