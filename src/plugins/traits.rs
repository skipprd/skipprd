use arrow::record_batch::RecordBatch;
use arrow_schema::SchemaRef;
use async_trait::async_trait;
use datafusion::error::DataFusionError;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::physical_plan::RecordBatchStream;
use futures::StreamExt;
use serde_derive::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

pub use crate::buffer::compaction_transaction::SinkWriteSemantics;

use crate::buffer::compaction_transaction::{SinkGroupingSupport, SinkRetrySemantics};
use crate::discover::OutputMetadata;
use crate::plugins::cdc::{SinkCapability, SourceCapability, SyncContext};
use crate::plugins::source_contract::SourceNamespaceContract;
use crate::plugins::source_sync::SourceSyncContext;

#[derive(Clone, Copy, Debug)]
pub enum SourceCdcContract {
    None,
    Configurable {
        mode: SourceCdcMode,
        capability: &'static SourceCapability,
    },
}

impl SourceCdcContract {
    pub fn capability(self) -> Option<&'static SourceCapability> {
        match self {
            Self::Configurable { mode, capability } if mode.includes_cdc_stream() => {
                Some(capability)
            }
            Self::Configurable { .. } | Self::None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCdcMode {
    /// Bounded read only; do not emit CDC metadata or consume a change stream.
    #[default]
    Snapshot,
    /// Initial snapshot on first run, then resume from CDC checkpoints thereafter.
    SnapshotThenCdc,
    /// No initial snapshot; start or resume from the source's CDC stream only.
    CdcOnly,
}

impl SourceCdcMode {
    pub fn includes_cdc_stream(self) -> bool {
        matches!(self, Self::SnapshotThenCdc | Self::CdcOnly)
    }

    pub fn includes_initial_snapshot(self) -> bool {
        matches!(self, Self::Snapshot | Self::SnapshotThenCdc)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceOnceContract {
    /// `sync()` returns after the current bounded snapshot/read is complete.
    Finite,
    /// `sync()` may stream indefinitely; runtime `--once` idle supervision is required.
    HostIdleBounded,
    /// The plugin has its own idle/exhaustion condition in addition to host supervision.
    PluginIdleBounded,
}

#[derive(Clone, Copy, Debug)]
pub struct SourceExecutionContract {
    pub cdc: SourceCdcContract,
    pub once: SourceOnceContract,
}

impl SourceExecutionContract {
    pub fn finite() -> Self {
        Self {
            cdc: SourceCdcContract::None,
            once: SourceOnceContract::Finite,
        }
    }

    pub fn stream(once: SourceOnceContract) -> Self {
        Self {
            cdc: SourceCdcContract::None,
            once,
        }
    }

    pub fn configurable_cdc(
        mode: SourceCdcMode,
        capability: &'static SourceCapability,
        once: SourceOnceContract,
    ) -> Self {
        Self {
            cdc: SourceCdcContract::Configurable { mode, capability },
            once,
        }
    }
}

/// Reads records from an external system and feeds them into the pipeline.
#[async_trait]
pub trait DataSource: Send + Sync {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error>;

    /// Return this source's typed execution contract.
    ///
    /// This is intentionally required for every source connector so new plugins
    /// must declare CDC semantics and `--once` termination behavior at compile time.
    fn execution_contract(&self) -> SourceExecutionContract;

    /// Return the active CDC capability descriptor for this source.
    fn capability(&self) -> Option<&'static SourceCapability> {
        self.execution_contract().cdc.capability()
    }

    /// Per-namespace extraction contracts (write policy, keys, cursors).
    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        vec![]
    }
}

/// Context passed to sinks for a single compacted batch.
#[derive(Clone, Debug)]
pub struct SinkWriteContext<'a> {
    pub filename: String,
    pub compaction_id: String,
    pub idempotency_key: String,
    pub wal_refs: Vec<crate::runtime_plugins::protocol::RuntimeWalPartRef>,
    pub write_semantics: crate::buffer::compaction_transaction::SinkWriteSemantics,
    pub schema_fingerprint: String,
    pub cdc_ctx: Option<&'a SyncContext>,
    pub source_contract: Option<&'a SourceNamespaceContract>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupedWalRefs(Vec<crate::runtime_plugins::protocol::RuntimeWalPartRef>);

impl GroupedWalRefs {
    pub fn new(
        refs: Vec<crate::runtime_plugins::protocol::RuntimeWalPartRef>,
    ) -> Result<Self, SinkWriteRejection> {
        if refs.is_empty() {
            return Err(SinkWriteRejection::MissingWalRefs);
        }
        Ok(Self(refs))
    }

    pub fn as_slice(&self) -> &[crate::runtime_plugins::protocol::RuntimeWalPartRef] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clone_vec(&self) -> Vec<crate::runtime_plugins::protocol::RuntimeWalPartRef> {
        self.0.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupedWalKind {
    Append,
    Cdc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupedWalPartitionKey {
    pub sink_ref: String,
    pub namespace: String,
    pub partition: String,
    pub time: Option<i64>,
    pub schema_fingerprint: String,
    pub kind: GroupedWalKind,
}

impl GroupedWalPartitionKey {
    pub fn from_refs(
        refs: &GroupedWalRefs,
        schema_fingerprint: &str,
        cdc_ctx: Option<&SyncContext>,
    ) -> Self {
        let first = refs
            .as_slice()
            .first()
            .expect("GroupedWalRefs guarantees at least one ref");
        Self {
            sink_ref: first.sink_ref.clone(),
            namespace: first.namespace.clone(),
            partition: first.partition.clone(),
            time: first.time,
            schema_fingerprint: if schema_fingerprint.is_empty() {
                first.schema_fingerprint.clone()
            } else {
                schema_fingerprint.to_string()
            },
            kind: if cdc_ctx.is_some() {
                GroupedWalKind::Cdc
            } else {
                GroupedWalKind::Append
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct GroupedSinkWriteContext<'a> {
    pub filename: String,
    pub compaction_id: String,
    pub idempotency_key: String,
    pub wal_refs: GroupedWalRefs,
    pub grouping_key: GroupedWalPartitionKey,
    pub write_semantics: crate::buffer::compaction_transaction::SinkWriteSemantics,
    pub schema_fingerprint: String,
    pub cdc_ctx: Option<&'a SyncContext>,
    pub source_contract: Option<&'a SourceNamespaceContract>,
}

impl<'a> TryFrom<SinkWriteContext<'a>> for GroupedSinkWriteContext<'a> {
    type Error = SinkWriteRejection;

    fn try_from(ctx: SinkWriteContext<'a>) -> Result<Self, Self::Error> {
        if ctx.idempotency_key.is_empty() {
            return Err(SinkWriteRejection::MissingIdempotencyKey);
        }
        let wal_refs = GroupedWalRefs::new(ctx.wal_refs)?;
        let grouping_key =
            GroupedWalPartitionKey::from_refs(&wal_refs, &ctx.schema_fingerprint, ctx.cdc_ctx);
        Ok(Self {
            filename: ctx.filename,
            compaction_id: ctx.compaction_id,
            idempotency_key: ctx.idempotency_key,
            wal_refs,
            grouping_key,
            write_semantics: ctx.write_semantics,
            schema_fingerprint: ctx.schema_fingerprint,
            cdc_ctx: ctx.cdc_ctx,
            source_contract: ctx.source_contract,
        })
    }
}

impl<'a> GroupedSinkWriteContext<'a> {
    pub fn to_sink_write_context(&self) -> SinkWriteContext<'a> {
        SinkWriteContext {
            filename: self.filename.clone(),
            compaction_id: self.compaction_id.clone(),
            idempotency_key: self.idempotency_key.clone(),
            wal_refs: self.wal_refs.clone_vec(),
            write_semantics: self.write_semantics,
            schema_fingerprint: self.schema_fingerprint.clone(),
            cdc_ctx: self.cdc_ctx,
            source_contract: self.source_contract,
        }
    }

    pub fn chunk_filename(&self, chunk_index: u64, final_single_chunk: bool) -> String {
        if chunk_index == 0 && final_single_chunk {
            self.filename.clone()
        } else {
            format!("{}&grouped_chunk={:08}", self.filename, chunk_index)
        }
    }

    pub fn chunk_sink_write_context(
        &self,
        chunk_index: u64,
        final_single_chunk: bool,
    ) -> SinkWriteContext<'a> {
        let mut ctx = self.to_sink_write_context();
        ctx.filename = self.chunk_filename(chunk_index, final_single_chunk);
        if !final_single_chunk {
            ctx.idempotency_key = format!("{}-chunk-{:08}", self.idempotency_key, chunk_index);
            ctx.compaction_id = format!("{}-chunk-{:08}", self.compaction_id, chunk_index);
        }
        ctx
    }

    pub fn chunk_sink_write_context_with_cdc<'b>(
        &'b self,
        chunk_index: u64,
        final_single_chunk: bool,
        cdc_ctx: Option<&'b SyncContext>,
    ) -> SinkWriteContext<'b> {
        let filename = self.chunk_filename(chunk_index, final_single_chunk);
        let (compaction_id, idempotency_key) = if final_single_chunk {
            (self.compaction_id.clone(), self.idempotency_key.clone())
        } else {
            (
                format!("{}-chunk-{:08}", self.compaction_id, chunk_index),
                format!("{}-chunk-{:08}", self.idempotency_key, chunk_index),
            )
        };
        SinkWriteContext {
            filename,
            compaction_id,
            idempotency_key,
            wal_refs: self.wal_refs.clone_vec(),
            write_semantics: self.write_semantics,
            schema_fingerprint: self.schema_fingerprint.clone(),
            cdc_ctx,
            source_contract: self.source_contract,
        }
    }

    pub fn chunk_cdc_context(&self, chunk: &RecordBatchChunk) -> io::Result<Option<SyncContext>> {
        self.chunk_cdc_context_for_range(chunk.chunk_index, chunk.row_offset, chunk.rows)
    }

    pub fn chunk_cdc_context_for_range(
        &self,
        chunk_index: u64,
        row_offset: u64,
        rows: u64,
    ) -> io::Result<Option<SyncContext>> {
        let Some(cdc) = self.cdc_ctx else {
            return Ok(None);
        };
        let part_meta = match cdc.part_meta.kind {
            crate::plugins::cdc::WalPartKind::Append => {
                crate::plugins::cdc::WalPartMeta::append(rows)
            }
            crate::plugins::cdc::WalPartKind::Cdc => {
                let start = row_offset as usize;
                let end = start.saturating_add(rows as usize);
                let rows = cdc.part_meta.rows.get(start..end).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "CDC metadata slice out of range for grouped chunk {}: rows {}..{} of {}",
                            chunk_index,
                            start,
                            end,
                            cdc.part_meta.rows.len()
                        ),
                    )
                })?;
                crate::plugins::cdc::WalPartMeta::cdc(rows.to_vec(), rows.len() as u64)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?
            }
        };
        Ok(Some(SyncContext {
            part_meta,
            contract: cdc.contract.clone(),
        }))
    }
}

#[derive(Clone, Debug)]
pub struct RecordBatchChunk {
    pub chunk_index: u64,
    pub row_offset: u64,
    pub final_chunk: bool,
    pub rows: u64,
    pub bytes: usize,
    pub batches: Vec<RecordBatch>,
}

impl RecordBatchChunk {
    pub fn into_stream(self, schema: SchemaRef) -> SendableRecordBatchStream {
        Box::pin(ChunkRecordBatchStream {
            schema,
            batches: self.batches.into_iter(),
        })
    }
}

struct ChunkRecordBatchStream {
    schema: SchemaRef,
    batches: std::vec::IntoIter<RecordBatch>,
}

impl futures::Stream for ChunkRecordBatchStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.batches.next().map(Ok))
    }
}

impl RecordBatchStream for ChunkRecordBatchStream {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

#[derive(Clone, Debug)]
pub struct GroupedBatchReaderConfig {
    pub max_rows: usize,
    pub max_bytes: usize,
}

impl Default for GroupedBatchReaderConfig {
    fn default() -> Self {
        Self {
            max_rows: 100_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct GroupedBatchReader {
    stream: SendableRecordBatchStream,
    schema: SchemaRef,
    grouping_key: GroupedWalPartitionKey,
    config: GroupedBatchReaderConfig,
    chunk_index: u64,
    rows_emitted: u64,
    pending: Option<RecordBatch>,
    finished: bool,
}

impl GroupedBatchReader {
    pub fn new(
        stream: SendableRecordBatchStream,
        grouping_key: GroupedWalPartitionKey,
        config: GroupedBatchReaderConfig,
    ) -> Self {
        let schema = stream.schema();
        Self {
            stream,
            schema,
            grouping_key,
            config,
            chunk_index: 0,
            rows_emitted: 0,
            pending: None,
            finished: false,
        }
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    pub fn grouping_key(&self) -> &GroupedWalPartitionKey {
        &self.grouping_key
    }

    pub async fn next_chunk(&mut self) -> io::Result<Option<RecordBatchChunk>> {
        if self.finished {
            return Ok(None);
        }

        let mut batches = Vec::new();
        let mut rows = 0usize;
        let mut bytes = 0usize;

        loop {
            let next_batch = if let Some(batch) = self.pending.take() {
                Some(batch)
            } else {
                match self.stream.next().await {
                    Some(Ok(batch)) => Some(batch),
                    Some(Err(err)) => return Err(io::Error::other(err.to_string())),
                    None => None,
                }
            };

            let Some(batch) = next_batch else {
                self.finished = true;
                if batches.is_empty() {
                    return Ok(None);
                }
                break;
            };

            let batch_rows = batch.num_rows();
            let batch_bytes = batch
                .columns()
                .iter()
                .map(|column| column.get_array_memory_size())
                .sum::<usize>();
            let would_exceed = !batches.is_empty()
                && (rows.saturating_add(batch_rows) > self.config.max_rows
                    || bytes.saturating_add(batch_bytes) > self.config.max_bytes);
            if would_exceed {
                self.pending = Some(batch);
                break;
            }
            rows = rows.saturating_add(batch_rows);
            bytes = bytes.saturating_add(batch_bytes);
            batches.push(batch);

            if rows >= self.config.max_rows || bytes >= self.config.max_bytes {
                break;
            }
        }

        if !self.finished && self.pending.is_none() {
            match self.stream.next().await {
                Some(Ok(batch)) => {
                    self.pending = Some(batch);
                }
                Some(Err(err)) => return Err(io::Error::other(err.to_string())),
                None => {
                    self.finished = true;
                }
            }
        }

        let chunk_index = self.chunk_index;
        let row_offset = self.rows_emitted;
        self.chunk_index = self.chunk_index.saturating_add(1);
        self.rows_emitted = self.rows_emitted.saturating_add(rows as u64);
        Ok(Some(RecordBatchChunk {
            chunk_index,
            row_offset,
            final_chunk: self.finished,
            rows: rows as u64,
            bytes,
            batches,
        }))
    }

    /// Flatten bounded chunks into their original batch sequence without
    /// materializing the full grouped envelope.
    ///
    /// The returned progress handle counts transport chunks after they are
    /// pulled from this reader. The stream does not request the next chunk
    /// until every batch in the current chunk has been consumed.
    pub fn into_stream(self) -> (SendableRecordBatchStream, GroupedBatchStreamProgress) {
        let progress = GroupedBatchStreamProgress::default();
        let stream = GroupedBatchStream {
            schema: self.schema(),
            reader: Some(self),
            pending_batches: Vec::new().into_iter(),
            pending_chunk: None,
            progress: progress.clone(),
            finished: false,
        };
        (Box::pin(stream), progress)
    }
}

#[derive(Clone, Debug, Default)]
pub struct GroupedBatchStreamProgress {
    transport_chunk_count: Arc<AtomicU32>,
}

impl GroupedBatchStreamProgress {
    pub fn transport_chunk_count(&self) -> u32 {
        self.transport_chunk_count.load(Ordering::Relaxed)
    }

    fn note_chunk(&self) {
        let _ = self.transport_chunk_count.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |count| Some(count.saturating_add(1)),
        );
    }
}

type PendingGroupedChunk = Pin<
    Box<
        dyn Future<Output = (GroupedBatchReader, io::Result<Option<RecordBatchChunk>>)>
            + Send
            + 'static,
    >,
>;

struct GroupedBatchStream {
    schema: SchemaRef,
    reader: Option<GroupedBatchReader>,
    pending_batches: std::vec::IntoIter<RecordBatch>,
    pending_chunk: Option<PendingGroupedChunk>,
    progress: GroupedBatchStreamProgress,
    finished: bool,
}

impl futures::Stream for GroupedBatchStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(batch) = self.pending_batches.next() {
                return Poll::Ready(Some(Ok(batch)));
            }
            if self.finished {
                return Poll::Ready(None);
            }
            if self.pending_chunk.is_none() {
                let mut reader = self
                    .reader
                    .take()
                    .expect("grouped stream reader must be available");
                self.pending_chunk = Some(Box::pin(async move {
                    let chunk = reader.next_chunk().await;
                    (reader, chunk)
                }));
            }

            let pending = self
                .pending_chunk
                .as_mut()
                .expect("grouped chunk future must be available");
            let (reader, result) = match pending.as_mut().poll(cx) {
                Poll::Ready(result) => result,
                Poll::Pending => return Poll::Pending,
            };
            self.pending_chunk = None;
            self.reader = Some(reader);
            match result {
                Ok(Some(chunk)) => {
                    self.progress.note_chunk();
                    self.pending_batches = chunk.batches.into_iter();
                }
                Ok(None) => self.finished = true,
                Err(error) => {
                    self.finished = true;
                    return Poll::Ready(Some(Err(DataFusionError::Execution(error.to_string()))));
                }
            }
        }
    }
}

impl RecordBatchStream for GroupedBatchStream {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

impl SinkWriteContext<'_> {
    pub fn is_grouped(&self) -> bool {
        !self.wal_refs.is_empty()
    }

    pub fn validate_grouped<W: SinkWriteSupport>(&self) -> Result<(), SinkWriteRejection> {
        if !self.is_grouped() {
            return Ok(());
        }
        if self.idempotency_key.is_empty() {
            return Err(SinkWriteRejection::MissingIdempotencyKey);
        }
        if W::GROUPING == SinkGroupingSupport::None {
            return Err(SinkWriteRejection::UnsupportedGrouping {
                grouping: W::GROUPING,
            });
        }
        if matches!(self.write_semantics, SinkWriteSemantics::ExactOnce) && !W::EXACT_ONCE_ALLOWED {
            return Err(SinkWriteRejection::UnsupportedExactOnce);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SinkWriteOutcome {
    Applied,
    AlreadyApplied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SinkWriteRejection {
    MissingIdempotencyKey,
    MissingWalRefs,
    UnsupportedGrouping { grouping: SinkGroupingSupport },
    UnsupportedExactOnce,
}

impl std::fmt::Display for SinkWriteRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingIdempotencyKey => write!(f, "grouped sink write missing idempotency key"),
            Self::MissingWalRefs => write!(f, "grouped sink write missing WAL references"),
            Self::UnsupportedGrouping { grouping } => {
                write!(f, "sink does not support grouped writes: {grouping:?}")
            }
            Self::UnsupportedExactOnce => {
                write!(f, "sink does not support exact-once grouped writes")
            }
        }
    }
}

impl std::error::Error for SinkWriteRejection {}

pub trait SinkWriteSupport: 'static {
    const RETRY: SinkRetrySemantics;
    const GROUPING: SinkGroupingSupport;
    const EXACT_ONCE_ALLOWED: bool;
    const CAN_RETURN_ALREADY_APPLIED: bool;
    const BOUNDED_GROUPED_STREAM: bool;
    const GROUPED_CONTRACT: GroupedSinkContract;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupedSinkContract {
    BoundedObjectWrite,
    BoundedTransactionalApply,
    BoundedAtLeastOnceEmit,
    DebugDiscard,
    Unsupported,
}

pub struct DeterministicObjectOverwrite;
impl SinkWriteSupport for DeterministicObjectOverwrite {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::DeterministicOverwrite;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::CdcEncodedBatches;
    const EXACT_ONCE_ALLOWED: bool = false;
    const CAN_RETURN_ALREADY_APPLIED: bool = true;
    const BOUNDED_GROUPED_STREAM: bool = true;
    const GROUPED_CONTRACT: GroupedSinkContract = GroupedSinkContract::BoundedObjectWrite;
}

pub struct TransactionalTableCommit;
impl SinkWriteSupport for TransactionalTableCommit {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::TransactionalIdempotent;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::FinalStateBatches;
    const EXACT_ONCE_ALLOWED: bool = true;
    const CAN_RETURN_ALREADY_APPLIED: bool = true;
    const BOUNDED_GROUPED_STREAM: bool = true;
    const GROUPED_CONTRACT: GroupedSinkContract = GroupedSinkContract::BoundedTransactionalApply;
}

pub struct FinalStateIdempotentApply;
impl SinkWriteSupport for FinalStateIdempotentApply {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::FinalStateIdempotent;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::FinalStateBatches;
    const EXACT_ONCE_ALLOWED: bool = true;
    const CAN_RETURN_ALREADY_APPLIED: bool = true;
    const BOUNDED_GROUPED_STREAM: bool = true;
    const GROUPED_CONTRACT: GroupedSinkContract = GroupedSinkContract::BoundedTransactionalApply;
}

pub struct AtLeastOnceMessageDelivery;
impl SinkWriteSupport for AtLeastOnceMessageDelivery {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::AtLeastOnce;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::CdcEncodedBatches;
    const EXACT_ONCE_ALLOWED: bool = false;
    const CAN_RETURN_ALREADY_APPLIED: bool = false;
    const BOUNDED_GROUPED_STREAM: bool = true;
    const GROUPED_CONTRACT: GroupedSinkContract = GroupedSinkContract::BoundedAtLeastOnceEmit;
}

pub struct NonRetryableDebugOutput;
impl SinkWriteSupport for NonRetryableDebugOutput {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::NonRetryable;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::None;
    const EXACT_ONCE_ALLOWED: bool = false;
    const CAN_RETURN_ALREADY_APPLIED: bool = false;
    const BOUNDED_GROUPED_STREAM: bool = false;
    const GROUPED_CONTRACT: GroupedSinkContract = GroupedSinkContract::Unsupported;
}

pub struct SftpAtomicRename;
impl SinkWriteSupport for SftpAtomicRename {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::DeterministicOverwrite;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::CdcEncodedBatches;
    const EXACT_ONCE_ALLOWED: bool = false;
    const CAN_RETURN_ALREADY_APPLIED: bool = true;
    const BOUNDED_GROUPED_STREAM: bool = true;
    const GROUPED_CONTRACT: GroupedSinkContract = GroupedSinkContract::BoundedObjectWrite;
}

pub struct SftpAtLeastOnce;
impl SinkWriteSupport for SftpAtLeastOnce {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::AtLeastOnce;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::CdcEncodedBatches;
    const EXACT_ONCE_ALLOWED: bool = false;
    const CAN_RETURN_ALREADY_APPLIED: bool = false;
    const BOUNDED_GROUPED_STREAM: bool = true;
    const GROUPED_CONTRACT: GroupedSinkContract = GroupedSinkContract::BoundedAtLeastOnceEmit;
}

pub trait SinkSpec: 'static {
    const NAME: &'static str;
    const CAPABILITY: SinkCapability;
    type WriteSupport: SinkWriteSupport;
}

pub trait HasSinkSpec {
    type Spec: SinkSpec;
}

pub trait SchemaSinkSpec: 'static {
    const NAME: &'static str;
}

pub trait HasSchemaSinkSpec {
    type Spec: SchemaSinkSpec;
}

pub struct ConfiguredSink<S: SinkSpec, P> {
    pub plugin: P,
    _spec: PhantomData<S>,
}

impl<S: SinkSpec, P> ConfiguredSink<S, P> {
    pub fn new(plugin: P) -> Self {
        Self {
            plugin,
            _spec: PhantomData,
        }
    }
}

#[macro_export]
macro_rules! declare_sink_spec {
    ($spec:ident, $plugin:ty, $capability:path, $support:ty) => {
        const _: () = {
            assert!(!$capability.grouping_support.is_none());
            assert!($capability
                .grouping_support
                .equals(<$support as $crate::plugins::SinkWriteSupport>::GROUPING));
            assert!($capability
                .retry_semantics
                .equals(<$support as $crate::plugins::SinkWriteSupport>::RETRY));
            assert!(
                $capability.grouping_support.is_none()
                    || !$capability.retry_semantics.requires_idempotent_replay()
                    || <$support as $crate::plugins::SinkWriteSupport>::CAN_RETURN_ALREADY_APPLIED
            );
            assert!(
                $capability.grouping_support.is_none()
                    || <$support as $crate::plugins::SinkWriteSupport>::BOUNDED_GROUPED_STREAM
            );
        };

        pub struct $spec;

        impl $crate::plugins::SinkSpec for $spec {
            const NAME: &'static str = $capability.name;
            const CAPABILITY: $crate::plugins::cdc::SinkCapability = $capability;
            type WriteSupport = $support;
        }

        impl $crate::plugins::HasSinkSpec for $plugin {
            type Spec = $spec;
        }
    };
}

#[macro_export]
macro_rules! declare_schema_sink_spec {
    ($spec:ident, $plugin:ty, $name:expr) => {
        pub struct $spec;

        impl $crate::plugins::SchemaSinkSpec for $spec {
            const NAME: &'static str = $name;
        }

        impl $crate::plugins::HasSchemaSinkSpec for $plugin {
            type Spec = $spec;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::cdc::source_capabilities;
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn runtime_ref() -> crate::runtime_plugins::protocol::RuntimeWalPartRef {
        crate::runtime_plugins::protocol::RuntimeWalPartRef {
            segment_id: "seg-1".to_string(),
            source: "local".to_string(),
            start: 0,
            len: 10,
            sink_ref: "primary".to_string(),
            namespace: "users".to_string(),
            partition: "p=1".to_string(),
            time: Some(10),
            schema_fingerprint: "schema".to_string(),
            cdc_meta_hash: None,
        }
    }

    #[test]
    fn source_cdc_mode_uses_snake_case_config_strings() {
        let mode: SourceCdcMode = serde_json::from_str("\"snapshot_then_cdc\"").unwrap();
        assert_eq!(mode, SourceCdcMode::SnapshotThenCdc);

        let mode: SourceCdcMode = serde_json::from_str("\"cdc_only\"").unwrap();
        assert_eq!(mode, SourceCdcMode::CdcOnly);
    }

    #[test]
    fn source_cdc_contract_exposes_capability_only_for_streaming_modes() {
        let snapshot = SourceExecutionContract::configurable_cdc(
            SourceCdcMode::Snapshot,
            &source_capabilities::POSTGRES,
            SourceOnceContract::Finite,
        );
        assert!(snapshot.cdc.capability().is_none());

        let cdc = SourceExecutionContract::configurable_cdc(
            SourceCdcMode::SnapshotThenCdc,
            &source_capabilities::POSTGRES,
            SourceOnceContract::HostIdleBounded,
        );
        assert!(cdc.cdc.capability().is_some());
    }

    #[test]
    fn grouped_context_validation_enforces_support_marker() {
        let ctx = SinkWriteContext {
            filename: "namespace=users-c=abc".to_string(),
            compaction_id: "abc".to_string(),
            idempotency_key: "abc".to_string(),
            wal_refs: vec![runtime_ref()],
            write_semantics: SinkWriteSemantics::IdempotentAtLeastOnce,
            schema_fingerprint: "schema".to_string(),
            cdc_ctx: None,
            source_contract: None,
        };

        assert!(ctx
            .validate_grouped::<DeterministicObjectOverwrite>()
            .is_ok());
        assert!(matches!(
            ctx.validate_grouped::<NonRetryableDebugOutput>(),
            Err(SinkWriteRejection::UnsupportedGrouping { .. })
        ));
    }

    #[test]
    fn grouped_chunk_context_preserves_wal_refs_for_idempotency() {
        let sink_ctx = SinkWriteContext {
            filename: "namespace=users-c=abc".to_string(),
            compaction_id: "abc".to_string(),
            idempotency_key: "abc".to_string(),
            wal_refs: vec![runtime_ref()],
            write_semantics: SinkWriteSemantics::IdempotentAtLeastOnce,
            schema_fingerprint: "schema".to_string(),
            cdc_ctx: None,
            source_contract: None,
        };
        let grouped = GroupedSinkWriteContext::try_from(sink_ctx).unwrap();

        let chunk_ctx = grouped.chunk_sink_write_context_with_cdc(1, false, None);

        assert!(chunk_ctx.is_grouped());
        assert_eq!(chunk_ctx.wal_refs, grouped.wal_refs.clone_vec());
        assert_eq!(chunk_ctx.compaction_id, "abc-chunk-00000001");
        assert_eq!(chunk_ctx.idempotency_key, "abc-chunk-00000001");
    }

    #[tokio::test]
    async fn grouped_batch_reader_emits_bounded_chunks_with_offsets() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch_one =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1, 2]))])
                .unwrap();
        let batch_two =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![3]))])
                .unwrap();
        let stream: SendableRecordBatchStream = Box::pin(ChunkRecordBatchStream {
            schema,
            batches: vec![batch_one, batch_two].into_iter(),
        });
        let refs = GroupedWalRefs::new(vec![runtime_ref()]).unwrap();
        let key = GroupedWalPartitionKey::from_refs(&refs, "schema", None);
        let mut reader = GroupedBatchReader::new(
            stream,
            key,
            GroupedBatchReaderConfig {
                max_rows: 2,
                max_bytes: usize::MAX,
            },
        );

        let first = reader.next_chunk().await.unwrap().unwrap();
        assert_eq!(first.chunk_index, 0);
        assert_eq!(first.row_offset, 0);
        assert_eq!(first.rows, 2);
        assert!(!first.final_chunk);

        let second = reader.next_chunk().await.unwrap().unwrap();
        assert_eq!(second.chunk_index, 1);
        assert_eq!(second.row_offset, 2);
        assert_eq!(second.rows, 1);
        assert!(second.final_chunk);
        assert!(reader.next_chunk().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn grouped_batch_reader_marks_exact_limit_chunk_final_at_eof() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1, 2]))])
                .unwrap();
        let stream: SendableRecordBatchStream = Box::pin(ChunkRecordBatchStream {
            schema,
            batches: vec![batch].into_iter(),
        });
        let refs = GroupedWalRefs::new(vec![runtime_ref()]).unwrap();
        let key = GroupedWalPartitionKey::from_refs(&refs, "schema", None);
        let mut reader = GroupedBatchReader::new(
            stream,
            key,
            GroupedBatchReaderConfig {
                max_rows: 2,
                max_bytes: usize::MAX,
            },
        );

        let chunk = reader.next_chunk().await.unwrap().unwrap();
        assert_eq!(chunk.rows, 2);
        assert!(chunk.final_chunk);
        assert!(reader.next_chunk().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn grouped_batch_reader_stream_flattens_lazily_in_row_order() {
        struct CountingBatchStream {
            schema: SchemaRef,
            batches: std::vec::IntoIter<RecordBatch>,
            consumed: Arc<AtomicUsize>,
        }

        impl futures::Stream for CountingBatchStream {
            type Item = Result<RecordBatch, DataFusionError>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                let next = self.batches.next();
                if next.is_some() {
                    self.consumed.fetch_add(1, AtomicOrdering::Relaxed);
                }
                Poll::Ready(next.map(Ok))
            }
        }

        impl RecordBatchStream for CountingBatchStream {
            fn schema(&self) -> SchemaRef {
                Arc::clone(&self.schema)
            }
        }

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batches = [1_i64, 2, 3]
            .into_iter()
            .map(|value| {
                RecordBatch::try_new(
                    schema.clone(),
                    vec![Arc::new(Int64Array::from(vec![value]))],
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let consumed = Arc::new(AtomicUsize::new(0));
        let source: SendableRecordBatchStream = Box::pin(CountingBatchStream {
            schema,
            batches: batches.into_iter(),
            consumed: consumed.clone(),
        });
        let refs = GroupedWalRefs::new(vec![runtime_ref()]).unwrap();
        let key = GroupedWalPartitionKey::from_refs(&refs, "schema", None);
        let reader = GroupedBatchReader::new(
            source,
            key,
            GroupedBatchReaderConfig {
                max_rows: 1,
                max_bytes: usize::MAX,
            },
        );

        let (mut stream, progress) = reader.into_stream();
        assert_eq!(consumed.load(AtomicOrdering::Relaxed), 0);

        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(
            first
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0),
            1
        );
        assert!(
            consumed.load(AtomicOrdering::Relaxed) < 3,
            "flattening must not precollect the grouped envelope"
        );

        let mut values = vec![1];
        while let Some(batch) = stream.next().await {
            let batch = batch.unwrap();
            values.push(
                batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .value(0),
            );
        }
        assert_eq!(values, vec![1, 2, 3]);
        assert_eq!(progress.transport_chunk_count(), 3);
    }

    #[test]
    fn grouped_context_slices_cdc_metadata_by_chunk_offset() {
        use crate::plugins::cdc::{MutationKind, SyncContext, WalPartMeta, WalRowMeta};

        let rows = (0..4)
            .map(|idx| WalRowMeta {
                mutation: MutationKind::Insert,
                event_id: vec![idx],
                order_token: vec![idx],
            })
            .collect::<Vec<_>>();
        let cdc_ctx = SyncContext {
            part_meta: WalPartMeta::cdc(rows, 4).unwrap(),
            contract: None,
        };
        let sink_ctx = SinkWriteContext {
            filename: "namespace=users-c=abc".to_string(),
            compaction_id: "abc".to_string(),
            idempotency_key: "abc".to_string(),
            wal_refs: vec![runtime_ref()],
            write_semantics: SinkWriteSemantics::IdempotentAtLeastOnce,
            schema_fingerprint: "schema".to_string(),
            cdc_ctx: Some(&cdc_ctx),
            source_contract: None,
        };
        let grouped = GroupedSinkWriteContext::try_from(sink_ctx).unwrap();

        let sliced = grouped
            .chunk_cdc_context_for_range(1, 2, 2)
            .unwrap()
            .expect("cdc context");
        assert_eq!(sliced.part_meta.row_count, 2);
        assert_eq!(sliced.part_meta.rows[0].event_id, vec![2]);
        assert_eq!(sliced.part_meta.rows[1].event_id, vec![3]);
    }
}

/// Writes record batches to a destination (S3, disk, database, etc.).
///
/// This trait handles the DATA plane only. Schema/DDL operations belong
/// to [`SchemaSink`]. The schema_sink pairing is declared on the config
/// entry, not on this trait.
#[async_trait]
pub trait DataSink: Send + Sync {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&SyncContext>,
    ) -> Result<(), std::io::Error>;

    async fn sync_with_context(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<(), std::io::Error> {
        self.sync(stream, ctx.filename, ctx.cdc_ctx).await
    }

    async fn sync_with_context_result(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        if ctx.is_grouped() {
            let grouped = GroupedSinkWriteContext::try_from(ctx)
                .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err))?;
            let reader = GroupedBatchReader::new(
                stream,
                grouped.grouping_key.clone(),
                GroupedBatchReaderConfig::default(),
            );
            return self.sync_grouped(reader, grouped).await;
        }
        self.sync_with_context(stream, ctx).await?;
        Ok(SinkWriteOutcome::Applied)
    }

    async fn sync_grouped(
        &self,
        reader: GroupedBatchReader,
        ctx: GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error>;

    /// Return the compile-time capability descriptor for this sink.
    fn capability(&self) -> &'static SinkCapability;

    /// Resolve capability for a persisted WAL `sink_ref`. Multi-sink routers override this.
    fn capability_for_sink_ref(&self, sink_ref: &str) -> Option<&'static SinkCapability> {
        let _ = sink_ref;
        Some(self.capability())
    }

    async fn install_schema_state(
        &self,
        _schema_version: u64,
        _namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

/// Creates or updates schema definitions at a destination (Glue catalog,
/// SQL DDL, REST API, Iceberg catalog, etc.).
///
/// Paired with a [`DataSink`] via the sink's config entry. The schema sync
/// background worker calls this independently of data writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaSyncRequest<'a> {
    pub namespace: &'a str,
    pub compaction_id: &'a str,
    pub source_contract: Option<&'a SourceNamespaceContract>,
}

#[async_trait]
pub trait SchemaSink: Send + Sync {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error>;

    async fn sync_schema_request(
        &self,
        request: SchemaSyncRequest<'_>,
        metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error> {
        let _ = request.source_contract;
        self.sync_schema(request.namespace, metadata).await
    }

    async fn install_schema_state(
        &self,
        _schema_version: u64,
        _namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

/// Reads schema definitions from an external catalog or system.
///
/// Not yet implemented -- trait is defined to establish the contract.
///
/// Example future implementations:
///
/// - **GlueSchemaSource**: reads table/column definitions from AWS Glue
///   (`get_table`, `get_database`) for diffing before updates.
/// - **PostgresSchemaSource**: reads `information_schema.columns` to
///   discover current table shapes.
/// - **IcebergSchemaSource**: reads Iceberg table metadata from a REST catalog.
/// - **ParquetSchemaSource**: reads Arrow schema from a Parquet file footer.
///
/// Example implementation sketch:
///
/// ```ignore
/// pub struct GlueSchemaSource {
///     glue_client: aws_sdk_glue::Client,
///     database: String,
/// }
///
/// #[async_trait::async_trait]
/// impl SchemaSource for GlueSchemaSource {
///     async fn read_schema(
///         &self,
///         namespace: &str,
///     ) -> Result<Option<OutputMetadata>, std::io::Error> {
///         let resp = self.glue_client.get_table()
///             .database_name(&self.database)
///             .name(namespace)
///             .send().await
///             .map_err(|e| std::io::Error::other(e.to_string()))?;
///         todo!("map resp.table().columns() -> OutputMetadata")
///     }
/// }
/// ```
#[async_trait]
pub trait SchemaSource: Send + Sync {
    async fn read_schema(&self, namespace: &str) -> Result<Option<OutputMetadata>, std::io::Error>;
}
