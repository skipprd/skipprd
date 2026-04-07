use crate::helpers::configuration::Config;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tracing::{debug, info};

fn update_retry_ema_x100() -> u64 {
    static LAST_WAL_RETRIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let total = crate::metrics::counters::S3_WAL_RETRIES_TOTAL.load(Ordering::Relaxed);
    let last = LAST_WAL_RETRIES.swap(total, Ordering::SeqCst);
    let delta = total.saturating_sub(last); // retries during this tick
                                            // EMA_x100 = 0.8 * prev + 0.2 * (delta * 100)
    let prev = crate::metrics::counters::S3_WAL_RETRY_EMA_X100.load(Ordering::Relaxed);
    let ema = ((prev.saturating_mul(80))
        .saturating_add(delta.saturating_mul(100).saturating_mul(20)))
        / 100;
    crate::metrics::counters::set_s3_wal_retry_ema_x100(ema);
    ema
}

/// Apply one-time environment overrides and CI caps for upload, WAL compaction, and S3 download.
pub fn apply_env_caps() {
    // Upload concurrency override (no CI-specific caps)
    if let Ok(v) = Config::getenv("UPLOAD_CONCURRENCY", "").parse::<usize>() {
        if v > 0 {
            crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET
                .store(v, std::sync::atomic::Ordering::Relaxed);
            if Config::log_wal_enabled() {
                info!("tune: upload_concurrency set by env={}", v);
            }
        }
    }
    // WAL compaction concurrency override
    if let Ok(v) = Config::getenv("WAL_COMPACTION_CONCURRENCY", "").parse::<usize>() {
        if v > 0 {
            crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
                .store(v, std::sync::atomic::Ordering::Relaxed);
            if Config::log_wal_enabled() {
                info!("tune: wal_compaction set by env={}", v);
            }
        }
    }
    // S3 download concurrency override (global target). Per-plugin may still clamp via memory semaphore
    if let Ok(v) = Config::getenv("S3_DOWNLOAD_CONCURRENCY", "").parse::<usize>() {
        if v > 0 {
            let clamped = v.clamp(8, 512);
            crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET
                .store(clamped, std::sync::atomic::Ordering::Relaxed);
            if Config::log_wal_enabled() {
                info!("tune: s3_download set by env={} (clamped)", clamped);
            }
        }
    }
}

/// Periodic tuning tick: adjusts upload, WAL compaction, and S3 download targets.
/// Includes error-aware throttling using EMA of WAL retries.
pub fn tick(active: usize, capacity: usize, queued: usize, pressure: f64) {
    // Use env-defined maxima or reasonable defaults; no CI-specific caps
    let max_upload = Config::getenv("UPLOAD_CONCURRENCY_MAX", "")
        .parse::<usize>()
        .ok()
        .filter(|v| *v > 0)
        .unwrap_or(32);
    let max_wal = Config::getenv("WAL_COMPACTION_CONCURRENCY_MAX", "")
        .parse::<usize>()
        .ok()
        .filter(|v| *v > 0)
        .unwrap_or(16);
    let max_dl = Config::getenv("S3_DOWNLOAD_CONCURRENCY_MAX", "")
        .parse::<usize>()
        .ok()
        .filter(|v| *v > 0)
        .unwrap_or(256);

    // Upload tuning: grow when high pressure and full CPU; shrink when low pressure
    let upload_cur = crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(Ordering::Relaxed);
    let upload_next = if active >= capacity && pressure > 0.8 {
        upload_cur.saturating_add(1).min(max_upload)
    } else if pressure < 0.4 {
        upload_cur.saturating_sub(1).max(4)
    } else {
        upload_cur
    };
    if upload_next != upload_cur {
        crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.store(upload_next, Ordering::Relaxed);
        if Config::log_wal_enabled() {
            debug!(
                "tune: upload_concurrency {} -> {} (active={}/{} queue={} pressure={:.2})",
                upload_cur, upload_next, active, capacity, queued, pressure
            );
        }
    }

    // Error-aware tuning: compute EMA of S3 WAL retry rate and throttle if above threshold
    {
        let ema = update_retry_ema_x100();
        // If EMA > 2.0 retries/tick, throttle; if < 0.5, allow gentle restore
        if ema > 200 {
            let uc = crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(Ordering::Relaxed);
            let wc =
                crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(Ordering::Relaxed);
            let new_uc = uc.saturating_sub(1).max(2);
            let new_wc = wc.saturating_sub(1).max(2);
            if new_uc != uc {
                crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET
                    .store(new_uc, Ordering::Relaxed);
                if Config::log_wal_enabled() {
                    debug!(
                        "tune: upload_concurrency {} -> {} (ema_wal_retry_x100={})",
                        uc, new_uc, ema
                    );
                }
            }
            if new_wc != wc {
                crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
                    .store(new_wc, Ordering::Relaxed);
                if Config::log_wal_enabled() {
                    debug!(
                        "tune: wal_compaction {} -> {} (ema_wal_retry_x100={})",
                        wc, new_wc, ema
                    );
                }
            }
        } else if ema < 50 {
            // Gentle restore bounded by max caps
            let uc = crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(Ordering::Relaxed);
            let wc =
                crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(Ordering::Relaxed);
            let new_uc = uc.saturating_add(1).min(max_upload);
            let new_wc = wc.saturating_add(1).min(max_wal);
            if new_uc != uc {
                crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET
                    .store(new_uc, Ordering::Relaxed);
                if Config::log_wal_enabled() {
                    debug!(
                        "tune: upload_concurrency {} -> {} (ema_wal_retry_x100={})",
                        uc, new_uc, ema
                    );
                }
            }
            if new_wc != wc {
                crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
                    .store(new_wc, Ordering::Relaxed);
                if Config::log_wal_enabled() {
                    debug!(
                        "tune: wal_compaction {} -> {} (ema_wal_retry_x100={})",
                        wc, new_wc, ema
                    );
                }
            }
        }
    }

    // WAL compaction tuning (pressure/CPU-based)
    let wal_cur =
        crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(Ordering::Relaxed);
    let wal_next = if active >= capacity && pressure > 0.8 {
        wal_cur.saturating_add(1).min(max_wal)
    } else if pressure < 0.4 {
        wal_cur.saturating_sub(1).max(2)
    } else {
        wal_cur
    };
    if wal_next != wal_cur {
        crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
            .store(wal_next, Ordering::Relaxed);
        if Config::log_wal_enabled() {
            debug!(
                "tune: wal_compaction {} -> {} (active={}/{} queue={} pressure={:.2})",
                wal_cur, wal_next, active, capacity, queued, pressure
            );
        }
    }

    // S3 download tuning (upper bound; memory semaphore still applies)
    let dl_cur = crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET.load(Ordering::Relaxed);
    let dl_next = if active < capacity && pressure < 0.2 {
        dl_cur.saturating_add(4).min(max_dl)
    } else if pressure > 0.8 {
        dl_cur.saturating_sub(16).max(64)
    } else {
        dl_cur
    };
    if dl_next != dl_cur {
        crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET.store(dl_next, Ordering::Relaxed);
        if Config::log_wal_enabled() {
            debug!(
                "tune: s3_download {} -> {} (active={}/{} queue={} pressure={:.2})",
                dl_cur, dl_next, active, capacity, queued, pressure
            );
        }
    }
}

/// Drain-time tuning: prefer fast completion once ingest is over.
/// This ramps compaction/upload concurrency toward CPU-shaped maxima and only
/// backs off when S3 retry pressure rises.
pub fn drain_tick(num_cpus: usize, has_backlog: bool) {
    if !has_backlog {
        return;
    }

    let max_upload = Config::getenv("UPLOAD_CONCURRENCY_MAX", "")
        .parse::<usize>()
        .ok()
        .filter(|v| *v > 0)
        .unwrap_or_else(|| (num_cpus.saturating_mul(2)).clamp(16, 128));
    let max_wal = Config::getenv("WAL_COMPACTION_CONCURRENCY_MAX", "")
        .parse::<usize>()
        .ok()
        .filter(|v| *v > 0)
        .unwrap_or_else(|| num_cpus.clamp(4, 64));

    let ema = update_retry_ema_x100();
    let upload_cur = crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(Ordering::Relaxed);
    let wal_cur =
        crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(Ordering::Relaxed);

    let (upload_next, wal_next) = if ema > 200 {
        (
            upload_cur.saturating_sub(1).max(2),
            wal_cur.saturating_sub(1).max(2),
        )
    } else {
        (
            upload_cur.saturating_add(4).min(max_upload),
            wal_cur.saturating_add(2).min(max_wal),
        )
    };

    if upload_next != upload_cur {
        crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.store(upload_next, Ordering::Relaxed);
    }
    if wal_next != wal_cur {
        crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET
            .store(wal_next, Ordering::Relaxed);
    }

    if (upload_next != upload_cur || wal_next != wal_cur) && Config::log_wal_enabled() {
        info!(
            "drain tune: upload_concurrency {} -> {}, wal_compaction {} -> {} (retry_ema_x100={})",
            upload_cur, upload_next, wal_cur, wal_next, ema
        );
    }
}

/// Inputs for chunk size computation
pub struct ChunkSizeInputs {
    pub current_chunk_size: usize,
    pub active_cores: usize,
    pub num_cpus: usize,
    pub queue_length: usize,
    pub max_queue_length: usize,
    pub avail_mem_mib_opt: Option<usize>,
    pub max_chunk_size: usize,
}

/// Compute the next optimal chunk size based on CPU saturation, queue occupancy and memory limits.
pub fn compute_optimal_chunk_size(inputs: &ChunkSizeInputs) -> usize {
    let optimal_chunk_size_min = 4_000_000usize;
    let mut adjustment_factor = 1.0;

    // Queue occupancy guidance
    let occupancy = if inputs.max_queue_length == 0 {
        0.0
    } else {
        (inputs.queue_length as f32) / (inputs.max_queue_length as f32)
    };
    if inputs.active_cores >= inputs.num_cpus && occupancy >= 0.9 {
        adjustment_factor = 1.1;
    } else if inputs.active_cores < inputs.num_cpus && occupancy < 0.8 {
        adjustment_factor = 0.9;
    }

    let mut next = (inputs.current_chunk_size as f64 * adjustment_factor) as usize;
    if next < optimal_chunk_size_min {
        next = optimal_chunk_size_min;
    }

    // Memory-based clamp
    let denom = (inputs.num_cpus * 2).max(2);
    if let Some(avail_mib) = inputs.avail_mem_mib_opt {
        let budget_bytes = ((avail_mib as usize).saturating_mul(1024 * 1024)) / 5; // 20%
        let dyn_cap = (budget_bytes / denom).max(optimal_chunk_size_min);
        let dyn_cap = std::cmp::min(dyn_cap, inputs.max_chunk_size);
        if next > dyn_cap {
            next = dyn_cap;
        }
    } else {
        if next > inputs.max_chunk_size {
            next = inputs.max_chunk_size;
        }
    }
    next
}

/// Update throughput history, compute trend, compute and return next chunk size.
/// Returns (optimal_chunk_size, throughput_trend_per_sec).
pub fn tune_chunk_size(
    current_chunk_size: usize,
    active_cores: usize,
    queue_length: usize,
    num_cpus: usize,
    max_queue_length: usize,
    current_throughput: u64,
    max_chunk_size: usize,
    avail_mem_mib_opt: Option<usize>,
    throughput_history: &mut VecDeque<(Instant, u64)>,
) -> (usize, f64) {
    let now = Instant::now();
    // Maintain history (last 10 samples)
    throughput_history.push_back((now, current_throughput));
    while throughput_history.len() > 10 {
        throughput_history.pop_front();
    }
    // Compute trend
    let trend = if throughput_history.len() < 2 {
        0.0
    } else {
        let oldest = throughput_history.front().unwrap();
        let newest = throughput_history.back().unwrap();
        let time_diff = newest.0.duration_since(oldest.0).as_secs_f64();
        if time_diff == 0.0 {
            0.0
        } else {
            (newest.1 as f64 - oldest.1 as f64) / time_diff
        }
    };
    // Compute next size
    let inputs = ChunkSizeInputs {
        current_chunk_size,
        active_cores,
        num_cpus,
        queue_length,
        max_queue_length,
        avail_mem_mib_opt,
        max_chunk_size,
    };
    let next = compute_optimal_chunk_size(&inputs);
    (next, trend)
}
