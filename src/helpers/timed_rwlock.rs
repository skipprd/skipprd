use std::collections::HashMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Instant, Duration};
use std::sync::atomic::{AtomicU64, Ordering};
use dashmap::DashMap;
use lazy_static::lazy_static;

lazy_static! {
    static ref PROFILE_PERFORMANCE: bool = true;
    static ref WAITING_ON: DashMap<String, Instant> = DashMap::new();
    static ref TOTAL_WAIT_TIMES: DashMap<String, AtomicU64> = DashMap::new();
}

pub struct TimedRwLock<T> {
    name: String,
    lock: RwLock<T>,
}

impl<T> TimedRwLock<T> {
    pub fn new(name: String, t: T) -> TimedRwLock<T> {
        TimedRwLock {
            name,
            lock: RwLock::new(t),
        }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        let start_time = if *PROFILE_PERFORMANCE {
            Some(Instant::now())
        } else {
            None
        };

        let result = self.lock.read().unwrap();

        if let Some(start_time) = start_time {
            let elapsed = start_time.elapsed();
            let nanos = elapsed.as_nanos() as u64;
            TOTAL_WAIT_TIMES
                .entry(self.name.clone())
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(nanos, Ordering::Relaxed);
        }

        result
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        let start_time = if *PROFILE_PERFORMANCE {
            // println!("{} is waiting on a write lock", self.name);
            Some(Instant::now())
        } else {
            None
        };

        let result = self.lock.write().unwrap();

        if let Some(start_time) = start_time {
            let elapsed = start_time.elapsed();
            let nanos = elapsed.as_nanos() as u64;
            TOTAL_WAIT_TIMES
                .entry(self.name.clone())
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(nanos, Ordering::Relaxed);
        }

        result
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }

    pub fn currently_waiting() -> Vec<(String, Duration)> {
        WAITING_ON
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().elapsed()))
            .collect()
    }

    pub fn get_total_wait_times() -> Vec<(String, Duration)> {
        let totals = TOTAL_WAIT_TIMES
            .iter()
            .map(|entry| (entry.key().clone(), Duration::from_nanos(entry.value().load(Ordering::Relaxed))))
            .collect();
        TOTAL_WAIT_TIMES.clear();
        totals
    }
}
