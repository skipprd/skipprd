// use parking_lot::{RwLock, Mutex, RwLockReadGuard, RwLockWriteGuard};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, Mutex};
use std::time::{Instant, Duration};
use std::collections::HashMap;
use lazy_static::lazy_static;

lazy_static! {
    static ref PROFILE_PERFORMANCE: bool = true;
    static ref WAITING_ON: Mutex<HashMap<String, Instant>> = Mutex::new(HashMap::new());
    static ref TOTAL_WAIT_TIMES: Mutex<HashMap<String, Duration>> = Mutex::new(HashMap::new());
}

pub struct TimedRwLock<T> {
    name: String,
    lock: RwLock<T>,
    wait_time: RwLock<Duration>,
}

impl<T> TimedRwLock<T> {
    pub fn new(name: String, t: T) -> TimedRwLock<T> {
        TimedRwLock {
            name,
            lock: RwLock::new(t),
            wait_time: RwLock::new(Duration::new(0, 0)),
        }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        if *PROFILE_PERFORMANCE {
            let mut waiting_on = WAITING_ON.lock().unwrap();
            waiting_on.insert(self.name.clone(), Instant::now());
        }

        let result = self.lock.read().unwrap();

        if *PROFILE_PERFORMANCE {
            let mut waiting_on = WAITING_ON.lock().unwrap();
            if let Some(start_time) = waiting_on.remove(&self.name) {
                let elapsed = start_time.elapsed();
                let mut total_wait_time = TOTAL_WAIT_TIMES.lock().unwrap();
                *total_wait_time.entry(self.name.clone()).or_insert(Duration::new(0, 0)) += elapsed;
            }
        }

        result
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        if *PROFILE_PERFORMANCE {
            let mut waiting_on = WAITING_ON.lock().unwrap();
            waiting_on.insert(self.name.clone(), Instant::now());
        }

        let result = self.lock.write().unwrap();

        if *PROFILE_PERFORMANCE {
            let mut waiting_on = WAITING_ON.lock().unwrap();
            if let Some(start_time) = waiting_on.remove(&self.name) {
                let elapsed = start_time.elapsed();
                let mut total_wait_time = TOTAL_WAIT_TIMES.lock().unwrap();
                *total_wait_time.entry(self.name.clone()).or_insert(Duration::new(0, 0)) += elapsed;
            }
        }

        result
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }

    pub fn currently_waiting() -> HashMap<String, Duration> {
        let waiting_on = WAITING_ON.lock().unwrap();
        waiting_on.iter().map(|(name, start_time)| (name.clone(), start_time.elapsed())).collect()
    }

    pub fn get_total_wait_times() -> HashMap<String, Duration> {
        let mut totals = TOTAL_WAIT_TIMES.lock().unwrap();
        let cloned_totals = totals.clone();

        // clear the totals
        totals.clear();

        cloned_totals
    }
}
