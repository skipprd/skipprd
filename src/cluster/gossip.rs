//! Live membership and fence rumors.
//!
//! Tables/DynamoDB membership rows are cold discovery only. Authenticated
//! Chitchat (WU-5.1) is the live transport. Gossip never grants a lease: a
//! fence rumor updates local [`LeaseGuard`]s, and suspicion starts the
//! observe-then-steal workflow.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use chitchat::transport::UdpTransport;
use chitchat::{spawn_chitchat, ChitchatConfig, ChitchatHandle, ChitchatId, FailureDetectorConfig};
use hmac::{Hmac, Mac};
use serde_derive::{Deserialize, Serialize};
use sha2::Sha256;
use skippr_lease::{
    ClusterId, HostId, LeaseEpoch, LeaseGuard, NodeId, PipelineKey, LEASE_TIMEOUT, PROTOCOL_MAX,
    PROTOCOL_MIN,
};
use tokio::sync::{broadcast, watch, RwLock};

use crate::cluster::membership::MembershipEndpoints;

static DIRECTORY: OnceLock<StdRwLock<Option<Arc<GossipService>>>> = OnceLock::new();

pub fn install_gossip(gossip: Arc<GossipService>) {
    *DIRECTORY
        .get_or_init(|| StdRwLock::new(None))
        .write()
        .expect("gossip directory") = Some(gossip);
}

pub fn gossip_directory() -> Option<Arc<GossipService>> {
    DIRECTORY.get()?.read().ok()?.clone()
}

pub fn clear_gossip() {
    if let Some(lock) = DIRECTORY.get() {
        *lock.write().expect("gossip directory") = None;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalHeadHint {
    pub pipeline: String,
    pub committed_index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GossipAd {
    #[serde(with = "serde_node")]
    pub node_id: NodeId,
    #[serde(with = "serde_host")]
    pub host_id: HostId,
    pub replica: SocketAddr,
    pub flight: SocketAddr,
    pub gossip: SocketAddr,
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub primary: Option<String>,
    pub ready: bool,
    #[serde(default)]
    pub heartbeat: u64,
    #[serde(default)]
    pub disk_pressure: bool,
    #[serde(default)]
    pub wal_heads: Vec<WalHeadHint>,
    /// This node's in-process Ballista scheduler bind (advertised IP).
    #[serde(default)]
    pub scheduler: Option<SocketAddr>,
}

impl GossipAd {
    pub fn for_node(
        node_id: NodeId,
        host_id: HostId,
        endpoints: MembershipEndpoints,
        ready: bool,
    ) -> Self {
        Self {
            node_id,
            host_id,
            replica: endpoints.replica,
            flight: endpoints.flight,
            gossip: endpoints.gossip,
            protocol_min: PROTOCOL_MIN,
            protocol_max: PROTOCOL_MAX,
            primary: None,
            ready,
            heartbeat: 0,
            disk_pressure: false,
            wal_heads: Vec::new(),
            scheduler: None,
        }
    }
}

/// Lowest `NodeId` among ads that published a scheduler, including self.
/// Replica `ready` is not a gate.
pub fn elect_scheduler(
    local_node: NodeId,
    local_scheduler: SocketAddr,
    ads: &[GossipAd],
) -> SocketAddr {
    elect_scheduler_live(local_node, local_scheduler, ads, |_| true)
}

/// Same as [`elect_scheduler`], but skip ads whose scheduler fails `live`.
/// The local scheduler is always eligible so a dead min-UUID peer cannot pin
/// the cluster to an unreachable address.
pub fn elect_scheduler_live(
    local_node: NodeId,
    local_scheduler: SocketAddr,
    ads: &[GossipAd],
    live: impl Fn(SocketAddr) -> bool,
) -> SocketAddr {
    let filtered: Vec<GossipAd> = ads
        .iter()
        .filter(|ad| {
            ad.scheduler
                .map(|addr| addr == local_scheduler || live(addr))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let mut winner_id = local_node.to_string();
    let mut winner_addr = local_scheduler;
    for ad in &filtered {
        let Some(scheduler) = ad.scheduler else {
            continue;
        };
        let id = ad.node_id.to_string();
        if id < winner_id {
            winner_id = id;
            winner_addr = scheduler;
        }
    }
    winner_addr
}

mod serde_node {
    use serde::{Deserialize, Deserializer, Serializer};
    use skippr_lease::NodeId;

    pub fn serialize<S: Serializer>(id: &NodeId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&id.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<NodeId, D::Error> {
        let raw = String::deserialize(d)?;
        uuid::Uuid::parse_str(&raw)
            .map(NodeId::from_uuid)
            .map_err(serde::de::Error::custom)
    }
}

mod serde_host {
    use serde::{Deserialize, Deserializer, Serializer};
    use skippr_lease::HostId;

    pub fn serialize<S: Serializer>(id: &HostId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(id.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<HostId, D::Error> {
        let raw = String::deserialize(d)?;
        HostId::new(raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum GossipMessage {
    Ad(GossipAd),
    Fence {
        tenant: String,
        workspace: String,
        pipeline: String,
        epoch: u64,
    },
    Suspect {
        node_id: String,
        #[serde(default)]
        pipeline: String,
    },
}

struct TrackedAd {
    ad: GossipAd,
    last_seen: Instant,
}

fn upsert_ad(map: &mut HashMap<String, TrackedAd>, ad: GossipAd) -> bool {
    let id = ad.node_id.to_string();
    let now = Instant::now();
    match map.get_mut(&id) {
        Some(existing) => {
            let changed = existing.ad != ad;
            if ad.heartbeat > existing.ad.heartbeat {
                existing.last_seen = now;
            }
            existing.ad = ad;
            changed
        }
        None => {
            map.insert(id, TrackedAd { ad, last_seen: now });
            true
        }
    }
}

fn live_ads(map: &HashMap<String, TrackedAd>) -> Vec<GossipAd> {
    map.values()
        .filter(|tracked| tracked.last_seen.elapsed() < LEASE_TIMEOUT)
        .map(|tracked| tracked.ad.clone())
        .collect()
}

pub struct GossipService {
    bind: SocketAddr,
    hmac_key: Vec<u8>,
    ads: Arc<RwLock<HashMap<String, TrackedAd>>>,
    handle: ChitchatHandle,
    fence_tx: broadcast::Sender<(PipelineKey, LeaseEpoch)>,
    suspect_tx: broadcast::Sender<SuspectRumor>,
    poll_stop: watch::Sender<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SuspectRumor {
    pub node_id: NodeId,
    pub pipeline: Option<String>,
}

const TEST_GOSSIP_HMAC_KEY: &[u8] = b"skippr-unit-test-gossip-hmac-key";

fn sign_payload(key: &[u8], payload: &str) -> Option<String> {
    if payload.is_empty() || key.is_empty() {
        return None;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(key).ok()?;
    mac.update(payload.as_bytes());
    Some(hex::encode(mac.finalize().into_bytes()))
}

fn payload_authentic(key: &[u8], payload: &str, hmac_hex: &str) -> bool {
    let Ok(expected) = hex::decode(hmac_hex) else {
        return false;
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(payload.as_bytes());
    mac.verify_slice(&expected).is_ok()
}

impl GossipService {
    pub async fn start(bind: SocketAddr) -> Result<Self, std::io::Error> {
        Self::start_with_seeds(bind, Vec::new()).await
    }

    pub async fn start_with_seeds(
        bind: SocketAddr,
        seeds: Vec<SocketAddr>,
    ) -> Result<Self, std::io::Error> {
        let advertise_ip = if bind.ip().is_unspecified() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            bind.ip()
        };
        Self::start_for_cluster(
            bind,
            seeds,
            ClusterId::new("skippr-test").expect("test cluster id"),
            NodeId::generate(),
            advertise_ip,
            TEST_GOSSIP_HMAC_KEY.to_vec(),
        )
        .await
    }

    pub async fn start_for_cluster(
        bind: SocketAddr,
        seeds: Vec<SocketAddr>,
        cluster_id: ClusterId,
        node_id: NodeId,
        advertise_ip: IpAddr,
        hmac_key: Vec<u8>,
    ) -> Result<Self, std::io::Error> {
        if hmac_key.iter().all(|b| b.is_ascii_whitespace()) || hmac_key.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SKIPPR_CLUSTER_GOSSIP_KEY is required",
            ));
        }
        if advertise_ip.is_unspecified() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "gossip advertise address is unspecified",
            ));
        }
        let probe = tokio::net::UdpSocket::bind(bind).await?;
        let local = probe.local_addr()?;
        drop(probe);
        let advertise = SocketAddr::new(advertise_ip, local.port());
        let chitchat_id = ChitchatId::new(node_id.to_string(), 0, advertise);
        let config = ChitchatConfig {
            chitchat_id,
            cluster_id: cluster_id.to_string(),
            gossip_interval: Duration::from_millis(50),
            listen_addr: advertise,
            seed_nodes: seeds.iter().map(ToString::to_string).collect(),
            failure_detector_config: FailureDetectorConfig {
                initial_interval: Duration::from_millis(50),
                ..FailureDetectorConfig::default()
            },
            marked_for_deletion_grace_period: Duration::from_secs(3_600),
            catchup_callback: None,
            extra_liveness_predicate: None,
        };
        let handle = spawn_chitchat(config, Vec::new(), &UdpTransport)
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        let ads = Arc::new(RwLock::new(HashMap::new()));
        let (fence_tx, _) = broadcast::channel(64);
        let (suspect_tx, _) = broadcast::channel(64);
        let (poll_stop, mut poll_rx) = watch::channel(false);
        let recv_ads = ads.clone();
        let recv_fence = fence_tx.clone();
        let recv_suspect = suspect_tx.clone();
        let chitchat = handle.chitchat();
        let watch_key = hmac_key.clone();
        tokio::spawn(async move {
            loop {
                if *poll_rx.borrow() {
                    break;
                }
                let mut pending_ads = Vec::new();
                let mut pending_fences = Vec::new();
                let mut pending_suspects = Vec::new();
                {
                    // Chitchat's phi detector withholds live_nodes until two
                    // heartbeats, so joiners are absent from live_nodes while
                    // already present in node_states. Dead KVs linger for the
                    // GC window; known_ads() drops them after LEASE_TIMEOUT.
                    let guard = chitchat.lock().await;
                    for state in guard.node_states().values() {
                        if let (Some(payload), Some(hmac)) = (state.get("ad"), state.get("ad_hmac"))
                        {
                            if payload_authentic(&watch_key, payload, hmac) {
                                pending_ads.push(payload.to_string());
                            }
                        }
                        if let (Some(payload), Some(hmac)) =
                            (state.get("fence"), state.get("fence_hmac"))
                        {
                            if payload_authentic(&watch_key, payload, hmac) {
                                pending_fences.push(payload.to_string());
                            }
                        }
                        if let (Some(payload), Some(hmac)) =
                            (state.get("suspect"), state.get("suspect_hmac"))
                        {
                            if payload_authentic(&watch_key, payload, hmac) {
                                pending_suspects.push(payload.to_string());
                            }
                        }
                    }
                }
                {
                    let mut ads = recv_ads.write().await;
                    for payload in pending_ads {
                        if let Ok(ad) = serde_json::from_str::<GossipAd>(&payload) {
                            upsert_ad(&mut ads, ad);
                        }
                    }
                }
                for payload in pending_fences {
                    if let Ok(message) = serde_json::from_str::<GossipMessage>(&payload) {
                        if let GossipMessage::Fence {
                            tenant,
                            workspace,
                            pipeline,
                            epoch,
                        } = message
                        {
                            if let Ok(key) = PipelineKey::new(tenant, workspace, pipeline) {
                                let _ = recv_fence.send((key, LeaseEpoch::new(epoch)));
                            }
                        }
                    }
                }
                for payload in pending_suspects {
                    if let Ok(message) = serde_json::from_str::<GossipMessage>(&payload) {
                        if let GossipMessage::Suspect { node_id, pipeline } = message {
                            if let Ok(uuid) = uuid::Uuid::parse_str(&node_id) {
                                let pipeline = if pipeline.is_empty() {
                                    None
                                } else {
                                    Some(pipeline)
                                };
                                let _ = recv_suspect.send(SuspectRumor {
                                    node_id: NodeId::from_uuid(uuid),
                                    pipeline,
                                });
                            }
                        }
                    }
                }
                tokio::select! {
                    _ = poll_rx.changed() => {
                        if *poll_rx.borrow() {
                            break;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
        });
        Ok(Self {
            bind: advertise,
            hmac_key,
            ads,
            handle,
            fence_tx,
            suspect_tx,
            poll_stop,
        })
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind
    }

    pub fn subscribe_fences(&self) -> broadcast::Receiver<(PipelineKey, LeaseEpoch)> {
        self.fence_tx.subscribe()
    }

    pub fn subscribe_suspects(&self) -> broadcast::Receiver<SuspectRumor> {
        self.suspect_tx.subscribe()
    }

    pub async fn add_seeds(&self, extra: Vec<SocketAddr>) {
        for addr in extra {
            let _ = self.handle.gossip(addr);
        }
    }

    pub async fn publish_ad(&self, ad: GossipAd) {
        {
            let mut ads = self.ads.write().await;
            upsert_ad(&mut ads, ad.clone());
        }
        let Ok(payload) = serde_json::to_string(&ad) else {
            return;
        };
        let Some(hmac) = sign_payload(&self.hmac_key, &payload) else {
            return;
        };
        {
            let chitchat = self.handle.chitchat();
            let mut guard = chitchat.lock().await;
            let state = guard.self_node_state();
            state.set("ad", payload);
            state.set("ad_hmac", hmac);
        }
        let _ = self.handle.gossip(ad.gossip);
    }

    pub async fn broadcast_fence(&self, pipeline: &PipelineKey, epoch: LeaseEpoch) {
        let message = GossipMessage::Fence {
            tenant: pipeline.tenant().to_string(),
            workspace: pipeline.workspace().to_string(),
            pipeline: pipeline.pipeline().to_string(),
            epoch: epoch.get(),
        };
        self.publish_control("fence", &message).await;
    }

    pub async fn broadcast_suspect(&self, node_id: NodeId, pipeline: Option<&PipelineKey>) {
        let message = GossipMessage::Suspect {
            node_id: node_id.to_string(),
            pipeline: pipeline
                .map(|key| key.pipeline().to_string())
                .unwrap_or_default(),
        };
        self.publish_control("suspect", &message).await;
    }

    async fn publish_control(&self, key: &str, message: &GossipMessage) {
        let Ok(payload) = serde_json::to_string(message) else {
            return;
        };
        let Some(hmac) = sign_payload(&self.hmac_key, &payload) else {
            return;
        };
        {
            let chitchat = self.handle.chitchat();
            let mut guard = chitchat.lock().await;
            let state = guard.self_node_state();
            state.set(key, payload);
            state.set(format!("{key}_hmac"), hmac);
        }
        for ad in live_ads(&*self.ads.read().await) {
            let _ = self.handle.gossip(ad.gossip);
        }
    }

    pub async fn apply_fence_rumor(guard: &LeaseGuard, epoch: LeaseEpoch) {
        guard.observe_epoch(epoch);
    }

    pub fn suspicion_starts_steal(message: &GossipMessage) -> bool {
        matches!(message, GossipMessage::Suspect { .. })
    }

    pub async fn known_ads(&self) -> Vec<GossipAd> {
        live_ads(&*self.ads.read().await)
    }

    pub async fn drain(&self) {
        let _ = self.poll_stop.send(true);
        self.handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::{LeaseSession, PipelineKey, SystemClock};
    use std::time::Duration;

    #[tokio::test]
    async fn fence_rumor_does_not_grant_a_lease() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let session = LeaseSession {
            owner: NodeId::generate(),
            epoch: skippr_lease::LeaseEpoch::new(1),
            heartbeat: 1,
            initialized: true,
            local_deadline: skippr_lease::MonoInstant::from_nanos(u64::MAX),
        };
        let guard = LeaseGuard::owner_elect(key, session.clone(), Arc::new(SystemClock::new()));
        guard.activate(session).unwrap();
        GossipService::apply_fence_rumor(&guard, skippr_lease::LeaseEpoch::new(2)).await;
        assert_eq!(guard.lifecycle(), skippr_lease::PipelineLifecycle::Fenced);
        assert!(matches!(guard.role(), skippr_lease::PipelineRole::Idle));
        let _ = Duration::from_secs(1);
    }

    #[test]
    fn suspicion_is_not_immediate_promotion() {
        let message = GossipMessage::Suspect {
            node_id: NodeId::generate().to_string(),
            pipeline: "events".into(),
        };
        assert!(GossipService::suspicion_starts_steal(&message));
        assert!(!matches!(message, GossipMessage::Fence { .. }));
    }

    #[test]
    fn gossip_ads_do_not_carry_replica_hints() {
        let ad = GossipAd::for_node(
            NodeId::generate(),
            HostId::new("host-a").unwrap(),
            crate::cluster::membership::MembershipEndpoints {
                replica: "127.0.0.1:1".parse().unwrap(),
                flight: "127.0.0.1:2".parse().unwrap(),
                gossip: "127.0.0.1:3".parse().unwrap(),
            },
            true,
        );
        let json = serde_json::to_string(&ad).unwrap();
        assert!(!json.contains("replica_hints"));
        let back: GossipAd = serde_json::from_str(&json).unwrap();
        assert_eq!(back.node_id, ad.node_id);
        assert_eq!(back.flight, ad.flight);
        assert_eq!(back.ready, ad.ready);
        assert!(back.wal_heads.is_empty());
        assert!(back.scheduler.is_none());
    }

    #[test]
    fn gossip_ads_round_trip_wal_heads_and_scheduler() {
        let mut ad = GossipAd::for_node(
            NodeId::generate(),
            HostId::new("host-a").unwrap(),
            crate::cluster::membership::MembershipEndpoints {
                replica: "127.0.0.1:1".parse().unwrap(),
                flight: "127.0.0.1:2".parse().unwrap(),
                gossip: "127.0.0.1:3".parse().unwrap(),
            },
            true,
        );
        ad.wal_heads = vec![WalHeadHint {
            pipeline: "events".into(),
            committed_index: 7,
        }];
        ad.scheduler = Some("127.0.0.1:4".parse().unwrap());
        let json = serde_json::to_string(&ad).unwrap();
        let back: GossipAd = serde_json::from_str(&json).unwrap();
        assert_eq!(back.wal_heads, ad.wal_heads);
        assert_eq!(back.scheduler, ad.scheduler);
    }

    fn node(n: u8) -> NodeId {
        NodeId::from_uuid(uuid::Uuid::from_u128(n as u128))
    }

    fn ad_with_scheduler(id: NodeId, scheduler: &str, ready: bool) -> GossipAd {
        let mut ad = GossipAd::for_node(
            id,
            HostId::new("host").unwrap(),
            crate::cluster::membership::MembershipEndpoints {
                replica: "127.0.0.1:1".parse().unwrap(),
                flight: "127.0.0.1:2".parse().unwrap(),
                gossip: "127.0.0.1:3".parse().unwrap(),
            },
            ready,
        );
        ad.scheduler = Some(scheduler.parse().unwrap());
        ad
    }

    #[test]
    fn elect_scheduler_picks_min_node_id() {
        let local = node(5);
        let local_sched: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let winner = elect_scheduler(
            local,
            local_sched,
            &[
                ad_with_scheduler(node(9), "10.0.0.9:9", true),
                ad_with_scheduler(node(2), "10.0.0.2:2", true),
            ],
        );
        assert_eq!(winner, "10.0.0.2:2".parse().unwrap());
    }

    #[test]
    fn elect_scheduler_skips_missing_scheduler() {
        let local = node(5);
        let local_sched: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let mut peer = ad_with_scheduler(node(1), "10.0.0.1:1", true);
        peer.scheduler = None;
        let winner = elect_scheduler(local, local_sched, &[peer]);
        assert_eq!(winner, local_sched);
    }

    #[test]
    fn elect_scheduler_lower_uuid_steals() {
        let local = node(5);
        let local_sched: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let first = elect_scheduler(local, local_sched, &[]);
        assert_eq!(first, local_sched);
        let stolen = elect_scheduler(
            local,
            local_sched,
            &[ad_with_scheduler(node(1), "10.0.0.1:1", true)],
        );
        assert_eq!(stolen, "10.0.0.1:1".parse().unwrap());
    }

    #[test]
    fn elect_scheduler_ignores_ready() {
        let local = node(5);
        let local_sched: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let winner = elect_scheduler(
            local,
            local_sched,
            &[ad_with_scheduler(node(1), "10.0.0.1:1", false)],
        );
        assert_eq!(winner, "10.0.0.1:1".parse().unwrap());
    }

    #[test]
    fn elect_scheduler_live_skips_unreachable_min_uuid() {
        let local = node(5);
        let local_sched: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let dead = ad_with_scheduler(node(1), "10.0.0.1:1", true);
        let peer = ad_with_scheduler(node(3), "10.0.0.3:3", true);
        let winner = elect_scheduler_live(local, local_sched, &[dead, peer], |addr| {
            addr == "10.0.0.3:3".parse().unwrap()
        });
        assert_eq!(winner, "10.0.0.3:3".parse().unwrap());
        let local_only = elect_scheduler_live(
            local,
            local_sched,
            &[ad_with_scheduler(node(3), "10.0.0.3:3", true)],
            |_| false,
        );
        assert_eq!(local_only, local_sched);
    }

    #[tokio::test]
    async fn two_nodes_exchange_ads() {
        let a = GossipService::start("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let b =
            GossipService::start_with_seeds("127.0.0.1:0".parse().unwrap(), vec![a.bind_addr()])
                .await
                .unwrap();
        b.publish_ad(GossipAd::for_node(
            NodeId::generate(),
            HostId::new("host-b").unwrap(),
            crate::cluster::membership::MembershipEndpoints {
                replica: "127.0.0.1:1".parse().unwrap(),
                flight: "127.0.0.1:2".parse().unwrap(),
                gossip: b.bind_addr(),
            },
            true,
        ))
        .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!a.known_ads().await.is_empty());
        a.drain().await;
        b.drain().await;
    }

    #[tokio::test]
    async fn unspecified_bind_advertises_loopback_not_unspecified() {
        let gossip = GossipService::start_for_cluster(
            "0.0.0.0:0".parse().unwrap(),
            Vec::new(),
            ClusterId::new("skippr-test").unwrap(),
            NodeId::generate(),
            "127.0.0.1".parse().unwrap(),
            TEST_GOSSIP_HMAC_KEY.to_vec(),
        )
        .await
        .unwrap();
        assert!(gossip.bind_addr().ip().is_loopback());
        assert!(!gossip.bind_addr().ip().is_unspecified());
        gossip.drain().await;
    }

    #[tokio::test]
    async fn new_ad_echoes_known_peers_back() {
        let a = GossipService::start("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let b = GossipService::start("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let a_id = NodeId::generate();
        let b_id = NodeId::generate();
        a.publish_ad(GossipAd::for_node(
            a_id,
            HostId::new("host-a").unwrap(),
            crate::cluster::membership::MembershipEndpoints {
                replica: "127.0.0.1:11".parse().unwrap(),
                flight: "127.0.0.1:12".parse().unwrap(),
                gossip: a.bind_addr(),
            },
            true,
        ))
        .await;
        b.add_seeds(vec![a.bind_addr()]).await;
        b.publish_ad(GossipAd::for_node(
            b_id,
            HostId::new("host-b").unwrap(),
            crate::cluster::membership::MembershipEndpoints {
                replica: "127.0.0.1:21".parse().unwrap(),
                flight: "127.0.0.1:22".parse().unwrap(),
                gossip: b.bind_addr(),
            },
            true,
        ))
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            a.known_ads().await.iter().any(|ad| ad.node_id == b_id),
            "a should learn b from the seed publish"
        );
        assert!(
            b.known_ads().await.iter().any(|ad| ad.node_id == a_id),
            "b should learn a from anti-entropy echo"
        );
        a.drain().await;
        b.drain().await;
    }

    #[tokio::test]
    async fn suspect_rumor_is_observed_not_promoted() {
        let a = GossipService::start("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let b =
            GossipService::start_with_seeds("127.0.0.1:0".parse().unwrap(), vec![a.bind_addr()])
                .await
                .unwrap();
        let mut rx = a.subscribe_suspects();
        let node = NodeId::generate();
        b.broadcast_suspect(node, None).await;
        let rumor = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rumor.node_id.to_string(), node.to_string());
        a.drain().await;
        b.drain().await;
    }

    #[tokio::test]
    async fn known_ads_omit_expired_heartbeats() {
        let gossip = GossipService::start("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let ad = GossipAd::for_node(
            NodeId::generate(),
            HostId::new("host-stale").unwrap(),
            crate::cluster::membership::MembershipEndpoints {
                replica: "127.0.0.1:1".parse().unwrap(),
                flight: "127.0.0.1:2".parse().unwrap(),
                gossip: gossip.bind_addr(),
            },
            true,
        );
        {
            let mut ads = gossip.ads.write().await;
            ads.insert(
                ad.node_id.to_string(),
                TrackedAd {
                    ad,
                    last_seen: Instant::now() - LEASE_TIMEOUT,
                },
            );
        }
        assert!(gossip.known_ads().await.is_empty());
        gossip.drain().await;
    }
}
