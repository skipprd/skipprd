use crate::buffer::compaction_index::CompactionLaneKey;
use crate::metrics::counters as metrics_hot;
use futures::stream::StreamExt;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

const BUDGET_RETUNE_INTERVAL: Duration = Duration::from_secs(10);
const BUDGET_RETUNE_WAVES: usize = 4;

pub(crate) enum CompactorCommand {
    Wake,
    DrainAndStop(tokio::sync::oneshot::Sender<bool>),
}

pub(crate) trait SchedulableCompactionWork {
    fn lane(&self) -> CompactionLaneKey;
}

pub(crate) struct CompactionPlanBatch<W> {
    pub(crate) works: Vec<W>,
    pub(crate) ready_work_remaining: bool,
}

pub(crate) struct CompactionCycleControl<'a> {
    pub(crate) rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<CompactorCommand>,
    pub(crate) drain_reply: &'a mut Option<tokio::sync::oneshot::Sender<bool>>,
    pub(crate) stop_requested: &'a mut bool,
}

impl CompactionCycleControl<'_> {
    fn handle_command(&mut self, command: Option<CompactorCommand>) {
        match command {
            Some(CompactorCommand::Wake) => {}
            Some(CompactorCommand::DrainAndStop(reply)) => {
                if self.drain_reply.is_none() {
                    *self.drain_reply = Some(reply);
                } else {
                    let _ = reply.send(false);
                }
            }
            None => *self.stop_requested = true,
        }
    }

    fn drain_pending_commands(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(command) => self.handle_command(Some(command)),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.handle_command(None);
                    break;
                }
            }
        }
    }

    pub(crate) fn force(&self, initial_force: bool) -> bool {
        initial_force || self.drain_reply.is_some()
    }

    fn stopping(&self) -> bool {
        *self.stop_requested
    }
}

struct ScheduledCompactionMetricGuard {
    sink_ref: String,
}

impl ScheduledCompactionMetricGuard {
    fn new(sink_ref: String) -> Self {
        metrics_hot::inc_compaction_active_job(&sink_ref);
        Self { sink_ref }
    }
}

impl Drop for ScheduledCompactionMetricGuard {
    fn drop(&mut self) {
        metrics_hot::dec_compaction_active_job(&self.sink_ref);
    }
}

pub(crate) async fn drive_work_conserving<W, P, E>(
    concurrency: usize,
    initial_force: bool,
    mut planner: P,
    mut execute: E,
    mut control: Option<&mut CompactionCycleControl<'_>>,
) -> bool
where
    W: SchedulableCompactionWork + Send + 'static,
    P: FnMut(
        usize,
        bool,
        &HashMap<CompactionLaneKey, usize>,
        &HashSet<CompactionLaneKey>,
    ) -> CompactionPlanBatch<W>,
    E: FnMut(W) -> Pin<Box<dyn Future<Output = bool> + Send>>,
{
    let concurrency = concurrency.max(1);
    let cycle_started = Instant::now();
    let max_completions_before_retune = concurrency.saturating_mul(BUDGET_RETUNE_WAVES).max(1);
    let mut completions = 0usize;
    let mut retune_requested = false;
    let mut made_progress = false;
    let mut active_by_lane = HashMap::<CompactionLaneKey, usize>::new();
    let mut blocked_lanes = HashSet::<CompactionLaneKey>::new();
    let mut in_flight: futures::stream::FuturesUnordered<
        Pin<Box<dyn Future<Output = (CompactionLaneKey, bool)> + Send>>,
    > = futures::stream::FuturesUnordered::new();

    if let Some(control) = control.as_deref_mut() {
        control.drain_pending_commands();
    }

    loop {
        let stopping = control
            .as_deref()
            .map(CompactionCycleControl::stopping)
            .unwrap_or(false);
        if !stopping && !retune_requested {
            loop {
                let slots = concurrency.saturating_sub(in_flight.len());
                if slots == 0 {
                    break;
                }
                let force = control
                    .as_deref()
                    .map(|control| control.force(initial_force))
                    .unwrap_or(initial_force);
                let batch = planner(slots, force, &active_by_lane, &blocked_lanes);
                if batch.works.is_empty() {
                    if batch.ready_work_remaining {
                        metrics_hot::add_compaction_idle_slots_with_ready_work(slots);
                    }
                    break;
                }
                debug_assert!(batch.works.len() <= slots);
                metrics_hot::add_compaction_scheduler_top_up();
                for work in batch.works {
                    let lane = work.lane();
                    *active_by_lane.entry(lane.clone()).or_default() += 1;
                    let metric_guard = ScheduledCompactionMetricGuard::new(lane.sink_ref.clone());
                    let future = execute(work);
                    in_flight.push(Box::pin(async move {
                        let _metric_guard = metric_guard;
                        (lane, future.await)
                    }));
                }
            }
        }

        if in_flight.is_empty() {
            break;
        }

        let completed = if control
            .as_deref()
            .map(CompactionCycleControl::stopping)
            .unwrap_or(false)
        {
            Some(in_flight.next().await)
        } else if let Some(control) = control.as_deref_mut() {
            tokio::select! {
                result = in_flight.next() => Some(result),
                command = control.rx.recv() => {
                    control.handle_command(command);
                    None
                }
            }
        } else {
            Some(in_flight.next().await)
        };

        let Some(completed) = completed else {
            continue;
        };
        let Some((lane, compacted)) = completed else {
            break;
        };
        let remove_lane = if let Some(active) = active_by_lane.get_mut(&lane) {
            *active = active.saturating_sub(1);
            *active == 0
        } else {
            false
        };
        if remove_lane {
            active_by_lane.remove(&lane);
        }
        if compacted {
            made_progress = true;
        } else {
            // Retry at most once per lane per cycle. Other namespaces and sinks
            // remain eligible, so one failure cannot stop useful work.
            blocked_lanes.insert(lane);
        }
        completions = completions.saturating_add(1);
        if completions >= max_completions_before_retune
            || cycle_started.elapsed() >= BUDGET_RETUNE_INTERVAL
        {
            // Return to the compactor loop after draining work already in flight.
            // The next cycle samples a fresh FlushBudget generation, so a
            // continuously non-empty ready queue cannot pin startup concurrency.
            retune_requested = true;
        }
    }

    made_progress
}
