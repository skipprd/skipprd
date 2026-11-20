use std::io;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::ingest::tuner::FlushBudgetSnapshot;
use crate::metrics::counters;
use crate::plugins::cdc::SinkCapability;
use crate::serdes::output_format::OutputFormat;

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

    /// Native table sinks share `sink_sessions`. File-format sinks use
    /// `scheduler_jobs` for both the sink semaphore and the per-`sink_ref`
    /// plan limit so a group is not decoded and then parked on a smaller permit.
    pub(crate) fn from_snapshot(
        snapshot: FlushBudgetSnapshot,
        capability: &SinkCapability,
    ) -> Self {
        let scheduler_limit = snapshot.scheduler_jobs.max(1);
        let decode_limit = snapshot.decode_jobs.max(1);
        let sink_limit = if OutputFormat::is_native_set(capability.supported_formats) {
            snapshot.sink_sessions.max(1)
        } else {
            scheduler_limit
        };
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

/// Latest flush budget. Jobs load it when they acquire. A swap replaces the
/// `Arc`; permits already held on the previous semaphore run to completion.
pub(crate) struct FlushBudgetSlot {
    current: std::sync::Mutex<Arc<FlushExecutionBudget>>,
}

impl FlushBudgetSlot {
    pub(crate) fn new(budget: Arc<FlushExecutionBudget>) -> Arc<Self> {
        Arc::new(Self {
            current: std::sync::Mutex::new(budget),
        })
    }

    pub(crate) fn load(&self) -> Arc<FlushExecutionBudget> {
        self.current.lock().expect("flush budget slot").clone()
    }

    pub(crate) fn store(&self, budget: Arc<FlushExecutionBudget>) {
        *self.current.lock().expect("flush budget slot") = budget;
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

    fn snapshot(
        scheduler_jobs: usize,
        decode_jobs: usize,
        sink_sessions: usize,
    ) -> FlushBudgetSnapshot {
        FlushBudgetSnapshot {
            scheduler_jobs,
            decode_jobs,
            sink_sessions,
            generation: 42,
            ..FlushBudgetSnapshot::default()
        }
    }

    #[test]
    fn native_snapshot_keeps_sink_sessions_and_decode_jobs() {
        use crate::plugins::cdc::sink_capabilities;
        let budget =
            FlushExecutionBudget::from_snapshot(snapshot(7, 5, 3), &sink_capabilities::POSTGRES);
        assert_eq!(budget.scheduler_limit(), 7);
        assert_eq!(budget.decode_limit(), 5);
        assert_eq!(budget.sink_limit(), 3);
        assert_eq!(budget.generation(), 42);
    }

    #[test]
    fn supported_formats_choose_compact_sink_limit() {
        use crate::plugins::cdc::sink_capabilities;
        let snapshot = snapshot(6, 4, 2);
        for capability in [
            &sink_capabilities::S3,
            &sink_capabilities::GCS,
            &sink_capabilities::AZURE_BLOB,
            &sink_capabilities::FILE,
            &sink_capabilities::SFTP,
            &sink_capabilities::ATHENA,
            &sink_capabilities::AMQP,
            &sink_capabilities::STDOUT,
        ] {
            let budget = FlushExecutionBudget::from_snapshot(snapshot, capability);
            assert_eq!(
                budget.sink_limit(),
                budget.scheduler_limit(),
                "{}",
                capability.name
            );
            assert_eq!(budget.decode_limit(), 4, "{}", capability.name);
            assert_ne!(
                budget.sink_limit(),
                snapshot.sink_sessions,
                "{}",
                capability.name
            );
        }
        for capability in [
            &sink_capabilities::POSTGRES,
            &sink_capabilities::SNOWFLAKE,
            &sink_capabilities::BIGQUERY,
            &sink_capabilities::REDSHIFT,
            &sink_capabilities::DATABRICKS,
            &sink_capabilities::MOTHERDUCK,
            &sink_capabilities::CLICKHOUSE,
            &sink_capabilities::SYNAPSE,
            &sink_capabilities::SKIPPRLAKE,
            &sink_capabilities::ATHENA_ICEBERG,
            &sink_capabilities::DUCKDB,
        ] {
            let budget = FlushExecutionBudget::from_snapshot(snapshot, capability);
            assert_eq!(
                budget.sink_limit(),
                snapshot.sink_sessions,
                "{}",
                capability.name
            );
            assert_eq!(budget.decode_limit(), 4, "{}", capability.name);
            assert_eq!(budget.scheduler_limit(), 6, "{}", capability.name);
        }
    }

    #[tokio::test]
    async fn file_format_sink_semaphore_equals_scheduler_jobs() {
        use crate::plugins::cdc::sink_capabilities;
        let budget =
            FlushExecutionBudget::from_snapshot(snapshot(3, 2, 1), &sink_capabilities::FILE);
        assert_eq!(budget.sink_limit(), 3);
        assert_eq!(budget.decode_limit(), 2);
        let _a = budget.acquire_sink().await.unwrap();
        let _b = budget.acquire_sink().await.unwrap();
        let _c = budget.acquire_sink().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), budget.acquire_sink())
                .await
                .is_err()
        );
    }
}
