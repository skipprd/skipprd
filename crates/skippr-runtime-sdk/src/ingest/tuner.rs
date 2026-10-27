use std::sync::atomic::Ordering;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::metrics::counters;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushMode {
    Ingest,
    Paused,
    Drain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlushBudgetSnapshot {
    pub scheduler_jobs: usize,
    pub decode_jobs: usize,
    pub sink_sessions: usize,
    pub upload_sessions: usize,
    pub multipart_parts: usize,
    pub catalog_operations: usize,
    pub ingest_reserved_cores: usize,
    pub generation: u64,
}

impl Default for FlushBudgetSnapshot {
    fn default() -> Self {
        Self {
            scheduler_jobs: 1,
            decode_jobs: 1,
            sink_sessions: 1,
            upload_sessions: 1,
            multipart_parts: 1,
            catalog_operations: 1,
            ingest_reserved_cores: 1,
            generation: 0,
        }
    }
}

static CHILD_BUDGET: Lazy<Mutex<FlushBudgetSnapshot>> =
    Lazy::new(|| Mutex::new(FlushBudgetSnapshot::default()));
static INITIALIZED: Lazy<Mutex<bool>> = Lazy::new(|| Mutex::new(false));

fn seed() -> FlushBudgetSnapshot {
    let num_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(2);
    let catalog_operations = (num_cpus / 8).clamp(2, 16);
    FlushBudgetSnapshot {
        scheduler_jobs: num_cpus.saturating_mul(2).clamp(2, 128),
        decode_jobs: num_cpus.clamp(1, 128),
        sink_sessions: (num_cpus / 4).clamp(2, 16),
        upload_sessions: num_cpus.saturating_mul(2).clamp(4, 256),
        multipart_parts: (num_cpus / 8).clamp(2, 8),
        catalog_operations,
        ingest_reserved_cores: (num_cpus / 4).max(1),
        generation: 1,
    }
}

/// Seed this child process from hardware. Plugins do not share the host tuner mutex.
pub fn apply_env_caps() {
    let mut initialized = INITIALIZED
        .lock()
        .expect("child flush budget mutex poisoned");
    if *initialized {
        return;
    }
    let snapshot = seed();
    counters::ATHENA_GLUE_CP_TARGET.store(snapshot.catalog_operations, Ordering::Relaxed);
    *CHILD_BUDGET
        .lock()
        .expect("child flush budget mutex poisoned") = snapshot;
    *initialized = true;
}

pub fn current_flush_budget() -> FlushBudgetSnapshot {
    apply_env_caps();
    *CHILD_BUDGET
        .lock()
        .expect("child flush budget mutex poisoned")
}

pub fn update_flush_budget(
    _mode: FlushMode,
    _has_backlog: bool,
    _ingest_sample: Option<(usize, usize)>,
) -> FlushBudgetSnapshot {
    apply_env_caps();
    current_flush_budget()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_budget_seeds_catalog_operations() {
        apply_env_caps();
        assert!(current_flush_budget().catalog_operations >= 1);
    }
}
