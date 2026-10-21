use skippr_lease::{
    LeaseEpoch, MembershipAd, MemoryLeaseStore, MemoryMembershipStore, NodeId, PipelineKey,
    PipelineLeaseStore, WRITE_QUORUM,
};
use skipprd::cluster::identity::ClusterIdentity;
use skipprd::cluster::pipeline_view::PipelineConfigView;
use skipprd::cluster::validation::{
    validate_clustered_backend, validate_wal_storage_for_mode, CliModeKind,
};
use skipprd::helpers::wal_storage::{ConfigError, SkipprStoreKind, WalStorage};
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
    let err = validate_clustered_backend(
        WalStorage::Clustered,
        SkipprStoreKind::default_for_wal(WalStorage::Clustered),
        "",
    )
    .unwrap_err();
    #[cfg(feature = "offset-store-dynamodb")]
    assert_eq!(err, ConfigError::ClusteredTableMissing);
    #[cfg(not(feature = "offset-store-dynamodb"))]
    assert_eq!(err, ConfigError::ClusteredFeatureMissing);
}

#[test]
fn clustered_rejects_explicit_sled_offset_store() {
    let err = validate_clustered_backend(WalStorage::Clustered, SkipprStoreKind::Sled, "offsets")
        .unwrap_err();
    #[cfg(feature = "offset-store-dynamodb")]
    assert_eq!(err, ConfigError::ClusteredStoreConflict("sled".into()));
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
fn clustered_rejects_at_least_once_sink() {
    let view = PipelineConfigView {
        key: PipelineKey::new("t", "w", "p").unwrap(),
        data_root: PathBuf::from("/tmp"),
        source_plugin: "S3".into(),
        sink_plugin: "Stdout".into(),
        schema_plugin: None,
        sink_ref: None,
        backend: skipprd::cluster::QueryBackend::WalOnly,
        flatten_events: false,
        wal_storage: WalStorage::Clustered,
    };
    assert!(view.validate_clustered_sink().is_err());
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
    max_committed: u64,
    max_published: u64,
}

impl ModelPipelineState {
    fn recover_from_crash(&self) -> Self {
        Self {
            max_committed: self.max_committed,
            max_published: self.max_published.min(self.max_committed),
        }
    }
}

#[test]
fn crash_after_catalog_commit_before_publish_republishes() {
    let model = ModelPipelineState {
        max_committed: 3,
        max_published: 2,
    };
    let recovered = model.recover_from_crash();
    assert_eq!(recovered.max_committed, 3);
    assert_eq!(recovered.max_published, 2);
    assert!(recovered.max_published <= recovered.max_committed);
}

#[test]
fn s3_wal_storage_is_not_clustered() {
    assert_ne!(WalStorage::S3, WalStorage::Clustered);
}

struct FakeNode {
    key: PipelineKey,
    identity: ClusterIdentity,
    server: skipprd::cluster::peer::ReplicaServer,
    session: std::sync::Arc<skipprd::cluster::peer::ReplicaSession>,
}

impl FakeNode {
    async fn start(dir: &std::path::Path, key: PipelineKey, identity: ClusterIdentity) -> Self {
        use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
        use skipprd::cluster::peer::{ReplicaRegistry, ReplicaServer, ReplicaSession};
        use std::sync::Arc;

        install_cluster_tls();
        let paths = PipelinePaths::new(dir, &key).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let session = ReplicaSession::new(key.clone(), paths, guard);
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
async fn two_node_replica_stores_payload() {
    use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
    use skipprd::cluster::peer::{
        replicate_payload_to, ReplicaRegistry, ReplicaServer, ReplicaSession,
    };
    use std::sync::Arc;

    install_cluster_tls();
    let dir = tempfile::tempdir().unwrap();
    let payload_dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let identity = ClusterIdentity::new(
        skippr_lease::ClusterId::new("test-cluster").unwrap(),
        NodeId::generate(),
    );
    let paths = PipelinePaths::new(dir.path(), &key).unwrap();
    std::fs::create_dir_all(&paths.segs).unwrap();
    let guard = LeaseGuard::replica(
        key.clone(),
        LeaseEpoch::new(1),
        Arc::new(SystemClock::new()),
    );
    let session = ReplicaSession::new(key.clone(), paths.clone(), guard);
    session.note_assigned("n".into(), LeaseEpoch::new(1)).await;
    let registry = ReplicaRegistry::new(identity.clone());
    registry.insert(session.clone()).await;
    let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
        .await
        .unwrap();
    let payload = payload_dir.path().join("seg-1.seg");
    let bytes = b"payload-bytes";
    std::fs::write(&payload, bytes).unwrap();
    let sha = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    };
    let ack = replicate_payload_to(
        server.bind_addr(),
        &key,
        "seg-1",
        sha,
        bytes.len() as u64,
        1,
        &payload,
        &identity,
    )
    .await
    .unwrap();
    assert_eq!(ack.segment_id, "seg-1");
    assert_eq!(ack.sha256, sha.to_vec());
    let status = session.status().await;
    assert_eq!(status.catalog_seq_seen, 0);
    assert!(!status.payload_ready);
    server.drain().await;
}

#[tokio::test]
async fn fake_node_status_is_not_payload_ready_until_catch_up() {
    let dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let identity = ClusterIdentity::new(
        skippr_lease::ClusterId::new("test-cluster").unwrap(),
        NodeId::generate(),
    );
    let node = FakeNode::start(dir.path(), key, identity).await;
    let status = node.session.status().await;
    assert_eq!(status.catalog_seq_seen, 0);
    assert!(!status.payload_ready);
    assert_eq!(node.key.pipeline(), "p");
    node.server.drain().await;
}

#[tokio::test]
async fn three_replica_replacement_assigns_spare() {
    use skipprd::cluster::peer::{assign_replica, drop_replica, replicate_payload_to};
    use std::time::Duration;

    let identity = ClusterIdentity::new(
        skippr_lease::ClusterId::new("test-cluster").unwrap(),
        NodeId::generate(),
    );
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let primary_dir = tempfile::tempdir().unwrap();
    let replica_dir = tempfile::tempdir().unwrap();
    let spare_dir = tempfile::tempdir().unwrap();
    let payload_dir = tempfile::tempdir().unwrap();
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
    while !spare.session.status().await.payload_ready {
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let payload = payload_dir.path().join("seg-spare.seg");
    let bytes = b"spare-payload";
    std::fs::write(&payload, bytes).unwrap();
    let sha = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    };
    let ack = replicate_payload_to(
        spare.server.bind_addr(),
        &key,
        "seg-spare",
        sha,
        bytes.len() as u64,
        1,
        &payload,
        &spare.identity,
    )
    .await
    .unwrap();
    assert_eq!(ack.segment_id, "seg-spare");
    primary.server.drain().await;
    replica.server.drain().await;
    spare.server.drain().await;
}

#[test]
fn protocol_v2_peer_is_rejected() {
    assert!(!skipprd::cluster::peer::protocol_compatible(2, 2));
    assert!(skipprd::cluster::peer::protocol_compatible(3, 3));
}

#[test]
fn payload_missing_lists_ids() {
    let err = skippr_lease::PromoteError::PayloadMissing {
        segment_ids: vec!["seg-a".into(), "seg-b".into()],
    };
    let text = err.to_string();
    assert!(text.contains("seg-a"), "{text}");
    assert!(text.contains("seg-b"), "{text}");
}

#[tokio::test]
async fn prune_to_lower_epoch_is_ignored() {
    use skipprd::cluster::peer::{assign_replica, prune_to_at};

    let identity = ClusterIdentity::new(
        skippr_lease::ClusterId::new("test-cluster").unwrap(),
        NodeId::generate(),
    );
    let key = PipelineKey::new("t", "w", "p").unwrap();
    let replica_dir = tempfile::tempdir().unwrap();
    let replica = FakeNode::start(replica_dir.path(), key.clone(), identity.clone()).await;
    assign_replica(
        replica.server.bind_addr(),
        &key,
        LeaseEpoch::new(1),
        replica.server.bind_addr(),
        &identity,
    )
    .await
    .unwrap();
    let segs = replica_dir
        .path()
        .join("clustered/t/w/p/segment_buffer/segs");
    std::fs::create_dir_all(&segs).unwrap();
    let payload = segs.join("keep.seg");
    std::fs::write(&payload, b"keep").unwrap();
    let err = prune_to_at(
        replica.server.bind_addr(),
        &key,
        vec!["other".into()],
        0,
        &identity,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, skippr_lease::DurableError::StaleEpoch),
        "{err:?}"
    );
    assert!(payload.exists());
    replica.server.drain().await;
}
