use crate::clock::Clock;
use crate::error::FenceError;
use crate::identity::{LeaseEpoch, LeaseSession, PipelineKey};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::watch;

pub const LEASE_TIMEOUT: Duration = Duration::from_secs(30);
pub const LEASE_RENEW_PERIOD: Duration = Duration::from_secs(10);
pub const REPLICATION_FACTOR: u32 = 2;
pub const WRITE_QUORUM: u32 = 2;

#[derive(Clone, Debug)]
pub enum WriteAuthority {
    SingleNode,
    Leased(LeaseSession),
}

#[derive(Clone, Debug)]
pub enum PipelineRole {
    Idle,
    OwnerElect(LeaseSession),
    ActivePrimary(WriteAuthority),
    Replica { epoch_seen: LeaseEpoch },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipelineLifecycle {
    Idle,
    OwnerElect,
    Active,
    Fenced,
    Draining,
}

enum WriterKind {
    WalCommit,
    OffsetPublish,
}

pub struct LeaseGuard {
    key: PipelineKey,
    role: RwLock<PipelineRole>,
    lifecycle: watch::Sender<PipelineLifecycle>,
    clock: Arc<dyn Clock>,
}

fn merge_renewed_deadline(current: &LeaseSession, mut incoming: LeaseSession) -> LeaseSession {
    if current.local_deadline > incoming.local_deadline {
        incoming.local_deadline = current.local_deadline;
        incoming.heartbeat = incoming.heartbeat.max(current.heartbeat);
    }
    incoming.initialized |= current.initialized;
    incoming
}

impl LeaseGuard {
    pub fn single_node(key: PipelineKey, clock: Arc<dyn Clock>) -> Arc<Self> {
        let (lifecycle, _) = watch::channel(PipelineLifecycle::Active);
        Arc::new(Self {
            key,
            role: RwLock::new(PipelineRole::ActivePrimary(WriteAuthority::SingleNode)),
            lifecycle,
            clock,
        })
    }

    pub fn owner_elect(
        key: PipelineKey,
        session: LeaseSession,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        let (lifecycle, _) = watch::channel(PipelineLifecycle::OwnerElect);
        Arc::new(Self {
            key,
            role: RwLock::new(PipelineRole::OwnerElect(session)),
            lifecycle,
            clock,
        })
    }

    pub fn replica(key: PipelineKey, epoch_seen: LeaseEpoch, clock: Arc<dyn Clock>) -> Arc<Self> {
        let (lifecycle, _) = watch::channel(PipelineLifecycle::Idle);
        Arc::new(Self {
            key,
            role: RwLock::new(PipelineRole::Replica { epoch_seen }),
            lifecycle,
            clock,
        })
    }

    pub fn key(&self) -> &PipelineKey {
        &self.key
    }

    pub fn subscribe(&self) -> watch::Receiver<PipelineLifecycle> {
        self.lifecycle.subscribe()
    }

    pub fn lifecycle(&self) -> PipelineLifecycle {
        *self.lifecycle.borrow()
    }

    pub fn role(&self) -> PipelineRole {
        self.role.read().expect("lease role poisoned").clone()
    }

    pub fn require_active_epoch(&self) -> Result<LeaseEpoch, FenceError> {
        self.require_writer_epoch(WriterKind::WalCommit)
    }

    /// OwnerElect, leased ActivePrimary, or single-node. Offset reconcile/publish
    /// may run before activate. Does not authorize WAL commits.
    pub fn require_offset_epoch(&self) -> Result<LeaseEpoch, FenceError> {
        self.require_writer_epoch(WriterKind::OffsetPublish)
    }

    pub fn require_same_active_epoch(&self, expected: LeaseEpoch) -> Result<(), FenceError> {
        self.require_same_epoch(self.require_active_epoch()?, expected)
    }

    pub fn require_same_offset_epoch(&self, expected: LeaseEpoch) -> Result<(), FenceError> {
        self.require_same_epoch(self.require_offset_epoch()?, expected)
    }

    fn require_same_epoch(
        &self,
        current: LeaseEpoch,
        expected: LeaseEpoch,
    ) -> Result<(), FenceError> {
        if current == expected {
            Ok(())
        } else {
            self.fence();
            Err(FenceError::EpochMismatch)
        }
    }

    fn require_writer_epoch(&self, kind: WriterKind) -> Result<LeaseEpoch, FenceError> {
        if matches!(
            self.lifecycle(),
            PipelineLifecycle::Draining | PipelineLifecycle::Fenced
        ) {
            return Err(FenceError::NotActive);
        }
        let expired = {
            match &*self.role.read().expect("lease role poisoned") {
                PipelineRole::ActivePrimary(WriteAuthority::SingleNode) => {
                    return Ok(LeaseEpoch::SINGLE_NODE);
                }
                PipelineRole::ActivePrimary(WriteAuthority::Leased(session))
                    if self.clock.monotonic_now() < session.local_deadline =>
                {
                    return Ok(session.epoch);
                }
                PipelineRole::OwnerElect(session)
                    if matches!(kind, WriterKind::OffsetPublish)
                        && self.clock.monotonic_now() < session.local_deadline =>
                {
                    return Ok(session.epoch);
                }
                PipelineRole::ActivePrimary(WriteAuthority::Leased(_)) => true,
                PipelineRole::OwnerElect(_) if matches!(kind, WriterKind::OffsetPublish) => true,
                PipelineRole::OwnerElect(_) | PipelineRole::Idle | PipelineRole::Replica { .. } => {
                    return Err(FenceError::NotActive);
                }
            }
        };
        if expired {
            self.fence();
            Err(FenceError::DeadlineExpired)
        } else {
            Err(FenceError::NotActive)
        }
    }

    pub fn activate(&self, session: LeaseSession) -> Result<(), FenceError> {
        let mut role = self.role.write().expect("lease role poisoned");
        let (merged, from_elect) = match &*role {
            PipelineRole::OwnerElect(current) if current.epoch == session.epoch => {
                (merge_renewed_deadline(current, session), true)
            }
            PipelineRole::ActivePrimary(WriteAuthority::Leased(current))
                if current.epoch == session.epoch =>
            {
                (merge_renewed_deadline(current, session), false)
            }
            _ => return Err(FenceError::NotActive),
        };
        if self.clock.monotonic_now() >= merged.local_deadline {
            *role = PipelineRole::Idle;
            let _ = self.lifecycle.send_replace(PipelineLifecycle::Fenced);
            return Err(FenceError::DeadlineExpired);
        }
        *role = PipelineRole::ActivePrimary(WriteAuthority::Leased(merged));
        if from_elect {
            let _ = self.lifecycle.send_replace(PipelineLifecycle::Active);
        }
        Ok(())
    }

    pub fn leased_session(&self) -> Option<LeaseSession> {
        match &*self.role.read().expect("lease role poisoned") {
            PipelineRole::OwnerElect(session)
            | PipelineRole::ActivePrimary(WriteAuthority::Leased(session)) => Some(session.clone()),
            _ => None,
        }
    }

    pub fn install_deadline(&self, session: LeaseSession) {
        let mut role = self.role.write().expect("lease role poisoned");
        match &mut *role {
            PipelineRole::OwnerElect(current)
            | PipelineRole::ActivePrimary(WriteAuthority::Leased(current))
                if current.epoch == session.epoch && current.owner == session.owner =>
            {
                *current = session;
            }
            _ => {}
        }
    }

    pub fn observe_epoch(&self, epoch: LeaseEpoch) {
        let mut role = self.role.write().expect("lease role poisoned");
        match &*role {
            PipelineRole::Replica { epoch_seen } if epoch > *epoch_seen => {
                *role = PipelineRole::Replica { epoch_seen: epoch };
            }
            PipelineRole::ActivePrimary(WriteAuthority::Leased(session))
            | PipelineRole::OwnerElect(session)
                if epoch > session.epoch =>
            {
                drop(role);
                self.fence();
            }
            _ => {}
        }
    }

    pub fn reject_stale_epoch(&self, epoch: LeaseEpoch) -> Result<(), FenceError> {
        match &*self.role.read().expect("lease role poisoned") {
            PipelineRole::Replica { epoch_seen } if epoch < *epoch_seen => {
                Err(FenceError::EpochMismatch)
            }
            PipelineRole::ActivePrimary(WriteAuthority::Leased(session))
            | PipelineRole::OwnerElect(session)
                if epoch < session.epoch =>
            {
                Err(FenceError::EpochMismatch)
            }
            _ => Ok(()),
        }
    }

    pub fn fence(&self) {
        let mut role = self.role.write().expect("lease role poisoned");
        *role = PipelineRole::Idle;
        let _ = self.lifecycle.send_replace(PipelineLifecycle::Fenced);
    }

    pub fn begin_drain(&self) {
        let _ = self.lifecycle.send_replace(PipelineLifecycle::Draining);
    }

    pub async fn run_until_fenced<T, E, Fut>(&self, fut: Fut) -> Result<Result<T, E>, FenceError>
    where
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        let mut rx = self.subscribe();
        tokio::pin!(fut);
        tokio::select! {
            biased;
            changed = rx.changed() => {
                let _ = changed;
                if matches!(
                    *rx.borrow(),
                    PipelineLifecycle::Fenced | PipelineLifecycle::Draining
                ) {
                    Err(FenceError::Fenced)
                } else {
                    Ok(fut.await)
                }
            }
            result = &mut fut => Ok(result),
        }
    }

    pub async fn sleep_or_fence(&self, delay: Duration) -> Result<(), FenceError> {
        let mut rx = self.subscribe();
        tokio::select! {
            biased;
            changed = rx.changed() => {
                let _ = changed;
                if matches!(*rx.borrow(), PipelineLifecycle::Fenced) {
                    Err(FenceError::Fenced)
                } else {
                    tokio::time::sleep(delay).await;
                    Ok(())
                }
            }
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{MonoInstant, SystemClock};
    use crate::identity::{LeaseEpoch, LeaseSession, NodeId, PipelineKey};

    fn key() -> PipelineKey {
        PipelineKey::new("t", "w", "p").unwrap()
    }

    fn leased_session(deadline: MonoInstant) -> LeaseSession {
        LeaseSession {
            owner: NodeId::generate(),
            epoch: LeaseEpoch::new(1),
            heartbeat: 1,
            initialized: true,
            local_deadline: deadline,
        }
    }

    #[test]
    fn single_node_is_epoch_zero() {
        let guard = LeaseGuard::single_node(key(), Arc::new(SystemClock::new()));
        assert_eq!(
            guard.require_active_epoch().unwrap(),
            LeaseEpoch::SINGLE_NODE
        );
        assert_eq!(
            guard.require_offset_epoch().unwrap(),
            LeaseEpoch::SINGLE_NODE
        );
    }

    #[test]
    fn expired_deadline_fences() {
        let clock = crate::clock::TestClock::new();
        let session = leased_session(MonoInstant::from_nanos(1));
        let guard = LeaseGuard::owner_elect(key(), session.clone(), clock.clone());
        guard.activate(session).unwrap();
        clock.advance(Duration::from_secs(1));
        assert_eq!(
            guard.require_active_epoch().unwrap_err(),
            FenceError::DeadlineExpired
        );
        assert_eq!(guard.lifecycle(), PipelineLifecycle::Fenced);
    }

    #[test]
    fn higher_observed_epoch_fences_active() {
        let clock = crate::clock::TestClock::new();
        let session = leased_session(MonoInstant::from_nanos(u64::MAX));
        let guard = LeaseGuard::owner_elect(key(), session.clone(), clock);
        guard.activate(session).unwrap();
        guard.observe_epoch(LeaseEpoch::new(2));
        assert_eq!(guard.lifecycle(), PipelineLifecycle::Fenced);
        assert!(matches!(guard.role(), PipelineRole::Idle));
    }

    #[tokio::test]
    async fn run_until_fenced_stops_work() {
        let guard = LeaseGuard::single_node(key(), Arc::new(SystemClock::new()));
        let g = Arc::clone(&guard);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            g.fence();
        });
        let result = guard
            .run_until_fenced(async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok::<(), ()>(())
            })
            .await;
        assert_eq!(result.unwrap_err(), FenceError::Fenced);
    }

    #[test]
    fn draining_rejects_new_wal_writes() {
        let guard = LeaseGuard::single_node(key(), Arc::new(SystemClock::new()));
        assert!(guard.require_active_epoch().is_ok());
        guard.begin_drain();
        assert_eq!(guard.lifecycle(), PipelineLifecycle::Draining);
        assert_eq!(
            guard.require_active_epoch().unwrap_err(),
            FenceError::NotActive
        );
    }

    #[test]
    fn leased_session_is_present_while_active() {
        let clock = crate::clock::TestClock::new();
        let session = leased_session(MonoInstant::from_nanos(u64::MAX));
        let guard = LeaseGuard::owner_elect(key(), session.clone(), clock);
        assert!(guard.leased_session().is_some());
        guard.activate(session).unwrap();
        assert!(guard.leased_session().is_some());
        guard.fence();
        assert!(guard.leased_session().is_none());
    }

    #[test]
    fn activate_keeps_renewed_deadline() {
        let clock = crate::clock::TestClock::new();
        let stale = leased_session(MonoInstant::from_nanos(5_000_000_000));
        let guard = LeaseGuard::owner_elect(key(), stale.clone(), clock.clone());
        let mut renewed = stale.clone();
        renewed.local_deadline = MonoInstant::from_nanos(30_000_000_000);
        renewed.heartbeat = 2;
        guard.install_deadline(renewed);
        guard.activate(stale).unwrap();
        clock.advance(Duration::from_secs(10));
        assert_eq!(guard.require_active_epoch().unwrap(), LeaseEpoch::new(1));
        assert_eq!(guard.lifecycle(), PipelineLifecycle::Active);
    }

    #[test]
    fn activate_rejects_expired_deadline() {
        let clock = crate::clock::TestClock::new();
        let session = leased_session(MonoInstant::from_nanos(1));
        let guard = LeaseGuard::owner_elect(key(), session.clone(), clock.clone());
        clock.advance(Duration::from_secs(1));
        assert_eq!(
            guard.activate(session).unwrap_err(),
            FenceError::DeadlineExpired
        );
        assert_eq!(guard.lifecycle(), PipelineLifecycle::Fenced);
        assert!(guard.leased_session().is_none());
    }

    #[test]
    fn owner_elect_can_reconcile_but_not_commit() {
        let clock = crate::clock::TestClock::new();
        let session = leased_session(MonoInstant::from_nanos(u64::MAX));
        let guard = LeaseGuard::owner_elect(key(), session, clock);
        assert_eq!(
            guard.require_active_epoch().unwrap_err(),
            FenceError::NotActive
        );
        assert_eq!(guard.require_offset_epoch().unwrap(), LeaseEpoch::new(1));
        guard.require_same_offset_epoch(LeaseEpoch::new(1)).unwrap();
        assert_eq!(guard.lifecycle(), PipelineLifecycle::OwnerElect);
    }
}
