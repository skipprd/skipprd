use std::io;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::ingest::tuner::FlushBudgetSnapshot;
use crate::metrics::counters;

/// One scheduler-cycle view of the authoritative flush budget.
///
/// Scheduler slots and both expensive-stage semaphores are built from the same
/// tuner generation so a cycle cannot mix independently updated atomics.
#[derive(Clone)]
pub(crate) struct FlushExecutionBudget {
    decode: Arc<Semaphore>,
    sink: Arc<Semaphore>,
    scheduler_limit: usize,
    decode_limit: usize,
    sink_limit: usize,
    generation: u64,
}

impl FlushExecutionBudget {
    #[cfg(test)]
    pub(crate) fn new(decode_limit: usize, sink_limit: usize) -> Self {
        let decode_limit = decode_limit.max(1);
        let sink_limit = sink_limit.max(1);
        Self {
            decode: Arc::new(Semaphore::new(decode_limit)),
            sink: Arc::new(Semaphore::new(sink_limit)),
            scheduler_limit: decode_limit,
            decode_limit,
            sink_limit,
            generation: 0,
        }
    }

    pub(crate) fn from_snapshot(snapshot: FlushBudgetSnapshot) -> Self {
        let scheduler_limit = snapshot.scheduler_jobs.max(1);
        let decode_limit = snapshot.decode_jobs.max(1);
        let sink_limit = snapshot.sink_sessions.max(1);
        Self {
            decode: Arc::new(Semaphore::new(decode_limit)),
            sink: Arc::new(Semaphore::new(sink_limit)),
            scheduler_limit,
            decode_limit,
            sink_limit,
            generation: snapshot.generation,
        }
    }

    pub(crate) fn scheduler_limit(&self) -> usize {
        self.scheduler_limit
    }

    pub(crate) fn decode_limit(&self) -> usize {
        self.decode_limit
    }

    pub(crate) fn sink_limit(&self) -> usize {
        self.sink_limit
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) async fn acquire_decode(&self) -> io::Result<OwnedSemaphorePermit> {
        let started = Instant::now();
        let permit = self
            .decode
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("WAL decode execution budget closed"))?;
        counters::record_compaction_decode_permit_wait(started.elapsed());
        Ok(permit)
    }

    pub(crate) async fn acquire_sink(&self) -> io::Result<OwnedSemaphorePermit> {
        let started = Instant::now();
        let permit = self
            .sink
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("sink apply execution budget closed"))?;
        counters::record_compaction_sink_permit_wait(started.elapsed());
        Ok(permit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn decode_and_sink_budgets_are_independent() {
        let budget = FlushExecutionBudget::new(1, 2);
        assert_eq!(budget.decode_limit(), 1);
        assert_eq!(budget.sink_limit(), 2);

        let _decode = budget.acquire_decode().await.unwrap();
        let _sink_a = budget.acquire_sink().await.unwrap();
        let _sink_b = budget.acquire_sink().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), budget.acquire_decode())
                .await
                .is_err()
        );
    }

    #[test]
    fn one_snapshot_sets_all_scheduler_stage_limits() {
        let snapshot = FlushBudgetSnapshot {
            scheduler_jobs: 7,
            decode_jobs: 5,
            sink_sessions: 3,
            generation: 42,
            ..FlushBudgetSnapshot::default()
        };
        let budget = FlushExecutionBudget::from_snapshot(snapshot);
        assert_eq!(budget.scheduler_limit(), 7);
        assert_eq!(budget.decode_limit(), 5);
        assert_eq!(budget.sink_limit(), 3);
        assert_eq!(budget.generation(), 42);
    }
}
