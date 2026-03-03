use super::{
    LlmStepStatus, ThreadEvent, ThreadEventKind, ThreadEventStatus, ThreadLog, ThreadStep, ToolStepStatus,
};

pub(crate) fn build_thread_events_from_log(log: &ThreadLog, max_events: usize) -> Vec<ThreadEvent> {
    let paired_tool_ids: std::collections::HashSet<String> = {
        let mut starts: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut ends: std::collections::HashSet<String> = std::collections::HashSet::new();
        for s in log.steps.iter() {
            match s {
                ThreadStep::ToolStart { tool_id, .. } => {
                    starts.insert(tool_id.clone());
                }
                ThreadStep::ToolEnd { tool_id, .. } => {
                    ends.insert(tool_id.clone());
                }
                _ => {}
            }
        }
        starts.intersection(&ends).cloned().collect()
    };
    let mut events: Vec<ThreadEvent> = Vec::new();
    for (idx, step) in log.steps.iter().enumerate() {
        if let ThreadStep::ToolStart { tool_id, .. } | ThreadStep::ToolEnd { tool_id, .. } = step {
            if !paired_tool_ids.contains(tool_id) {
                continue;
            }
        }
        let current_phase = match step {
            ThreadStep::Phase { phase, .. } => Some(phase.as_str()),
            _ => events
                .iter()
                .rev()
                .find_map(|e| e.phase.as_deref())
                .or(Some("preflight")),
        };
        if let Some(ev) = step_to_timeline_event(idx, step, current_phase) {
            events.push(ev);
        }
    }
    if events.len() > max_events {
        let drop_n = events.len() - max_events;
        events.drain(0..drop_n);
    }
    events
}

fn step_to_timeline_event(
    step_idx: usize,
    step: &ThreadStep,
    current_phase: Option<&str>,
) -> Option<ThreadEvent> {
    match step {
        ThreadStep::ToolStart {
            tool_id,
            name,
            clean_name,
            payload,
            ctx,
            ts,
            ..
        } => Some(ThreadEvent {
            step_idx,
            event_kind: ThreadEventKind::ToolStart,
            ts: ts.clone(),
            tool_id: Some(tool_id.clone()),
            name: Some(name.clone()),
            clean_name: Some(clean_name.clone()),
            status: Some(ThreadEventStatus::Running),
            runtime_ms: None,
            payload: payload.clone(),
            error: None,
            call_id: None,
            model: None,
            phase: Some(current_phase.unwrap_or("preflight").to_string()),
            ctx: ctx.clone(),
        }),
        ThreadStep::ToolEnd {
            tool_id,
            name,
            clean_name,
            status,
            payload,
            ctx,
            observation,
            ts,
            ..
        } => Some(ThreadEvent {
            step_idx,
            event_kind: ThreadEventKind::ToolEnd,
            ts: ts.clone(),
            tool_id: Some(tool_id.clone()),
            name: Some(name.clone()),
            clean_name: Some(clean_name.clone()),
            status: Some(if *status == ToolStepStatus::Failed {
                ThreadEventStatus::Failed
            } else {
                ThreadEventStatus::Ok
            }),
            runtime_ms: None,
            payload: payload.clone(),
            error: if observation.ok {
                None
            } else {
                observation.first_error_or_context()
            },
            call_id: None,
            model: None,
            phase: Some(current_phase.unwrap_or("preflight").to_string()),
            ctx: ctx.clone(),
        }),
        ThreadStep::LlmStart {
            call_id,
            model,
            phase,
            ctx,
            ts,
            ..
        } => Some(ThreadEvent {
            step_idx,
            event_kind: ThreadEventKind::LlmStart,
            ts: ts.clone(),
            tool_id: None,
            name: None,
            clean_name: None,
            status: Some(ThreadEventStatus::Running),
            runtime_ms: None,
            payload: None,
            error: None,
            call_id: Some(*call_id),
            model: model.clone(),
            phase: Some(phase.clone()),
            ctx: ctx.clone(),
        }),
        ThreadStep::LlmEnd {
            call_id,
            model,
            phase,
            status,
            error,
            ctx,
            ts,
            ..
        } => Some(ThreadEvent {
            step_idx,
            event_kind: ThreadEventKind::LlmEnd,
            ts: ts.clone(),
            tool_id: None,
            name: None,
            clean_name: None,
            status: Some(if *status == LlmStepStatus::Failed {
                ThreadEventStatus::Failed
            } else {
                ThreadEventStatus::Ok
            }),
            runtime_ms: None,
            payload: None,
            error: error.clone(),
            call_id: Some(*call_id),
            model: model.clone(),
            phase: Some(phase.clone()),
            ctx: ctx.clone(),
        }),
        _ => None,
    }
}
