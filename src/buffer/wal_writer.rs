use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arrow_schema::ArrowError;
use once_cell::sync::{Lazy, OnceCell};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::buffer::ingest_buffer::{Buffers, IngestBufferBatch};
use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::helpers::Helpers;

pub type WalPersistResult = Result<(), String>;

pub struct WalCommitUnit {
    pub submit_id: u64,
    pub batches: Vec<IngestBufferBatch>,
    pub offsets_db: Arc<Offsets>,
    pub raw_bytes: usize,
    pub arrow_bytes: usize,
    pub done: oneshot::Sender<WalPersistResult>,
}

struct RequestAckState {
    remaining: usize,
    done: oneshot::Sender<WalPersistResult>,
}

enum WalWriterCommand {
    Submit(WalCommitUnit),
    Flush {
        offsets_db: Arc<Offsets>,
        done: oneshot::Sender<Result<(), ArrowError>>,
    },
}

static WAL_WRITER_TX: OnceCell<mpsc::Sender<WalWriterCommand>> = OnceCell::new();
static WAL_WRITER_CAPACITY: AtomicUsize = AtomicUsize::new(0);
static WAL_WRITER_PENDING_COUNT: AtomicUsize = AtomicUsize::new(0);
static WAL_WRITER_PENDING_BYTES: AtomicUsize = AtomicUsize::new(0);
static WAL_WRITER_COALESCE_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static WAL_WRITER_COALESCE_BATCHES_TOTAL: AtomicU64 = AtomicU64::new(0);
static WAL_WRITER_PERSIST_LATENCY_NS_TOTAL: AtomicU64 = AtomicU64::new(0);
static WAL_WRITER_PERSIST_COUNT: AtomicU64 = AtomicU64::new(0);
static WAL_WRITER_ACK_LATENCY_NS_TOTAL: AtomicU64 = AtomicU64::new(0);
static WAL_WRITER_ACK_COUNT: AtomicU64 = AtomicU64::new(0);
static REQUEST_ACKS: Lazy<Mutex<HashMap<u64, RequestAckState>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const ACK_DELAY_MIN_MS: u64 = 100;
const ACK_DELAY_COLD_START_MS: u64 = 250;
const ACK_DELAY_MAX_MS: u64 = 500;
const ACK_DELAY_PERSIST_LATENCY_MULTIPLIER: f64 = 4.0;

fn writer_capacity(ingest_threads: usize) -> usize {
    ingest_threads.clamp(1, 32)
}

fn clamp_ack_delay_ms(ms: u64) -> u64 {
    ms.clamp(ACK_DELAY_MIN_MS, ACK_DELAY_MAX_MS)
}

fn adaptive_ack_delay_ms(
    live_segment_bytes: u64,
    wal_target_bytes: u64,
    submit_rate_bytes_per_sec: f64,
    persist_avg_ms: f64,
) -> u64 {
    let latency_budget_ms = if persist_avg_ms > 0.0 {
        clamp_ack_delay_ms((persist_avg_ms * ACK_DELAY_PERSIST_LATENCY_MULTIPLIER) as u64)
    } else {
        ACK_DELAY_COLD_START_MS
    };

    if live_segment_bytes >= wal_target_bytes {
        return ACK_DELAY_MIN_MS;
    }

    if submit_rate_bytes_per_sec <= 0.0 {
        return latency_budget_ms;
    }

    let remaining_bytes = wal_target_bytes.saturating_sub(live_segment_bytes);
    if remaining_bytes == 0 {
        return ACK_DELAY_MIN_MS;
    }

    let estimated_fill_ms = ((remaining_bytes as f64 / submit_rate_bytes_per_sec) * 1000.0) as u64;
    clamp_ack_delay_ms(estimated_fill_ms.min(latency_budget_ms))
}

fn next_ack_deadline(
    pending_acks: &[(oneshot::Sender<WalPersistResult>, Instant, u64, u64)],
    live_segment_bytes: u64,
    submit_rate_bytes_per_sec: f64,
    max_delay: Duration,
) -> Option<Instant> {
    let oldest = pending_acks
        .iter()
        .map(|(_, queued_at, _, _)| *queued_at)
        .min()?;
    let adaptive_delay = Duration::from_millis(adaptive_ack_delay_ms(
        live_segment_bytes,
        Config::get_wal_bytes_per_file(),
        submit_rate_bytes_per_sec,
        persist_avg_ms(),
    ));
    let ack_deadline = oldest + adaptive_delay;
    let max_deadline = oldest + max_delay;
    Some(ack_deadline.min(max_deadline))
}

pub fn start(ingest_threads: usize) {
    if WAL_WRITER_TX.get().is_some() {
        return;
    }
    let capacity = writer_capacity(ingest_threads);
    let (tx, rx) = mpsc::channel::<WalWriterCommand>(capacity);
    if WAL_WRITER_TX.set(tx).is_ok() {
        WAL_WRITER_CAPACITY.store(capacity, Ordering::Relaxed);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(run_writer(rx));
        } else {
            crate::ingest_work::INGEST_RT.spawn(run_writer(rx));
        }
        info!("WAL writer started with queue_capacity={}", capacity);
    }
}

pub fn submit_blocking(unit: WalCommitUnit) -> Result<(), String> {
    let tx = WAL_WRITER_TX
        .get()
        .ok_or_else(|| "WAL writer has not been started".to_string())?;
    tx.blocking_send(WalWriterCommand::Submit(unit))
        .map_err(|_| "WAL writer is stopped".to_string())
}

pub async fn submit(unit: WalCommitUnit) -> Result<(), String> {
    let tx = WAL_WRITER_TX
        .get()
        .ok_or_else(|| "WAL writer has not been started".to_string())?;
    tx.send(WalWriterCommand::Submit(unit))
        .await
        .map_err(|_| "WAL writer is stopped".to_string())
}

pub fn register_request_ack(
    request_id: u64,
    expected_units: usize,
) -> oneshot::Receiver<WalPersistResult> {
    let (tx, rx) = oneshot::channel();
    if request_id == 0 || expected_units == 0 {
        let _ = tx.send(Ok(()));
        return rx;
    }
    REQUEST_ACKS.lock().unwrap().insert(
        request_id,
        RequestAckState {
            remaining: expected_units,
            done: tx,
        },
    );
    rx
}

fn complete_request_unit(request_id: u64, result: &WalPersistResult) {
    if request_id == 0 {
        return;
    }
    let mut guard = REQUEST_ACKS.lock().unwrap();
    let Some(state) = guard.get_mut(&request_id) else {
        return;
    };
    if result.is_err() {
        let state = guard.remove(&request_id).unwrap();
        let _ = state.done.send(result.clone());
        return;
    }
    state.remaining = state.remaining.saturating_sub(1);
    if state.remaining == 0 {
        let state = guard.remove(&request_id).unwrap();
        let _ = state.done.send(Ok(()));
    }
}

pub fn complete_request_without_wal(request_id: u64) {
    complete_request_unit(request_id, &Ok(()));
}

pub fn fail_request(request_id: u64, message: impl Into<String>) {
    complete_request_unit(request_id, &Err(message.into()));
}

pub async fn flush_and_drain(offsets_db: Arc<Offsets>) -> Result<(), ArrowError> {
    let Some(tx) = WAL_WRITER_TX.get() else {
        return crate::buffer::ingest_buffer::flush_all_segments_direct(offsets_db).await;
    };
    let (done_tx, done_rx) = oneshot::channel();
    tx.send(WalWriterCommand::Flush {
        offsets_db,
        done: done_tx,
    })
    .await
    .map_err(|_| {
        ArrowError::ExternalError(Box::new(std::io::Error::other("WAL writer is stopped")))
    })?;
    done_rx.await.map_err(|_| {
        ArrowError::ExternalError(Box::new(std::io::Error::other(
            "WAL writer flush response dropped",
        )))
    })?
}

pub fn pending_count() -> usize {
    WAL_WRITER_PENDING_COUNT.load(Ordering::Relaxed)
}

pub fn pending_bytes() -> usize {
    WAL_WRITER_PENDING_BYTES.load(Ordering::Relaxed)
}

pub fn queue_capacity() -> usize {
    WAL_WRITER_CAPACITY.load(Ordering::Relaxed)
}

pub fn coalesce_avg_bytes() -> u64 {
    let count = WAL_WRITER_COALESCE_BATCHES_TOTAL.load(Ordering::Relaxed);
    if count == 0 {
        0
    } else {
        WAL_WRITER_COALESCE_BYTES_TOTAL.load(Ordering::Relaxed) / count
    }
}

pub fn persist_avg_ms() -> f64 {
    let count = WAL_WRITER_PERSIST_COUNT.load(Ordering::Relaxed);
    if count == 0 {
        0.0
    } else {
        (WAL_WRITER_PERSIST_LATENCY_NS_TOTAL.load(Ordering::Relaxed) / count) as f64 / 1_000_000.0
    }
}

pub fn ack_avg_ms() -> f64 {
    let count = WAL_WRITER_ACK_COUNT.load(Ordering::Relaxed);
    if count == 0 {
        0.0
    } else {
        (WAL_WRITER_ACK_LATENCY_NS_TOTAL.load(Ordering::Relaxed) / count) as f64 / 1_000_000.0
    }
}

pub fn has_pending_work() -> bool {
    pending_count() > 0 || Buffers::live_segment_bytes() > 0
}

async fn run_writer(mut rx: mpsc::Receiver<WalWriterCommand>) {
    let mut pending_acks: Vec<(oneshot::Sender<WalPersistResult>, Instant, u64, u64)> = Vec::new();
    let mut pending_offsets: Option<Arc<Offsets>> = None;
    let mut last_flush = Instant::now();
    let mut last_submit_at: Option<Instant> = None;
    let mut submit_rate_bytes_per_sec = 0.0f64;

    loop {
        let max_delay = Duration::from_secs(Config::get_wal_max_delay_seconds().max(1));
        let next = if pending_acks.is_empty() {
            rx.recv().await
        } else {
            let deadline = next_ack_deadline(
                &pending_acks,
                Buffers::live_segment_bytes(),
                submit_rate_bytes_per_sec,
                max_delay,
            )
            .unwrap_or_else(|| last_flush + max_delay);
            tokio::select! {
                cmd = rx.recv() => cmd,
                _ = tokio::time::sleep_until(deadline.into()) => None,
            }
        };

        match next {
            Some(WalWriterCommand::Submit(unit)) => {
                pending_offsets = Some(unit.offsets_db.clone());
                let ack_started = Instant::now();
                let arrow_bytes = unit.arrow_bytes as u64;
                WAL_WRITER_PENDING_COUNT.fetch_add(1, Ordering::Relaxed);
                WAL_WRITER_PENDING_BYTES.fetch_add(unit.arrow_bytes, Ordering::Relaxed);
                match Buffers::append_batches_to_live(unit.batches) {
                    Ok(added_bytes) => {
                        WAL_WRITER_COALESCE_BYTES_TOTAL.fetch_add(added_bytes, Ordering::Relaxed);
                        WAL_WRITER_COALESCE_BATCHES_TOTAL.fetch_add(1, Ordering::Relaxed);
                        let now = Instant::now();
                        if let Some(previous) = last_submit_at {
                            let elapsed = now.saturating_duration_since(previous);
                            if elapsed >= Duration::from_millis(1) && added_bytes > 0 {
                                let instant_rate =
                                    added_bytes as f64 / elapsed.as_secs_f64().max(0.001);
                                submit_rate_bytes_per_sec = if submit_rate_bytes_per_sec > 0.0 {
                                    (submit_rate_bytes_per_sec * 0.8) + (instant_rate * 0.2)
                                } else {
                                    instant_rate
                                };
                            }
                        }
                        last_submit_at = Some(now);
                        pending_acks.push((unit.done, ack_started, arrow_bytes, unit.submit_id));
                        if Config::log_wal_enabled() {
                            debug!(
                                "WAL writer queued submit_id={} raw_bytes={} arrow_bytes={} live_bytes={} pending_count={} submit_rate_bps={:.0}",
                                unit.submit_id,
                                unit.raw_bytes,
                                unit.arrow_bytes,
                                Buffers::live_segment_bytes(),
                                pending_acks.len(),
                                submit_rate_bytes_per_sec
                            );
                        }
                    }
                    Err(err) => {
                        WAL_WRITER_PENDING_COUNT.fetch_sub(1, Ordering::Relaxed);
                        WAL_WRITER_PENDING_BYTES.fetch_sub(unit.arrow_bytes, Ordering::Relaxed);
                        complete_request_unit(unit.submit_id, &Err(err.clone()));
                        let _ = unit.done.send(Err(err));
                    }
                }
                if Buffers::live_segment_bytes() >= Config::get_wal_bytes_per_file() {
                    if let Some(offsets) = pending_offsets.clone() {
                        flush_live_and_ack(&offsets, &mut pending_acks, &mut last_flush).await;
                    }
                } else if let Some(deadline) = next_ack_deadline(
                    &pending_acks,
                    Buffers::live_segment_bytes(),
                    submit_rate_bytes_per_sec,
                    max_delay,
                ) {
                    if Instant::now() >= deadline {
                        if let Some(offsets) = pending_offsets.clone() {
                            flush_live_and_ack(&offsets, &mut pending_acks, &mut last_flush).await;
                        }
                    }
                }
            }
            Some(WalWriterCommand::Flush { offsets_db, done }) => {
                let result =
                    flush_everything(&offsets_db, &mut pending_acks, &mut last_flush).await;
                let _ = done.send(result);
                pending_offsets = None;
            }
            None => {
                if pending_acks.is_empty() {
                    break;
                }
                if let Some(offsets) = pending_offsets.clone() {
                    flush_live_and_ack(&offsets, &mut pending_acks, &mut last_flush).await;
                }
            }
        }
    }
    error!("WAL writer stopped");
}

async fn flush_everything(
    offsets_db: &Arc<Offsets>,
    pending_acks: &mut Vec<(oneshot::Sender<WalPersistResult>, Instant, u64, u64)>,
    last_flush: &mut Instant,
) -> Result<(), ArrowError> {
    flush_live_and_ack(offsets_db, pending_acks, last_flush).await;
    Buffers::flush_snapshot_queue_for_writer(offsets_db.as_ref()).await?;
    Ok(())
}

async fn flush_live_and_ack(
    offsets_db: &Arc<Offsets>,
    pending_acks: &mut Vec<(oneshot::Sender<WalPersistResult>, Instant, u64, u64)>,
    last_flush: &mut Instant,
) {
    if pending_acks.is_empty() && Buffers::live_segment_bytes() == 0 {
        return;
    }
    let started = Instant::now();
    let result = Buffers::flush_live_segment_for_writer(offsets_db.as_ref()).await;
    let elapsed_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
    WAL_WRITER_PERSIST_LATENCY_NS_TOTAL.fetch_add(elapsed_ns, Ordering::Relaxed);
    WAL_WRITER_PERSIST_COUNT.fetch_add(1, Ordering::Relaxed);

    let ack_result = result.as_ref().map(|_| ()).map_err(|err| err.to_string());
    let acked = std::mem::take(pending_acks);
    for (done, queued_at, bytes, submit_id) in acked {
        WAL_WRITER_PENDING_COUNT.fetch_sub(1, Ordering::Relaxed);
        WAL_WRITER_PENDING_BYTES.fetch_sub(bytes as usize, Ordering::Relaxed);
        WAL_WRITER_ACK_LATENCY_NS_TOTAL.fetch_add(
            queued_at.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        WAL_WRITER_ACK_COUNT.fetch_add(1, Ordering::Relaxed);
        complete_request_unit(submit_id, &ack_result);
        let _ = done.send(ack_result.clone());
    }
    *last_flush = Instant::now();

    if let Ok((rows, bytes)) = result {
        if Config::log_wal_enabled() {
            info!(
                "WAL writer flushed live segment rows={} bytes={}",
                rows,
                Helpers::human_readable_size(bytes)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_capacity_is_derived_from_ingest_threads() {
        assert_eq!(writer_capacity(0), 1);
        assert_eq!(writer_capacity(1), 1);
        assert_eq!(writer_capacity(8), 8);
        assert_eq!(writer_capacity(64), 32);
    }

    #[test]
    fn adaptive_ack_delay_stays_within_latency_budget() {
        assert_eq!(clamp_ack_delay_ms(10), ACK_DELAY_MIN_MS);
        assert_eq!(clamp_ack_delay_ms(750), ACK_DELAY_MAX_MS);

        assert_eq!(
            adaptive_ack_delay_ms(0, 20 * 1024 * 1024, 0.0, 0.0),
            ACK_DELAY_COLD_START_MS
        );
        assert_eq!(
            adaptive_ack_delay_ms(19 * 1024 * 1024, 20 * 1024 * 1024, 100_000_000.0, 25.0),
            ACK_DELAY_MIN_MS
        );
        assert_eq!(
            adaptive_ack_delay_ms(0, 20 * 1024 * 1024, 1_000_000.0, 200.0),
            ACK_DELAY_MAX_MS
        );
    }

    #[tokio::test]
    async fn request_ack_waits_for_all_registered_units() {
        let request_id = 9_000_001;
        let mut rx = register_request_ack(request_id, 2);
        complete_request_unit(request_id, &Ok(()));
        assert!(rx.try_recv().is_err());
        complete_request_unit(request_id, &Ok(()));
        assert_eq!(rx.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn request_ack_completes_with_first_error() {
        let request_id = 9_000_002;
        let rx = register_request_ack(request_id, 2);
        complete_request_unit(request_id, &Err("persist failed".to_string()));
        assert_eq!(rx.await.unwrap(), Err("persist failed".to_string()));
    }

    #[tokio::test]
    async fn request_ack_can_be_failed_explicitly() {
        let request_id = 9_000_003;
        let rx = register_request_ack(request_id, 1);
        fail_request(request_id, "capacity exhausted");
        assert_eq!(rx.await.unwrap(), Err("capacity exhausted".to_string()));
    }

    #[tokio::test]
    async fn fail_request_makes_late_unit_completions_harmless() {
        let request_id = 9_000_005;
        let rx = register_request_ack(request_id, 3);
        fail_request(request_id, "yielded before remaining units scheduled");
        // Sibling units that already ran, or complete after the shared fail, must not panic
        // or resurrect the request.
        complete_request_without_wal(request_id);
        complete_request_unit(request_id, &Ok(()));
        fail_request(request_id, "duplicate fail");
        assert_eq!(
            rx.await.unwrap(),
            Err("yielded before remaining units scheduled".to_string())
        );
    }

    #[tokio::test]
    async fn empty_request_ack_completes_immediately() {
        let rx = register_request_ack(9_000_004, 0);
        assert_eq!(rx.await.unwrap(), Ok(()));
    }
}
