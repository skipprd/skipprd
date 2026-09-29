use std::sync::Arc;

use crate::clock::{Clock, TestClock, TestSleeper};
use crate::error::LeaseError;
use crate::guard::LEASE_TIMEOUT;
use crate::identity::{NodeId, PipelineKey};
use crate::protocol::acquire_pipeline;
use crate::store::PipelineLeaseStore;

/// Shared lease/epoch contract for memory, sled, DynamoDB, and Cloud Tables.
pub async fn assert_pipeline_lease_conformance(store: Arc<dyn PipelineLeaseStore>) {
    one_owner_and_renew(store.as_ref()).await;
    unchanged_heartbeat_takeover(store.as_ref()).await;
    monotonic_epoch_on_release(store.as_ref()).await;
    conditional_release_rejects_stale(store.as_ref()).await;
}

async fn one_owner_and_renew(store: &dyn PipelineLeaseStore) {
    let key = PipelineKey::new("t", "w", "one-owner").unwrap();
    let owner = NodeId::generate();
    let created = store.create(&key, &owner).await.unwrap();
    assert_eq!(created.epoch.get(), 1);
    assert_eq!(created.heartbeat, 1);
    let session = crate::identity::LeaseSession::from_observation(
        created,
        crate::clock::MonoInstant::from_nanos(u64::MAX),
    );
    let renewed = store.renew(&key, &session).await.unwrap();
    assert_eq!(renewed.heartbeat, 2);
    assert_eq!(renewed.epoch.get(), 1);
    assert_eq!(renewed.owner, owner);
}

async fn unchanged_heartbeat_takeover(store: &dyn PipelineLeaseStore) {
    let clock = TestClock::new();
    let sleeper = TestSleeper::new(clock.clone());
    let owner = NodeId::generate();
    let key = PipelineKey::new("t", "w", "takeover").unwrap();
    store.create(&key, &owner).await.unwrap();
    let thief = NodeId::generate();
    let stolen = acquire_pipeline(store, clock.as_ref(), &sleeper, &key, &thief)
        .await
        .unwrap();
    assert_eq!(stolen.owner, thief);
    assert_eq!(stolen.epoch.get(), 2);
    assert_eq!(stolen.heartbeat, 1);
    assert!(clock.monotonic_now().as_nanos() >= LEASE_TIMEOUT.as_nanos() as u64);
}

async fn monotonic_epoch_on_release(store: &dyn PipelineLeaseStore) {
    let key = PipelineKey::new("t", "w", "release").unwrap();
    let owner = NodeId::generate();
    let created = store.create(&key, &owner).await.unwrap();
    let session = crate::identity::LeaseSession::from_observation(
        created.clone(),
        crate::clock::MonoInstant::from_nanos(u64::MAX),
    );
    store.release_after_drain(&key, &session).await.unwrap();
    let next = NodeId::generate();
    let acquired = store
        .acquire_released(
            &key,
            &store.read_consistent(&key).await.unwrap().unwrap(),
            &next,
        )
        .await
        .unwrap();
    assert_eq!(acquired.owner, next);
    assert_eq!(acquired.epoch.get(), 2);
    assert!(!acquired.released);
}

async fn conditional_release_rejects_stale(store: &dyn PipelineLeaseStore) {
    let key = PipelineKey::new("t", "w", "stale-release").unwrap();
    let owner = NodeId::generate();
    let created = store.create(&key, &owner).await.unwrap();
    let session = crate::identity::LeaseSession::from_observation(
        created,
        crate::clock::MonoInstant::from_nanos(u64::MAX),
    );
    store.release_after_drain(&key, &session).await.unwrap();
    let err = store.release_after_drain(&key, &session).await.unwrap_err();
    assert_eq!(err, LeaseError::Lost);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryLeaseStore;

    #[tokio::test]
    async fn memory_lease_store_conformance() {
        assert_pipeline_lease_conformance(Arc::new(MemoryLeaseStore::new())).await;
    }
}
