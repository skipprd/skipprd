//! Per-pipeline schema gate. An ALTER holds it exclusively; ingest,
//! compaction, and query are admitted only while no ALTER holds it.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use skippr_lease::PipelineKey;

/// Upper bound an ALTER waits for in-flight work to drain.
const DRAIN_CEILING: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GateError {
    #[error("schema change in progress for this pipeline; retry after it commits")]
    SchemaChangeInProgress,
    #[error("in-flight ingest, compaction, or query did not drain within {0:?}")]
    DrainTimeout(Duration),
}

#[derive(Default)]
struct State {
    altering: bool,
    admitted: usize,
}

#[derive(Default)]
struct Gate {
    state: Mutex<State>,
    changed: Condvar,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn gate(key: &PipelineKey) -> Arc<Gate> {
    static GATES: OnceLock<Mutex<HashMap<PipelineKey, Arc<Gate>>>> = OnceLock::new();
    GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key.clone())
        .or_default()
        .clone()
}

/// Ingest, compaction, or query in flight. Dropping it releases the slot.
#[must_use]
pub struct Admission(Arc<Gate>);

impl Drop for Admission {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.admitted -= 1;
        self.0.changed.notify_all();
    }
}

/// Ingest workers (dedicated pool, not tokio) wait out an ALTER.
pub fn admit(key: &PipelineKey) -> Admission {
    let gate = gate(key);
    {
        let mut state = gate
            .changed
            .wait_while(gate.lock(), |state| state.altering)
            .unwrap_or_else(PoisonError::into_inner);
        state.admitted += 1;
    }
    Admission(gate)
}

/// Compaction and query do not wait; they defer or fail.
pub fn try_admit(key: &PipelineKey) -> Result<Admission, GateError> {
    let gate = gate(key);
    {
        let mut state = gate.lock();
        if state.altering {
            return Err(GateError::SchemaChangeInProgress);
        }
        state.admitted += 1;
    }
    Ok(Admission(gate))
}

/// Held by one ALTER. Dropping it re-admits ingest, compaction, and query.
#[must_use]
pub struct Exclusive(Arc<Gate>);

impl Drop for Exclusive {
    fn drop(&mut self) {
        self.0.lock().altering = false;
        self.0.changed.notify_all();
    }
}

/// Stop new admissions, then wait for in-flight work to drain.
pub async fn exclusive(key: &PipelineKey) -> Result<Exclusive, GateError> {
    let gate = gate(key);
    tokio::task::spawn_blocking(move || {
        let mut state = gate
            .changed
            .wait_while(gate.lock(), |state| state.altering)
            .unwrap_or_else(PoisonError::into_inner);
        state.altering = true;
        let exclusive = Exclusive(gate.clone());
        let (state, timeout) = gate
            .changed
            .wait_timeout_while(state, DRAIN_CEILING, |state| state.admitted > 0)
            .unwrap_or_else(PoisonError::into_inner);
        drop(state);
        if timeout.timed_out() {
            return Err(GateError::DrainTimeout(DRAIN_CEILING));
        }
        Ok(exclusive)
    })
    .await
    .unwrap_or_else(|err| std::panic::resume_unwind(err.into_panic()))
}

#[cfg(test)]
pub(crate) fn is_exclusive(key: &PipelineKey) -> bool {
    gate(key).lock().altering
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pipeline: &str) -> PipelineKey {
        PipelineKey::new("t", "w", pipeline).unwrap()
    }

    #[tokio::test]
    async fn exclusive_blocks_this_pipeline_only() {
        let held = exclusive(&key("gate_a")).await.unwrap();
        assert_eq!(
            try_admit(&key("gate_a")).err(),
            Some(GateError::SchemaChangeInProgress)
        );
        let other = try_admit(&key("gate_b")).unwrap();
        drop(held);
        let again = try_admit(&key("gate_a")).unwrap();
        drop((other, again));
    }

    #[tokio::test]
    async fn exclusive_waits_for_in_flight_admissions() {
        let k = key("gate_drain");
        let in_flight = admit(&k);
        let pending = tokio::spawn({
            let k = k.clone();
            async move { exclusive(&k).await.map(|_| ()) }
        });
        while !is_exclusive(&k) {
            tokio::task::yield_now().await;
        }
        assert!(!pending.is_finished());
        assert_eq!(try_admit(&k).err(), Some(GateError::SchemaChangeInProgress));
        drop(in_flight);
        pending.await.unwrap().unwrap();
        assert!(!is_exclusive(&k));
    }

    #[test]
    fn admit_waits_until_alter_releases() {
        let k = key("gate_ingest");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let held = rt.block_on(exclusive(&k)).unwrap();
        let waiter = std::thread::spawn({
            let k = k.clone();
            move || drop(admit(&k))
        });
        assert!(!waiter.is_finished());
        drop(held);
        waiter.join().unwrap();
    }
}
