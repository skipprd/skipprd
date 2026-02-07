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
    count: usize,
    max_dur: Duration,
    order_idx: usize,
}

#[derive(Clone, Debug)]
struct SpanState {
    label: String,
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
    tool_spans: HashMap<String, SpanState>, // tool_id -> span
    llm_spans: HashMap<i32, SpanState>,     // call_id -> span
    // Persist the most recent concrete work item context per plan kind
    // so focused rendering doesn't "disappear" when some events arrive without ctx.
    focus_by_kind: HashMap<String, WorkItemKey>,
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
            tool_spans: HashMap::new(),
            llm_spans: HashMap::new(),
            focus_by_kind: HashMap::new(),
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

    // Always repaint from top-left to avoid drift/duplication when the terminal scrolls
    // or when output exceeds the visible viewport.
    crossterm::execute!(stdout, MoveTo(0, 0), Clear(ClearType::FromCursorDown))?;

    // Bound render to the viewport. If it doesn't fit, truncate and add an ellipsis line.
    if new_lines.len() > max_lines {
        new_lines.truncate(max_lines);
        if let Some(last) = new_lines.last_mut() {
            *last = "…".to_string();
        }
    }

    for (i, line) in new_lines.iter().enumerate() {
        crossterm::execute!(stdout, Clear(ClearType::CurrentLine), MoveToColumn(0))?;
        write!(stdout, "{line}")?;
        // Avoid scrolling: don't emit a trailing newline on the last painted line.
        if i + 1 < new_lines.len() {
            writeln!(stdout)?;
        }
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
            }
            if model.is_some() {
                tv.model_plan = model;
            }
            tv.last_update = Instant::now();
            ensure_selected(m, &thread_id);
        }
        TerminalEvent::PlansChanged(ev) => {
            let _ = ev;
        }
        TerminalEvent::Phase(ev) => {
            let _ = ev;
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
            tv.tool_spans.insert(
                ev.tool_id.clone(),
                SpanState {
                    label: ev.clean_name.clone().unwrap_or(ev.name.clone()),
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
                    tv.focus_by_kind.insert(pk, key_from_ctx(&Some(ctx.clone())));
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
            tv.tool_spans
                .entry(ev.tool_id.clone())
                .and_modify(|s| {
                    s.label = ev.clean_name.clone().unwrap_or(ev.name.clone());
                    s.status = st;
                    s.ended_at = Some(Instant::now());
                    s.phase = effective_phase.clone();
                    s.ctx = ev.ctx.clone();
                })
                .or_insert_with(|| SpanState {
                    label: ev.clean_name.clone().unwrap_or(ev.name.clone()),
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
                    tv.focus_by_kind.insert(pk, key_from_ctx(&Some(ctx.clone())));
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
                    tv.focus_by_kind.insert(pk, key_from_ctx(&Some(ctx.clone())));
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
                    s.status = st;
                    s.ended_at = Some(Instant::now());
                    s.phase = Some(ev.phase.clone());
                    s.ctx = ev.ctx.clone();
                })
                .or_insert_with(|| SpanState {
                    label,
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
                    tv.focus_by_kind.insert(pk, key_from_ctx(&Some(ctx.clone())));
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
        format!("{}m{}s", s / 60, s % 60)
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
            if best_running.as_ref().map(|(_, d)| max_d > *d).unwrap_or(true) {
                best_running = Some((k.clone(), max_d));
            }
        }
        if best_any.as_ref().map(|(_, d)| max_d > *d).unwrap_or(true) {
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

fn render_model(m: &Model) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let up = format!("{}s", m.started.elapsed().as_secs());
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
            label: s.label.clone(),
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
        let mut by_key: HashMap<(SpanStatus, String), SpanAgg> = HashMap::new();
        for s in v.drain(..) {
            let k = (s.status, s.label.clone());
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
        v.sort_by(|a, b| a.order_idx.cmp(&b.order_idx));
    }
    for v in span_buckets.values_mut() {
        // Consolidate duplicates (same status + label).
        let mut by_key: HashMap<(SpanStatus, String), SpanAgg> = HashMap::new();
        for s in v.drain(..) {
            let k = (s.status, s.label.clone());
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
        v.sort_by(|a, b| a.order_idx.cmp(&b.order_idx));
    }

    let mut saw_cleanse_phase = false;
    let mut saw_model_phase = false;

    for ph in t.phases.iter() {
        let key = format!("phase:{ph}");
        let blocked = t
            .items
            .get(&key)
            .map(|it| it.status == "blocked")
            .unwrap_or(false);
        let g = if blocked {
            "[!]".magenta().to_string()
        } else if done.contains(ph) {
            "[✓]".green().to_string()
        } else if ph == &cur {
            "[~]".yellow().to_string()
        } else {
            "[ ]".dark_grey().to_string()
        };

        let runtime = t.items.get(&key).and_then(|it| it.runtime_ms).map(fmt_ms);
        if let Some(r) = runtime {
            lines.push(format!(
                "{g} {}  {}",
                ph.as_str().white(),
                r.dark_grey()
            ));
        } else {
            lines.push(format!("{g} {}", ph.as_str().white()));
        }

        // Under phase: show no-ctx spans for *that phase*, so tools don't relocate
        // to whatever phase happens to be active right now.
        if let Some(spans) = phase_buckets.get(ph) {
            push_span_list(&mut lines, "  ", spans, spinner_idx);
        }

        // Render a focused plan view under its phase.
        if ph == "cleanse_plan" {
            saw_cleanse_phase = true;
            if let Some(ref p) = t.cleanse_plan {
                render_plan_compact(
                    &mut lines,
                    "cleanse",
                    p,
                    &span_buckets,
                    t.focus_by_kind.get("cleanse"),
                    spinner_idx,
                );
            }
        }
        if ph == "model_plan" {
            saw_model_phase = true;
            if let Some(ref p) = t.model_plan {
                render_plan_compact(
                    &mut lines,
                    "model",
                    p,
                    &span_buckets,
                    t.focus_by_kind.get("model"),
                    spinner_idx,
                );
            }
        }
    }

    // If we have a plan snapshot but didn't see its phase, append it.
    if t.cleanse_plan.is_some() && !saw_cleanse_phase {
        lines.push(String::new());
        render_plan_compact(
            &mut lines,
            "cleanse",
            t.cleanse_plan.as_ref().unwrap(),
            &span_buckets,
            t.focus_by_kind.get("cleanse"),
            spinner_idx,
        );
    }
    if t.model_plan.is_some() && !saw_model_phase {
        lines.push(String::new());
        render_plan_compact(
            &mut lines,
            "model",
            t.model_plan.as_ref().unwrap(),
            &span_buckets,
            t.focus_by_kind.get("model"),
            spinner_idx,
        );
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

