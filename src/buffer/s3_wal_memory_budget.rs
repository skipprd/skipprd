//! Single autotuned memory envelope for S3 WAL ingest admission + S3 WAL body LRU.
//! The two caps always sum to at most **70%** of the OS memory hint (MemAvailable on Linux,
//! total RAM on macOS). No env knobs — callers pass the same hint from here.

/// When `/proc/meminfo` (etc.) is unavailable, match `Ingest`’s historical default hint.
pub const FALLBACK_MEMORY_HINT_BYTES: usize = 2 * 1024 * 1024 * 1024;

const COMBINED_PCT: usize = 70;

const MIN_ADMISSION: usize = 256 * 1024 * 1024;
const MIN_LRU: usize = 32 * 1024 * 1024;
const MAX_ADMISSION: usize = 64 * 1024 * 1024 * 1024;
const MAX_LRU: usize = 16 * 1024 * 1024 * 1024;

#[cfg(target_os = "linux")]
fn read_meminfo_kib(label: &str) -> Option<u64> {
    let data = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in data.lines() {
        let line = line.trim_start();
        if line.starts_with(label) {
            return line.split_whitespace().nth(1)?.parse().ok();
        }
    }
    None
}

#[cfg(target_os = "linux")]
pub fn memory_hint_bytes() -> Option<usize> {
    const KIB: usize = 1024;
    read_meminfo_kib("MemAvailable:")
        .or_else(|| read_meminfo_kib("MemFree:"))
        .or_else(|| read_meminfo_kib("MemTotal:"))
        .map(|kib| (kib as usize).saturating_mul(KIB))
}

#[cfg(target_os = "macos")]
fn darwin_total_ram_bytes() -> Option<usize> {
    use std::mem;
    use std::ptr;
    let mut mib: [i32; 2] = [libc::CTL_HW, libc::HW_MEMSIZE];
    let mut out: u64 = 0;
    let mut sz = mem::size_of_val(&out);
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            &mut out as *mut u64 as *mut libc::c_void,
            &mut sz,
            ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(out as usize)
}

#[cfg(target_os = "macos")]
pub fn memory_hint_bytes() -> Option<usize> {
    darwin_total_ram_bytes()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn memory_hint_bytes() -> Option<usize> {
    None
}

/// Hard ceiling for S3 WAL-related byte budgets: `hint * 70 / 100`.
#[inline]
pub fn combined_pool_bytes(hint_bytes: usize) -> usize {
    hint_bytes.saturating_mul(COMBINED_PCT).saturating_div(100)
}

/// `(admission_cap_bytes, lru_max_bytes)` with `admission + lru <= combined_pool_bytes(hint)`.
pub fn split_s3_wal_memory_budget(hint_bytes: usize) -> (usize, usize) {
    let pool = combined_pool_bytes(hint_bytes);
    if pool == 0 {
        return (0, 0);
    }

    // Tiny machines: cannot satisfy both per-component minimums inside the pool.
    if pool < MIN_ADMISSION.saturating_add(MIN_LRU) {
        let lru = MIN_LRU.min(pool);
        let admission = pool.saturating_sub(lru);
        return (admission, lru);
    }

    // Target ~5/7 of pool for ingest admission (~50% of MemAvailable), ~2/7 for body LRU (~20%).
    let mut admission = (pool.saturating_mul(5) / 7).min(MAX_ADMISSION);
    let mut lru = pool.saturating_sub(admission).min(MAX_LRU);
    if admission.saturating_add(lru) > pool {
        lru = pool.saturating_sub(admission);
    }

    // Meet MIN_LRU by shifting budget from admission (admission is already at least pool - MAX_LRU).
    if lru < MIN_LRU {
        let need = MIN_LRU.saturating_sub(lru);
        let shift = need.min(admission);
        admission -= shift;
        lru += shift;
    }

    // Meet MIN_ADMISSION by shifting from LRU.
    if admission < MIN_ADMISSION {
        let need = MIN_ADMISSION.saturating_sub(admission);
        let shift = need.min(lru);
        lru -= shift;
        admission += shift;
    }

    // Re-clamp absolute maxima without exceeding `pool`.
    admission = admission.min(MAX_ADMISSION).min(pool);
    lru = lru.min(MAX_LRU).min(pool.saturating_sub(admission));

    // If we still violate mins (only when pool is awkward vs caps), prefer admission headroom.
    if admission < MIN_ADMISSION && pool >= MIN_ADMISSION {
        admission = MIN_ADMISSION.min(pool).min(MAX_ADMISSION);
        lru = pool.saturating_sub(admission).min(MAX_LRU);
    }
    if lru < MIN_LRU && pool.saturating_sub(admission) >= MIN_LRU {
        lru = MIN_LRU.min(MAX_LRU).min(pool.saturating_sub(admission));
        admission = pool.saturating_sub(lru).min(MAX_ADMISSION);
    }

    debug_assert!(
        admission.saturating_add(lru) <= pool,
        "S3 WAL memory split exceeded pool: admission={admission} lru={lru} pool={pool}"
    );

    (admission, lru)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_never_exceeds_70pct_hint() {
        for hint in [
            512_usize * 1024 * 1024,
            2 * 1024 * 1024 * 1024,
            8 * 1024 * 1024 * 1024,
            128 * 1024 * 1024 * 1024,
        ] {
            let pool = combined_pool_bytes(hint);
            let (a, l) = split_s3_wal_memory_budget(hint);
            assert!(
                a.saturating_add(l) <= pool,
                "hint={hint} pool={pool} admission={a} lru={l}"
            );
        }
    }

    #[test]
    fn split_matches_target_ratio_when_unclamped() {
        let hint = 10 * 1024 * 1024 * 1024;
        let pool = combined_pool_bytes(hint);
        let (a, l) = split_s3_wal_memory_budget(hint);
        assert_eq!(pool, hint * 70 / 100);
        // 10 GiB hint → 7 GiB pool; 5/7 ≈ 5 GiB admission, 2 GiB LRU — both under per-pool max.
        assert_eq!(a, pool * 5 / 7);
        assert_eq!(l, pool - a);
    }
}
