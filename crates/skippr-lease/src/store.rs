use crate::error::LeaseError;
use crate::identity::{LeaseEpoch, LeaseObservation, LeaseSession, NodeId, PipelineKey};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

#[async_trait]
pub trait PipelineLeaseStore: Send + Sync {
    async fn create(
        &self,
        key: &PipelineKey,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError>;

    async fn read_consistent(
        &self,
        key: &PipelineKey,
    ) -> Result<Option<LeaseObservation>, LeaseError>;

    async fn acquire_released(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError>;

    async fn steal_unchanged(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError>;

    async fn renew(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<LeaseObservation, LeaseError>;

    async fn mark_initialized(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<(), LeaseError>;

    async fn release_after_drain(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<(), LeaseError>;
}

#[derive(Default)]
pub struct MemoryLeaseStore {
    rows: Mutex<HashMap<String, LeaseObservation>>,
}

impl MemoryLeaseStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn cas(
        &self,
        key: &PipelineKey,
        expected: &LeaseObservation,
        next: LeaseObservation,
    ) -> Result<LeaseObservation, LeaseError> {
        let mut rows = self.rows.lock().expect("memory lease store poisoned");
        match rows.get(&key.dynamo_pk()) {
            Some(current) if current == expected => {
                rows.insert(key.dynamo_pk(), next.clone());
                Ok(next)
            }
            Some(_) => Err(LeaseError::ConditionalRace),
            None => Err(LeaseError::ConditionalRace),
        }
    }
}

#[async_trait]
impl PipelineLeaseStore for MemoryLeaseStore {
    async fn create(
        &self,
        key: &PipelineKey,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        let mut rows = self.rows.lock().expect("memory lease store poisoned");
        if rows.contains_key(&key.dynamo_pk()) {
            return Err(LeaseError::ConditionalRace);
        }
        let observation = LeaseObservation {
            owner: *owner,
            epoch: LeaseEpoch::new(1),
            heartbeat: 1,
            released: false,
            initialized: false,
        };
        rows.insert(key.dynamo_pk(), observation.clone());
        Ok(observation)
    }

    async fn read_consistent(
        &self,
        key: &PipelineKey,
    ) -> Result<Option<LeaseObservation>, LeaseError> {
        let rows = self.rows.lock().expect("memory lease store poisoned");
        Ok(rows.get(&key.dynamo_pk()).cloned())
    }

    async fn acquire_released(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        if !observed.released {
            return Err(LeaseError::ConditionalRace);
        }
        let next = LeaseObservation {
            owner: *owner,
            epoch: observed
                .epoch
                .next()
                .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?,
            heartbeat: 1,
            released: false,
            initialized: observed.initialized,
        };
        self.cas(key, observed, next)
    }

    async fn steal_unchanged(
        &self,
        key: &PipelineKey,
        observed: &LeaseObservation,
        owner: &NodeId,
    ) -> Result<LeaseObservation, LeaseError> {
        if observed.released {
            return Err(LeaseError::ConditionalRace);
        }
        let next = LeaseObservation {
            owner: *owner,
            epoch: observed
                .epoch
                .next()
                .map_err(|err| LeaseError::StoreUnavailable(err.to_string()))?,
            heartbeat: 1,
            released: false,
            initialized: observed.initialized,
        };
        self.cas(key, observed, next)
    }

    async fn renew(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<LeaseObservation, LeaseError> {
        let mut rows = self.rows.lock().expect("memory lease store poisoned");
        let current = rows.get_mut(&key.dynamo_pk()).ok_or(LeaseError::Lost)?;
        if current.owner != session.owner || current.epoch != session.epoch || current.released {
            return Err(LeaseError::Lost);
        }
        current.heartbeat = current.heartbeat.saturating_add(1);
        Ok(current.clone())
    }

    async fn mark_initialized(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<(), LeaseError> {
        let mut rows = self.rows.lock().expect("memory lease store poisoned");
        let current = rows.get_mut(&key.dynamo_pk()).ok_or(LeaseError::Lost)?;
        if current.owner != session.owner || current.epoch != session.epoch || current.released {
            return Err(LeaseError::Lost);
        }
        current.initialized = true;
        Ok(())
    }

    async fn release_after_drain(
        &self,
        key: &PipelineKey,
        session: &LeaseSession,
    ) -> Result<(), LeaseError> {
        let mut rows = self.rows.lock().expect("memory lease store poisoned");
        let current = rows.get_mut(&key.dynamo_pk()).ok_or(LeaseError::Lost)?;
        if current.owner != session.owner || current.epoch != session.epoch || current.released {
            return Err(LeaseError::Lost);
        }
        current.released = true;
        current.heartbeat = current.heartbeat.saturating_add(1);
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MembershipAd {
    pub node_id: NodeId,
    pub host_id: crate::identity::HostId,
    pub heartbeat: u64,
    pub ready: bool,
}

#[derive(Default)]
pub struct MemoryMembershipStore {
    rows: Mutex<HashMap<String, MembershipAd>>,
}

impl MemoryMembershipStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&self, cluster_pk: &str, ad: MembershipAd) {
        self.rows
            .lock()
            .expect("memory membership poisoned")
            .insert(format!("{cluster_pk}#{}", ad.node_id), ad);
    }

    pub fn query(&self, cluster_pk: &str) -> Vec<MembershipAd> {
        let prefix = format!("{cluster_pk}#");
        self.rows
            .lock()
            .expect("memory membership poisoned")
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(_, ad)| ad.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, TestClock, TestSleeper};
    use crate::guard::LEASE_TIMEOUT;
    use crate::protocol::acquire_pipeline;
    use std::sync::Arc;

    fn key() -> PipelineKey {
        PipelineKey::new("t", "w", "p").unwrap()
    }

    #[tokio::test]
    async fn create_then_renew_increments_heartbeat() {
        let store = MemoryLeaseStore::new();
        let owner = NodeId::generate();
        let created = store.create(&key(), &owner).await.unwrap();
        assert_eq!(created.epoch.get(), 1);
        assert_eq!(created.heartbeat, 1);
        let session = LeaseSession::from_observation(
            created,
            crate::clock::MonoInstant::from_nanos(u64::MAX),
        );
        let renewed = store.renew(&key(), &session).await.unwrap();
        assert_eq!(renewed.heartbeat, 2);
        assert_eq!(renewed.epoch.get(), 1);
    }

    #[tokio::test]
    async fn steal_requires_unchanged_observation() {
        let store = MemoryLeaseStore::new();
        let clock = TestClock::new();
        let sleeper = TestSleeper::new(clock.clone());
        let owner = NodeId::generate();
        let key = key();
        store.create(&key, &owner).await.unwrap();
        let thief = NodeId::generate();
        let stolen = acquire_pipeline(&store, clock.as_ref(), &sleeper, &key, &thief)
            .await
            .unwrap();
        assert_eq!(stolen.owner, thief);
        assert_eq!(stolen.epoch.get(), 2);
        assert_eq!(stolen.heartbeat, 1);
        assert!(clock.monotonic_now().as_nanos() >= LEASE_TIMEOUT.as_nanos() as u64);
    }

    #[tokio::test]
    async fn renew_during_observation_blocks_steal() {
        let store = Arc::new(MemoryLeaseStore::new());
        let clock = TestClock::new();
        let owner = NodeId::generate();
        let key = key();
        let created = store.create(&key, &owner).await.unwrap();
        let session = LeaseSession::from_observation(
            created,
            crate::clock::MonoInstant::from_nanos(u64::MAX),
        );

        struct RenewOnSleep {
            clock: Arc<TestClock>,
            store: Arc<MemoryLeaseStore>,
            key: PipelineKey,
            session: LeaseSession,
        }
        #[async_trait::async_trait]
        impl crate::clock::Sleeper for RenewOnSleep {
            async fn sleep_until(&self, deadline: crate::clock::MonoInstant) {
                self.store
                    .renew(&self.key, &self.session)
                    .await
                    .expect("owner renew during observation");
                let now = self.clock.monotonic_now();
                if now < deadline {
                    self.clock.advance(std::time::Duration::from_nanos(
                        deadline.as_nanos().saturating_sub(now.as_nanos()),
                    ));
                }
            }
        }

        let sleeper = RenewOnSleep {
            clock: clock.clone(),
            store: store.clone(),
            key: key.clone(),
            session,
        };
        let thief = NodeId::generate();
        let result = acquire_pipeline(store.as_ref(), clock.as_ref(), &sleeper, &key, &thief).await;
        assert!(matches!(result, Err(LeaseError::Held(_))));
    }

    #[tokio::test]
    async fn release_then_acquire_increments_epoch() {
        let store = MemoryLeaseStore::new();
        let owner = NodeId::generate();
        let created = store.create(&key(), &owner).await.unwrap();
        let session = LeaseSession::from_observation(
            created.clone(),
            crate::clock::MonoInstant::from_nanos(u64::MAX),
        );
        store.release_after_drain(&key(), &session).await.unwrap();
        let next = NodeId::generate();
        let acquired = store
            .acquire_released(
                &key(),
                &store.read_consistent(&key()).await.unwrap().unwrap(),
                &next,
            )
            .await
            .unwrap();
        assert_eq!(acquired.owner, next);
        assert_eq!(acquired.epoch.get(), 2);
        assert!(!acquired.released);
    }

    #[tokio::test]
    async fn concurrent_create_yields_one_epoch_one_owner() {
        let store = Arc::new(MemoryLeaseStore::new());
        let clock = TestClock::new();
        struct HangSleeper;
        #[async_trait::async_trait]
        impl crate::clock::Sleeper for HangSleeper {
            async fn sleep_until(&self, _deadline: crate::clock::MonoInstant) {
                std::future::pending::<()>().await;
            }
        }
        let key = key();
        let a = NodeId::generate();
        let b = NodeId::generate();
        let sleeper = HangSleeper;
        tokio::select! {
            left = acquire_pipeline(store.as_ref(), clock.as_ref(), &sleeper, &key, &a) => {
                assert_eq!(left.unwrap().epoch.get(), 1);
            }
            right = acquire_pipeline(store.as_ref(), clock.as_ref(), &sleeper, &key, &b) => {
                assert_eq!(right.unwrap().epoch.get(), 1);
            }
        }
        let held = store.read_consistent(&key).await.unwrap().unwrap();
        assert_eq!(held.epoch.get(), 1);
        assert!(!held.released);
    }

    #[tokio::test]
    async fn lease_row_is_never_deleted() {
        let store = MemoryLeaseStore::new();
        let owner = NodeId::generate();
        store.create(&key(), &owner).await.unwrap();
        assert!(store.read_consistent(&key()).await.unwrap().is_some());
    }
}
