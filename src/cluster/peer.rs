use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use skippr_lease::{
    race_deadline, Clock, DurableError, LeaseEpoch, LeaseGuard, PipelineKey, PipelinePaths,
    ReplicaNack, SegmentId, Sleeper, CONTROL_FRAME_MAX_BYTES, CURRENT_PROTOCOL,
    FETCH_ENTRIES_MAX_COUNT, PAYLOAD_CHUNK_BYTES, PROTOCOL_MAX, PROTOCOL_MIN, RPC_IDLE_TIMEOUT,
};

use crate::cluster::identity::ClusterIdentity;
use crate::cluster::tls::{self, MaybeTlsStream};
use std::sync::{OnceLock, RwLock as StdRwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex, RwLock};

use crate::buffer::durable::apply::DurableApplicator;
use crate::buffer::durable::codec::{
    decode_control, encode_control, envelope_from_proto, envelope_to_proto, hash32,
    pipeline_from_proto, pipeline_to_proto, proto,
};
use crate::buffer::durable::log::MutationLog;
use crate::buffer::durable::mutation::{EntryComparison, MutationEnvelope};
use crate::buffer::durable::snapshot::install_snapshot_from_stream;

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
    pub log: Mutex<MutationLog>,
    pub applicator: DurableApplicator,
    pub assigned_primary: Mutex<Option<(String, LeaseEpoch)>>,
    pub ready: AtomicBool,
    pub diverged: AtomicBool,
}

impl ReplicaSession {
    pub fn new(
        key: PipelineKey,
        paths: PipelinePaths,
        guard: Arc<LeaseGuard>,
        log: MutationLog,
    ) -> Arc<Self> {
        let applicator = DurableApplicator::new(paths.clone());
        Arc::new(Self {
            key,
            paths,
            guard,
            log: Mutex::new(log),
            applicator,
            assigned_primary: Mutex::new(None),
            ready: AtomicBool::new(false),
            diverged: AtomicBool::new(false),
        })
    }

    pub async fn note_assigned(&self, primary: String, epoch: LeaseEpoch) {
        self.guard.observe_epoch(epoch);
        *self.assigned_primary.lock().await = Some((primary, epoch));
    }

    pub async fn status(&self) -> proto::ReplicaStatus {
        match durable_state_for_rpc(self).await {
            Ok(state) => proto::ReplicaStatus {
                epoch_seen: epoch_seen_for_rpc(self),
                base_index: state.base_index.get(),
                committed_index: state.committed_index.get(),
                applied_index: state.applied_index.get(),
                head_hash: state.head_hash.to_vec(),
                ready: self.ready.load(Ordering::SeqCst)
                    && !self.diverged.load(Ordering::SeqCst)
                    && state.committed_index == state.applied_index,
                pipeline: Some(pipeline_to_proto(&self.key)),
            },
            Err(err) => {
                tracing::error!(
                    error = %err,
                    pipeline = %self.key.pipeline(),
                    "durable log unreadable; advertising not ready"
                );
                proto::ReplicaStatus {
                    epoch_seen: epoch_seen_for_rpc(self),
                    ready: false,
                    pipeline: Some(pipeline_to_proto(&self.key)),
                    ..Default::default()
                }
            }
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
    crate::buffer::durable::durable_store_for(&session.key)
        .map(|store| store.paths().clone())
        .unwrap_or_else(|| session.paths.clone())
}

async fn reload_session_log(session: &ReplicaSession) -> Result<(), DurableError> {
    let log = MutationLog::open(session.paths.clone())?;
    *session.log.lock().await = log;
    Ok(())
}

async fn durable_state_for_rpc(
    session: &ReplicaSession,
) -> Result<crate::buffer::durable::log::DurableState, DurableError> {
    if let Some(store) = crate::buffer::durable::durable_store_for(&session.key) {
        return Ok(store.durable_state().await);
    }
    Ok(MutationLog::open(replica_paths(session))?.state())
}

async fn rpc_base_index(session: &ReplicaSession) -> u64 {
    durable_state_for_rpc(session)
        .await
        .map(|state| state.base_index.get())
        .unwrap_or(0)
}

async fn rpc_committed_index(session: &ReplicaSession) -> u64 {
    durable_state_for_rpc(session)
        .await
        .map(|state| state.committed_index.get())
        .unwrap_or(0)
}

fn snapshot_path_for_rpc(session: &ReplicaSession) -> std::path::PathBuf {
    replica_paths(session).snapshot_current()
}

async fn commit_segment_payload_file(
    session: &ReplicaSession,
    envelope: &MutationEnvelope,
) -> Result<Option<std::path::PathBuf>, DurableError> {
    let crate::buffer::durable::mutation::DurableMutation::CommitSegment { descriptor, .. } =
        &envelope.body
    else {
        return Ok(None);
    };
    if descriptor.payload_len == 0 {
        return Ok(None);
    }
    let id = skippr_lease::SegmentId::new(&descriptor.segment_id)
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let path = replica_paths(session).segment(&id);
    let meta = tokio::fs::metadata(&path).await.map_err(|_| {
        DurableError::Io(format!(
            "missing payload for segment {}",
            descriptor.segment_id
        ))
    })?;
    if meta.len() == 0 {
        return Err(DurableError::Io(format!(
            "empty payload for segment {}",
            descriptor.segment_id
        )));
    }
    Ok(Some(path))
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
        if crate::buffer::durable::all_durable_stores()
            .iter()
            .any(|store| {
                matches!(
                    store.guard().role(),
                    skippr_lease::PipelineRole::ActivePrimary(_)
                )
            })
        {
            return true;
        }
        let sessions = self.sessions.read().await;
        if sessions.is_empty() {
            return false;
        }
        let mut ready = true;
        for session in sessions.values() {
            if session.diverged.load(Ordering::SeqCst) {
                continue;
            }
            let status = session.status().await;
            ready &= status.ready;
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
        let dir = tokio::fs::File::open(parent).await?;
        dir.sync_all().await?;
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

async fn stream_length_prefixed_file(
    stream: &mut MaybeTlsStream,
    path: &Path,
) -> Result<(), DurableError> {
    let len = tokio::fs::metadata(path).await?.len();
    stream.write_all(&len.to_le_bytes()).await?;
    stream_payload_from_path(stream, path).await
}

async fn read_length_prefixed_payload(
    stream: &mut MaybeTlsStream,
) -> Result<Vec<u8>, DurableError> {
    let mut len_buf = [0u8; 8];
    stream.read_exact(&mut len_buf).await?;
    let len = u64::from_le_bytes(len_buf);
    if len == 0 {
        return Ok(Vec::new());
    }
    stream_payload_to_vec(stream, len).await
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
            Some(proto::control_frame::Msg::Fetch(fetch)) => {
                handle_fetch(stream, registry, fetch).await?;
            }
            Some(proto::control_frame::Msg::FetchSnapshot(fetch)) => {
                handle_fetch_snapshot(stream, registry, fetch).await?;
            }
            Some(proto::control_frame::Msg::Install(header)) => {
                handle_install(stream, registry, header).await?;
            }
            Some(proto::control_frame::Msg::Drop(drop)) => {
                let key = pipeline_from_proto(drop.pipeline.as_ref())?;
                if let Some(session) = registry.remove(&key).await {
                    purge_replica_pipeline(&session).await;
                }
                write_ack(stream, 0, [0u8; 32], true).await?;
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
    for dir in [
        &session.paths.segs,
        &session.paths.completions,
        &session.paths.compactions,
        &session.paths.durable,
        &session.paths.snapshots,
    ] {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    if let Err(err) = reload_session_log(session).await {
        tracing::error!(error = %err, "purged replica log did not reopen empty");
    }
}

async fn purge_diverged_replica(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    session: &ReplicaSession,
    pipeline: &PipelineKey,
) -> Result<(), DurableError> {
    session.diverged.store(true, Ordering::SeqCst);
    session.ready.store(false, Ordering::SeqCst);
    crate::metrics::counters::add_cluster_divergence(1);
    crate::metrics::counters::add_cluster_purge(1);
    purge_replica_pipeline(session).await;
    registry.remove(pipeline).await;
    let head = session.log.lock().await.committed_index().get();
    write_nack(stream, ReplicaNack::Diverged, head).await
}

async fn handle_replicate(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    req: proto::ReplicateRequest,
) -> Result<(), DurableError> {
    let proto_env = req
        .envelope
        .ok_or_else(|| DurableError::ProtocolMismatch("replicate missing envelope".into()))?;
    let envelope = envelope_from_proto(proto_env)?;
    let session = registry.get(&envelope.pipeline).await.ok_or_else(|| {
        DurableError::ProtocolMismatch(format!("unknown pipeline {}", envelope.pipeline.pipeline()))
    })?;
    if session.guard.reject_stale_epoch(envelope.epoch).is_err() {
        let head = session.log.lock().await.committed_index().get();
        return write_nack(stream, ReplicaNack::StaleEpoch, head).await;
    }
    let assigned_epoch = {
        let assigned = session.assigned_primary.lock().await;
        match assigned.as_ref() {
            Some((_, epoch)) => *epoch,
            None => {
                let head = session.log.lock().await.committed_index().get();
                return write_nack(stream, ReplicaNack::NotAssigned, head).await;
            }
        }
    };
    if envelope.epoch != assigned_epoch {
        let head = session.log.lock().await.committed_index().get();
        return write_nack(stream, ReplicaNack::StaleEpoch, head).await;
    }
    let comparison = match {
        let log = session.log.lock().await;
        log.compare(&envelope)
    } {
        Ok(comparison) => comparison,
        Err(DurableError::Diverged(_)) => {
            return purge_diverged_replica(stream, registry, &session, &envelope.pipeline).await;
        }
        Err(err) => return Err(err),
    };
    match comparison {
        EntryComparison::AlreadyAppliedSameHash => {
            write_ack(stream, envelope.index.get(), envelope.entry_hash()?, true).await
        }
        EntryComparison::SameIndexDifferentHash => {
            purge_diverged_replica(stream, registry, &session, &envelope.pipeline).await
        }
        EntryComparison::Gap { head } => {
            write_nack(stream, ReplicaNack::NotCaughtUp, head.get()).await
        }
        EntryComparison::Next => {
            if crate::cluster::disk::disk_under_pressure(&session.paths.root)
                && req.payload_len == 0
            {
                return write_nack(
                    stream,
                    ReplicaNack::DiskFull,
                    session.log.lock().await.committed_index().get(),
                )
                .await;
            }
            if req.payload_len > 0 {
                let dest = match &envelope.body {
                    crate::buffer::durable::mutation::DurableMutation::CommitSegment {
                        descriptor,
                        ..
                    } => {
                        let id = SegmentId::new(&descriptor.segment_id)
                            .map_err(|err| DurableError::Io(err.to_string()))?;
                        session.paths.segment(&id)
                    }
                    _ => session.paths.durable.join("payload.bin"),
                };
                let hash = match stream_payload_to_path(stream, req.payload_len, &dest).await {
                    Ok(hash) => hash,
                    Err(DurableError::DiskFull) => {
                        let head = session.log.lock().await.committed_index().get();
                        return write_nack(stream, ReplicaNack::DiskFull, head).await;
                    }
                    Err(err) => return Err(err),
                };
                let expected = hash32(&req.payload_sha256)?;
                if hash != expected {
                    let _ = tokio::fs::remove_file(dest.with_extension("part")).await;
                    return write_nack(stream, ReplicaNack::PayloadHashMismatch, 0).await;
                }
            }
            if crate::cluster::disk::disk_under_pressure(&session.paths.root) {
                let head = session.log.lock().await.committed_index().get();
                return write_nack(stream, ReplicaNack::DiskFull, head).await;
            }
            let mut log = session.log.lock().await;
            log.append_prepared(&envelope)?;
            crate::cluster::failpoint::hit(
                crate::cluster::failpoint::FailpointName::ReplicaAfterPrepared,
                &session.paths.root,
            )?;
            let hash = envelope.entry_hash()?;
            log.append_committed(envelope.index, hash)?;
            session.applicator.apply(&envelope)?;
            log.mark_applied(envelope.index, hash)?;
            if matches!(
                envelope.body,
                crate::buffer::durable::mutation::DurableMutation::ReclaimSegment { .. }
            ) {
                let snapshot = crate::buffer::durable::snapshot::retain_live_snapshot(
                    &mut log,
                    &session.paths,
                    &session.key,
                )?;
                crate::buffer::ingest_buffer::Buffers::sync_planner_from_snapshot(&snapshot);
            }
            drop(log);
            write_ack(stream, envelope.index.get(), hash, false).await
        }
    }
}

async fn handle_assign(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    assign: proto::AssignReplica,
) -> Result<(), DurableError> {
    let key = pipeline_from_proto(assign.pipeline.as_ref())?;
    let Some(session) = registry.get(&key).await else {
        return write_nack(stream, ReplicaNack::UnknownPipeline, 0).await;
    };
    let Ok(endpoint) = assign.primary_endpoint.parse::<SocketAddr>() else {
        session.ready.store(false, Ordering::SeqCst);
        return write_nack(stream, ReplicaNack::InvalidPrimaryEndpoint, 0).await;
    };
    let epoch = LeaseEpoch::new(assign.epoch);
    let assignment = (assign.primary_node.clone(), epoch);
    {
        let current = session.assigned_primary.lock().await;
        if current.as_ref() == Some(&assignment) && session.ready.load(Ordering::SeqCst) {
            session.guard.observe_epoch(epoch);
            write_ack(stream, 0, [0u8; 32], true).await?;
            return Ok(());
        }
    }
    session
        .note_assigned(assign.primary_node.clone(), epoch)
        .await;
    session.ready.store(false, Ordering::SeqCst);
    write_ack(stream, 0, [0u8; 32], true).await?;
    let identity = registry.identity.clone();
    tokio::spawn(async move {
        // Catch-up must not hold `session.log` across donor RPCs: Status
        // and advertised_ready take the same mutex.
        let mut result = catch_up_assigned_session(&session, endpoint, &identity).await;
        if let Err(DurableError::Diverged(reason)) = &result {
            tracing::warn!(
                error = %reason,
                "assigned replica catch-up diverged; purging and retrying"
            );
            purge_replica_pipeline(&session).await;
            result = catch_up_assigned_session(&session, endpoint, &identity).await;
        }
        session.ready.store(result.is_ok(), Ordering::SeqCst);
        if let Err(err) = result {
            tracing::warn!(error = %err, "assigned replica catch-up failed");
        } else {
            tracing::info!(
                pipeline = %session.key.pipeline(),
                "assigned replica catch-up reached donor head"
            );
        }
    });
    Ok(())
}

async fn catch_up_assigned_session(
    session: &ReplicaSession,
    endpoint: SocketAddr,
    identity: &ClusterIdentity,
) -> Result<(), DurableError> {
    let mut log = MutationLog::open(session.paths.clone())?;
    crate::cluster::catchup::catch_up_from_donor(
        &mut log,
        &session.paths,
        &session.key,
        endpoint,
        identity,
    )
    .await?;
    *session.log.lock().await = log;
    Ok(())
}

async fn handle_fetch_snapshot(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    fetch: proto::FetchSnapshotRequest,
) -> Result<(), DurableError> {
    let key = pipeline_from_proto(fetch.pipeline.as_ref())?;
    let session = registry
        .get(&key)
        .await
        .ok_or_else(|| DurableError::ProtocolMismatch("unknown pipeline".into()))?;
    let path = snapshot_path_for_rpc(&session);
    if !path.exists() {
        return write_nack(
            stream,
            ReplicaNack::NoSnapshot,
            rpc_base_index(&session).await,
        )
        .await;
    }
    let bytes = tokio::fs::read(&path).await?;
    if !crate::buffer::durable::snapshot::is_live_snapshot_pack(&bytes) {
        return write_nack(
            stream,
            ReplicaNack::EmptySnapshot,
            rpc_base_index(&session).await,
        )
        .await;
    }
    let hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        hasher.finalize()
    };
    let state = durable_state_for_rpc(&session).await?;
    let header = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Install(
            proto::InstallSnapshotHeader {
                pipeline: Some(pipeline_to_proto(&key)),
                base_index: state.base_index.get(),
                base_hash: state.head_hash.to_vec(),
                payload_bytes: bytes.len() as u64,
                snapshot_sha256: hash.to_vec(),
            },
        )),
    };
    write_frame(stream, &encode_control(&header)?).await?;
    stream_payload_from_path(stream, &path).await
}

async fn handle_fetch(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    fetch: proto::FetchEntriesRequest,
) -> Result<(), DurableError> {
    let key = pipeline_from_proto(fetch.pipeline.as_ref())?;
    let session = registry
        .get(&key)
        .await
        .ok_or_else(|| DurableError::ProtocolMismatch("unknown pipeline".into()))?;
    let envelopes: Vec<_> = if let Some(store) = crate::buffer::durable::durable_store_for(&key) {
        store
            .committed_envelopes_in_range(fetch.from_index, fetch.to_index, FETCH_ENTRIES_MAX_COUNT)
            .await
    } else {
        let log = MutationLog::open(replica_paths(&session))?;
        log.envelopes_in_range(fetch.from_index, fetch.to_index)
            .take(FETCH_ENTRIES_MAX_COUNT)
            .cloned()
            .collect()
    };
    let mut payload_paths = Vec::new();
    for envelope in &envelopes {
        match commit_segment_payload_file(&session, envelope).await {
            Ok(path) => payload_paths.push(path),
            Err(_) => {
                return write_nack(
                    stream,
                    ReplicaNack::SnapshotUnavailable,
                    rpc_committed_index(&session).await,
                )
                .await;
            }
        }
    }
    let mut entries = Vec::new();
    for envelope in &envelopes {
        entries.push(envelope_to_proto(envelope)?);
    }
    let reply = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::FetchOk(
            proto::FetchEntriesResponse {
                entries,
                payloads: Vec::new(),
            },
        )),
    };
    write_frame(stream, &encode_control(&reply)?).await?;
    for path in payload_paths.into_iter().flatten() {
        stream_length_prefixed_file(stream, &path).await?;
    }
    Ok(())
}

async fn handle_install(
    stream: &mut MaybeTlsStream,
    registry: &ReplicaRegistry,
    header: proto::InstallSnapshotHeader,
) -> Result<(), DurableError> {
    let key = pipeline_from_proto(header.pipeline.as_ref())?;
    let session = registry
        .get(&key)
        .await
        .ok_or_else(|| DurableError::ProtocolMismatch("unknown pipeline".into()))?;
    let dest = session.paths.durable.join("incoming.snapshot");
    let hash = stream_payload_to_path(stream, header.payload_bytes, &dest).await?;
    let expected = hash32(&header.snapshot_sha256)?;
    if hash != expected {
        return write_nack(stream, ReplicaNack::SnapshotHashMismatch, 0).await;
    }
    let bytes = tokio::fs::read(&dest).await?;
    install_snapshot_from_stream(&session.paths, &uuid::Uuid::new_v4().to_string(), &bytes).await?;
    crate::buffer::ingest_buffer::Buffers::sync_planner_from_pipeline_paths(&session.paths);
    reload_session_log(&session).await?;
    write_ack(stream, header.base_index, expected, false).await
}

async fn write_ack(
    stream: &mut MaybeTlsStream,
    commit_index: u64,
    entry_hash: [u8; 32],
    already_applied: bool,
) -> Result<(), DurableError> {
    let reply = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Ack(proto::ReplicaAck {
            commit_index,
            entry_hash: entry_hash.to_vec(),
            already_applied,
        })),
    };
    write_frame(stream, &encode_control(&reply)?).await?;
    Ok(())
}

async fn write_nack(
    stream: &mut MaybeTlsStream,
    nack: ReplicaNack,
    head_index: u64,
) -> Result<(), DurableError> {
    let reply = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Nack(proto::ReplicaNack {
            code: replica_nack_code(nack) as i32,
            head_index,
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
        ReplicaNack::Diverged => proto::ReplicaNackCode::Diverged,
        ReplicaNack::DiskFull => proto::ReplicaNackCode::DiskFull,
        ReplicaNack::PayloadHashMismatch => proto::ReplicaNackCode::PayloadHashMismatch,
        ReplicaNack::UnknownPipeline => proto::ReplicaNackCode::UnknownPipeline,
        ReplicaNack::InvalidPrimaryEndpoint => proto::ReplicaNackCode::InvalidPrimaryEndpoint,
        ReplicaNack::NoSnapshot => proto::ReplicaNackCode::NoSnapshot,
        ReplicaNack::EmptySnapshot => proto::ReplicaNackCode::EmptySnapshot,
        ReplicaNack::SnapshotHashMismatch => proto::ReplicaNackCode::SnapshotHashMismatch,
        ReplicaNack::SnapshotUnavailable => proto::ReplicaNackCode::SnapshotUnavailable,
    }
}

fn replica_nack_from_proto(nack: &proto::ReplicaNack) -> Result<ReplicaNack, DurableError> {
    match proto::ReplicaNackCode::try_from(nack.code) {
        Ok(proto::ReplicaNackCode::StaleEpoch) => Ok(ReplicaNack::StaleEpoch),
        Ok(proto::ReplicaNackCode::NotAssigned) => Ok(ReplicaNack::NotAssigned),
        Ok(proto::ReplicaNackCode::NotCaughtUp) => Ok(ReplicaNack::NotCaughtUp),
        Ok(proto::ReplicaNackCode::Diverged) => Ok(ReplicaNack::Diverged),
        Ok(proto::ReplicaNackCode::DiskFull) => Ok(ReplicaNack::DiskFull),
        Ok(proto::ReplicaNackCode::PayloadHashMismatch) => Ok(ReplicaNack::PayloadHashMismatch),
        Ok(proto::ReplicaNackCode::UnknownPipeline) => Ok(ReplicaNack::UnknownPipeline),
        Ok(proto::ReplicaNackCode::InvalidPrimaryEndpoint) => {
            Ok(ReplicaNack::InvalidPrimaryEndpoint)
        }
        Ok(proto::ReplicaNackCode::NoSnapshot) => Ok(ReplicaNack::NoSnapshot),
        Ok(proto::ReplicaNackCode::EmptySnapshot) => Ok(ReplicaNack::EmptySnapshot),
        Ok(proto::ReplicaNackCode::SnapshotHashMismatch) => Ok(ReplicaNack::SnapshotHashMismatch),
        Ok(proto::ReplicaNackCode::SnapshotUnavailable) => Ok(ReplicaNack::SnapshotUnavailable),
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

pub async fn replicate_to(
    endpoint: SocketAddr,
    envelope: &MutationEnvelope,
    payload: &Path,
    identity: &ClusterIdentity,
) -> Result<proto::ReplicaAck, DurableError> {
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let payload_len = if payload.exists() {
        tokio::fs::metadata(payload)
            .await
            .map_err(|err| DurableError::Io(err.to_string()))?
            .len()
    } else {
        0
    };
    let payload_sha256 = if payload_len > 0 {
        sha256_path(payload).await?.to_vec()
    } else {
        envelope.payload_sha256.to_vec()
    };
    let req = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::Replicate(
            proto::ReplicateRequest {
                envelope: Some(envelope_to_proto(envelope)?),
                payload_len,
                payload_sha256,
                entry_hash: envelope.entry_hash()?.to_vec(),
            },
        )),
    };
    write_frame(&mut stream, &encode_control(&req)?)
        .await
        .map_err(rpc_io_error)?;
    if payload_len > 0 {
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
            base_index: 0,
            committed_index: 0,
            applied_index: 0,
            head_hash: Vec::new(),
            ready: false,
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

#[derive(Debug)]
pub struct FetchedWalEntry {
    pub envelope: MutationEnvelope,
    pub payload: Vec<u8>,
}

pub async fn fetch_entries_from(
    endpoint: SocketAddr,
    key: &PipelineKey,
    from_index: u64,
    to_index: u64,
    identity: &ClusterIdentity,
) -> Result<Vec<FetchedWalEntry>, DurableError> {
    if from_index > to_index {
        return Ok(Vec::new());
    }
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let mut from = from_index;
    let mut out = Vec::new();
    while from <= to_index {
        let req = proto::ControlFrame {
            msg: Some(proto::control_frame::Msg::Fetch(
                proto::FetchEntriesRequest {
                    pipeline: Some(pipeline_to_proto(key)),
                    from_index: from,
                    to_index,
                },
            )),
        };
        write_frame(&mut stream, &encode_control(&req)?)
            .await
            .map_err(rpc_io_error)?;
        let reply = read_frame(&mut stream).await.map_err(rpc_io_error)?;
        match decode_control(&reply)?.msg {
            Some(proto::control_frame::Msg::FetchOk(ok)) => {
                let proto::FetchEntriesResponse { entries, payloads } = ok;
                if !payloads.is_empty() {
                    return Err(DurableError::ProtocolMismatch(
                        "fetch payloads must stream after the control frame".into(),
                    ));
                }
                if entries.is_empty() {
                    break;
                }
                for proto in entries {
                    let envelope = envelope_from_proto(proto)?;
                    let mut payload = Vec::new();
                    if let crate::buffer::durable::mutation::DurableMutation::CommitSegment {
                        descriptor,
                        ..
                    } = &envelope.body
                    {
                        if descriptor.payload_len > 0 {
                            payload = read_length_prefixed_payload(&mut stream).await?;
                            if payload.is_empty() {
                                return Err(DurableError::Io(format!(
                                    "missing payload for segment {}",
                                    descriptor.segment_id
                                )));
                            }
                            crate::metrics::counters::add_cluster_catchup_bytes(
                                payload.len() as u64
                            );
                        }
                    }
                    from = envelope.index.get().saturating_add(1);
                    out.push(FetchedWalEntry { envelope, payload });
                }
            }
            Some(proto::control_frame::Msg::Nack(nack)) => {
                return Err(replica_nack_error(&nack));
            }
            other => return Err(DurableError::QuorumLost(format!("{other:?}"))),
        }
    }
    Ok(out)
}

async fn stream_payload_to_vec(
    stream: &mut MaybeTlsStream,
    total: u64,
) -> Result<Vec<u8>, DurableError> {
    let mut buf = vec![0u8; total as usize];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn fetch_snapshot_from(
    endpoint: SocketAddr,
    key: &PipelineKey,
    identity: &ClusterIdentity,
    paths: &PipelinePaths,
) -> Result<(), DurableError> {
    let mut stream = connect_replica(endpoint).await?;
    handshake(&mut stream, identity).await?;
    let req = proto::ControlFrame {
        msg: Some(proto::control_frame::Msg::FetchSnapshot(
            proto::FetchSnapshotRequest {
                pipeline: Some(pipeline_to_proto(key)),
            },
        )),
    };
    write_frame(&mut stream, &encode_control(&req)?)
        .await
        .map_err(rpc_io_error)?;
    let reply = read_frame(&mut stream).await.map_err(rpc_io_error)?;
    match decode_control(&reply)?.msg {
        Some(proto::control_frame::Msg::Install(header)) => {
            if header.payload_bytes == 0 {
                return Err(DurableError::ProtocolMismatch(
                    "empty snapshot payload".into(),
                ));
            }
            let dest = paths.durable.join("incoming.snapshot");
            let hash = stream_payload_to_path(&mut stream, header.payload_bytes, &dest).await?;
            let expected = hash32(&header.snapshot_sha256)?;
            if hash != expected {
                return Err(DurableError::ProtocolMismatch(
                    "snapshot hash mismatch".into(),
                ));
            }
            let bytes = tokio::fs::read(&dest).await?;
            crate::metrics::counters::add_cluster_catchup_bytes(bytes.len() as u64);
            install_snapshot_from_stream(paths, &uuid::Uuid::new_v4().to_string(), &bytes).await?;
            crate::buffer::ingest_buffer::Buffers::sync_planner_from_pipeline_paths(paths);
            Ok(())
        }
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

    async fn replicate_at(
        &self,
        endpoint: SocketAddr,
        envelope: &MutationEnvelope,
        payload: &Path,
    ) -> Result<(), DurableError> {
        replicate_to(endpoint, envelope, payload, &self.identity)
            .await
            .map(|_| ())
    }

    async fn replicate(
        &self,
        envelope: &MutationEnvelope,
        payload: &Path,
    ) -> Result<(), DurableError> {
        self.replicate_at(self.endpoint(), envelope, payload).await
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
    async fn replicate(
        &self,
        _envelope: &MutationEnvelope,
        _payload: &Path,
    ) -> Result<(), DurableError> {
        let mut responses = self.responses.lock().await;
        if responses.is_empty() {
            return Err(DurableError::QuorumLost("scripted peer exhausted".into()));
        }
        responses.remove(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::durable::mutation::DurableMutation;
    use skippr_lease::{CommitIndex, LeaseEpoch, NodeId, PipelineKey, SystemClock, GENESIS_HASH};

    fn identity() -> ClusterIdentity {
        ClusterIdentity::new(
            skippr_lease::ClusterId::new("test-cluster").unwrap(),
            NodeId::generate(),
        )
    }

    #[test]
    fn oversized_len_is_rejected_before_allocating_announced_total() {
        assert!(CONTROL_FRAME_MAX_BYTES < 64 * 1024 * 1024);
        let announced = CONTROL_FRAME_MAX_BYTES + 1;
        assert!(announced > CONTROL_FRAME_MAX_BYTES);
    }

    #[test]
    fn protocol_intersection_requires_overlap() {
        assert!(protocol_compatible(PROTOCOL_MIN, PROTOCOL_MAX));
        assert!(!protocol_compatible(1, 1));
        assert!(!protocol_compatible(PROTOCOL_MAX + 1, PROTOCOL_MAX + 2));
    }

    #[test]
    fn replica_nack_codes_round_trip_to_durable() {
        let codes = [
            ReplicaNack::StaleEpoch,
            ReplicaNack::NotAssigned,
            ReplicaNack::NotCaughtUp,
            ReplicaNack::Diverged,
            ReplicaNack::DiskFull,
            ReplicaNack::PayloadHashMismatch,
            ReplicaNack::UnknownPipeline,
            ReplicaNack::InvalidPrimaryEndpoint,
            ReplicaNack::NoSnapshot,
            ReplicaNack::EmptySnapshot,
            ReplicaNack::SnapshotHashMismatch,
            ReplicaNack::SnapshotUnavailable,
        ];
        for nack in codes {
            let wire = proto::ReplicaNack {
                code: replica_nack_code(nack.clone()) as i32,
                head_index: 7,
            };
            assert_eq!(replica_nack_from_proto(&wire).unwrap(), nack);
            assert_eq!(replica_nack_error(&wire), nack.into_durable());
        }
    }

    #[test]
    fn unspecified_and_unknown_nack_codes_fail_closed() {
        let unspecified = proto::ReplicaNack {
            code: proto::ReplicaNackCode::Unspecified as i32,
            head_index: 0,
        };
        assert!(matches!(
            replica_nack_from_proto(&unspecified),
            Err(DurableError::ProtocolMismatch(_))
        ));
        let unknown = proto::ReplicaNack {
            code: 99,
            head_index: 0,
        };
        assert!(matches!(
            replica_nack_error(&unknown),
            DurableError::ProtocolMismatch(_)
        ));
    }

    #[test]
    fn hello_ok_must_match_cluster_hash_and_protocol() {
        let id = identity();
        let ok = proto::ServerHello {
            cluster_hash: id.hash(),
            node_id: id.node_label(),
            protocol: CURRENT_PROTOCOL,
        };
        accept_hello_ok(&ok, &id).unwrap();
        let mut bad_hash = ok.clone();
        bad_hash.cluster_hash = "other".into();
        assert!(accept_hello_ok(&bad_hash, &id).is_err());
        let mut bad_protocol = ok;
        bad_protocol.protocol = PROTOCOL_MAX + 1;
        assert!(accept_hello_ok(&bad_protocol, &id).is_err());
    }

    #[tokio::test]
    async fn unassigned_replicate_is_nacked() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let log = MutationLog::open(paths.clone()).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let session = ReplicaSession::new(key.clone(), paths, guard, log);
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session).await;
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let err = replicate_to(
            server.bind_addr(),
            &MutationEnvelope {
                protocol_version: 1,
                pipeline: key,
                epoch: LeaseEpoch::new(1),
                index: CommitIndex::new(1),
                previous_hash: GENESIS_HASH,
                payload_sha256: [0u8; 32],
                body: DurableMutation::ReclaimSegment {
                    segment_id: "s".into(),
                },
            },
            Path::new("/nonexistent"),
            &identity,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DurableError::NotCaughtUp));
        server.drain().await;
    }

    #[tokio::test]
    async fn replicate_after_newer_assign_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let log = MutationLog::open(paths.clone()).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let session = ReplicaSession::new(key.clone(), paths, guard, log);
        session.note_assigned("n".into(), LeaseEpoch::new(2)).await;
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session).await;
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let err = replicate_to(
            server.bind_addr(),
            &MutationEnvelope {
                protocol_version: 1,
                pipeline: key,
                epoch: LeaseEpoch::new(1),
                index: CommitIndex::new(1),
                previous_hash: GENESIS_HASH,
                payload_sha256: [0u8; 32],
                body: DurableMutation::ReclaimSegment {
                    segment_id: "s".into(),
                },
            },
            Path::new("/nonexistent"),
            &identity,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DurableError::StaleEpoch));
        server.drain().await;
    }

    #[tokio::test]
    async fn replica_server_applies_replicate() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let log = MutationLog::open(paths.clone()).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let session = ReplicaSession::new(key.clone(), paths, guard, log);
        session.note_assigned("n".into(), LeaseEpoch::new(1)).await;
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session).await;
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
                segment_id: "s".into(),
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
        assert!(!ack.already_applied);
        assert_eq!(ack.commit_index, 1);
        server.drain().await;
    }

    #[tokio::test]
    async fn replicate_payload_hash_is_the_streamed_file() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let log = MutationLog::open(paths.clone()).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let session = ReplicaSession::new(key.clone(), paths, guard, log);
        session.note_assigned("n".into(), LeaseEpoch::new(1)).await;
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session).await;
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let payload = dir.path().join("segment.seg");
        std::fs::write(&payload, b"footer-included-bytes").unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [9u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        let ack = replicate_to(server.bind_addr(), &envelope, &payload, &identity)
            .await
            .unwrap();
        assert_eq!(ack.commit_index, 1);
        server.drain().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fetch_entries_uses_ingest_store_when_session_log_is_behind() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "fetch-ingest-store").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let identity = identity();
        let session_log = MutationLog::open(paths.clone()).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let session = ReplicaSession::new(key.clone(), paths.clone(), guard, session_log);
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session).await;
        let mut store_log = MutationLog::open(paths.clone()).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        store_log.append_prepared(&envelope).unwrap();
        let hash = envelope.entry_hash().unwrap();
        store_log.append_committed(envelope.index, hash).unwrap();
        store_log.mark_applied(envelope.index, hash).unwrap();
        let store = crate::buffer::durable::store::PipelineDurableStore::new(
            key.clone(),
            paths,
            LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new())),
            store_log,
            crate::buffer::durable::replicate::ReplicationMode::LocalOnly,
            crate::buffer::durable::store::OffsetMode::Dynamo(
                crate::buffer::durable::store::MemoryOffsetPublisher::new(),
            ),
        );
        crate::buffer::durable::install_durable_store(store);
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let entries = fetch_entries_from(server.bind_addr(), &key, 1, 1, &identity).await;
        crate::buffer::durable::remove_durable_store(&key);
        let entries = entries.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].envelope.index.get(), 1);
        server.drain().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fetch_entries_uses_disk_when_session_log_is_behind() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "fetch-disk-behind").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let identity = identity();
        let session_log = MutationLog::open(paths.clone()).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let session = ReplicaSession::new(key.clone(), paths.clone(), guard, session_log);
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session).await;
        let mut disk = MutationLog::open(paths).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        disk.append_prepared(&envelope).unwrap();
        let hash = envelope.entry_hash().unwrap();
        disk.append_committed(envelope.index, hash).unwrap();
        disk.mark_applied(envelope.index, hash).unwrap();
        drop(disk);
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let entries = fetch_entries_from(server.bind_addr(), &key, 1, 1, &identity)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].envelope.index.get(), 1);
        server.drain().await;
    }

    #[tokio::test]
    async fn mismatched_cluster_identity_is_rejected() {
        let registry = ReplicaRegistry::new(ClusterIdentity::new(
            skippr_lease::ClusterId::new("test-cluster").unwrap(),
            NodeId::generate(),
        ));
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let err = replicate_to(
            server.bind_addr(),
            &MutationEnvelope {
                protocol_version: 1,
                pipeline: PipelineKey::new("t", "w", "p").unwrap(),
                epoch: LeaseEpoch::new(1),
                index: CommitIndex::new(1),
                previous_hash: GENESIS_HASH,
                payload_sha256: [0u8; 32],
                body: DurableMutation::ReclaimSegment {
                    segment_id: "s".into(),
                },
            },
            Path::new("/nonexistent"),
            &ClusterIdentity::new(
                skippr_lease::ClusterId::new("other-cluster").unwrap(),
                NodeId::generate(),
            ),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            DurableError::QuorumLost(_) | DurableError::ProtocolMismatch(_)
        ));
        server.drain().await;
    }

    #[tokio::test]
    async fn diverged_purge_removes_only_that_pipeline_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let keep = PipelineKey::new("t", "w", "keep").unwrap();
        let drop_key = PipelineKey::new("t", "w", "drop").unwrap();
        let keep_paths = PipelinePaths::new(dir.path(), &keep).unwrap();
        let drop_paths = PipelinePaths::new(dir.path(), &drop_key).unwrap();
        std::fs::create_dir_all(&keep_paths.segs).unwrap();
        std::fs::create_dir_all(&drop_paths.segs).unwrap();
        std::fs::write(keep_paths.segs.join("keep.seg"), b"keep").unwrap();
        std::fs::write(drop_paths.segs.join("drop.seg"), b"drop").unwrap();
        let clock = Arc::new(SystemClock::new());
        let keep_session = ReplicaSession::new(
            keep.clone(),
            keep_paths.clone(),
            LeaseGuard::replica(keep, LeaseEpoch::new(1), clock.clone()),
            MutationLog::open(keep_paths.clone()).unwrap(),
        );
        let drop_session = ReplicaSession::new(
            drop_key.clone(),
            drop_paths.clone(),
            LeaseGuard::replica(drop_key, LeaseEpoch::new(1), clock),
            MutationLog::open(drop_paths.clone()).unwrap(),
        );
        drop_session.diverged.store(true, Ordering::SeqCst);
        purge_replica_pipeline(&drop_session).await;
        assert!(!drop_paths.segs.join("drop.seg").exists());
        assert!(keep_paths.segs.join("keep.seg").exists());
        assert!(!keep_session.diverged.load(Ordering::SeqCst));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fetch_entries_nacks_missing_commit_segment_payload() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "fetch-missing-payload").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [7u8; 32],
            body: DurableMutation::CommitSegment {
                descriptor: crate::buffer::durable::mutation::SegmentDescriptor {
                    segment_id: "missing".into(),
                    payload_len: 4,
                    payload_sha256: [7u8; 32],
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
        let session = ReplicaSession::new(
            key.clone(),
            paths,
            LeaseGuard::replica(
                key.clone(),
                LeaseEpoch::new(1),
                Arc::new(SystemClock::new()),
            ),
            log,
        );
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session).await;
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let err = fetch_entries_from(server.bind_addr(), &key, 1, 1, &identity)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("snapshot unavailable"));
        assert_eq!(FETCH_ENTRIES_MAX_COUNT, 8);
        server.drain().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fetch_entries_streams_whole_segment_file_not_data_payload_len() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "fetch-whole-segment").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        std::fs::create_dir_all(&paths.segs).unwrap();
        let id = SegmentId::new("live").unwrap();
        let bytes = b"SEGF-header-and-parts-larger-than-data-len";
        std::fs::write(paths.segment(&id), bytes).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [7u8; 32],
            body: DurableMutation::CommitSegment {
                descriptor: crate::buffer::durable::mutation::SegmentDescriptor {
                    segment_id: "live".into(),
                    payload_len: 4,
                    payload_sha256: [7u8; 32],
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
        let session = ReplicaSession::new(
            key.clone(),
            paths,
            LeaseGuard::replica(
                key.clone(),
                LeaseEpoch::new(1),
                Arc::new(SystemClock::new()),
            ),
            log,
        );
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session).await;
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let entries = fetch_entries_from(server.bind_addr(), &key, 1, 1, &identity)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].payload, bytes);
        server.drain().await;
    }

    #[tokio::test]
    async fn new_replica_session_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let log = MutationLog::open(paths.clone()).unwrap();
        let session = ReplicaSession::new(
            key,
            paths,
            LeaseGuard::replica(
                PipelineKey::new("t", "w", "p").unwrap(),
                LeaseEpoch::new(1),
                Arc::new(SystemClock::new()),
            ),
            log,
        );
        let status = session.status().await;
        assert!(!status.ready);
    }

    #[tokio::test]
    async fn status_reads_disk_log_when_session_is_behind() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let session_log = MutationLog::open(paths.clone()).unwrap();
        let session = ReplicaSession::new(
            key.clone(),
            paths.clone(),
            LeaseGuard::replica(
                key.clone(),
                LeaseEpoch::new(1),
                Arc::new(SystemClock::new()),
            ),
            session_log,
        );
        let mut disk = MutationLog::open(paths).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "s".into(),
            },
        };
        disk.append_prepared(&envelope).unwrap();
        let hash = envelope.entry_hash().unwrap();
        disk.append_committed(envelope.index, hash).unwrap();
        disk.mark_applied(envelope.index, hash).unwrap();
        drop(disk);
        let status = session.status().await;
        assert_eq!(status.committed_index, 1);
        assert_eq!(session.log.lock().await.committed_index().get(), 0);
    }

    #[test]
    fn diverged_assign_catch_up_purges_and_retries() {
        let src = include_str!("peer.rs");
        assert!(src.contains("assigned replica catch-up diverged; purging and retrying"));
        assert!(src.contains("catch_up_assigned_session"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn advertised_ready_when_this_node_is_active_primary() {
        use crate::buffer::durable::log::MutationLog;
        use crate::buffer::durable::replicate::ReplicationMode;
        use crate::buffer::durable::store::{
            install_durable_store, remove_durable_store, MemoryOffsetPublisher, OffsetMode,
            PipelineDurableStore,
        };

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let session = ReplicaSession::new(
            key.clone(),
            paths.clone(),
            LeaseGuard::replica(
                key.clone(),
                LeaseEpoch::new(1),
                Arc::new(SystemClock::new()),
            ),
            MutationLog::open(paths.clone()).unwrap(),
        );
        let identity = identity();
        let registry = ReplicaRegistry::new(identity);
        crate::buffer::durable::store::clear_active_durable_store();
        registry.insert(session).await;
        assert!(!registry.advertised_ready().await);
        let store_key = PipelineKey::new("t", "w", "ingest").unwrap();
        let store_paths = PipelinePaths::new(dir.path(), &store_key).unwrap();
        let store = PipelineDurableStore::new(
            store_key.clone(),
            store_paths.clone(),
            LeaseGuard::single_node(store_key.clone(), Arc::new(SystemClock::new())),
            MutationLog::open(store_paths).unwrap(),
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
        );
        install_durable_store(store);
        assert!(registry.advertised_ready().await);
        remove_durable_store(&store_key);
        assert!(!registry.advertised_ready().await);
    }

    #[tokio::test]
    async fn assign_unknown_pipeline_nacks() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let log = MutationLog::open(paths.clone()).unwrap();
        let session = ReplicaSession::new(
            key,
            paths,
            LeaseGuard::replica(
                PipelineKey::new("t", "w", "p").unwrap(),
                LeaseEpoch::new(1),
                Arc::new(SystemClock::new()),
            ),
            log,
        );
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session).await;
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let other = PipelineKey::new("t", "w", "other").unwrap();
        let err = assign_replica(
            server.bind_addr(),
            &other,
            LeaseEpoch::new(1),
            server.bind_addr(),
            &identity,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown pipeline"));
        server.drain().await;
    }

    #[tokio::test]
    async fn assign_invalid_endpoint_nacks() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let log = MutationLog::open(paths.clone()).unwrap();
        let session = ReplicaSession::new(
            key.clone(),
            paths,
            LeaseGuard::replica(
                key.clone(),
                LeaseEpoch::new(1),
                Arc::new(SystemClock::new()),
            ),
            log,
        );
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        registry.insert(session.clone()).await;
        let server = ReplicaServer::start_with_registry("127.0.0.1:0".parse().unwrap(), registry)
            .await
            .unwrap();
        let mut stream = connect_replica(server.bind_addr()).await.unwrap();
        handshake(&mut stream, &identity).await.unwrap();
        let req = proto::ControlFrame {
            msg: Some(proto::control_frame::Msg::Assign(proto::AssignReplica {
                pipeline: Some(pipeline_to_proto(&key)),
                epoch: 1,
                primary_node: identity.node_label(),
                primary_endpoint: "not-an-addr".into(),
            })),
        };
        write_frame(&mut stream, &encode_control(&req).unwrap())
            .await
            .unwrap();
        let reply = read_frame(&mut stream).await.unwrap();
        match decode_control(&reply).unwrap().msg {
            Some(proto::control_frame::Msg::Nack(nack)) => {
                assert_eq!(
                    nack.code,
                    proto::ReplicaNackCode::InvalidPrimaryEndpoint as i32
                );
            }
            other => panic!("expected Nack, got {other:?}"),
        }
        assert!(!session.ready.load(Ordering::SeqCst));
        server.drain().await;
    }
}
