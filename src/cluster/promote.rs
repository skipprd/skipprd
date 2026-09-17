use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use skippr_lease::{
    acquire_pipeline, renew_until_lost, Clock, CommitIndex, DurableError, HostId, LeaseEpoch,
    LeaseGuard, NodeId, PipelineKey, PipelineLeaseStore, PipelinePaths, PromoteError, Sleeper,
    GENESIS_HASH, LEASE_TIMEOUT,
};

use crate::cluster::catchup::catch_up_from_donor;
use crate::cluster::gossip::GossipService;
use crate::cluster::identity::ClusterIdentity;
use crate::cluster::peer::{assign_replica, query_status};
use crate::cluster::placement::{rank_replicas, ReplicaCandidate};
use crate::helpers::configuration::Config;

/// Control RPCs during promote/replace must fail fast. Replication of WAL
/// payloads still uses the longer replica idle timeout.
const REPLICA_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct PeerStatus {
    pub node_id: NodeId,
    pub committed: CommitIndex,
    pub head_hash: [u8; 32],
    pub initialized: bool,
    pub endpoint: SocketAddr,
}

#[derive(Clone, Debug)]
pub enum ClusterHead {
    Genesis,
    Peer(PeerStatus),
}

impl ClusterHead {
    pub fn committed(&self) -> CommitIndex {
        match self {
            Self::Genesis => CommitIndex::ZERO,
            Self::Peer(peer) => peer.committed,
        }
    }

    pub fn head_hash(&self) -> [u8; 32] {
        match self {
            Self::Genesis => GENESIS_HASH,
            Self::Peer(peer) => peer.head_hash,
        }
    }

    pub fn endpoint(&self) -> Option<SocketAddr> {
        match self {
            Self::Genesis => None,
            Self::Peer(peer) => Some(peer.endpoint),
        }
    }
}

pub fn select_highest_hash_consistent(
    statuses: &[PeerStatus],
    lease_initialized: bool,
) -> Result<ClusterHead, PromoteError> {
    if statuses.is_empty() {
        if lease_initialized {
            return Err(PromoteError::NoHeadAvailable);
        }
        return Ok(ClusterHead::Genesis);
    }
    let mut by_index: BTreeMap<u64, Vec<&PeerStatus>> = BTreeMap::new();
    for status in statuses {
        by_index
            .entry(status.committed.get())
            .or_default()
            .push(status);
    }
    let (index, group) = by_index
        .iter()
        .next_back()
        .ok_or(PromoteError::NoHeadAvailable)?;
    let hash = group[0].head_hash;
    if group.iter().any(|status| status.head_hash != hash) {
        return Err(PromoteError::Diverged {
            index: CommitIndex::new(*index),
            detail: "peers disagree on head hash".into(),
        });
    }
    Ok(ClusterHead::Peer((*group[0]).clone()))
}

pub struct PromoteContext {
    pub app_config: Config,
    pub paths: PipelinePaths,
    pub identity: ClusterIdentity,
    pub local_replica: SocketAddr,
    pub gossip: Arc<GossipService>,
    pub legacy_root: Option<std::path::PathBuf>,
    pub flatten_events: bool,
}

pub struct PromoteOutcome {
    pub guard: Arc<LeaseGuard>,
    pub replica_endpoint: SocketAddr,
    pub session: skippr_lease::LeaseSession,
    pub needs_initialize: bool,
}

pub async fn statuses_from_gossip(
    gossip: &GossipService,
    key: &PipelineKey,
    identity: &ClusterIdentity,
) -> Vec<PeerStatus> {
    let ads = gossip.known_ads().await;
    let futs = ads.into_iter().map(|ad| {
        let identity = identity.clone();
        let key = key.clone();
        async move {
            let status = tokio::time::timeout(
                REPLICA_PROBE_TIMEOUT,
                query_status(ad.replica, &key, &identity),
            )
            .await
            .ok()?
            .ok()?;
            let head_hash = replica_status_hash(&status.head_hash).ok()?;
            Some(PeerStatus {
                node_id: ad.node_id,
                committed: CommitIndex::new(status.committed_index),
                head_hash,
                initialized: status.ready,
                endpoint: ad.replica,
            })
        }
    });
    futures::future::join_all(futs)
        .await
        .into_iter()
        .flatten()
        .collect()
}

pub(crate) fn replica_status_hash(head_hash: &[u8]) -> Result<[u8; 32], DurableError> {
    if head_hash.is_empty() {
        return Ok(GENESIS_HASH);
    }
    if head_hash.len() != 32 {
        return Err(DurableError::ProtocolMismatch(
            "replica StatusOk head_hash must be 32 bytes".into(),
        ));
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(head_hash);
    Ok(hash)
}

fn promote_durable_err(err: DurableError) -> PromoteError {
    match err {
        DurableError::Diverged(detail) => PromoteError::Diverged {
            index: CommitIndex::ZERO,
            detail,
        },
        DurableError::UnprovenPrepared(index) => PromoteError::UnprovenPrepared { index },
        other => PromoteError::Other(other.to_string()),
    }
}

pub fn candidates_from_gossip(ads: &[crate::cluster::gossip::GossipAd]) -> Vec<ReplicaCandidate> {
    ads.iter()
        .map(|ad| ReplicaCandidate {
            node_id: ad.node_id,
            host_id: ad.host_id.clone(),
            ready: ad.ready,
            protocol_min: ad.protocol_min,
            protocol_max: ad.protocol_max,
            endpoint: ad.replica,
            committed_lag: 0,
            disk_pressure: ad.disk_pressure,
        })
        .collect()
}

pub async fn promote_pipeline(
    leases: Arc<dyn PipelineLeaseStore>,
    clock: Arc<dyn Clock>,
    sleeper: Arc<dyn Sleeper>,
    key: PipelineKey,
    node: NodeId,
    statuses: Vec<PeerStatus>,
    candidates: Vec<ReplicaCandidate>,
    self_host: &HostId,
    ctx: PromoteContext,
) -> Result<PromoteOutcome, PromoteError> {
    let session = acquire_pipeline(
        leases.as_ref(),
        clock.as_ref(),
        sleeper.as_ref(),
        &key,
        &node,
    )
    .await?;
    crate::metrics::counters::add_cluster_lease_acquire(1);
    let guard = LeaseGuard::owner_elect(key.clone(), session.clone(), clock.clone());
    let hold = tokio::spawn({
        let leases = Arc::clone(&leases);
        let clock = Arc::clone(&clock);
        let sleeper = Arc::clone(&sleeper);
        let key = key.clone();
        let guard = Arc::clone(&guard);
        async move {
            renew_until_lost(
                leases.as_ref(),
                clock.as_ref(),
                sleeper.as_ref(),
                &key,
                guard.as_ref(),
            )
            .await;
            crate::metrics::counters::add_cluster_lease_lost(1);
        }
    });
    let result = promote_after_acquire(
        session.clone(),
        guard,
        clock,
        sleeper,
        key.clone(),
        node,
        statuses,
        candidates,
        self_host,
        ctx,
    )
    .await;
    hold.abort();
    if result.is_err() {
        if let Err(err) = leases.release_after_drain(&key, &session).await {
            tracing::error!(
                error = %err,
                pipeline = %key.pipeline(),
                "failed promote could not release lease"
            );
        }
    }
    result
}

async fn promote_after_acquire(
    session: skippr_lease::LeaseSession,
    guard: Arc<LeaseGuard>,
    clock: Arc<dyn Clock>,
    sleeper: Arc<dyn Sleeper>,
    key: PipelineKey,
    node: NodeId,
    mut statuses: Vec<PeerStatus>,
    candidates: Vec<ReplicaCandidate>,
    self_host: &HostId,
    ctx: PromoteContext,
) -> Result<PromoteOutcome, PromoteError> {
    ctx.gossip.broadcast_fence(&key, session.epoch).await;
    crate::metrics::counters::add_cluster_fence(1);
    if statuses.is_empty() {
        statuses = statuses_from_gossip(&ctx.gossip, &key, &ctx.identity).await;
    }
    let donor = select_highest_hash_consistent(&statuses, session.initialized)?;
    let mut log = crate::buffer::durable::log::MutationLog::open(ctx.paths.clone())
        .map_err(promote_durable_err)?;
    if let Some(endpoint) = donor.endpoint() {
        catch_up_from_donor(&mut log, &ctx.paths, &key, endpoint, &ctx.identity)
            .await
            .map_err(promote_durable_err)?;
    }
    let peers: Vec<_> = statuses.iter().map(|status| status.endpoint).collect();
    let applicator = crate::buffer::durable::apply::DurableApplicator::new(ctx.paths.clone());
    crate::buffer::durable::store::recover_unproven_prepared(
        &mut log,
        &applicator,
        &key,
        &ctx.identity,
        &peers,
    )
    .await
    .map_err(promote_durable_err)?;
    crate::cluster::schema::load_and_install_pipeline_schema(
        &ctx.app_config,
        &key,
        &ctx.paths,
        &log,
        ctx.flatten_events,
        session.initialized,
    )
    .await?;
    let mut ranked = candidates;
    for candidate in &mut ranked {
        if let Some(status) = statuses.iter().find(|s| s.node_id == candidate.node_id) {
            candidate.endpoint = status.endpoint;
            candidate.committed_lag = donor
                .committed()
                .get()
                .saturating_sub(status.committed.get());
        }
    }
    let ranked = rank_replicas(&key, node, self_host, ranked);
    if ranked.is_empty() {
        return Err(PromoteError::Replica(
            "no eligible synchronous replica".into(),
        ));
    }
    let mut last_err = None;
    let mut chosen = None;
    for replica in &ranked {
        match timeout_assign(
            replica.endpoint,
            &key,
            session.epoch,
            ctx.local_replica,
            &ctx.identity,
        )
        .await
        {
            Ok(()) => {}
            Err(err) => {
                tracing::warn!(
                    endpoint = %replica.endpoint,
                    error = %err,
                    "replica assign failed; trying next candidate"
                );
                last_err = Some(err.to_string());
                continue;
            }
        }
        let status = match tokio::time::timeout(
            REPLICA_PROBE_TIMEOUT,
            query_status(replica.endpoint, &key, &ctx.identity),
        )
        .await
        {
            Ok(Ok(status)) => status,
            _ => {
                last_err = Some("replica status query failed".into());
                continue;
            }
        };
        match wait_until_replica_ready(
            replica.endpoint,
            &key,
            &ctx.identity,
            clock.as_ref(),
            sleeper.as_ref(),
            status,
        )
        .await
        {
            Ok(_) => {
                tracing::info!(endpoint = %replica.endpoint, "assigned replica");
                chosen = Some(replica.endpoint);
                break;
            }
            Err(err) => last_err = Some(err.to_string()),
        }
    }
    let endpoint = chosen.ok_or_else(|| {
        PromoteError::Replica(last_err.unwrap_or_else(|| "no eligible synchronous replica".into()))
    })?;
    if query_status(endpoint, &key, &ctx.identity).await.is_ok() {
        catch_up_from_donor(&mut log, &ctx.paths, &key, endpoint, &ctx.identity)
            .await
            .map_err(|err| PromoteError::Replica(err.to_string()))?;
    }
    let needs_initialize = !session.initialized;
    if needs_initialize {
        crate::cluster::baseline::install_if_needed(
            &key,
            &ctx.paths,
            ctx.legacy_root.as_deref(),
            session.initialized,
        )
        .map_err(|err| PromoteError::Other(err.to_string()))?;
    }
    let session = guard.leased_session().unwrap_or(session);
    Ok(PromoteOutcome {
        guard,
        replica_endpoint: endpoint,
        session,
        needs_initialize,
    })
}

async fn timeout_assign(
    endpoint: SocketAddr,
    key: &PipelineKey,
    epoch: LeaseEpoch,
    primary_endpoint: SocketAddr,
    identity: &ClusterIdentity,
) -> Result<(), DurableError> {
    match tokio::time::timeout(
        REPLICA_PROBE_TIMEOUT,
        assign_replica(endpoint, key, epoch, primary_endpoint, identity),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(DurableError::ProtocolMismatch(
            "replica assign timed out".into(),
        )),
    }
}

pub async fn assign_reachable_replica(
    ranked: &[ReplicaCandidate],
    key: &PipelineKey,
    epoch: LeaseEpoch,
    primary_endpoint: SocketAddr,
    identity: &ClusterIdentity,
    skip: Option<SocketAddr>,
) -> Result<SocketAddr, DurableError> {
    let mut last_err = None;
    for replica in ranked {
        if skip == Some(replica.endpoint) {
            continue;
        }
        match timeout_assign(replica.endpoint, key, epoch, primary_endpoint, identity).await {
            Ok(()) => {
                tracing::info!(endpoint = %replica.endpoint, "assigned replica");
                return Ok(replica.endpoint);
            }
            Err(err) => {
                tracing::warn!(
                    endpoint = %replica.endpoint,
                    error = %err,
                    "replica assign failed; trying next candidate"
                );
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        DurableError::ProtocolMismatch("no eligible synchronous replica".into())
    }))
}

pub async fn wait_until_replica_ready(
    endpoint: SocketAddr,
    key: &PipelineKey,
    identity: &ClusterIdentity,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
    mut remote: crate::buffer::durable::codec::proto::ReplicaStatus,
) -> Result<crate::buffer::durable::codec::proto::ReplicaStatus, DurableError> {
    let wait_deadline = clock.monotonic_now().saturating_add(LEASE_TIMEOUT);
    while clock.monotonic_now() < wait_deadline {
        if remote.ready {
            break;
        }
        sleeper
            .sleep_until(
                clock
                    .monotonic_now()
                    .saturating_add(Duration::from_millis(200)),
            )
            .await;
        remote = query_status(endpoint, key, identity).await?;
    }
    if !remote.ready {
        return Err(DurableError::NotCaughtUp);
    }
    Ok(remote)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::NodeId;

    fn endpoint() -> SocketAddr {
        "127.0.0.1:9".parse().unwrap()
    }

    #[test]
    fn hash_disagreement_is_diverged_not_majority() {
        let a = PeerStatus {
            node_id: NodeId::generate(),
            committed: CommitIndex::new(3),
            head_hash: [1u8; 32],
            initialized: true,
            endpoint: endpoint(),
        };
        let b = PeerStatus {
            node_id: NodeId::generate(),
            committed: CommitIndex::new(3),
            head_hash: [2u8; 32],
            initialized: true,
            endpoint: endpoint(),
        };
        let err = select_highest_hash_consistent(&[a, b], true).unwrap_err();
        assert!(matches!(err, PromoteError::Diverged { .. }));
    }

    #[test]
    fn empty_initialized_lease_is_no_head() {
        let err = select_highest_hash_consistent(&[], true).unwrap_err();
        assert!(matches!(err, PromoteError::NoHeadAvailable));
    }

    #[test]
    fn empty_uninitialized_is_head_zero() {
        let head = select_highest_hash_consistent(&[], false).unwrap();
        assert!(matches!(head, ClusterHead::Genesis));
        assert_eq!(head.committed(), CommitIndex::ZERO);
        assert_eq!(head.head_hash(), GENESIS_HASH);
    }

    #[tokio::test]
    async fn assign_reachable_empty_ranked_is_error() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let identity = crate::cluster::identity::ClusterIdentity::new(
            skippr_lease::ClusterId::new("test-cluster").unwrap(),
            NodeId::generate(),
        );
        let err = assign_reachable_replica(
            &[],
            &key,
            skippr_lease::LeaseEpoch::new(1),
            endpoint(),
            &identity,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DurableError::ProtocolMismatch(_)));
    }

    #[test]
    fn stolen_lease_loads_metadata_json_after_steal_ttl() {
        let src = include_str!("promote.rs");
        assert!(src.contains("load_and_install_pipeline_schema"));
        let acquire = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/crates/skippr-lease/src/protocol.rs"
        ));
        assert_eq!(
            acquire
                .matches("sleep_until(observed_at + LEASE_TIMEOUT)")
                .count(),
            1
        );
        let schema_src = include_str!("schema.rs");
        assert!(schema_src.contains("load_pipeline_metadata"));
        assert!(schema_src.contains("install_from_pipeline_metadata"));
    }

    #[test]
    fn owner_elect_renews_before_active_primary() {
        let src = include_str!("promote.rs");
        assert!(src.contains("renew_until_lost"));
        assert!(src.contains("promote_after_acquire"));
        assert!(src.contains("guard.leased_session()"));
    }

    #[test]
    fn failed_promote_releases_lease() {
        let src = include_str!("promote.rs");
        let after = src
            .split("let result = promote_after_acquire")
            .nth(1)
            .expect("promote_after_acquire result");
        assert!(after.contains("release_after_drain"));
        assert!(after.contains("if result.is_err()"));
    }

    #[test]
    fn unproven_prepared_maps_to_typed_promote_error() {
        let err = promote_durable_err(DurableError::UnprovenPrepared(37));
        assert!(matches!(err, PromoteError::UnprovenPrepared { index: 37 }));
    }

    #[test]
    fn replica_status_hash_rejects_non_32_byte_values() {
        assert_eq!(replica_status_hash(&[]).unwrap(), GENESIS_HASH);
        assert!(replica_status_hash(&[1, 2, 3]).is_err());
        assert_eq!(replica_status_hash(&[7u8; 32]).unwrap(), [7u8; 32]);
    }
}
