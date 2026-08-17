use crate::clock::{Clock, MonoInstant, Sleeper};
use crate::error::LeaseError;
use crate::guard::{LeaseGuard, LEASE_RENEW_PERIOD, LEASE_TIMEOUT};
use crate::identity::{LeaseObservation, LeaseSession, NodeId, PipelineKey};
use crate::store::PipelineLeaseStore;

pub fn to_session(
    clock: &dyn Clock,
    started: MonoInstant,
    observation: LeaseObservation,
) -> Result<LeaseSession, LeaseError> {
    let now = clock.monotonic_now();
    let deadline = started + LEASE_TIMEOUT;
    if now >= deadline {
        return Err(LeaseError::Lost);
    }
    Ok(LeaseSession::from_observation(observation, deadline))
}

pub async fn acquire_pipeline(
    store: &dyn PipelineLeaseStore,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
    key: &PipelineKey,
    node: &NodeId,
) -> Result<LeaseSession, LeaseError> {
    loop {
        let started = clock.monotonic_now();
        match store.read_consistent(key).await? {
            None => match store.create(key, node).await {
                Ok(observation) => return Ok(to_session(clock, started, observation)?),
                Err(LeaseError::ConditionalRace) => {
                    let current = store
                        .read_consistent(key)
                        .await?
                        .ok_or(LeaseError::ConditionalRace)?;
                    return Err(LeaseError::Held(current.owner));
                }
                Err(err) => return Err(err),
            },
            Some(first) if first.released => {
                let started = clock.monotonic_now();
                return Ok(to_session(
                    clock,
                    started,
                    store.acquire_released(key, &first, node).await?,
                )?);
            }
            Some(first) if first.owner == *node && !first.released => {
                let current =
                    LeaseSession::from_observation(first, clock.monotonic_now() + LEASE_TIMEOUT);
                return Ok(to_session(
                    clock,
                    started,
                    store.renew(key, &current).await?,
                )?);
            }
            Some(first) => {
                // Sole previous-primary TTL wait. Steal is refused if the
                // observation changes. Callers MUST NOT sleep another
                // LEASE_TIMEOUT before GET metadata.json.
                let observed_at = clock.monotonic_now();
                sleeper.sleep_until(observed_at + LEASE_TIMEOUT).await;
                let second = store
                    .read_consistent(key)
                    .await?
                    .ok_or(LeaseError::ConditionalRace)?;
                if second != first {
                    return Err(LeaseError::Held(second.owner));
                }
                let started = clock.monotonic_now();
                return Ok(to_session(
                    clock,
                    started,
                    store.steal_unchanged(key, &second, node).await?,
                )?);
            }
        }
    }
}

pub async fn renew_pipeline(
    store: &dyn PipelineLeaseStore,
    clock: &dyn Clock,
    key: &PipelineKey,
    session: &LeaseSession,
) -> Result<LeaseSession, LeaseError> {
    let started = clock.monotonic_now();
    to_session(clock, started, store.renew(key, session).await?)
}

/// Renew the leased session until the guard is fenced or renew fails.
/// Local deadline is measured from the monotonic instant the renew request starts.
pub async fn renew_until_lost(
    store: &dyn PipelineLeaseStore,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
    key: &PipelineKey,
    guard: &LeaseGuard,
) {
    loop {
        let Some(session) = guard.leased_session() else {
            return;
        };
        match renew_pipeline(store, clock, key, &session).await {
            Ok(next) => {
                guard.install_deadline(next);
            }
            Err(_) => {
                guard.fence();
                return;
            }
        }
        sleeper
            .sleep_until(clock.monotonic_now().saturating_add(LEASE_RENEW_PERIOD))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{TestClock, TestSleeper};
    use crate::guard::{LeaseGuard, PipelineLifecycle};
    use crate::identity::{LeaseEpoch, NodeId};
    use crate::store::MemoryLeaseStore;
    use std::time::Duration;

    #[tokio::test]
    async fn renew_extends_local_deadline() {
        let store = MemoryLeaseStore::new();
        let clock = TestClock::new();
        let sleeper = TestSleeper::new(clock.clone());
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let node = NodeId::generate();
        let session = acquire_pipeline(&store, clock.as_ref(), &sleeper, &key, &node)
            .await
            .unwrap();
        let first_deadline = session.local_deadline;
        clock.advance(Duration::from_secs(5));
        let renewed = renew_pipeline(&store, clock.as_ref(), &key, &session)
            .await
            .unwrap();
        assert!(renewed.local_deadline > first_deadline);
        assert_eq!(renewed.epoch, LeaseEpoch::new(1));
        let _ = PipelineLifecycle::Active;
        let _ = LeaseGuard::owner_elect(key, renewed, clock);
    }

    #[tokio::test]
    async fn owner_reenters_unactivated_lease_and_renews() {
        let store = MemoryLeaseStore::new();
        let clock = TestClock::new();
        let sleeper = TestSleeper::new(clock.clone());
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let node = NodeId::generate();
        let first = acquire_pipeline(&store, clock.as_ref(), &sleeper, &key, &node)
            .await
            .unwrap();
        clock.advance(Duration::from_secs(5));
        let again = acquire_pipeline(&store, clock.as_ref(), &sleeper, &key, &node)
            .await
            .unwrap();
        assert_eq!(again.owner, node);
        assert_eq!(again.epoch, first.epoch);
        assert!(again.heartbeat > first.heartbeat);
        assert!(again.local_deadline > first.local_deadline);
    }
}
