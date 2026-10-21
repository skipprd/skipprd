use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use skippr_lease::{
    race_deadline, Clock, DurableError, LeaseEpoch, LeaseGuard, PipelineKey, PipelinePaths,
    ReplicaNack, SegmentId, Sleeper, CONTROL_FRAME_MAX_BYTES, CURRENT_PROTOCOL,
    PAYLOAD_CHUNK_BYTES, PROTOCOL_MAX, PROTOCOL_MIN, RPC_IDLE_TIMEOUT,
};

use crate::buffer::wal_handle;
use crate::cluster::codec::{
    decode_control, encode_control, hash32, pipeline_from_proto, pipeline_to_proto, proto,
};
use crate::cluster::identity::ClusterIdentity;
use crate::cluster::tls::{self, MaybeTlsStream};
use std::sync::{OnceLock, RwLock as StdRwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex, RwLock};

static RPC_CLOCK: OnceLock<StdRwLock<Option<(Arc<dyn Clock>, Arc<dyn Sleeper>)>>> = OnceLock::new();
static PROCESS_REGISTRY: OnceLock<StdRwLock<Option<Arc<ReplicaRegistry>>>> = OnceLock::new();

pub fn install_rpc_clock(clock: Arc<dyn Clock>, sleeper: Arc<dyn Sleeper>) {
    *RPC_CLOCK
        .get_or_init(|| StdRwLock::new(None))
        .write()
        .expect("rpc clock poisoned") = Some((clock, sleeper));
}

pub fn install_process_registry(registry: Arc<ReplicaRegistry>) {
    *PROCESS_REGISTRY
        .get_or_init(|| StdRwLock::new(None))
        .write()
        .expect("process replica registry") = Some(registry);
}

pub fn process_registry() -> Option<Arc<ReplicaRegistry>> {
    PROCESS_REGISTRY.get()?.read().ok()?.clone()
}

pub fn clear_process_registry() {
    if let Some(lock) = PROCESS_REGISTRY.get() {
        *lock.write().expect("process replica registry") = None;
    }
}

async fn with_rpc_timeout<T>(
    duration: std::time::Duration,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, ()> {
    let clocks = RPC_CLOCK
        .get_or_init(|| StdRwLock::new(None))
        .read()
        .expect("rpc clock poisoned")
        .clone();
    if let Some((clock, sleeper)) = clocks {
        race_deadline(sleeper.as_ref(), clock.as_ref(), duration, fut).await
    } else {
        tokio::time::timeout(duration, fut).await.map_err(|_| ())
    }
}

#[derive(Clone, Debug)]
pub struct PeerEndpoint {
    pub replica: SocketAddr,
    pub flight: SocketAddr,
    pub gossip: SocketAddr,
}

pub struct ReplicaSession {
    pub key: PipelineKey,
    pub paths: PipelinePaths,
    pub guard: Arc<LeaseGuard>,
    pub assigned_primary: Mutex<Option<(String, LeaseEpoch)>>,
    pub ready: AtomicBool,
}

impl ReplicaSession {
    pub fn new(key: PipelineKey, paths: PipelinePaths, guard: Arc<LeaseGuard>) -> Arc<Self> {
        Arc::new(Self {
            key,
            paths,
            guard,
            assigned_primary: Mutex::new(None),
            ready: AtomicBool::new(false),
        })
    }

    pub async fn note_assigned(&self, primary: String, epoch: LeaseEpoch) {
        self.guard.observe_epoch(epoch);
        *self.assigned_primary.lock().await = Some((primary, epoch));
    }

    pub async fn status(&self) -> proto::ReplicaStatus {
        let (catalog_seq_seen, payload_ready) = match wal_handle::wal_handle_for(&self.key) {
            Some(handle) => (
                handle.catalog_seq_seen().unwrap_or(0),
                handle.payload_ready().unwrap_or(false) && self.ready.load(Ordering::SeqCst),
            ),
            None => (0, false),
        };
        proto::ReplicaStatus {
            epoch_seen: epoch_seen_for_rpc(self),
            catalog_seq_seen,
            payload_ready,
            pipeline: Some(pipeline_to_proto(&self.key)),
        }
    }
}

fn epoch_seen_for_rpc(session: &ReplicaSession) -> u64 {
    match session.guard.role() {
        skippr_lease::PipelineRole::Replica { epoch_seen } => epoch_seen.get(),
        skippr_lease::PipelineRole::ActivePrimary(skippr_lease::WriteAuthority::Leased(s))
        | skippr_lease::PipelineRole::OwnerElect(s) => s.epoch.get(),
        _ => 0,
    }
}

fn replica_paths(session: &ReplicaSession) -> PipelinePaths {
    session.paths.clone()
}

#[derive(Clone)]
pub struct ReplicaRegistry {
    pub identity: ClusterIdentity,
    sessions: Arc<RwLock<HashMap<PipelineKey, Arc<ReplicaSession>>>>,
}

impl ReplicaRegistry {
    pub fn new(identity: ClusterIdentity) -> Arc<Self> {
        Arc::new(Self {
            identity,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub async fn insert(&self, session: Arc<ReplicaSession>) {
        self.sessions
            .write()
            .await
            .insert(session.key.clone(), session);
    }

    pub async fn get(&self, key: &PipelineKey) -> Option<Arc<ReplicaSession>> {
        self.sessions.read().await.get(key).cloned()
    }

    pub async fn snapshot(&self) -> Vec<Arc<ReplicaSession>> {
        self.sessions.read().await.values().cloned().collect()
    }

    pub async fn remove(&self, key: &PipelineKey) -> Option<Arc<ReplicaSession>> {
        self.sessions.write().await.remove(key)
    }

    pub async fn advertised_ready(&self) -> bool {
        if wal_handle::all_wal_handles().iter().any(|handle| {
            matches!(
                handle.guard().role(),
                skippr_lease::PipelineRole::ActivePrimary(_)
            )
        }) {
            return true;
        }
        let sessions = self.sessions.read().await;
        if sessions.is_empty() {
            return false;
        }
        let mut ready = true;
        for session in sessions.values() {
            let status = session.status().await;
            ready &= status.payload_ready;
        }
        ready
    }
}

pub async fn write_frame(stream: &mut MaybeTlsStream, payload: &[u8]) -> std::io::Result<()> {
    with_rpc_timeout(RPC_IDLE_TIMEOUT, write_frame_inner(stream, payload))
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "replica RPC idle timeout")
        })?
}

async fn write_frame_inner(stream: &mut MaybeTlsStream, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > CONTROL_FRAME_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "control frame exceeds 16 MiB",
        ));
    }
    stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .await?;
    stream.write_all(payload).await?;
    Ok(())
}

pub async fn read_frame(stream: &mut MaybeTlsStream) -> std::io::Result<Vec<u8>> {
    with_rpc_timeout(RPC_IDLE_TIMEOUT, read_frame_inner(stream))
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "replica RPC idle timeout")
        })?
}

async fn read_frame_inner(stream: &mut MaybeTlsStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > CONTROL_FRAME_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "control frame exceeds 16 MiB",
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn connect_replica(endpoint: SocketAddr) -> Result<MaybeTlsStream, DurableError> {
    let stream = with_rpc_timeout(RPC_IDLE_TIMEOUT, TcpStream::connect(endpoint))
        .await
        .map_err(|_| DurableError::Io("replica connect timeout".into()))?
        .map_err(|err| DurableError::Io(err.to_string()))?;
    tls::connect(stream)
        .await
        .map_err(|err| DurableError::Io(err.to_string()))
}

fn rpc_io_error(err: std::io::Error) -> DurableError {
    if err.kind() == std::io::ErrorKind::TimedOut {
        DurableError::Timeout
    } else {
        DurableError::QuorumLost(err.to_string())
    }
}

pub async fn stream_payload_to_path(
    stream: &mut MaybeTlsStream,
    total: u64,
    dest: &Path,
) -> Result<[u8; 32], DurableError> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let tmp = dest.with_extension("part");
    let _ = tokio::fs::remove_file(&tmp).await;
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut remaining = total;
    let mut buf = vec![0u8; PAYLOAD_CHUNK_BYTES];
    while remaining > 0 {
        let n = remaining.min(PAYLOAD_CHUNK_BYTES as u64) as usize;
        stream.read_exact(&mut buf[..n]).await?;
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).await?;
        remaining -= n as u64;
    }
    file.sync_all().await?;
    tokio::fs::rename(&tmp, dest).await?;
    if let Some(parent) = dest.parent() {
        crate::helpers::fsync::fsync_dir(parent)?;
    }
    Ok(hasher.finalize().into())
}

async fn sha256_path(path: &Path) -> Result<[u8; 32], DurableError> {
    use sha2::{Digest, Sha256};
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; PAYLOAD_CHUNK_BYTES];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

pub async fn stream_payload_from_path(
    stream: &mut MaybeTlsStream,
    path: &Path,
) -> Result<(), DurableError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; PAYLOAD_CHUNK_BYTES];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n]).await?;
    }
    Ok(())
}

pub fn protocol_compatible(peer_min: u32, peer_max: u32) -> bool {
    let lo = PROTOCOL_MIN.max(peer_min);
    let hi = PROTOCOL_MAX.min(peer_max);
    lo <= hi
}

fn accept_hello_ok(
    ok: &proto::ServerHello,
    identity: &ClusterIdentity,
) -> Result<(), DurableError> {
    if ok.cluster_hash != identity.hash() {
        return Err(DurableError::ProtocolMismatch(
            "hello cluster_hash mismatch".into(),
        ));
    }
    if ok.protocol < PROTOCOL_MIN || ok.protocol > PROTOCOL_MAX {
        return Err(DurableError::ProtocolMismatch(
            "hello protocol out of range".into(),
        ));
    }
    Ok(())
}

pub struct ReplicaServer {
    bind: SocketAddr,
    stop: watch::Sender<bool>,
    registry: Arc<ReplicaRegistry>,
}

impl ReplicaServer {
    pub async fn start_with_registry(
        bind: SocketAddr,
        registry: Arc<ReplicaRegistry>,
    ) -> Result<Self, DurableError> {
        let _ = crate::cluster::tls::server_config()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let local = listener
            .local_addr()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let (stop, mut rx) = watch::channel(false);
        let serve_registry = registry.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        if *rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break; };
                        let registry = serve_registry.clone();
                        tokio::spawn(async move {
                            let Ok(mut stream) = tls::accept(stream).await else { return; };
                            let _ = serve_replica_conn(&mut stream, &registry).await;
                        });
                    }
                }
            }
        });
        Ok(Self {
            bind: local,
            stop,
            registry,
        })
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind
    }

    pub fn registry(&self) -> &Arc<ReplicaRegistry> {
        &self.registry
    }

    pub async fn drain(&self) {
        let _ = self.stop.send_replace(true);
    }
}

async fn serve_replica_conn(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
) -> Result<(), DurableError> {
    let hello_bytes = read_frame(stream).await?;
    let hello = decode_control(&hello_bytes)?;
    let Some(proto::control_frame::Msg::Hello(hello)) = hello.msg else {
        return Err(DurableError::ProtocolMismatch(
            "expected ClientHello".into(),
        ));
    };
    if hello.cluster_hash != registry.identity.hash() {
        tracing::warn!("replica handshake rejected: cluster_hash does not match");
        return Err(DurableError::ProtocolMismatch(
            "cluster_hash does not match cluster identity".into(),
        ));
    }
    if !protocol_compatible(hello.protocol_min, hello.protocol_max) {
        tracing::warn!("replica handshake rejected: protocol range does not overlap");
        return Err(DurableError::ProtocolMismatch(
            "protocol range does not overlap".into(),
        ));
    }
    let reply = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::HelloOk(proto::ServerHello {
            cluster_hash: registry.identity.hash(),
            node_id: registry.identity.node_label(),
            protocol: CURRENT_PROTOCOL,
        })),
    };
    write_frame(stream, &encode_control(&reply)?).await?;
    loop {
        let frame = match read_frame(stream).await {
            Ok(frame) => frame,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let control = decode_control(&frame)?;
        match control.msg {
            Some(proto::control_frame::Msg::Status(status)) => {
                let key = pipeline_from_proto(status.pipeline.as_ref())?;
                let session = registry.get(&key).await.ok_or_else(|| {
                    DurableError::ProtocolMismatch(format!("unknown pipeline {}", key.pipeline()))
                })?;
                let reply = proto::ControlFrame {
                    msg: Some(proto::control_frame::Msg::StatusOk(session.status().await)),
                };
                write_frame(stream, &encode_control(&reply)?).await?;
            }
            Some(proto::control_frame::Msg::Replicate(req)) => {
                handle_replicate(stream, registry, req).await?;
            }
            Some(proto::control_frame::Msg::Assign(assign)) => {
                handle_assign(stream, registry, assign).await?;
            }
            Some(proto::control_frame::Msg::FetchPayload(fetch)) => {
                handle_fetch_payload(stream, registry, fetch).await?;
            }
            Some(proto::control_frame::Msg::DropPayload(drop)) => {
                handle_drop_payload(stream, registry, drop).await?;
            }
            Some(proto::control_frame::Msg::PruneTo(prune)) => {
                handle_prune_to(stream, registry, prune).await?;
            }
            Some(proto::control_frame::Msg::Drop(drop)) => {
                let key = pipeline_from_proto(drop.pipeline.as_ref())?;
                if let Some(session) = registry.remove(&key).await {
                    purge_replica_pipeline(&session).await;
                }
                write_ack(stream, "", [0u8; 32]).await?;
            }
            _ => return Ok(()),
        }
    }
}

async fn purge_replica_pipeline(session: &ReplicaSession) {
    tracing::warn!(
        pipeline = %session.key.pipeline(),
        "purged replica pipeline"
    );
    session.ready.store(false, Ordering::SeqCst);
    let _ = tokio::fs::remove_dir_all(&session.paths.segs).await;
}

async fn session_for_payload(
    registry: &ReplicaRegistry,
    pipeline: Option<&proto::PipelineKey>,
) -> Result<Arc<ReplicaSession>, ReplicaNack> {
    let Ok(key) = pipeline_from_proto(pipeline) else {
        return Err(ReplicaNack::UnknownPipeline);
    };
    registry.get(&key).await.ok_or(ReplicaNack::UnknownPipeline)
}

async fn handle_replicate(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    req: proto::ReplicatePayload,
) -> Result<(), DurableError> {
    let session = match session_for_payload(registry, req.pipeline.as_ref()).await {
        Ok(session) => session,
        Err(nack) => return write_nack(stream, nack).await,
    };
    let epoch = LeaseEpoch::new(req.epoch);
    if session.guard.reject_stale_epoch(epoch).is_err() {
        return write_nack(stream, ReplicaNack::StaleEpoch).await;
    }
    let assigned_epoch = {
        let assigned = session.assigned_primary.lock().await;
        match assigned.as_ref() {
            Some((_, assigned_epoch)) => *assigned_epoch,
            None => return write_nack(stream, ReplicaNack::NotAssigned).await,
        }
    };
    if epoch != assigned_epoch {
        return write_nack(stream, ReplicaNack::StaleEpoch).await;
    }
    let id = SegmentId::new(&req.segment_id).map_err(|err| DurableError::Io(err.to_string()))?;
    let paths = replica_paths(&session);
    let dest = paths.segment(&id);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if crate::cluster::disk::disk_under_pressure(&paths.root) && req.len == 0 {
        return write_nack(stream, ReplicaNack::DiskFull).await;
    }
    if req.len > 0 {
        let hash = match stream_payload_to_path(stream, req.len, &dest).await {
            Ok(hash) => hash,
            Err(DurableError::DiskFull) => return write_nack(stream, ReplicaNack::DiskFull).await,
            Err(err) => return Err(err),
        };
        let expected = hash32(&req.sha256)?;
        if hash != expected {
            let _ = tokio::fs::remove_file(&dest).await;
            return write_nack(stream, ReplicaNack::PayloadHashMismatch).await;
        }
    }
    if crate::cluster::disk::disk_under_pressure(&session.paths.root) {
        return write_nack(stream, ReplicaNack::DiskFull).await;
    }
    crate::cluster::failpoint::hit(
        crate::cluster::failpoint::FailpointName::AfterReplicaPayloadAck,
        &session.paths.root,
    )?;
    let sha = hash32(&req.sha256).unwrap_or([0u8; 32]);
    write_ack(stream, &req.segment_id, sha).await
}

async fn handle_assign(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    assign: proto::AssignReplica,
) -> Result<(), DurableError> {
    let key = pipeline_from_proto(assign.pipeline.as_ref())?;
    let Some(session) = registry.get(&key).await else {
        return write_nack(stream, ReplicaNack::UnknownPipeline).await;
    };
    let Ok(endpoint) = assign.primary_endpoint.parse::<SocketAddr>() else {
        session.ready.store(false, Ordering::SeqCst);
        return write_nack(stream, ReplicaNack::InvalidPrimaryEndpoint).await;
    };
    let epoch = LeaseEpoch::new(assign.epoch);
    let assignment = (assign.primary_node.clone(), epoch);
    {
        let current = session.assigned_primary.lock().await;
        if current.as_ref() == Some(&assignment) && session.ready.load(Ordering::SeqCst) {
            session.guard.observe_epoch(epoch);
            write_ack(stream, "", [0u8; 32]).await?;
            return Ok(());
        }
    }
    session
        .note_assigned(assign.primary_node.clone(), epoch)
        .await;
    session.ready.store(false, Ordering::SeqCst);
    write_ack(stream, "", [0u8; 32]).await?;
    let identity = registry.identity.clone();
    tokio::spawn(async move {
        let result =
            crate::cluster::catchup::catch_up_payloads(&session, endpoint, &identity).await;
        session.ready.store(result.is_ok(), Ordering::SeqCst);
        if let Err(err) = result {
            tracing::warn!(error = %err, "assigned replica payload catch-up failed");
        } else {
            tracing::info!(
                pipeline = %session.key.pipeline(),
                "assigned replica payload catch-up complete"
            );
        }
    });
    Ok(())
}

async fn handle_fetch_payload(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    fetch: proto::FetchPayload,
) -> Result<(), DurableError> {
    let session = match session_for_payload(registry, fetch.pipeline.as_ref()).await {
        Ok(session) => session,
        Err(nack) => return write_nack(stream, nack).await,
    };
    let id = SegmentId::new(&fetch.segment_id).map_err(|err| DurableError::Io(err.to_string()))?;
    let path = replica_paths(session.as_ref()).segment(&id);
    if !path.exists() {
        return write_nack(stream, ReplicaNack::NotCaughtUp).await;
    }
    let sha = sha256_path(&path).await?;
    let len = tokio::fs::metadata(&path).await?.len();
    let reply = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::FetchPayloadOk(
            proto::FetchPayloadResponse {
                segment_id: fetch.segment_id.clone(),
                sha256: sha.to_vec(),
                len,
            },
        )),
    };
    write_frame(stream, &encode_control(&reply)?).await?;
    stream_payload_from_path(stream, &path).await
}

async fn handle_drop_payload(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    drop: proto::DropPayload,
) -> Result<(), DurableError> {
    let session = match session_for_payload(registry, drop.pipeline.as_ref()).await {
        Ok(session) => session,
        Err(nack) => return write_nack(stream, nack).await,
    };
    let epoch = LeaseEpoch::new(drop.epoch);
    if session.guard.reject_stale_epoch(epoch).is_err() {
        return write_nack(stream, ReplicaNack::StaleEpoch).await;
    }
    if let Ok(id) = SegmentId::new(&drop.segment_id) {
        let path = replica_paths(session.as_ref()).segment(&id);
        let _ = tokio::fs::remove_file(path).await;
    }
    write_ack(stream, &drop.segment_id, [0u8; 32]).await
}

async fn handle_prune_to(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    prune: proto::PruneTo,
) -> Result<(), DurableError> {
    let session = match session_for_payload(registry, prune.pipeline.as_ref()).await {
        Ok(session) => session,
        Err(nack) => return write_nack(stream, nack).await,
    };
    let epoch = LeaseEpoch::new(prune.epoch);
    if session.guard.reject_stale_epoch(epoch).is_err() {
        return write_nack(stream, ReplicaNack::StaleEpoch).await;
    }
    let live: std::collections::HashSet<&str> =
        prune.live_segment_ids.iter().map(String::as_str).collect();
    let segs = replica_paths(session.as_ref()).segs;
    if segs.exists() {
        let mut dir = tokio::fs::read_dir(&segs).await?;
        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("seg") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if !live.contains(id) {
                let _ = tokio::fs::remove_file(path).await;
            }
        }
    }
    write_ack(stream, "", [0u8; 32]).await
}

async fn write_ack(
    stream: &mut MaybeTlsStream,
    segment_id: &str,
    sha256: [u8; 32],
) -> Result<(), DurableError> {
    let reply = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Ack(proto::ReplicaAck {
            segment_id: segment_id.to_string(),
            sha256: sha256.to_vec(),
        })),
    };
    write_frame(stream, &encode_control(&reply)?).await?;
    Ok(())
}

async fn write_nack(stream: &mut MaybeTlsStream, nack: ReplicaNack) -> Result<(), DurableError> {
    let reply = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Nack(proto::ReplicaNack {
            code: replica_nack_code(nack) as i32,
        })),
    };
    write_frame(stream, &encode_control(&reply)?).await?;
    Ok(())
}

fn replica_nack_code(nack: ReplicaNack) -> proto::ReplicaNackCode {
    match nack {
        ReplicaNack::StaleEpoch => proto::ReplicaNackCode::StaleEpoch,
        ReplicaNack::NotAssigned => proto::ReplicaNackCode::NotAssigned,
        ReplicaNack::NotCaughtUp => proto::ReplicaNackCode::NotCaughtUp,
        ReplicaNack::DiskFull => proto::ReplicaNackCode::DiskFull,
        ReplicaNack::PayloadHashMismatch => proto::ReplicaNackCode::PayloadHashMismatch,
        ReplicaNack::UnknownPipeline => proto::ReplicaNackCode::UnknownPipeline,
        ReplicaNack::InvalidPrimaryEndpoint => proto::ReplicaNackCode::InvalidPrimaryEndpoint,
    }
}

fn replica_nack_from_proto(nack: &proto::ReplicaNack) -> Result<ReplicaNack, DurableError> {
    match proto::ReplicaNackCode::try_from(nack.code) {
        Ok(proto::ReplicaNackCode::StaleEpoch) => Ok(ReplicaNack::StaleEpoch),
        Ok(proto::ReplicaNackCode::NotAssigned) => Ok(ReplicaNack::NotAssigned),
        Ok(proto::ReplicaNackCode::NotCaughtUp) => Ok(ReplicaNack::NotCaughtUp),
        Ok(proto::ReplicaNackCode::DiskFull) => Ok(ReplicaNack::DiskFull),
        Ok(proto::ReplicaNackCode::PayloadHashMismatch) => Ok(ReplicaNack::PayloadHashMismatch),
        Ok(proto::ReplicaNackCode::UnknownPipeline) => Ok(ReplicaNack::UnknownPipeline),
        Ok(proto::ReplicaNackCode::InvalidPrimaryEndpoint) => {
            Ok(ReplicaNack::InvalidPrimaryEndpoint)
        }
        Ok(proto::ReplicaNackCode::Unspecified) | Err(_) => Err(DurableError::ProtocolMismatch(
            "unspecified replica nack".into(),
        )),
    }
}

fn replica_nack_error(nack: &proto::ReplicaNack) -> DurableError {
    match replica_nack_from_proto(nack) {
        Ok(code) => code.into_durable(),
        Err(err) => err,
    }
}

async fn handshake(
    stream: &mut MaybeTlsStream,
    identity: &ClusterIdentity,
) -> Result<proto::ServerHello, DurableError> {
    let hello = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Hello(proto::ClientHello {
            cluster_hash: identity.hash(),
            node_id: identity.node_label(),
            protocol_min: PROTOCOL_MIN,
            protocol_max: PROTOCOL_MAX,
        })),
    };
    write_frame(stream, &encode_control(&hello)?)
        .await
        .map_err(rpc_io_error)?;
    let reply = read_frame(stream).await.map_err(rpc_io_error)?;
    match decode_control(&reply)?.msg {
        Some(proto::control_frame::Msg::HelloOk(ok)) => {
            accept_hello_ok(&ok, identity)?;
            Ok(ok)
        }
        _ => Err(DurableError::QuorumLost("expected ServerHello".into())),
    }
}

pub async fn replicate_payload_to(
    endpoint: SocketAddr,
    key: &PipelineKey,
    segment_id: &str,
    sha256: [u8; 32],
    len: u64,
    epoch: u64,
    payload: &Path,
    identity: &ClusterIdentity,
) -> Result<proto::ReplicaAck, DurableError> {
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let req = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Replicate(
            proto::ReplicatePayload {
                segment_id: segment_id.to_string(),
                sha256: sha256.to_vec(),
                len,
                epoch,
                pipeline: Some(pipeline_to_proto(key)),
            },
        )),
    };
    write_frame(&mut stream, &encode_control(&req)?)
        .await
        .map_err(rpc_io_error)?;
    if len > 0 && payload.exists() {
        stream_payload_from_path(&mut stream, payload).await?;
    }
    let ack_bytes = read_frame(&mut stream).await.map_err(rpc_io_error)?;
    match decode_control(&ack_bytes)?.msg {
        Some(proto::control_frame::Msg::Ack(ack)) => Ok(ack),
        Some(proto::control_frame::Msg::Nack(nack)) => Err(replica_nack_error(&nack)),
        _ => Err(DurableError::QuorumLost("replica nack".into())),
    }
}

pub async fn query_status(
    endpoint: SocketAddr,
    key: &PipelineKey,
    identity: &ClusterIdentity,
) -> Result<proto::ReplicaStatus, DurableError> {
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let req = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Status(proto::ReplicaStatus {
            epoch_seen: 0,
            catalog_seq_seen: 0,
            payload_ready: false,
            pipeline: Some(pipeline_to_proto(key)),
        })),
    };
    write_frame(&mut stream, &encode_control(&req)?)
        .await
        .map_err(rpc_io_error)?;
    let reply = read_frame(&mut stream).await.map_err(rpc_io_error)?;
    match decode_control(&reply)?.msg {
        Some(proto::control_frame::Msg::StatusOk(status)) => Ok(status),
        other => Err(DurableError::QuorumLost(format!(
            "expected StatusOk, got {other:?}"
        ))),
    }
}

pub async fn fetch_payload_from(
    endpoint: SocketAddr,
    key: &PipelineKey,
    segment_id: &str,
    dest: &Path,
    identity: &ClusterIdentity,
) -> Result<[u8; 32], DurableError> {
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let req = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::FetchPayload(
            proto::FetchPayload {
                segment_id: segment_id.to_string(),
                pipeline: Some(pipeline_to_proto(key)),
            },
        )),
    };
    write_frame(&mut stream, &encode_control(&req)?)
        .await
        .map_err(rpc_io_error)?;
    let reply = read_frame(&mut stream).await.map_err(rpc_io_error)?;
    match decode_control(&reply)?.msg {
        Some(proto::control_frame::Msg::FetchPayloadOk(ok)) => {
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let hash = stream_payload_to_path(&mut stream, ok.len, dest).await?;
            let expected = hash32(&ok.sha256)?;
            if hash != expected {
                let _ = tokio::fs::remove_file(dest).await;
                return Err(DurableError::ProtocolMismatch(
                    "fetched payload hash mismatch".into(),
                ));
            }
            crate::metrics::counters::add_cluster_catchup_bytes(ok.len);
            Ok(hash)
        }
        Some(proto::control_frame::Msg::Nack(nack)) => Err(replica_nack_error(&nack)),
        other => Err(DurableError::QuorumLost(format!("{other:?}"))),
    }
}

pub async fn drop_payload_at(
    endpoint: SocketAddr,
    key: &PipelineKey,
    segment_id: &str,
    epoch: u64,
    identity: &ClusterIdentity,
) -> Result<(), DurableError> {
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let req = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::DropPayload(proto::DropPayload {
            segment_id: segment_id.to_string(),
            epoch,
            pipeline: Some(pipeline_to_proto(key)),
        })),
    };
    write_frame(&mut stream, &encode_control(&req)?)
        .await
        .map_err(rpc_io_error)?;
    let reply = read_frame(&mut stream).await.map_err(rpc_io_error)?;
    match decode_control(&reply)?.msg {
        Some(proto::control_frame::Msg::Ack(_)) => Ok(()),
        Some(proto::control_frame::Msg::Nack(nack)) => Err(replica_nack_error(&nack)),
        other => Err(DurableError::QuorumLost(format!("{other:?}"))),
    }
}

pub async fn prune_to_at(
    endpoint: SocketAddr,
    key: &PipelineKey,
    live_segment_ids: Vec<String>,
    epoch: u64,
    identity: &ClusterIdentity,
) -> Result<(), DurableError> {
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let req = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::PruneTo(proto::PruneTo {
            live_segment_ids,
            epoch,
            pipeline: Some(pipeline_to_proto(key)),
        })),
    };
    write_frame(&mut stream, &encode_control(&req)?)
        .await
        .map_err(rpc_io_error)?;
    let reply = read_frame(&mut stream).await.map_err(rpc_io_error)?;
    match decode_control(&reply)?.msg {
        Some(proto::control_frame::Msg::Ack(_)) => Ok(()),
        Some(proto::control_frame::Msg::Nack(nack)) => Err(replica_nack_error(&nack)),
        other => Err(DurableError::QuorumLost(format!("{other:?}"))),
    }
}

pub async fn assign_replica(
    endpoint: SocketAddr,
    key: &PipelineKey,
    epoch: LeaseEpoch,
    primary_endpoint: SocketAddr,
    identity: &ClusterIdentity,
) -> Result<(), DurableError> {
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let req = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Assign(proto::AssignReplica {
            pipeline: Some(pipeline_to_proto(key)),
            epoch: epoch.get(),
            primary_node: identity.node_label(),
            primary_endpoint: primary_endpoint.to_string(),
        })),
    };
    write_frame(&mut stream, &encode_control(&req)?)
        .await
        .map_err(rpc_io_error)?;
    let reply = read_frame(&mut stream).await.map_err(rpc_io_error)?;
    match decode_control(&reply)?.msg {
        Some(proto::control_frame::Msg::Ack(_)) => Ok(()),
        Some(proto::control_frame::Msg::Nack(nack)) => Err(replica_nack_error(&nack)),
        other => Err(DurableError::ProtocolMismatch(format!(
            "assign expected Ack, got {other:?}"
        ))),
    }
}

pub async fn drop_replica(
    endpoint: SocketAddr,
    key: &PipelineKey,
    identity: &ClusterIdentity,
) -> Result<(), DurableError> {
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let req = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Drop(proto::DropReplica {
            pipeline: Some(pipeline_to_proto(key)),
        })),
    };
    write_frame(&mut stream, &encode_control(&req)?)
        .await
        .map_err(rpc_io_error)?;
    let _ = read_frame(&mut stream).await.map_err(rpc_io_error)?;
    Ok(())
}

pub struct TcpReplicaClient {
    endpoint: std::sync::RwLock<SocketAddr>,
    identity: ClusterIdentity,
}

impl TcpReplicaClient {
    pub fn new(endpoint: SocketAddr, identity: ClusterIdentity) -> Arc<Self> {
        Arc::new(Self {
            endpoint: std::sync::RwLock::new(endpoint),
            identity,
        })
    }

    pub fn endpoint(&self) -> SocketAddr {
        *self.endpoint.read().expect("replica endpoint")
    }

    pub fn retarget(&self, endpoint: SocketAddr) {
        *self.endpoint.write().expect("replica endpoint") = endpoint;
    }
}

#[async_trait::async_trait]
impl crate::buffer::durable::replicate::ReplicaClient for TcpReplicaClient {
    fn current_endpoint(&self) -> Option<SocketAddr> {
        Some(self.endpoint())
    }

    async fn replicate_payload_at(
        &self,
        endpoint: SocketAddr,
        pipeline: &PipelineKey,
        segment_id: &str,
        sha256: [u8; 32],
        len: u64,
        epoch: u64,
        payload: &Path,
    ) -> Result<(), DurableError> {
        replicate_payload_to(
            endpoint,
            pipeline,
            segment_id,
            sha256,
            len,
            epoch,
            payload,
            &self.identity,
        )
        .await
        .map(|_| ())
    }

    async fn replicate_payload(
        &self,
        pipeline: &PipelineKey,
        segment_id: &str,
        sha256: [u8; 32],
        len: u64,
        epoch: u64,
        payload: &Path,
    ) -> Result<(), DurableError> {
        self.replicate_payload_at(
            self.endpoint(),
            pipeline,
            segment_id,
            sha256,
            len,
            epoch,
            payload,
        )
        .await
    }
}

pub struct ScriptedPeer {
    pub responses: Mutex<Vec<Result<(), DurableError>>>,
}

impl ScriptedPeer {
    pub fn new(responses: Vec<Result<(), DurableError>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
        })
    }
}

#[async_trait::async_trait]
impl crate::buffer::durable::replicate::ReplicaClient for ScriptedPeer {
    async fn replicate_payload(
        &self,
        _pipeline: &PipelineKey,
        _segment_id: &str,
        _sha256: [u8; 32],
        _len: u64,
        _epoch: u64,
        _payload: &Path,
    ) -> Result<(), DurableError> {
        let mut responses = self.responses.lock().await;
        responses
            .pop()
            .unwrap_or(Err(DurableError::QuorumLost("script exhausted".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::{LeaseEpoch, PipelineKey, SystemClock};

    #[test]
    fn protocol_v3_rejects_v2_peer() {
        assert!(!protocol_compatible(2, 2));
        assert!(protocol_compatible(3, 3));
    }

    #[test]
    fn replica_session_status_is_payload_ready() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let session = ReplicaSession::new(key, paths, guard);
        assert!(!session.ready.load(Ordering::SeqCst));
    }
}
