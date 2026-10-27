use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use skippr_lease::{
    acquire_pipeline, renew_until_lost, Clock, DurableError, HostId, LeaseEpoch, LeaseGuard,
    NodeId, PipelineKey, PipelineLeaseStore, PipelinePaths, PromoteError, Sleeper, LEASE_TIMEOUT,
};

use crate::cluster::catchup::catch_up_from_peers;
use crate::cluster::gossip::GossipService;
use crate::cluster::identity::ClusterIdentity;
use crate::cluster::peer::{assign_replica, query_status};
use crate::cluster::placement::{rank_replicas, ReplicaCandidate};

const REPLICA_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct PeerStatus {
    pub node_id: NodeId,
    pub catalog_seq_seen: u64,
    pub payload_ready: bool,
    pub endpoint: SocketAddr,
}

pub struct PromoteContext {
    pub paths: PipelinePaths,
    pub identity: ClusterIdentity,
    pub local_replica: SocketAddr,
    pub gossip: Arc<GossipService>,
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
            Some(PeerStatus {
                node_id: ad.node_id,
                catalog_seq_seen: status.catalog_seq_seen,
                payload_ready: status.payload_ready,
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

fn promote_durable_err(err: DurableError) -> PromoteError {
    PromoteError::Other(err.to_string())
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
    let donors: Vec<SocketAddr> = statuses.iter().map(|status| status.endpoint).collect();
    let missing = catch_up_from_peers(&key, &ctx.paths.segs, &donors, &ctx.identity)
        .await
        .map_err(promote_durable_err)?;
    if !missing.is_empty() {
        return Err(PromoteError::PayloadMissing {
            segment_ids: missing,
        });
    }
    let mut ranked = candidates;
    for candidate in &mut ranked {
        if let Some(status) = statuses.iter().find(|s| s.node_id == candidate.node_id) {
            candidate.endpoint = status.endpoint;
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
    let needs_initialize = !session.initialized;
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
    mut remote: crate::cluster::codec::proto::ReplicaStatus,
) -> Result<crate::cluster::codec::proto::ReplicaStatus, DurableError> {
    let wait_deadline = clock.monotonic_now().saturating_add(LEASE_TIMEOUT);
    while clock.monotonic_now() < wait_deadline {
        if remote.payload_ready {
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
    if !remote.payload_ready {
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
    fn payload_missing_lists_ids() {
        let err = PromoteError::PayloadMissing {
            segment_ids: vec!["a".into(), "b".into()],
        };
        let text = err.to_string();
        assert!(text.contains("a"));
        assert!(text.contains("b"));
        let _ = (NodeId::generate(), endpoint());
    }
}
