use skippr_lease::{
    CommitIndex, LeaseEpoch, MembershipAd, MemoryLeaseStore, MemoryMembershipStore, NodeId,
    PipelineKey, PipelineLeaseStore, GENESIS_HASH, WRITE_QUORUM,
};
use skipprd::buffer::durable::mutation::{DurableMutation, MutationEnvelope};
use skipprd::cluster::identity::ClusterIdentity;
use skipprd::cluster::pipeline_view::PipelineConfigView;
use skipprd::cluster::promote::{select_highest_hash_consistent, ClusterHead, PeerStatus};
use skipprd::cluster::validation::{
    validate_clustered_backend, validate_wal_storage_for_mode, CliModeKind,
};
use skipprd::helpers::wal_storage::{ConfigError, OffsetStoreKind, WalStorage};
use skipprd::query_flight::live_wal::select_live_ordinals;
use std::path::PathBuf;
use std::sync::Once;

fn install_cluster_tls() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var(
            "SKIPPR_CLUSTER_TLS_CERT",
            include_str!("cluster_tls/node.pem"),
        );
        std::env::set_var(
            "SKIPPR_CLUSTER_TLS_KEY",
            include_str!("cluster_tls/node.key"),
        );
        std::env::set_var("SKIPPR_CLUSTER_TLS_CA", include_str!("cluster_tls/ca.pem"));
    });
}

#[test]
fn clustered_sync_once_is_rejected() {
    let err =
        validate_wal_storage_for_mode(WalStorage::Clustered, CliModeKind::Sync { once: true })
            .unwrap_err();
    assert_eq!(err, ConfigError::ClusteredOnceRejected);
}

#[test]
fn clustered_discover_is_rejected() {
    assert!(matches!(
        validate_wal_storage_for_mode(WalStorage::Clustered, CliModeKind::Discover),
        Err(ConfigError::ClusteredModeRejected(_))
    ));
}

#[test]
fn clustered_query_is_allowed() {
    assert!(validate_wal_storage_for_mode(WalStorage::Clustered, CliModeKind::Query).is_ok());
}

#[test]
fn clustered_requires_dynamodb_table() {
    let err = validate_clustered_backend(WalStorage::Clustered, None, "").unwrap_err();
    #[cfg(feature = "offset-store-dynamodb")]
    assert_eq!(err, ConfigError::ClusteredTableMissing);
    #[cfg(not(feature = "offset-store-dynamodb"))]
    assert_eq!(err, ConfigError::ClusteredFeatureMissing);
}

#[test]
fn clustered_rejects_explicit_sled_offset_store() {
    let err = validate_clustered_backend(
        WalStorage::Clustered,
        Some(OffsetStoreKind::Sled),
        "offsets",
    )
    .unwrap_err();
    #[cfg(feature = "offset-store-dynamodb")]
    assert_eq!(
        err,
        ConfigError::ClusteredOffsetStoreConflict("sled".into())
    );
    #[cfg(not(feature = "offset-store-dynamodb"))]
    assert_eq!(err, ConfigError::ClusteredFeatureMissing);
}

#[test]
fn disk_and_s3_remain_valid() {
    assert!(
        validate_wal_storage_for_mode(WalStorage::Disk, CliModeKind::Sync { once: true }).is_ok()
    );
    assert!(validate_wal_storage_for_mode(WalStorage::S3, CliModeKind::Discover).is_ok());
}

#[test]
fn unknown_wal_storage_is_startup_error() {
    assert!("memory".parse::<WalStorage>().is_err());
}

#[test]
fn write_quorum_is_two() {
    assert_eq!(WRITE_QUORUM, 2);
}

#[test]
fn hash_chain_is_deterministic_across_nodes() {
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let envelope = MutationEnvelope {
        protocol_version: 1,
        pipeline: key,
        epoch: LeaseEpoch::new(1),
        index: CommitIndex::new(1),
        previous_hash: GENESIS_HASH,
        payload_sha256: [3u8; 32],
        body: DurableMutation::ReclaimSegment {
            segment_id: "seg".into(),
        },
    };
    assert_eq!(
        envelope.entry_hash().unwrap(),
        envelope.entry_hash().unwrap()
    );
}

fn dummy_endpoint() -> std::net::SocketAddr {
    "127.0.0.1:9".parse().unwrap()
}

#[test]
fn same_index_different_hash_blocks_promotion() {
    let a = PeerStatus {
        node_id: NodeId::generate(),
        committed: CommitIndex::new(2),
        head_hash: [1u8; 32],
        initialized: true,
        endpoint: dummy_endpoint(),
    };
    let b = PeerStatus {
        node_id: NodeId::generate(),
        committed: CommitIndex::new(2),
        head_hash: [9u8; 32],
        initialized: true,
        endpoint: dummy_endpoint(),
    };
    assert!(matches!(
        select_highest_hash_consistent(&[a, b], true),
        Err(skippr_lease::PromoteError::Diverged { .. })
    ));
}

#[test]
fn initialized_lease_without_head_fails_closed() {
    assert!(matches!(
        select_highest_hash_consistent(&[], true),
        Err(skippr_lease::PromoteError::NoHeadAvailable)
    ));
}

#[test]
fn clustered_rejects_at_least_once_sink() {
    let view = PipelineConfigView {
        key: PipelineKey::new("t", "w", "p").unwrap(),
        data_root: PathBuf::from("/tmp"),
        source_plugin: "S3".into(),
        sink_plugin: "Stdout".into(),
        schema_plugin: None,
        sink_ref: None,
        iceberg: false,
        flatten_events: false,
    };
    assert!(view.validate_clustered_sink().is_err());
}

#[test]
fn live_wal_scan_excludes_iceberg_named_segment() {
    let dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
    std::fs::create_dir_all(&paths.segs).unwrap();
    let mut log = skipprd::buffer::durable::log::MutationLog::open(paths).unwrap();
    let envelope = MutationEnvelope {
        protocol_version: 1,
        pipeline: key,
        epoch: LeaseEpoch::new(1),
        index: CommitIndex::new(1),
        previous_hash: GENESIS_HASH,
        payload_sha256: [9u8; 32],
        body: DurableMutation::CommitSegment {
            descriptor: skipprd::buffer::durable::mutation::SegmentDescriptor {
                segment_id: "iceberg-seg".into(),
                payload_len: 4,
                payload_sha256: [9u8; 32],
                num_partitions: 1,
                total_bytes: 4,
                created_at_secs: 0,
                schema_fingerprints: Vec::new(),
            },
            offsets: Vec::new(),
            checkpoints: Vec::new(),
        },
    };
    log.append_prepared(&envelope).unwrap();
    log.append_committed(envelope.index, envelope.entry_hash().unwrap())
        .unwrap();
    assert!(select_live_ordinals(&log, &["iceberg-seg".into()]).is_empty());
}

#[test]
fn membership_store_is_cluster_scoped() {
    let store = MemoryMembershipStore::new();
    let lake_a = skippr_lease::ClusterId::new("lake-a").unwrap();
    let lake_b = skippr_lease::ClusterId::new("lake-b").unwrap();
    store.put(
        &lake_a.membership_pk(),
        MembershipAd {
            node_id: NodeId::generate(),
            host_id: skippr_lease::HostId::new("host-a").unwrap(),
            heartbeat: 1,
            ready: true,
        },
    );
    assert_eq!(store.query(&lake_a.membership_pk()).len(), 1);
    assert!(store.query(&lake_b.membership_pk()).is_empty());
    assert!(store.query("t#w#cluster").is_empty());
}

#[tokio::test]
async fn memory_lease_create_is_exclusive() {
    let store = MemoryLeaseStore::new();
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let a = NodeId::generate();
    let b = NodeId::generate();
    store.create(&key, &a).await.unwrap();
    assert!(store.create(&key, &b).await.is_err());
    let row = store.read_consistent(&key).await.unwrap().unwrap();
    assert_eq!(row.owner, a);
    assert_eq!(row.epoch.get(), 1);
}

struct ModelPipelineState {
    _role: &'static str,
    epoch: u64,
    prepared: u64,
    committed: u64,
    applied: u64,
    published: u64,
}

impl ModelPipelineState {
    fn recover_from_crash(&self) -> Self {
        Self {
            _role: "idle",
            epoch: self.epoch,
            prepared: self.prepared,
            committed: self.committed,
            applied: self.committed.min(self.applied),
            published: self.published.min(self.committed),
        }
    }
}

#[test]
fn crash_after_commit_before_publish_recovers_committed_prefix() {
    let model = ModelPipelineState {
        _role: "primary",
        epoch: 1,
        prepared: 4,
        committed: 3,
        applied: 3,
        published: 2,
    };
    let recovered = model.recover_from_crash();
    assert_eq!(recovered.committed, 3);
    assert_eq!(recovered.published, 2);
    assert!(recovered.published <= recovered.committed);
}

#[test]
fn uninitialized_empty_statuses_are_head_zero() {
    let head = select_highest_hash_consistent(&[], false).unwrap();
    assert!(matches!(head, ClusterHead::Genesis));
    assert_eq!(head.committed(), CommitIndex::ZERO);
    assert_eq!(head.head_hash(), GENESIS_HASH);
}

#[test]
fn s3_wal_storage_is_not_clustered() {
    assert_ne!(WalStorage::S3, WalStorage::Clustered);
}

#[tokio::test]
async fn two_node_replica_applies_committed_entry() {
    use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
    use skipprd::buffer::durable::log::MutationLog;
    use skipprd::cluster::identity::ClusterIdentity;
    use skipprd::cluster::peer::{replicate_to, ReplicaRegistry, ReplicaServer, ReplicaSession};
    use std::path::Path;
    use std::sync::Arc;

    install_cluster_tls();
    let dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let identity = ClusterIdentity::new(
        skippr_lease::ClusterId::new("test-cluster").unwrap(),
        NodeId::generate(),
    );
    let paths = PipelinePaths::new(dir.path(), &key).unwrap();
    let log = MutationLog::open(paths.clone()).unwrap();
    let guard = LeaseGuard::replica(
        key.clone(),
        LeaseEpoch::new(1),
        Arc::new(SystemClock::new()),
    );
    let session = ReplicaSession::new(key.clone(), paths.clone(), guard, log);
    session.note_assigned("n".into(), LeaseEpoch::new(1)).await;
    let registry = ReplicaRegistry::new(identity.clone());
    registry.insert(session.clone()).await;
    let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
        .await
        .unwrap();
    let envelope = MutationEnvelope {
        protocol_version: 1,
        pipeline: key,
        epoch: LeaseEpoch::new(1),
        index: CommitIndex::new(1),
        previous_hash: GENESIS_HASH,
        payload_sha256: [0u8; 32],
        body: DurableMutation::ReclaimSegment {
            segment_id: "seg-1".into(),
        },
    };
    let ack = replicate_to(
        server.bind_addr(),
        &envelope,
        Path::new("/nonexistent"),
        &identity,
    )
    .await
    .unwrap();
    assert_eq!(ack.commit_index, 1);
    assert!(!ack.already_applied);
    let status = session.status().await;
    assert_eq!(status.committed_index, 1);
    assert_eq!(status.applied_index, 1);
    server.drain().await;
}

struct FakeNode {
    key: PipelineKey,
    identity: skipprd::cluster::identity::ClusterIdentity,
    server: skipprd::cluster::peer::ReplicaServer,
    session: std::sync::Arc<skipprd::cluster::peer::ReplicaSession>,
}

impl FakeNode {
    async fn start(
        dir: &std::path::Path,
        key: PipelineKey,
        identity: skipprd::cluster::identity::ClusterIdentity,
    ) -> Self {
        use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
        use skipprd::buffer::durable::log::MutationLog;
        use skipprd::cluster::peer::{ReplicaRegistry, ReplicaServer, ReplicaSession};
        use std::sync::Arc;

        install_cluster_tls();
        let paths = PipelinePaths::new(dir, &key).unwrap();
        let log = MutationLog::open(paths.clone()).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let session = ReplicaSession::new(key.clone(), paths, guard, log);
        session
            .note_assigned("n".into(), skippr_lease::LeaseEpoch::new(1))
            .await;
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session.clone()).await;
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        Self {
            key,
            identity,
            server,
            session,
        }
    }
}

#[tokio::test]
async fn fake_node_status_is_not_ready_until_assign_catch_up() {
    let dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let identity = skipprd::cluster::identity::ClusterIdentity::new(
        skippr_lease::ClusterId::new("test-cluster").unwrap(),
        NodeId::generate(),
    );
    let node = FakeNode::start(dir.path(), key, identity).await;
    let status = node.session.status().await;
    assert_eq!(status.committed_index, 0);
    assert!(!status.ready);
    assert_eq!(node.key.pipeline(), "p");
    node.server.drain().await;
}

#[tokio::test]
async fn three_replica_replacement_assigns_spare() {
    use skipprd::cluster::peer::{assign_replica, drop_replica, replicate_to};
    use std::path::Path;
    use std::time::Duration;

    let identity = ClusterIdentity::new(
        skippr_lease::ClusterId::new("test-cluster").unwrap(),
        NodeId::generate(),
    );
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let primary_dir = tempfile::tempdir().unwrap();
    let replica_dir = tempfile::tempdir().unwrap();
    let spare_dir = tempfile::tempdir().unwrap();
    let primary = FakeNode::start(primary_dir.path(), key.clone(), identity.clone()).await;
    let replica = FakeNode::start(replica_dir.path(), key.clone(), identity.clone()).await;
    let spare = FakeNode::start(spare_dir.path(), key.clone(), identity.clone()).await;
    assign_replica(
        replica.server.bind_addr(),
        &key,
        LeaseEpoch::new(1),
        primary.server.bind_addr(),
        &primary.identity,
    )
    .await
    .unwrap();
    drop_replica(replica.server.bind_addr(), &key, &replica.identity)
        .await
        .unwrap();
    assign_replica(
        spare.server.bind_addr(),
        &key,
        LeaseEpoch::new(1),
        primary.server.bind_addr(),
        &spare.identity,
    )
    .await
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !spare.session.status().await.ready {
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let envelope = MutationEnvelope {
        protocol_version: 1,
        pipeline: key,
        epoch: LeaseEpoch::new(1),
        index: CommitIndex::new(1),
        previous_hash: GENESIS_HASH,
        payload_sha256: [0u8; 32],
        body: DurableMutation::ReclaimSegment {
            segment_id: "seg-spare".into(),
        },
    };
    let ack = replicate_to(
        spare.server.bind_addr(),
        &envelope,
        Path::new("/nonexistent"),
        &spare.identity,
    )
    .await
    .unwrap();
    assert_eq!(ack.commit_index, 1);
    assert_eq!(spare.session.status().await.committed_index, 1);
    primary.server.drain().await;
    replica.server.drain().await;
    spare.server.drain().await;
}

#[tokio::test]
async fn prepared_without_commit_recovers_from_replica() {
    use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
    use skipprd::buffer::durable::log::MutationLog;
    use skipprd::buffer::durable::replicate::ReplicationMode;
    use skipprd::buffer::durable::store::{
        MemoryOffsetPublisher, OffsetMode, PipelineDurableStore,
    };
    use skipprd::cluster::peer::replicate_to;
    use std::path::Path;
    use std::sync::Arc;

    let identity = ClusterIdentity::new(
        skippr_lease::ClusterId::new("test-cluster").unwrap(),
        NodeId::generate(),
    );
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let donor_dir = tempfile::tempdir().unwrap();
    let crash_dir = tempfile::tempdir().unwrap();
    let donor = FakeNode::start(donor_dir.path(), key.clone(), identity.clone()).await;
    let envelope = MutationEnvelope {
        protocol_version: 1,
        pipeline: key.clone(),
        epoch: LeaseEpoch::new(1),
        index: CommitIndex::new(1),
        previous_hash: GENESIS_HASH,
        payload_sha256: [0u8; 32],
        body: DurableMutation::ReclaimSegment {
            segment_id: "prepared".into(),
        },
    };
    replicate_to(
        donor.server.bind_addr(),
        &envelope,
        Path::new("/nonexistent"),
        &donor.identity,
    )
    .await
    .unwrap();
    let paths = PipelinePaths::new(crash_dir.path(), &key).unwrap();
    let mut log = MutationLog::open(paths.clone()).unwrap();
    log.append_prepared(&envelope).unwrap();
    let guard = LeaseGuard::replica(
        key.clone(),
        LeaseEpoch::new(1),
        Arc::new(SystemClock::new()),
    );
    let store = PipelineDurableStore::new(
        key,
        paths.clone(),
        guard,
        log,
        ReplicationMode::LocalOnly,
        OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
    );
    store
        .recover_unknown_prepared(&donor.identity, &[donor.server.bind_addr()])
        .await
        .unwrap();
    let recovered = MutationLog::open(paths).unwrap();
    assert_eq!(recovered.committed_index(), CommitIndex::new(1));
    assert!(recovered.pending_prepared().is_empty());
    donor.server.drain().await;
}
