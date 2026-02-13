//! Terminal stdout renderer for the ReAct WS server.
//!
//! Goal: show a concise, modern developer-focused view of what's happening:
//! phases, current status, workgroups/tasks, and tool/LLM activity.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::io::IsTerminal as _;
use std::io::Write as _;
use std::time::{Duration, Instant};

use once_cell::sync::OnceCell;
use serde_json::Value;

use crate::ws::api_gen::src::models as api;

use crossterm::cursor::{Hide, MoveTo, MoveToColumn, Show};
use crossterm::style::Stylize as _;
use crossterm::terminal::{self, Clear, ClearType};

#[derive(Clone, Debug)]
pub enum TerminalEvent {
    Info(String),
    Shutdown,
    ThreadState(api::ThreadStateSnapshot),
    Plans {
        thread_id: String,
        cleanse: Option<api::PlanSnapshot>,
        model: Option<api::PlanSnapshot>,
    },
    DbtProgress {
        /// dbt subcommand label emitted by the runner (e.g. "compile", "build")
        phase: String,
        /// Progress summary (e.g. "4 of 6 PASS")
        detail: String,
    },
    PlansChanged(api::PlansChangedResponse),
    Phase(api::PhaseResponse),
    ToolStart(api::ToolStartResponse),
    ToolEnd(api::ToolEndResponse),
    LlmStart(api::LlmStartResponse),
    LlmEnd(api::LlmEndResponse),
    RawJson(String),
}

#[derive(Clone)]
pub struct TerminalSink {
    tx: std::sync::mpsc::Sender<TerminalEvent>,
}

static SINK: OnceCell<TerminalSink> = OnceCell::new();

pub fn enabled() -> bool {
    SINK.get().is_some()
}

pub fn sink() -> Option<&'static TerminalSink> {
    SINK.get()
}

impl TerminalSink {
    pub fn emit(&self, ev: TerminalEvent) {
        let _ = self.tx.send(ev);
    }
}

/// Initialize the global terminal renderer.
///
/// Safe to call multiple times; subsequent calls are no-ops.
pub fn init() -> Result<(), String> {
    if SINK.get().is_some() {
        return Ok(());
    }
    if !std::io::stdout().is_terminal() {
        return Err("stdout is not a TTY; terminal mode disabled".to_string());
    }

    let (tx, rx) = std::sync::mpsc::channel::<TerminalEvent>();
    let sink = TerminalSink { tx };
    let _ = SINK.set(sink);

    std::thread::spawn(move || {
        let _ = run_terminal(rx);
    });
    Ok(())
}

pub fn shutdown() {
    if let Some(s) = sink() {
        s.emit(TerminalEvent::Shutdown);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SpanStatus {
    Running,
    Ok,
    Failed,
}

#[derive(Clone, Debug)]
struct SpanAgg {
    status: SpanStatus,
    label: String,
    description: Option<String>,
    count: usize,
    max_dur: Duration,
    order_idx: usize,
}

#[derive(Clone, Debug)]
struct SpanState {
    label: String,
    detail: Option<String>,
    description: Option<String>,
    status: SpanStatus,
    started_at: Instant,
    ended_at: Option<Instant>,
    phase: Option<String>,
    ctx: Option<api::ExecutionContext>,
}

#[derive(Clone, Debug)]
struct ThreadView {
    thread_id: String,
    suite_id: Option<String>,
    agent_type: Option<String>,
    current_phase: Option<String>,
    phases: Vec<String>,
    completed_phases: Vec<String>,
    total_runtime_ms: Option<i64>,
    items: BTreeMap<String, api::ThreadStateItem>,
    // Plan snapshots are optional; stored when clients request `plans` or when server loads them.
    cleanse_plan: Option<api::PlanSnapshot>,
    model_plan: Option<api::PlanSnapshot>,
    cleanse_plan_anchor_phase: Option<String>,
    model_plan_anchor_phase: Option<String>,
    tool_spans: HashMap<String, SpanState>, // tool_id -> span
    llm_spans: HashMap<i32, SpanState>,     // call_id -> span
    // Persist the most recent concrete work item context per plan kind
    // so focused rendering doesn't "disappear" when some events arrive without ctx.
    focus_by_kind: HashMap<String, WorkItemKey>,
    // Sticky expansion: once a checklist work item is focused, keep its checklist line visible.
    expanded_work_items: std::collections::HashSet<WorkItemKey>,
    // Latest transition metadata for each entered phase.
    phase_reason_code: HashMap<String, String>,
    phase_reason_detail: HashMap<String, Value>,
    last_update: Instant,
}

impl Default for ThreadView {
    fn default() -> Self {
        Self {
            thread_id: String::new(),
            suite_id: None,
            agent_type: None,
            current_phase: None,
            phases: Vec::new(),
            completed_phases: Vec::new(),
            total_runtime_ms: None,
            items: BTreeMap::new(),
            cleanse_plan: None,
            model_plan: None,
            cleanse_plan_anchor_phase: None,
            model_plan_anchor_phase: None,
            tool_spans: HashMap::new(),
            llm_spans: HashMap::new(),
            focus_by_kind: HashMap::new(),
            expanded_work_items: std::collections::HashSet::new(),
            phase_reason_code: HashMap::new(),
            phase_reason_detail: HashMap::new(),
            last_update: Instant::now(),
        }
    }
}

struct Model {
    threads: BTreeMap<String, ThreadView>,
    selected: Option<String>,
    started: Instant,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            threads: BTreeMap::new(),
            selected: None,
            started: Instant::now(),
        }
    }
}

fn run_terminal(rx: std::sync::mpsc::Receiver<TerminalEvent>) -> io::Result<()> {
    let mut stdout = io::stdout();
    let tick = Duration::from_millis(80);
    let mut m = Model {
        started: Instant::now(),
        ..Default::default()
    };
    let mut prev_lines: Vec<String> = Vec::new();

    // Hide cursor while we refresh in-place.
    let _ = crossterm::execute!(stdout, Hide);

    // Initial paint.
    refresh_stdout(&mut stdout, &mut prev_lines, render_model(&m))?;

    'ui: loop {
        // Wait for at least one event or a tick.
        match rx.recv_timeout(tick) {
            Ok(ev) => {
                if matches!(ev, TerminalEvent::Shutdown) {
                    break 'ui;
                }
                apply_event(&mut m, ev);
                // Drain additional queued events.
                while let Ok(ev2) = rx.try_recv() {
                    if matches!(ev2, TerminalEvent::Shutdown) {
                        break 'ui;
                    }
                    apply_event(&mut m, ev2);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Periodic refresh tick.
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break 'ui,
        }

        refresh_stdout(&mut stdout, &mut prev_lines, render_model(&m))?;
    }

    // Cleanup terminal.
    let _ = crossterm::execute!(stdout, Show);
    Ok(())
}

fn refresh_stdout(
    stdout: &mut io::Stdout,
    prev_lines: &mut Vec<String>,
    new_lines: Vec<String>,
) -> io::Result<()> {
    let (w, h) = terminal::size().unwrap_or((120, 40));
    let max_lines = h.max(3) as usize;
    let mut new_lines: Vec<String> = new_lines.into_iter().map(|l| fit_line(&l, w)).collect();

    // Bound render to the viewport. If it doesn't fit, truncate and add an ellipsis line.
    if new_lines.len() > max_lines {
        new_lines.truncate(max_lines);
        if let Some(last) = new_lines.last_mut() {
            *last = "…".to_string();
        }
    }

    // Skip all terminal writes when the frame is byte-for-byte identical.
    if *prev_lines == new_lines {
        return Ok(());
    }

    // Initial paint: clear the viewport region once so stale shell content disappears.
    if prev_lines.is_empty() {
        crossterm::execute!(stdout, MoveTo(0, 0), Clear(ClearType::FromCursorDown))?;
    }

    // Diff repaint: update only changed rows, and clear any rows that were removed.
    let common = prev_lines.len().min(new_lines.len());
    for row in 0..common {
        if prev_lines[row] == new_lines[row] {
            continue;
        }
        crossterm::execute!(
            stdout,
            MoveTo(0, row as u16),
            Clear(ClearType::CurrentLine),
            MoveToColumn(0)
        )?;
        write!(stdout, "{}", new_lines[row])?;
    }

    // Paint appended rows.
    for row in common..new_lines.len() {
        crossterm::execute!(
            stdout,
            MoveTo(0, row as u16),
            Clear(ClearType::CurrentLine),
            MoveToColumn(0)
        )?;
        write!(stdout, "{}", new_lines[row])?;
    }

    // Clear rows that no longer exist in the new frame.
    for row in new_lines.len()..prev_lines.len() {
        crossterm::execute!(stdout, MoveTo(0, row as u16), Clear(ClearType::CurrentLine))?;
    }

    stdout.flush()?;

    *prev_lines = new_lines;
    Ok(())
}

fn fit_line(s: &str, width: u16) -> String {
    // Prevent wrapping (which breaks cursor math) by truncating to terminal width.
    // We keep it cheap and byte-based since our output is primarily ASCII.
    let w = width.max(10) as usize;
    ansi_fit_line(s, w)
}

fn ansi_fit_line(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if ansi_display_width(s) <= width {
        return s.to_string();
    }

    let mut out = String::new();
    let mut w = 0usize;
    let mut i = 0usize;
    let bytes = s.as_bytes();

    while i < bytes.len() && w + 1 < width {
        // Skip ANSI CSI SGR sequences: ESC [ ... m
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let start = i;
            i += 2;
            while i < bytes.len() && bytes[i] != b'm' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'm' {
                i += 1;
                out.push_str(&String::from_utf8_lossy(&bytes[start..i]));
                continue;
            } else {
                // Malformed escape; stop copying.
                break;
            }
        }

        // Copy one UTF-8 char.
        let ch = match s[i..].chars().next() {
            None => break,
            Some(c) => c,
        };
        let len = ch.len_utf8();
        out.push(ch);
        i += len;
        w += 1;
    }

    out.push('…');
    // Ensure we reset styles so truncated lines don't leak color.
    if out.contains('\u{1b}') && !out.ends_with("\u{1b}[0m") {
        out.push_str("\u{1b}[0m");
    }
    out
}

fn ansi_display_width(s: &str) -> usize {
    // Count visible characters, ignoring ANSI CSI sequences.
    let mut w = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'm' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'm' {
                i += 1;
            }
            continue;
        }
        let ch = match s[i..].chars().next() {
            None => break,
            Some(c) => c,
        };
        i += ch.len_utf8();
        w += 1;
    }
    w
}

fn apply_event(m: &mut Model, ev: TerminalEvent) {
    match ev {
        TerminalEvent::Info(_s) => {}
        TerminalEvent::Shutdown => {}
        TerminalEvent::RawJson(s) => {
            let _ = s; // intentionally ignored; keep UI noise-free
        }
        TerminalEvent::ThreadState(snap) => {
            let tid = snap.thread_id.clone();
            let tv = m.threads.entry(tid.clone()).or_insert_with(|| ThreadView {
                thread_id: tid.clone(),
                last_update: Instant::now(),
                ..Default::default()
            });
            let prev_phase = tv.current_phase.clone();
            tv.suite_id = snap.suite_id.clone();
            tv.agent_type = snap.agent_type.clone();
            tv.current_phase = snap.current_phase.clone();
            // Merge phases/items so previously seen entries never disappear.
            if !snap.phases.is_empty() {
                let mut merged: Vec<String> = Vec::with_capacity(snap.phases.len() + tv.phases.len());
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for p in snap.phases.iter() {
                    if seen.insert(p.clone()) {
                        merged.push(p.clone());
                    }
                }
                for p in tv.phases.iter() {
                    if seen.insert(p.clone()) {
                        merged.push(p.clone());
                    }
                }
                tv.phases = merged;
            }

            // If we move back to an earlier phase, clear any old tool/LLM spans for that phase
            // so the user only sees the current attempt's tool calls.
            let cur_phase = tv.current_phase.clone();
            if let (Some(prev), Some(cur)) = (prev_phase.as_deref(), cur_phase.as_deref()) {
                if prev != cur {
                    let prev_idx = tv.phases.iter().position(|p| p == prev);
                    let cur_idx = tv.phases.iter().position(|p| p == cur);
                    if let (Some(pi), Some(ci)) = (prev_idx, cur_idx) {
                        if ci <= pi {
                            clear_spans_for_phase(tv, cur);
                        }
                    }
                }
            }

            // Completed phases only grow; merge instead of replacing.
            for p in snap.completed_phases.iter() {
                if !tv.completed_phases.iter().any(|x| x == p) {
                    tv.completed_phases.push(p.clone());
                }
            }
            tv.total_runtime_ms = Some(snap.total_runtime_ms);
            for it in snap.items.iter() {
                tv.items.insert(it.item_id.clone(), it.clone());
            }
            tv.last_update = Instant::now();
            ensure_selected(m, &tid);
        }
        TerminalEvent::Plans {
            thread_id,
            cleanse,
            model,
        } => {
            let tv = m.threads.entry(thread_id.clone()).or_insert_with(|| ThreadView {
                thread_id: thread_id.clone(),
                last_update: Instant::now(),
                ..Default::default()
            });
            if cleanse.is_some() {
                tv.cleanse_plan = cleanse;
                if tv.cleanse_plan_anchor_phase.is_none() {
                    // Plans should be sticky to the planning phase where they originate.
                    tv.cleanse_plan_anchor_phase = Some("cleanse_plan".to_string());
                }
            }
            if model.is_some() {
                tv.model_plan = model;
                if tv.model_plan_anchor_phase.is_none() {
                    // Plans should be sticky to the planning phase where they originate.
                    tv.model_plan_anchor_phase = Some("model_plan".to_string());
                }
            }
            tv.last_update = Instant::now();
            ensure_selected(m, &thread_id);
        }
        TerminalEvent::DbtProgress { phase, detail } => {
            let Some(tid) = m.selected.clone() else {
                return;
            };
            let Some(tv) = m.threads.get_mut(&tid) else {
                return;
            };
            let want = phase.trim().to_lowercase();
            let detail = detail.trim().to_string();
            if detail.is_empty() {
                return;
            }
            for s in tv.tool_spans.values_mut() {
                if s.status != SpanStatus::Running {
                    continue;
                }
                let base = s.label.to_lowercase();
                if !base.contains("validate dbt") {
                    continue;
                }
                if want == "build" && !base.contains("build") {
                    continue;
                }
                if want == "compile" && !base.contains("compile") {
                    continue;
                }
                s.detail = Some(detail.clone());
            }
            tv.last_update = Instant::now();
        }
        TerminalEvent::PlansChanged(ev) => {
            // PlansChanged is emitted before the full Plans snapshot.
            // Keep our view consistent by:
            // - ensuring the thread exists/selected
            // - clearing stale snapshots when the plan_key changed, so the next Plans event
            //   repopulates with the correct hierarchy.
            let tid = ev.thread_id.clone();
            let tv = m.threads.entry(tid.clone()).or_insert_with(|| ThreadView {
                thread_id: tid.clone(),
                last_update: Instant::now(),
                ..Default::default()
            });
            if let Some(ref new_key) = ev.cleanse_plan_key {
                if tv
                    .cleanse_plan
                    .as_ref()
                    .is_some_and(|p| p.plan_key.as_str() != new_key.as_str())
                {
                    tv.cleanse_plan = None;
                }
            }
            if let Some(ref new_key) = ev.model_plan_key {
                if tv
                    .model_plan
                    .as_ref()
                    .is_some_and(|p| p.plan_key.as_str() != new_key.as_str())
                {
                    tv.model_plan = None;
                }
            }
            tv.last_update = Instant::now();
            ensure_selected(m, &tid);
        }
        TerminalEvent::Phase(ev) => {
            let tid = ev.thread_id.clone();
            let tv = m.threads.entry(tid.clone()).or_insert_with(|| ThreadView {
                thread_id: tid.clone(),
                last_update: Instant::now(),
                ..Default::default()
            });
            // Mark from_phase as completed (phase we're exiting).
            if let Some(ref from) = ev.from_phase {
                let from = from.trim();
                if !from.is_empty() {
                    if !tv.completed_phases.iter().any(|p| p == from) {
                        tv.completed_phases.push(from.to_string());
                    }
                    let key = format!("phase:{}", from);
                    let mut item = api::ThreadStateItem::new(
                        key.clone(),
                        "phase".to_string(),
                        "ok".to_string(),
                    );
                    item.finished_at = Some(ev.ts.clone());
                    item.runtime_ms = ev.from_phase_total_runtime_ms;
                    tv.items.insert(key, item);
                }
            }
            // Set current phase (phase we're entering).
            tv.current_phase = Some(ev.phase.clone());
            if let Some(rc) = ev.reason_code.clone() {
                tv.phase_reason_code.insert(ev.phase.clone(), rc);
            }
            if let Some(rd) = ev.reason_detail.clone() {
                let mut obj = serde_json::Map::new();
                for (k, v) in rd {
                    obj.insert(k, v);
                }
                tv.phase_reason_detail
                    .insert(ev.phase.clone(), Value::Object(obj));
            }
            let key = format!("phase:{}", ev.phase);
            let mut item = api::ThreadStateItem::new(key.clone(), "phase".to_string(), "running".to_string());
            item.started_at = Some(ev.ts.clone());
            item.runtime_ms = if ev.runs.is_empty() {
                None
            } else {
                Some(ev.total_runtime_ms)
            };
            tv.items.insert(key, item);
            // Ensure phase is in phases list.
            if !tv.phases.iter().any(|p| p == &ev.phase) {
                tv.phases.push(ev.phase.clone());
            }
            tv.last_update = Instant::now();
            ensure_selected(m, &tid);
        }
        TerminalEvent::ToolStart(ev) => {
            let tid = ev.thread_id.clone();
            let tv = m.threads.entry(tid.clone()).or_insert_with(|| ThreadView {
                thread_id: tid.clone(),
                last_update: Instant::now(),
                ..Default::default()
            });
            // Persist an effective phase on the span so tools don't "relocate" when
            // later renders show only the active phase.
            let effective_phase: Option<String> =
                ev.phase.clone().or_else(|| tv.current_phase.clone());

            // DBT validate spans should be replaceable within the same phase:
            // - A new "compile" validate attempt should clear any stale prior DBT validate lines.
            // - A new "build"/"full" validate should replace only that specific kind.
            if ev.name == "dbt_validate" {
                let label = ev.clean_name.clone().unwrap_or_else(|| ev.name.clone());
                if let (Some(ref ph), Some(kind)) =
                    (effective_phase.as_ref(), dbt_validate_kind_from_label(&label))
                {
                    tv.tool_spans.retain(|_, s| {
                        if s.phase.as_deref() != Some(ph.as_str()) {
                            return true;
                        }
                        if !is_dbt_validate_label(&s.label) {
                            return true;
                        }
                        if kind == DbtValidateKind::Compile {
                            // Compile is the start of a targeted validate attempt in this suite.
                            // Clear all prior dbt_validate spans so stale "Validate DBT" lines
                            // do not persist when only compile/build are re-run.
                            return false;
                        }
                        // Otherwise, replace only the same kind.
                        dbt_validate_kind_from_label(&s.label) != Some(kind)
                    });
                }
            }
            tv.tool_spans.insert(
                ev.tool_id.clone(),
                SpanState {
                    label: ev.clean_name.clone().unwrap_or(ev.name.clone()),
                    detail: None,
                    description: None,
                    status: SpanStatus::Running,
                    started_at: Instant::now(),
                    ended_at: None,
                    phase: effective_phase.clone(),
                    ctx: ev.ctx.clone(),
                },
            );
            // Update focus when we have a concrete work-item ctx.
            if let Some(ref ctx) = ev.ctx {
                if let (Some(pk), Some(_plan_key), Some(_wg), Some(_task), Some(_ci)) = (
                    ctx.plan_kind.clone(),
                    ctx.plan_key.clone(),
                    ctx.workgroup_id.clone(),
                    ctx.task_id.clone(),
                    ctx.checklist_item_id.clone(),
                ) {
                    let k = key_from_ctx(&Some(ctx.clone()));
                    update_focus_with_stickiness(tv, pk, k.clone());
                    tv.expanded_work_items.insert(k);
                }
            }
            tv.last_update = Instant::now();
            ensure_selected(m, &tid);
        }
        TerminalEvent::ToolEnd(ev) => {
            let tid = ev.thread_id.clone();
            let tv = m.threads.entry(tid.clone()).or_insert_with(|| ThreadView {
                thread_id: tid.clone(),
                last_update: Instant::now(),
                ..Default::default()
            });
            let st = match ev.status {
                api::ToolEventStatus::Ok => SpanStatus::Ok,
                api::ToolEventStatus::Failed => SpanStatus::Failed,
                api::ToolEventStatus::Running => SpanStatus::Running,
            };
            let effective_phase: Option<String> =
                ev.phase.clone().or_else(|| tv.current_phase.clone());
            let (detail, description) = if ev.name == "dbt_validate" && st == SpanStatus::Failed {
                // Prefer condensed, accurate error summary when available in payload.
                let from_payload = ev
                    .payload
                    .as_ref()
                    .and_then(|m| m.get("error_summary"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                // Multiline description: show under the span.
                let desc = from_payload
                    .clone()
                    .map(|s| clean_multiline(&s, 14, 4000))
                    .filter(|s| !s.trim().is_empty());

                // One-line detail: keep the main span line short.
                let detail_src = from_payload
                    .as_deref()
                    .and_then(first_nonempty_line)
                    .map(|s| s.to_string())
                    .or_else(|| ev.error.clone());
                let dt = detail_src
                    .as_deref()
                    .map(|s| condense_one_line(s, 180))
                    .filter(|s| !s.trim().is_empty());

                (dt, desc)
            } else {
                (None, None)
            };
            tv.tool_spans
                .entry(ev.tool_id.clone())
                .and_modify(|s| {
                    s.label = ev.clean_name.clone().unwrap_or(ev.name.clone());
                    s.detail = detail.clone();
                    s.description = description.clone();
                    s.status = st;
                    s.ended_at = Some(Instant::now());
                    s.phase = effective_phase.clone();
                    s.ctx = ev.ctx.clone();
                })
                .or_insert_with(|| SpanState {
                    label: ev.clean_name.clone().unwrap_or(ev.name.clone()),
                    detail,
                    description,
                    status: st,
                    started_at: Instant::now(),
                    ended_at: Some(Instant::now()),
                    phase: effective_phase.clone(),
                    ctx: ev.ctx.clone(),
                });
            if let Some(ref ctx) = ev.ctx {
                if let (Some(pk), Some(_plan_key), Some(_wg), Some(_task), Some(_ci)) = (
                    ctx.plan_kind.clone(),
                    ctx.plan_key.clone(),
                    ctx.workgroup_id.clone(),
                    ctx.task_id.clone(),
                    ctx.checklist_item_id.clone(),
                ) {
                    let k = key_from_ctx(&Some(ctx.clone()));
                    update_focus_with_stickiness(tv, pk, k.clone());
                    tv.expanded_work_items.insert(k);
                }
            }
            tv.last_update = Instant::now();
            ensure_selected(m, &tid);
        }
        TerminalEvent::LlmStart(ev) => {
            let tid = ev.thread_id.clone();
            let tv = m.threads.entry(tid.clone()).or_insert_with(|| ThreadView {
                thread_id: tid.clone(),
                last_update: Instant::now(),
                ..Default::default()
            });
            let label = llm_label_for_phase(&ev.phase);
            tv.llm_spans.insert(
                ev.call_id,
                SpanState {
                    label,
                    detail: None,
                    description: None,
                    status: SpanStatus::Running,
                    started_at: Instant::now(),
                    ended_at: None,
                    phase: Some(ev.phase.clone()),
                    ctx: ev.ctx.clone(),
                },
            );
            if let Some(ref ctx) = ev.ctx {
                if let (Some(pk), Some(_plan_key), Some(_wg), Some(_task), Some(_ci)) = (
                    ctx.plan_kind.clone(),
                    ctx.plan_key.clone(),
                    ctx.workgroup_id.clone(),
                    ctx.task_id.clone(),
                    ctx.checklist_item_id.clone(),
                ) {
                    let k = key_from_ctx(&Some(ctx.clone()));
                    update_focus_with_stickiness(tv, pk, k.clone());
                    tv.expanded_work_items.insert(k);
                }
            }
            tv.last_update = Instant::now();
            ensure_selected(m, &tid);
        }
        TerminalEvent::LlmEnd(ev) => {
            let tid = ev.thread_id.clone();
            let tv = m.threads.entry(tid.clone()).or_insert_with(|| ThreadView {
                thread_id: tid.clone(),
                last_update: Instant::now(),
                ..Default::default()
            });
            let st = match ev.status {
                api::llm_end_response::Status::Ok => SpanStatus::Ok,
                api::llm_end_response::Status::Failed => SpanStatus::Failed,
            };
            let label = llm_label_for_phase(&ev.phase);
            tv.llm_spans
                .entry(ev.call_id)
                .and_modify(|s| {
                    s.label = label.clone();
                    s.detail = None;
                    s.description = None;
                    s.status = st;
                    s.ended_at = Some(Instant::now());
                    s.phase = Some(ev.phase.clone());
                    s.ctx = ev.ctx.clone();
                })
                .or_insert_with(|| SpanState {
                    label,
                    detail: None,
                    description: None,
                    status: st,
                    started_at: Instant::now(),
                    ended_at: Some(Instant::now()),
                    phase: Some(ev.phase.clone()),
                    ctx: ev.ctx.clone(),
                });
            if let Some(ref ctx) = ev.ctx {
                if let (Some(pk), Some(_plan_key), Some(_wg), Some(_task), Some(_ci)) = (
                    ctx.plan_kind.clone(),
                    ctx.plan_key.clone(),
                    ctx.workgroup_id.clone(),
                    ctx.task_id.clone(),
                    ctx.checklist_item_id.clone(),
                ) {
                    let k = key_from_ctx(&Some(ctx.clone()));
                    update_focus_with_stickiness(tv, pk, k.clone());
                    tv.expanded_work_items.insert(k);
                }
            }
            tv.last_update = Instant::now();
            ensure_selected(m, &tid);
        }
    }
}

fn clear_spans_for_phase(tv: &mut ThreadView, phase: &str) {
    tv.tool_spans
        .retain(|_, s| s.phase.as_deref() != Some(phase));
    tv.llm_spans
        .retain(|_, s| s.phase.as_deref() != Some(phase));
}

fn has_running_span_for_key(tv: &ThreadView, k: &WorkItemKey) -> bool {
    tv.tool_spans
        .values()
        .chain(tv.llm_spans.values())
        .any(|s| s.status == SpanStatus::Running && key_from_ctx(&s.ctx) == *k)
}

fn update_focus_with_stickiness(tv: &mut ThreadView, plan_kind: String, next_key: WorkItemKey) {
    // Keep focus stable while the currently focused item still has active spans.
    // This prevents rapid UI hopping across work items in noisy phases.
    let should_update = match tv.focus_by_kind.get(&plan_kind) {
        Some(prev_key) if prev_key != &next_key => !has_running_span_for_key(tv, prev_key),
        _ => true,
    };
    if should_update {
        tv.focus_by_kind.insert(plan_kind, next_key);
    }
}

fn ensure_selected(m: &mut Model, tid: &str) {
    // Follow the most recently updated thread.
    m.selected = Some(tid.to_string());
}

fn short_tid(tid: &str) -> String {
    if tid.len() <= 8 {
        tid.to_string()
    } else {
        tid[..8].to_string()
    }
}

fn fmt_dur(d: Duration) -> String {
    let ms = d.as_millis() as i64;
    fmt_ms(ms)
}

fn fmt_ms(ms: i64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", (ms as f64) / 1000.0)
    } else {
        let s = ms / 1000;
        if s < 3600 {
            format!("{}m{}s", s / 60, s % 60)
        } else {
            let h = s / 3600;
            let rem = s % 3600;
            format!("{}h{}m{}s", h, rem / 60, rem % 60)
        }
    }
}

fn normalize_validate_dbt_label(label: &str) -> String {
    // Prefer a compact label: "Validate DBT Build" instead of "Validate DBT (build)".
    let s = label.trim();
    s.replace(" (build)", " Build")
        .replace(" (compile)", " Compile")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DbtValidateKind {
    Compile,
    Build,
    Full,
}

fn dbt_validate_kind_from_label(label: &str) -> Option<DbtValidateKind> {
    let s = label.trim().to_ascii_lowercase();
    if !s.contains("validate dbt") {
        return None;
    }
    if s.contains("compile") {
        return Some(DbtValidateKind::Compile);
    }
    if s.contains("build") {
        return Some(DbtValidateKind::Build);
    }
    Some(DbtValidateKind::Full)
}

fn is_dbt_validate_label(label: &str) -> bool {
    label.trim().to_ascii_lowercase().contains("validate dbt")
}

fn condense_one_line(s: &str, max_len: usize) -> String {
    let mut out = s
        .trim()
        .replace('\n', " ; ")
        .replace('\r', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if out.len() > max_len {
        out.truncate(max_len);
        out.push('…');
    }
    out
}

fn clean_multiline(s: &str, max_lines: usize, max_chars: usize) -> String {
    let s = s.replace('\r', "");
    let mut out: Vec<String> = Vec::new();
    for line in s.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            // Keep at most a single empty line in a row.
            if out.last().map(|x| x.is_empty()).unwrap_or(false) {
                continue;
            }
            out.push(String::new());
            continue;
        }
        out.push(t.to_string());
        if out.len() >= max_lines {
            break;
        }
    }
    // Trim leading/trailing blank lines.
    while out.first().map(|x| x.is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|x| x.is_empty()).unwrap_or(false) {
        out.pop();
    }
    let mut joined = out.join("\n");
    if joined.len() > max_chars {
        joined.truncate(max_chars);
        joined.push('…');
    }
    joined
}

fn first_nonempty_line(s: &str) -> Option<&str> {
    for line in s.lines() {
        let t = line.trim();
        if !t.is_empty() {
            return Some(t);
        }
    }
    None
}

fn validate_dbt_sort_rank(label: &str) -> Option<u8> {
    // Force stable ordering among Validate DBT items, even if tool_end arrives late/missing tool_start.
    // Keep this conservative: only rank labels that clearly start with the DBT validate prefix.
    let s = normalize_validate_dbt_label(label).trim().to_ascii_lowercase();
    if !s.starts_with("validate dbt") {
        return None;
    }
    if s.starts_with("validate dbt compile") {
        return Some(0);
    }
    if s.starts_with("validate dbt build") {
        return Some(1);
    }
    Some(2)
}

fn span_display_label(s: &SpanState) -> String {
    let base = normalize_validate_dbt_label(&s.label);
    match (&s.status, s.detail.as_ref()) {
        (SpanStatus::Running, Some(d)) if !d.trim().is_empty() => format!("{base} {}", d.trim()),
        (SpanStatus::Failed, Some(d)) if !d.trim().is_empty() => format!("{base} — {}", d.trim()),
        _ => base,
    }
}

fn llm_label_for_phase(phase: &str) -> String {
    let p = phase.trim();
    if p.is_empty() {
        return "LLM".to_string();
    }
    // Deterministic, scripted labels for the common suite phase set.
    let friendly = match p {
        "cleanse_plan" => "Cleanse plan",
        "cleanse_author" => "Cleanse author",
        "cleanse_validate" => "Cleanse validate",
        "cleanse_review" => "Cleanse review",
        "model_plan" => "Model plan",
        "model_author" => "Model author",
        "model_validate" => "Model validate",
        "model_review" => "Model review",
        other => other,
    };
    format!("LLM · {friendly}")
}

fn glyph_span_colored(status: SpanStatus, spinner_idx: usize) -> String {
    match status {
        SpanStatus::Running => spinner_glyph(spinner_idx).yellow().to_string(),
        SpanStatus::Ok => "[✓]".green().to_string(),
        SpanStatus::Failed => "[x]".red().to_string(),
    }
}

const MAX_SPAN_LINES: usize = 5;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_glyph(spinner_idx: usize) -> String {
    let ch = SPINNER_FRAMES[spinner_idx % SPINNER_FRAMES.len()];
    format!("[{ch}]")
}

fn push_span_line(lines: &mut Vec<String>, indent: &str, s: &SpanAgg, spinner_idx: usize) {
    let sg = glyph_span_colored(s.status, spinner_idx);
    let dur = fmt_dur(s.max_dur).dark_grey().to_string();
    if s.count > 1 {
        lines.push(format!(
            "{indent}{sg} {}  {dur} (over {} runs)",
            s.label.as_str().white(),
            s.count
        ));
    } else {
        lines.push(format!(
            "{indent}{sg} {}  {dur}",
            s.label.as_str().white(),
        ));
    }

    // Optional multi-line description (indented continuation lines).
    if s.count == 1 {
        if let Some(desc) = s.description.as_ref() {
            let d = desc.trim();
            if !d.is_empty() {
                // Hard caps to keep the terminal readable.
                let cleaned = clean_multiline(d, 10, 2000);
                for line in cleaned.lines() {
                    let line = line.trim_end();
                    if line.trim().is_empty() {
                        continue;
                    }
                    lines.push(format!("{indent}    {}", line.dark_grey()));
                }
            }
        }
    }
}

fn push_span_list(lines: &mut Vec<String>, indent: &str, spans: &[SpanAgg], spinner_idx: usize) {
    if spans.is_empty() {
        return;
    }
    if spans.len() <= MAX_SPAN_LINES {
        for s in spans.iter() {
            push_span_line(lines, indent, s, spinner_idx);
        }
        return;
    }

    // Keep the most recent items (tail), and show a single compact elision line.
    let show = MAX_SPAN_LINES.saturating_sub(1);
    let omitted = spans.len().saturating_sub(show);
    lines.push(format!("{indent}{}", format!("... and {omitted} more").dark_grey()));
    for s in spans.iter().skip(spans.len().saturating_sub(show)) {
        push_span_line(lines, indent, s, spinner_idx);
    }
}

fn checklist_item_glyph(st: &api::PlanChecklistItemStatus) -> &'static str {
    match st {
        api::PlanChecklistItemStatus::Done => "[✓]",
        api::PlanChecklistItemStatus::InProgress => "[~]",
        api::PlanChecklistItemStatus::Blocked => "[!]",
        api::PlanChecklistItemStatus::NeedsUpdate => "[!]",
        api::PlanChecklistItemStatus::Pending => "[ ]",
    }
}

fn checklist_item_glyph_colored(st: &api::PlanChecklistItemStatus) -> String {
    match st {
        api::PlanChecklistItemStatus::Done => "[✓]".green().to_string(),
        api::PlanChecklistItemStatus::InProgress => "[~]".yellow().to_string(),
        api::PlanChecklistItemStatus::Blocked => "[!]".magenta().to_string(),
        api::PlanChecklistItemStatus::NeedsUpdate => "[!]".magenta().to_string(),
        api::PlanChecklistItemStatus::Pending => "[ ]".dark_grey().to_string(),
    }
}

fn lookup_checklist_item<'a>(
    p: &'a api::PlanSnapshot,
    task_id: &str,
    checklist_item_id: &str,
) -> Option<&'a api::PlanChecklistItem> {
    for t in p.tasks.iter() {
        let (tid, checklist) = match t {
            api::PlanTask::Cleanse(c) => (&c.task_id, &c.checklist),
            api::PlanTask::Model(m) => (&m.task_id, &m.checklist),
        };
        if tid == task_id {
            for ci in checklist.iter() {
                if ci.checklist_item_id == checklist_item_id {
                    return Some(ci);
                }
            }
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct WorkItemKey {
    plan_kind: Option<String>,
    plan_key: Option<String>,
    workgroup_id: Option<String>,
    task_id: Option<String>,
    checklist_item_id: Option<String>,
}

fn key_from_ctx(ctx: &Option<api::ExecutionContext>) -> WorkItemKey {
    match ctx.as_ref() {
        None => WorkItemKey {
            plan_kind: None,
            plan_key: None,
            workgroup_id: None,
            task_id: None,
            checklist_item_id: None,
        },
        Some(c) => WorkItemKey {
            plan_kind: c.plan_kind.clone(),
            plan_key: c.plan_key.clone(),
            workgroup_id: c.workgroup_id.clone(),
            task_id: c.task_id.clone(),
            checklist_item_id: c.checklist_item_id.clone(),
        },
    }
}

fn render_plan_tree(
    lines: &mut Vec<String>,
    kind: &str,
    p: &api::PlanSnapshot,
    span_buckets: &HashMap<WorkItemKey, Vec<SpanAgg>>,
) {
    let plan_status = format!("{:?}", p.status).to_lowercase();
    lines.push(format!(
        "  {} {} {}  {}",
        "[~]".cyan(),
        format!("{kind} plan").white(),
        p.plan_key.as_str().cyan(),
        plan_status.dark_grey(),
    ));

    for wg in p.work_groups.iter() {
        lines.push(format!(
            "    {} {}  {}",
            "[~]".cyan(),
            wg.label.as_str().white(),
            wg.group_id.as_str().dark_grey()
        ));
        for r in wg.items.iter() {
            let ci = lookup_checklist_item(p, &r.task_id, &r.checklist_item_id);
            let ig = ci
                .map(|x| checklist_item_glyph_colored(&x.status))
                .unwrap_or_else(|| "[ ]".dark_grey().to_string());
            let label = ci
                .map(|x| x.label.clone())
                .unwrap_or_else(|| r.checklist_item_id.clone());
            // Task line.
            lines.push(format!(
                "      {} {}",
                "[~]".cyan(),
                r.task_id.as_str().white()
            ));
            // Checklist item line (nested).
            lines.push(format!("        {ig} {}", label.white()));

            let k = WorkItemKey {
                plan_kind: Some(kind.to_string()),
                plan_key: Some(p.plan_key.clone()),
                workgroup_id: Some(wg.group_id.clone()),
                task_id: Some(r.task_id.clone()),
                checklist_item_id: Some(r.checklist_item_id.clone()),
            };
            if let Some(spans) = span_buckets.get(&k) {
                push_span_list(lines, "          ", spans, 0);
            }
        }
    }
}

fn pick_focus_key(
    kind: &str,
    span_buckets: &HashMap<WorkItemKey, Vec<SpanAgg>>,
    preferred: Option<&WorkItemKey>,
) -> Option<WorkItemKey> {
    if let Some(p) = preferred {
        // If we can render this directly from plan snapshots, prefer it even if no spans exist.
        if p.plan_kind.as_deref() == Some(kind) {
            return Some(p.clone());
        }
    }
    // Prefer an in-flight (running) work item for this plan kind.
    // Fallback: the work item with the largest observed duration.
    // Tie-break deterministically by WorkItemKey so focus does not flap.
    let mut best_running: Option<(WorkItemKey, Duration)> = None;
    let mut best_any: Option<(WorkItemKey, Duration)> = None;
    for (k, spans) in span_buckets.iter() {
        if k.plan_kind.as_deref() != Some(kind) {
            continue;
        }
        let mut max_d = Duration::from_millis(0);
        let mut has_running = false;
        for s in spans.iter() {
            if s.max_dur > max_d {
                max_d = s.max_dur;
            }
            if s.status == SpanStatus::Running {
                has_running = true;
            }
        }
        if has_running {
            let replace = match best_running.as_ref() {
                None => true,
                Some((bk, bd)) => max_d > *bd || (max_d == *bd && k < bk),
            };
            if replace {
                best_running = Some((k.clone(), max_d));
            }
        }
        let replace = match best_any.as_ref() {
            None => true,
            Some((bk, bd)) => max_d > *bd || (max_d == *bd && k < bk),
        };
        if replace {
            best_any = Some((k.clone(), max_d));
        }
    }
    best_running.or(best_any).map(|(k, _)| k)
}

fn render_plan_compact(
    lines: &mut Vec<String>,
    kind: &str,
    p: &api::PlanSnapshot,
    span_buckets: &HashMap<WorkItemKey, Vec<SpanAgg>>,
    preferred: Option<&WorkItemKey>,
    spinner_idx: usize,
) {
    let focus = pick_focus_key(kind, span_buckets, preferred);
    let plan_status = format!("{:?}", p.status).to_lowercase();
    lines.push(format!(
        "  {} {} {}  {}",
        "[~]".cyan(),
        format!("{kind} plan").white(),
        p.plan_key.as_str().cyan(),
        plan_status.dark_grey(),
    ));

    // Compact listing: keep workgroups/items in plan order, and only expand the focused item.
    for wg in p.work_groups.iter() {
        lines.push(format!(
            "    {} {}  {}",
            "[~]".cyan(),
            wg.label.as_str().white(),
            wg.group_id.as_str().dark_grey()
        ));
        for r in wg.items.iter() {
            let ci = lookup_checklist_item(p, &r.task_id, &r.checklist_item_id);
            let (ig, label) = match ci {
                None => ("[ ]".dark_grey().to_string(), r.checklist_item_id.clone()),
                Some(x) => (
                    checklist_item_glyph_colored(&x.status),
                    x.label.clone(),
                ),
            };

            // Dataset/task line: show status of its (current) checklist item.
            lines.push(format!("      {ig} {}", r.task_id.as_str().white()));

            let is_focus = focus.as_ref().is_some_and(|f| {
                f.plan_kind.as_deref() == Some(kind)
                    && f.plan_key.as_deref() == Some(p.plan_key.as_str())
                    && f.workgroup_id.as_deref() == Some(wg.group_id.as_str())
                    && f.task_id.as_deref() == Some(r.task_id.as_str())
                    && f.checklist_item_id.as_deref() == Some(r.checklist_item_id.as_str())
            });
            if is_focus {
                // Checklist item line (nested) + spans.
                lines.push(format!("        {ig} {}", label.white()));
                let k = WorkItemKey {
                    plan_kind: Some(kind.to_string()),
                    plan_key: Some(p.plan_key.clone()),
                    workgroup_id: Some(wg.group_id.clone()),
                    task_id: Some(r.task_id.clone()),
                    checklist_item_id: Some(r.checklist_item_id.clone()),
                };
                if let Some(spans) = span_buckets.get(&k) {
                    push_span_list(lines, "          ", spans, spinner_idx);
                }
            }
        }
    }
}

fn render_plan_summary_line(lines: &mut Vec<String>, kind: &str, p: &api::PlanSnapshot) {
    let plan_status = format!("{:?}", p.status).to_lowercase();
    lines.push(format!(
        "  {} {} {}  {}",
        "[~]".cyan(),
        format!("{kind} plan").white(),
        p.plan_key.as_str().cyan(),
        plan_status.dark_grey(),
    ));
}

fn summarize_plan_workgroups(
    p: &api::PlanSnapshot,
    allowed_kinds: &[api::PlanWorkGroupKind],
) -> Option<String> {
    let allowed: std::collections::HashSet<api::PlanWorkGroupKind> =
        allowed_kinds.iter().copied().collect();
    let mut wg_count = 0usize;
    let mut total_items = 0usize;
    let mut done_items = 0usize;
    let mut in_progress_items = 0usize;
    let mut blocked_items = 0usize;
    for wg in p.work_groups.iter() {
        if !allowed.contains(&wg.kind) {
            continue;
        }
        wg_count += 1;
        for r in wg.items.iter() {
            total_items += 1;
            match lookup_checklist_item(p, &r.task_id, &r.checklist_item_id).map(|x| &x.status) {
                Some(api::PlanChecklistItemStatus::Done) => done_items += 1,
                Some(api::PlanChecklistItemStatus::InProgress) => in_progress_items += 1,
                Some(api::PlanChecklistItemStatus::Blocked)
                | Some(api::PlanChecklistItemStatus::NeedsUpdate) => blocked_items += 1,
                Some(api::PlanChecklistItemStatus::Pending) | None => {}
            }
        }
    }
    if wg_count == 0 || total_items == 0 {
        return None;
    }
    Some(format!(
        "{wg_count} wgs, {done_items}/{total_items} items done{}{}",
        if in_progress_items > 0 {
            format!(", {in_progress_items} running")
        } else {
            "".to_string()
        },
        if blocked_items > 0 {
            format!(", {blocked_items} blocked")
        } else {
            "".to_string()
        }
    ))
}

fn phase_detail_summary(t: &ThreadView, ph: &str) -> Option<String> {
    fn short_plan_key(s: &str) -> String {
        s.rsplit('/').next().unwrap_or(s).to_string()
    }
    match ph {
        "cleanse_plan" => t.cleanse_plan.as_ref().map(|p| {
            let st = format!("{:?}", p.status).to_lowercase();
            format!("cleanse plan {}  {}", short_plan_key(p.plan_key.as_str()), st)
        }),
        "model_plan" => t.model_plan.as_ref().map(|p| {
            let st = format!("{:?}", p.status).to_lowercase();
            format!("model plan {}  {}", short_plan_key(p.plan_key.as_str()), st)
        }),
        "cleanse_author" => t.cleanse_plan.as_ref().and_then(|p| {
            summarize_plan_workgroups(
                p,
                &[
                    api::PlanWorkGroupKind::AuthorSql,
                    api::PlanWorkGroupKind::AuthorSchema,
                ],
            )
        }),
        "cleanse_validate" => t
            .cleanse_plan
            .as_ref()
            .and_then(|p| summarize_plan_workgroups(p, &[api::PlanWorkGroupKind::Validate])),
        "model_author" => t.model_plan.as_ref().and_then(|p| {
            summarize_plan_workgroups(
                p,
                &[
                    api::PlanWorkGroupKind::AuthorSql,
                    api::PlanWorkGroupKind::AuthorSchema,
                ],
            )
        }),
        "model_validate" => t
            .model_plan
            .as_ref()
            .and_then(|p| summarize_plan_workgroups(p, &[api::PlanWorkGroupKind::Validate])),
        _ => None,
    }
}

fn phase_reason_summary(t: &ThreadView, ph: &str) -> Option<String> {
    let rc = t.phase_reason_code.get(ph).map(|s| s.as_str()).unwrap_or("");
    let rd = t.phase_reason_detail.get(ph);
    match rc {
        "review_actionable_true" => {
            let review_phase = rd
                .and_then(|v| v.get("review_phase"))
                .and_then(|v| v.as_str())
                .unwrap_or("review");
            let tier = rd
                .and_then(|v| v.get("meta"))
                .and_then(|v| v.get("tier"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let targets = rd
                .and_then(|v| v.get("meta"))
                .and_then(|v| v.get("dataset_ids"))
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0usize);
            Some(format!(
                "replan from {review_phase} ({tier}, {targets} targets)"
            ))
        }
        "plan_auto_approved" | "plan_approved" | "plan_already_approved" => {
            let counts = rd
                .and_then(|v| v.get("plan_update_summary"))
                .and_then(|v| v.get("counts"));
            let touched = counts
                .and_then(|v| v.get("touched_tasks"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let added = counts
                .and_then(|v| v.get("added_tasks"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let removed = counts
                .and_then(|v| v.get("removed_tasks"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let review_items = counts
                .and_then(|v| v.get("review_items"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if touched == 0 && added == 0 && removed == 0 && review_items == 0 {
                return None;
            }
            Some(format!(
                "plan update: {touched} touched, +{added}/-{removed}, {review_items} review items"
            ))
        }
        _ => None,
    }
}

fn pick_focus_key_filtered(
    kind: &str,
    span_buckets: &HashMap<WorkItemKey, Vec<SpanAgg>>,
    preferred: Option<&WorkItemKey>,
    allowed_workgroup_ids: &std::collections::HashSet<String>,
) -> Option<WorkItemKey> {
    if let Some(p) = preferred {
        if p.plan_kind.as_deref() == Some(kind) {
            if let Some(wg) = p.workgroup_id.as_deref() {
                if allowed_workgroup_ids.contains(wg) {
                    return Some(p.clone());
                }
            }
        }
    }
    // Prefer in-flight work items within the allowed workgroups.
    // Tie-break deterministically by WorkItemKey so focus does not flap.
    let mut best_running: Option<(WorkItemKey, Duration)> = None;
    let mut best_any: Option<(WorkItemKey, Duration)> = None;
    for (k, spans) in span_buckets.iter() {
        if k.plan_kind.as_deref() != Some(kind) {
            continue;
        }
        if let Some(wg) = k.workgroup_id.as_deref() {
            if !allowed_workgroup_ids.contains(wg) {
                continue;
            }
        } else {
            continue;
        }
        let mut max_d = Duration::from_millis(0);
        let mut has_running = false;
        for s in spans.iter() {
            if s.max_dur > max_d {
                max_d = s.max_dur;
            }
            if s.status == SpanStatus::Running {
                has_running = true;
            }
        }
        if has_running {
            let replace = match best_running.as_ref() {
                None => true,
                Some((bk, bd)) => max_d > *bd || (max_d == *bd && k < bk),
            };
            if replace {
                best_running = Some((k.clone(), max_d));
            }
        }
        let replace = match best_any.as_ref() {
            None => true,
            Some((bk, bd)) => max_d > *bd || (max_d == *bd && k < bk),
        };
        if replace {
            best_any = Some((k.clone(), max_d));
        }
    }
    best_running.or(best_any).map(|(k, _)| k)
}

fn render_plan_workgroups_compact(
    lines: &mut Vec<String>,
    kind: &str,
    p: &api::PlanSnapshot,
    span_buckets: &HashMap<WorkItemKey, Vec<SpanAgg>>,
    expanded_work_items: &std::collections::HashSet<WorkItemKey>,
    preferred: Option<&WorkItemKey>,
    spinner_idx: usize,
    allowed_kinds: &[api::PlanWorkGroupKind],
) {
    let allowed_kind_set: std::collections::HashSet<api::PlanWorkGroupKind> =
        allowed_kinds.iter().copied().collect();
    let mut allowed_workgroup_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for wg in p.work_groups.iter() {
        if allowed_kind_set.contains(&wg.kind) {
            allowed_workgroup_ids.insert(wg.group_id.clone());
        }
    }
    if allowed_workgroup_ids.is_empty() {
        return;
    }

    let focus = pick_focus_key_filtered(kind, span_buckets, preferred, &allowed_workgroup_ids);
    for wg in p.work_groups.iter() {
        if !allowed_kind_set.contains(&wg.kind) {
            continue;
        }

        // Compute a compact workgroup status + summary.
        let mut total_items = 0usize;
        let mut done_items = 0usize;
        let mut in_progress_items = 0usize;
        let mut blocked_items = 0usize;
        let mut pending_items = 0usize;
        for r in wg.items.iter() {
            total_items += 1;
            match lookup_checklist_item(p, &r.task_id, &r.checklist_item_id).map(|x| &x.status) {
                Some(api::PlanChecklistItemStatus::Done) => done_items += 1,
                Some(api::PlanChecklistItemStatus::InProgress) => in_progress_items += 1,
                Some(api::PlanChecklistItemStatus::Blocked)
                | Some(api::PlanChecklistItemStatus::NeedsUpdate) => blocked_items += 1,
                Some(api::PlanChecklistItemStatus::Pending) | None => pending_items += 1,
            }
        }
        let wg_done = total_items > 0 && done_items == total_items;
        let wg_blocked = blocked_items > 0;
        let wg_running = in_progress_items > 0;
        let wg_glyph = if wg_blocked {
            "[!]".magenta().to_string()
        } else if wg_done {
            "[✓]".green().to_string()
        } else if wg_running {
            "[~]".yellow().to_string()
        } else {
            "[ ]".dark_grey().to_string()
        };
        let detail = format!(
            "{done_items}/{total_items} done{}{}",
            if wg_running { ", running" } else { "" },
            if pending_items > 0 {
                format!(", {pending_items} pending")
            } else {
                "".to_string()
            }
        );
        lines.push(format!(
            "  {wg_glyph} {}  {}  {}",
            wg.label.as_str().white(),
            wg.group_id.as_str().dark_grey(),
            detail.dark_grey()
        ));

        // Collapse completed workgroups into a single summary line.
        if wg_done {
            continue;
        }

        for r in wg.items.iter() {
            let ci = lookup_checklist_item(p, &r.task_id, &r.checklist_item_id);
            let (ig, label) = match ci {
                None => ("[ ]".dark_grey().to_string(), r.checklist_item_id.clone()),
                Some(x) => (
                    checklist_item_glyph_colored(&x.status),
                    x.label.clone(),
                ),
            };
            lines.push(format!("    {ig} {}", r.task_id.as_str().white()));

            let is_focus = focus.as_ref().is_some_and(|f| {
                f.plan_kind.as_deref() == Some(kind)
                    && f.plan_key.as_deref() == Some(p.plan_key.as_str())
                    && f.workgroup_id.as_deref() == Some(wg.group_id.as_str())
                    && f.task_id.as_deref() == Some(r.task_id.as_str())
                    && f.checklist_item_id.as_deref() == Some(r.checklist_item_id.as_str())
            });
            let k = WorkItemKey {
                plan_kind: Some(kind.to_string()),
                plan_key: Some(p.plan_key.clone()),
                workgroup_id: Some(wg.group_id.clone()),
                task_id: Some(r.task_id.clone()),
                checklist_item_id: Some(r.checklist_item_id.clone()),
            };
            let has_spans = span_buckets.contains_key(&k);
            let is_sticky = expanded_work_items.contains(&k);
            let show_leaf = is_focus || is_sticky || has_spans;
            if show_leaf {
                // Sticky checklist label line (never disappears once focused).
                lines.push(format!("      {ig} {}", label.white()));
                // Tool/LLM spans: render for any item that has spans, until spans are cleared by existing rules.
                if let Some(spans) = span_buckets.get(&k) {
                    push_span_list(lines, "        ", spans, spinner_idx);
                }
            }
        }
    }
}

fn render_model(m: &Model) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let up = fmt_ms(m.started.elapsed().as_millis() as i64);
    let spinner_idx =
        ((m.started.elapsed().as_millis() / 120) as usize) % SPINNER_FRAMES.len();
    let sel = m
        .selected
        .as_ref()
        .map(|s| short_tid(s))
        .unwrap_or_else(|| "-".to_string());

    out.push(format!(
        "{} {}{} {}",
        "Running".white(),
        "thread".dark_grey(),
        format!(" {sel}").cyan(),
        format!("... {up}").dark_grey()
    ));

    let tid = match m.selected.as_ref() {
        Some(t) => t,
        None => {
            out.push("waiting for events…".to_string());
            return out;
        }
    };
    let t = match m.threads.get(tid) {
        Some(t) => t,
        None => {
            out.push("waiting for events…".to_string());
            return out;
        }
    };

    out.extend(render_thread_detail(t, spinner_idx));
    out
}

fn render_thread_detail(t: &ThreadView, spinner_idx: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    let cur = t
        .current_phase
        .clone()
        .unwrap_or_else(|| "preflight".to_string());
    let done: std::collections::HashSet<String> =
        t.completed_phases.iter().cloned().collect();
    // Phases can be re-entered (e.g. actionable review -> cleanse_plan -> cleanse_author).
    // Additionally, phases "after" the current one should not render as completed, even if
    // they were completed in a prior loop. This avoids confusing UX where future phases
    // look "done" while we're actively back in authoring/planning.
    let cur_idx: Option<usize> = t.phases.iter().position(|p| p == &cur);

    // Plans are sticky to the planning phase where they originate (not the currently active phase).
    let anchor_cleanse: Option<String> = t
        .cleanse_plan_anchor_phase
        .clone()
        .or_else(|| t.cleanse_plan.as_ref().map(|_| "cleanse_plan".to_string()));
    let anchor_model: Option<String> = t
        .model_plan_anchor_phase
        .clone()
        .or_else(|| t.model_plan.as_ref().map(|_| "model_plan".to_string()));

    // Precompute span buckets (active + recent) in stable occurrence order.
    let mut span_buckets: HashMap<WorkItemKey, Vec<SpanAgg>> =
        HashMap::new();
    let mut phase_buckets: HashMap<String, Vec<SpanAgg>> = HashMap::new();
    let now = Instant::now();
    let keep_recent = Duration::from_secs(300);
    let mut spans: Vec<&SpanState> = t.tool_spans.values().chain(t.llm_spans.values()).collect();
    spans.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    for (idx, s) in spans.into_iter().enumerate() {
        if let Some(e) = s.ended_at {
            if now.duration_since(e) > keep_recent {
                continue;
            }
        }
        let dur = match s.ended_at {
            Some(e) => e.duration_since(s.started_at),
            None => now.duration_since(s.started_at),
        };
        let agg = SpanAgg {
            status: s.status,
            label: span_display_label(s),
            description: s.description.clone(),
            count: 1,
            max_dur: dur,
            order_idx: idx,
        };
        if s.ctx.is_none() {
            if let Some(ph) = s.phase.as_deref() {
                phase_buckets.entry(ph.to_string()).or_default().push(agg);
                continue;
            }
        }
        span_buckets.entry(key_from_ctx(&s.ctx)).or_default().push(agg);
    }

    for v in phase_buckets.values_mut() {
        let mut by_key: HashMap<(SpanStatus, String, Option<String>), SpanAgg> = HashMap::new();
        for s in v.drain(..) {
            let k = (s.status, s.label.clone(), s.description.clone());
            by_key
                .entry(k)
                .and_modify(|agg| {
                    agg.count += 1;
                    if s.max_dur > agg.max_dur {
                        agg.max_dur = s.max_dur;
                    }
                    if s.order_idx < agg.order_idx {
                        agg.order_idx = s.order_idx;
                    }
                })
                .or_insert(s);
        }
        v.extend(by_key.into_values());
        v.sort_by(|a, b| match (validate_dbt_sort_rank(&a.label), validate_dbt_sort_rank(&b.label)) {
            (Some(ra), Some(rb)) => ra.cmp(&rb).then_with(|| a.order_idx.cmp(&b.order_idx)),
            _ => a.order_idx.cmp(&b.order_idx),
        });
    }
    for v in span_buckets.values_mut() {
        // Consolidate duplicates (same status + label).
        let mut by_key: HashMap<(SpanStatus, String, Option<String>), SpanAgg> = HashMap::new();
        for s in v.drain(..) {
            let k = (s.status, s.label.clone(), s.description.clone());
            by_key
                .entry(k)
                .and_modify(|agg| {
                    agg.count += 1;
                    if s.max_dur > agg.max_dur {
                        agg.max_dur = s.max_dur;
                    }
                    if s.order_idx < agg.order_idx {
                        agg.order_idx = s.order_idx;
                    }
                })
                .or_insert(s);
        }
        v.extend(by_key.into_values());
        // Keep the original occurrence ordering (stable, non-jumpy).
        v.sort_by(|a, b| match (validate_dbt_sort_rank(&a.label), validate_dbt_sort_rank(&b.label)) {
            (Some(ra), Some(rb)) => ra.cmp(&rb).then_with(|| a.order_idx.cmp(&b.order_idx)),
            _ => a.order_idx.cmp(&b.order_idx),
        });
    }

    for (idx, ph) in t.phases.iter().enumerate() {
        let key = format!("phase:{ph}");
        let item_status: Option<&str> = t.items.get(&key).map(|it| it.status.as_str());
        let blocked = item_status == Some("blocked");
        let is_future = cur_idx.is_some_and(|ci| idx > ci);
        let g = if blocked {
            "[!]".magenta().to_string()
        } else if ph == &cur {
            "[~]".yellow().to_string()
        } else if is_future {
            // Future phases are always rendered as pending for the current loop.
            "[ ]".dark_grey().to_string()
        } else if item_status == Some("ok") || done.contains(ph) {
            "[✓]".green().to_string()
        } else {
            "[ ]".dark_grey().to_string()
        };

        let runtime = if is_future {
            None
        } else {
            t.items.get(&key).and_then(|it| it.runtime_ms).map(fmt_ms)
        };
        let mut phase_line = if let Some(r) = runtime {
            format!("{g} {}  {}", ph.as_str().white(), r.dark_grey())
        } else {
            format!("{g} {}", ph.as_str().white())
        };

        // Attach compact detail summaries for:
        // - the current phase (live progress/reasoning), and
        // - completed (historical) phases.
        if !is_future && (ph == &cur || item_status == Some("ok") || done.contains(ph)) {
            let mut parts: Vec<String> = Vec::new();
            if let Some(detail) = phase_detail_summary(t, ph.as_str()) {
                parts.push(detail);
            }
            if let Some(reason) = phase_reason_summary(t, ph.as_str()) {
                parts.push(reason);
            }
            if !parts.is_empty() {
                phase_line.push_str("  ");
                phase_line.push_str(&parts.join(" | ").dark_grey().to_string());
            }
        }

        // Collapse completed phases into a single parent line; keep current phase expanded
        // so workgroup details remain visible during re-entry loops.
        if !is_future && ph != &cur && (item_status == Some("ok") || done.contains(ph)) {
            lines.push(phase_line);
            continue;
        }

        lines.push(phase_line);

        // Under phase: show no-ctx spans for *that phase*, so tools don't relocate
        // to whatever phase happens to be active right now.
        if let Some(spans) = phase_buckets.get(ph) {
            push_span_list(&mut lines, "  ", spans, spinner_idx);
        }

        // Plan summaries are sticky to their planning phases.
        if anchor_cleanse.as_deref() == Some(ph.as_str()) {
            if let Some(ref p) = t.cleanse_plan {
                render_plan_summary_line(&mut lines, "cleanse", p);
            }
        }
        if anchor_model.as_deref() == Some(ph.as_str()) {
            if let Some(ref p) = t.model_plan {
                render_plan_summary_line(&mut lines, "model", p);
            }
        }

        // Workgroups render under their logical phases (author vs validate).
        if ph == "cleanse_author" {
            if let Some(ref p) = t.cleanse_plan {
                render_plan_workgroups_compact(
                    &mut lines,
                    "cleanse",
                    p,
                    &span_buckets,
                    &t.expanded_work_items,
                    t.focus_by_kind.get("cleanse"),
                    spinner_idx,
                    &[
                        api::PlanWorkGroupKind::AuthorSql,
                        api::PlanWorkGroupKind::AuthorSchema,
                    ],
                );
            }
        }
        if ph == "cleanse_validate" {
            if let Some(ref p) = t.cleanse_plan {
                render_plan_workgroups_compact(
                    &mut lines,
                    "cleanse",
                    p,
                    &span_buckets,
                    &t.expanded_work_items,
                    t.focus_by_kind.get("cleanse"),
                    spinner_idx,
                    &[api::PlanWorkGroupKind::Validate],
                );
            }
        }
        if ph == "model_author" {
            if let Some(ref p) = t.model_plan {
                render_plan_workgroups_compact(
                    &mut lines,
                    "model",
                    p,
                    &span_buckets,
                    &t.expanded_work_items,
                    t.focus_by_kind.get("model"),
                    spinner_idx,
                    &[
                        api::PlanWorkGroupKind::AuthorSql,
                        api::PlanWorkGroupKind::AuthorSchema,
                    ],
                );
            }
        }
        if ph == "model_validate" {
            if let Some(ref p) = t.model_plan {
                render_plan_workgroups_compact(
                    &mut lines,
                    "model",
                    p,
                    &span_buckets,
                    &t.expanded_work_items,
                    t.focus_by_kind.get("model"),
                    spinner_idx,
                    &[api::PlanWorkGroupKind::Validate],
                );
            }
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        // Remove ANSI CSI SGR sequences: ESC [ ... m
        let bytes = s.as_bytes();
        let mut out = String::new();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'm' {
                    i += 1;
                }
                continue;
            }
            // Copy one UTF-8 char.
            let ch = match std::str::from_utf8(&bytes[i..])
                .ok()
                .and_then(|rest| rest.chars().next())
            {
                None => break,
                Some(c) => c,
            };
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }

    #[test]
    fn render_thread_detail_does_not_mark_future_phases_done() {
        let mut tv = ThreadView::default();
        tv.thread_id = "t".to_string();
        tv.phases = vec![
            "preflight".to_string(),
            "cleanse_plan".to_string(),
            "cleanse_author".to_string(),
            "cleanse_validate".to_string(),
            "cleanse_review".to_string(),
        ];
        tv.current_phase = Some("cleanse_author".to_string());

        // Simulate historical completion of later phases.
        tv.completed_phases = vec![
            "preflight".to_string(),
            "cleanse_plan".to_string(),
            "cleanse_author".to_string(),
            "cleanse_validate".to_string(),
            "cleanse_review".to_string(),
        ];
        tv.items.insert(
            "phase:cleanse_validate".to_string(),
            api::ThreadStateItem::new(
                "phase:cleanse_validate".to_string(),
                "phase".to_string(),
                "ok".to_string(),
            ),
        );
        tv.items.insert(
            "phase:cleanse_review".to_string(),
            api::ThreadStateItem::new(
                "phase:cleanse_review".to_string(),
                "phase".to_string(),
                "ok".to_string(),
            ),
        );

        let rendered = strip_ansi(&render_thread_detail(&tv, 0).join("\n"));
        // Even if validate/review completed previously, they are "future" relative to current author phase,
        // so they must render as pending in the current loop.
        assert!(rendered.contains("[ ] cleanse_validate"));
        assert!(rendered.contains("[ ] cleanse_review"));
    }

    #[test]
    fn render_plan_tree_includes_work_item_and_span() {
        let ci = api::PlanChecklistItem::new(
            "sql_model".to_string(),
            "SQL model".to_string(),
            api::PlanChecklistItemStatus::InProgress,
            api::PlanChecklistOrigin::Initial,
        );
        let task = api::CleanseTaskSnapshot::new(
            "t1".to_string(),
            "ds1".to_string(),
            api::PlanTaskStatus::InProgress,
            vec![ci],
        );
        let wg = api::PlanWorkGroup::new(
            "wg1".to_string(),
            "Workgroup Foo".to_string(),
            api::PlanWorkGroupKind::AuthorSql,
            vec![api::PlanWorkGroupItemRef::new(
                "t1".to_string(),
                "sql_model".to_string(),
            )],
        );
        let plan = api::PlanSnapshot::new(
            api::plan_snapshot::PlanKind::Cleanse,
            "plan_k".to_string(),
            api::PlanStatus::Approved,
            vec![api::PlanTask::Cleanse(task)],
            vec![wg],
        );

        let mut span_buckets: HashMap<WorkItemKey, Vec<SpanAgg>> = HashMap::new();
        span_buckets.insert(
            WorkItemKey {
                plan_kind: Some("cleanse".to_string()),
                plan_key: Some("plan_k".to_string()),
                workgroup_id: Some("wg1".to_string()),
                task_id: Some("t1".to_string()),
                checklist_item_id: Some("sql_model".to_string()),
            },
            vec![SpanAgg {
                status: SpanStatus::Running,
                label: "Read file".to_string(),
                description: None,
                count: 1,
                max_dur: Duration::from_millis(1200),
                order_idx: 0,
            }],
        );

        let mut lines: Vec<String> = Vec::new();
        render_plan_tree(&mut lines, "cleanse", &plan, &span_buckets);
        let joined = lines
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("cleanse plan"));
        assert!(joined.contains("plan_k"));
        assert!(joined.contains("Workgroup Foo"));
        assert!(joined.contains("t1"));
        assert!(joined.contains("SQL model"));
        assert!(joined.contains("Read file"));
    }

    #[test]
    fn render_plan_workgroups_compact_keeps_checklist_line_sticky_after_focus_moves() {
        let ci = api::PlanChecklistItem::new(
            "sql_model".to_string(),
            "Author staging SQL model".to_string(),
            api::PlanChecklistItemStatus::InProgress,
            api::PlanChecklistOrigin::Initial,
        );
        let task1 = api::CleanseTaskSnapshot::new(
            "t1".to_string(),
            "ds1".to_string(),
            api::PlanTaskStatus::InProgress,
            vec![ci.clone()],
        );
        let task2 = api::CleanseTaskSnapshot::new(
            "t2".to_string(),
            "ds2".to_string(),
            api::PlanTaskStatus::InProgress,
            vec![ci.clone()],
        );
        let wg = api::PlanWorkGroup::new(
            "wg1".to_string(),
            "Author SQL models for batch 1".to_string(),
            api::PlanWorkGroupKind::AuthorSql,
            vec![
                api::PlanWorkGroupItemRef::new("t1".to_string(), "sql_model".to_string()),
                api::PlanWorkGroupItemRef::new("t2".to_string(), "sql_model".to_string()),
            ],
        );
        let plan = api::PlanSnapshot::new(
            api::plan_snapshot::PlanKind::Cleanse,
            "plan_k".to_string(),
            api::PlanStatus::Approved,
            vec![
                api::PlanTask::Cleanse(task1),
                api::PlanTask::Cleanse(task2),
            ],
            vec![wg],
        );

        // Focus moved to t2, but t1 is sticky-expanded.
        let preferred = WorkItemKey {
            plan_kind: Some("cleanse".to_string()),
            plan_key: Some("plan_k".to_string()),
            workgroup_id: Some("wg1".to_string()),
            task_id: Some("t2".to_string()),
            checklist_item_id: Some("sql_model".to_string()),
        };
        let mut expanded: std::collections::HashSet<WorkItemKey> = std::collections::HashSet::new();
        expanded.insert(WorkItemKey {
            plan_kind: Some("cleanse".to_string()),
            plan_key: Some("plan_k".to_string()),
            workgroup_id: Some("wg1".to_string()),
            task_id: Some("t1".to_string()),
            checklist_item_id: Some("sql_model".to_string()),
        });

        let span_buckets: HashMap<WorkItemKey, Vec<SpanAgg>> = HashMap::new();
        let mut lines: Vec<String> = Vec::new();
        render_plan_workgroups_compact(
            &mut lines,
            "cleanse",
            &plan,
            &span_buckets,
            &expanded,
            Some(&preferred),
            0,
            &[api::PlanWorkGroupKind::AuthorSql],
        );
        let joined = lines.join("\n");
        // Checklist label line should still be present for t1 due to sticky expansion.
        assert!(joined.contains("t1"));
        assert!(joined.contains("Author staging SQL model"));
    }

    #[test]
    fn render_plan_workgroups_compact_renders_spans_for_nonfocused_item_when_present() {
        let ci = api::PlanChecklistItem::new(
            "sql_model".to_string(),
            "Author staging SQL model".to_string(),
            api::PlanChecklistItemStatus::InProgress,
            api::PlanChecklistOrigin::Initial,
        );
        let task1 = api::CleanseTaskSnapshot::new(
            "t1".to_string(),
            "ds1".to_string(),
            api::PlanTaskStatus::InProgress,
            vec![ci.clone()],
        );
        let task2 = api::CleanseTaskSnapshot::new(
            "t2".to_string(),
            "ds2".to_string(),
            api::PlanTaskStatus::InProgress,
            vec![ci.clone()],
        );
        let wg = api::PlanWorkGroup::new(
            "wg1".to_string(),
            "Author SQL models for batch 1".to_string(),
            api::PlanWorkGroupKind::AuthorSql,
            vec![
                api::PlanWorkGroupItemRef::new("t1".to_string(), "sql_model".to_string()),
                api::PlanWorkGroupItemRef::new("t2".to_string(), "sql_model".to_string()),
            ],
        );
        let plan = api::PlanSnapshot::new(
            api::plan_snapshot::PlanKind::Cleanse,
            "plan_k".to_string(),
            api::PlanStatus::Approved,
            vec![
                api::PlanTask::Cleanse(task1),
                api::PlanTask::Cleanse(task2),
            ],
            vec![wg],
        );

        let preferred = WorkItemKey {
            plan_kind: Some("cleanse".to_string()),
            plan_key: Some("plan_k".to_string()),
            workgroup_id: Some("wg1".to_string()),
            task_id: Some("t2".to_string()),
            checklist_item_id: Some("sql_model".to_string()),
        };

        let mut span_buckets: HashMap<WorkItemKey, Vec<SpanAgg>> = HashMap::new();
        span_buckets.insert(
            WorkItemKey {
                plan_kind: Some("cleanse".to_string()),
                plan_key: Some("plan_k".to_string()),
                workgroup_id: Some("wg1".to_string()),
                task_id: Some("t1".to_string()),
                checklist_item_id: Some("sql_model".to_string()),
            },
            vec![SpanAgg {
                status: SpanStatus::Ok,
                label: "Apply Next Cleanse Batch".to_string(),
                description: None,
                count: 1,
                max_dur: Duration::from_millis(1200),
                order_idx: 0,
            }],
        );

        let expanded: std::collections::HashSet<WorkItemKey> = std::collections::HashSet::new();
        let mut lines: Vec<String> = Vec::new();
        render_plan_workgroups_compact(
            &mut lines,
            "cleanse",
            &plan,
            &span_buckets,
            &expanded,
            Some(&preferred),
            0,
            &[api::PlanWorkGroupKind::AuthorSql],
        );
        let joined = lines.join("\n");
        // Even though focus is on t2, spans for t1 should render.
        assert!(joined.contains("t1"));
        assert!(joined.contains("Apply Next Cleanse Batch"));
    }
}

