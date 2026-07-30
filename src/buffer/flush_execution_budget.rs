use std::io;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::metrics::counters;

/// Independent bounds for the two expensive stages of a WAL flush.
///
/// The scheduler's current tuner/env-derived targets are snapshotted when a
/// compaction cycle starts. The tuner remains the owner of those targets.
#[derive(Clone)]
pub(crate) struct FlushExecutionBudget {
    decode: Arc<Semaphore>,
    sink: Arc<Semaphore>,
    decode_limit: usize,
    sink_limit: usize,
}

impl FlushExecutionBudget {
    pub(crate) fn new(decode_limit: usize, sink_limit: usize) -> Self {
        let decode_limit = decode_limit.max(1);
        let sink_limit = sink_limit.max(1);
        Self {
            decode: Arc::new(Semaphore::new(decode_limit)),
            sink: Arc::new(Semaphore::new(sink_limit)),
            decode_limit,
            sink_limit,
        }
    }

    pub(crate) fn decode_limit(&self) -> usize {
        self.decode_limit
    }

    pub(crate) fn sink_limit(&self) -> usize {
        self.sink_limit
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
}
