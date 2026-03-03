use super::{
    ThreadItemError, ThreadItemKind, ThreadItemState, ThreadItemStatus, ThreadState, ThreadStep,
    ThreadStore, THREAD_STATE_SCHEMA_VERSION,
};

impl ThreadStore {
    pub(crate) fn new_thread_state(thread_id: &str) -> ThreadState {
        ThreadState {
            thread_state_schema_version: THREAD_STATE_SCHEMA_VERSION,
            thread_id: thread_id.to_string(),
            ..ThreadState::default()
        }
    }

    pub(crate) async fn materialize_thread_state_incremental(
        &self,
        thread_id: &str,
        want_step_count: usize,
        step: &ThreadStep,
    ) -> Result<(), String> {
        let mut state = self
            .get_thread_state(thread_id)
            .await
            .unwrap_or_else(|_| Self::new_thread_state(thread_id));
        let mut reset_snapshot = false;
        if state.last_materialized_step_count > want_step_count.saturating_sub(1) {
            return Err(format!(
                "thread_state materialization mismatch: state_count={} want_step_count={}",
                state.last_materialized_step_count, want_step_count
            ));
        }
        if state.last_materialized_step_count < want_step_count.saturating_sub(1) {
            // Hard cutover: do not replay thread logs for state reconstruction.
            // If a gap is detected, reset to an empty state snapshot and continue incrementally.
            state = Self::new_thread_state(thread_id);
            reset_snapshot = true;
        }
        apply_step_to_state(&mut state, want_step_count.saturating_sub(1), step);
        state.thread_state_schema_version = THREAD_STATE_SCHEMA_VERSION;
        state.thread_id = thread_id.to_string();
        state.last_materialized_step_count = want_step_count;
        state.total_runtime_ms = state
            .items
            .values()
            .filter(|it| it.kind == ThreadItemKind::Phase)
            .filter_map(|it| it.runtime_ms)
            .sum();
        if reset_snapshot {
            self.put_thread_state_replace(thread_id, &state).await?;
        } else {
            self.put_thread_state(thread_id, &state).await?;
        }
        Ok(())
    }
}

fn duration_ms(start_ts: &str, end_ts: &str) -> Option<u64> {
    let start = chrono::DateTime::parse_from_rfc3339(start_ts).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(end_ts).ok()?;
    let delta = end.signed_duration_since(start);
    let ms = delta.num_milliseconds();
    if ms <= 0 {
        return Some(0);
    }
    Some(ms as u64)
}

pub(crate) fn apply_step_to_state(st: &mut ThreadState, _step_idx: usize, step: &ThreadStep) {
    match step {
        ThreadStep::SwitchSuite { to, .. } => {
            if !to.trim().is_empty() {
                st.suite_id = Some(to.to_string());
            }
        }
        ThreadStep::SwitchAgent { to, .. } => {
            if !to.trim().is_empty() {
                st.agent_type = Some(to.to_string());
            }
        }
        ThreadStep::Phase {
            phase,
            from_phase,
            ts,
            ..
        } => {
            let ph = phase.trim().to_string();
            if ph.is_empty() {
                return;
            }

            if let Some(prev) = from_phase
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                let key = format!("phase:{}", prev);
                let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
                    kind: ThreadItemKind::Phase,
                    status: ThreadItemStatus::Ok,
                    started_at: None,
                    finished_at: Some(ts.clone()),
                    runtime_ms: None,
                    last_error: None,
                    outputs: None,
                });
                ent.kind = ThreadItemKind::Phase;
                ent.status = ThreadItemStatus::Ok;
                ent.finished_at.get_or_insert_with(|| ts.clone());
                if ent.runtime_ms.is_none() {
                    if let (Some(ref started), Some(ref finished)) = (&ent.started_at, &ent.finished_at) {
                        ent.runtime_ms = duration_ms(started, finished);
                    }
                }
            }

            let key = format!("phase:{}", ph);
            let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
                kind: ThreadItemKind::Phase,
                status: ThreadItemStatus::Running,
                started_at: Some(ts.clone()),
                finished_at: None,
                runtime_ms: None,
                last_error: None,
                outputs: None,
            });
            ent.kind = ThreadItemKind::Phase;
            ent.status = ThreadItemStatus::Running;
            ent.started_at.get_or_insert_with(|| ts.clone());
            st.current_phase = Some(ph);
        }
        ThreadStep::ToolStart { .. } => {}
        ThreadStep::ToolEnd { .. } => {}
        ThreadStep::LlmStart { .. } => {}
        ThreadStep::LlmEnd { .. } => {}
        ThreadStep::Final { ts, observation, .. } => {
            // A `final` marks the end of a run. Close out the currently-running phase so
            // UIs can mark the terminal phase (often `done`) as completed.
            let Some(ph) = st
                .current_phase
                .clone()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                return;
            };

            let key = format!("phase:{}", ph);
            let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
                kind: ThreadItemKind::Phase,
                status: ThreadItemStatus::Running,
                started_at: Some(ts.clone()),
                finished_at: None,
                runtime_ms: None,
                last_error: None,
                outputs: None,
            });
            ent.kind = ThreadItemKind::Phase;
            ent.status = if observation.ok {
                ThreadItemStatus::Ok
            } else {
                ThreadItemStatus::Failed
            };
            ent.finished_at = Some(ts.clone());
            if ent.runtime_ms.is_none() {
                if let (Some(ref started), Some(ref finished)) = (&ent.started_at, &ent.finished_at) {
                    ent.runtime_ms = duration_ms(started, finished);
                }
            }
        }
        ThreadStep::AskUser { prompt, ts, .. } => {
            block_current_phase(st, prompt, ts);
        }
        ThreadStep::AskApproval { prompt, ts, .. } => {
            block_current_phase(st, prompt, ts);
        }
        ThreadStep::GuardBlock { reason, ts, .. } => {
            block_current_phase(st, reason, ts);
        }
        _ => {}
    }
}

fn block_current_phase(st: &mut ThreadState, msg: &str, ts: &str) {
    let Some(ph) = st.current_phase.clone() else {
        return;
    };
    let key = format!("phase:{}", ph);
    let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
        kind: ThreadItemKind::Phase,
        status: ThreadItemStatus::Blocked,
        started_at: Some(ts.to_string()),
        finished_at: None,
        runtime_ms: None,
        last_error: None,
        outputs: None,
    });
    ent.kind = ThreadItemKind::Phase;
    ent.status = ThreadItemStatus::Blocked;
    ent.last_error = Some(ThreadItemError {
        summary: msg.to_string(),
        tool_step_idx: None,
        step_ts: Some(ts.to_string()),
    });
}
