//! Backend-agnostic, bounded streaming writes of Arrow batches to object storage.
//!
//! Parquet encoding runs on one blocking task. Encoded parts cross a bounded
//! channel to async multipart uploads, so neither the encoded object nor all of
//! its parts are retained in memory.

use std::collections::BTreeMap;
use std::error::Error;
use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::stream::FuturesUnordered;
use futures::{Stream, StreamExt};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use serde_derive::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

/// Stable inputs passed to a backend when beginning an object write.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObjectWriteRequest {
    pub object_key: String,
    pub content_type: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl ObjectWriteRequest {
    pub fn new(object_key: impl Into<String>) -> Self {
        Self {
            object_key: object_key.into(),
            content_type: "application/vnd.apache.parquet".to_string(),
            metadata: BTreeMap::new(),
        }
    }
}

/// Opaque resumable/multipart upload identity returned by a backend.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MultipartUpload {
    pub upload_id: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Metadata supplied by a backend after one numbered part is stored.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PartMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Canonical, ordered metadata for one uploaded part.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObjectPartReceipt {
    pub part_number: u32,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Object-level metadata returned after ordered parts are committed.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompletionMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Canonical receipt intended to bridge future sink apply/grouped write receipts.
///
/// It deliberately contains no wall-clock field: identical input and backend
/// metadata produce identical receipt statistics.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObjectWriteReceipt {
    pub version: u32,
    pub object_key: String,
    pub upload_id: String,
    pub rows: u64,
    pub bytes: u64,
    pub transport_chunk_count: u32,
    pub parts: Vec<ObjectPartReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub backend_metadata: BTreeMap<String, String>,
}

/// Cloud-neutral multipart/resumable object storage operations.
#[async_trait]
pub trait ObjectWriteBackend: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    async fn begin(&self, request: &ObjectWriteRequest) -> Result<MultipartUpload, Self::Error>;

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        bytes: Bytes,
    ) -> Result<PartMetadata, Self::Error>;

    /// `parts` is always sorted by ascending, one-based part number.
    async fn complete(
        &self,
        upload: &MultipartUpload,
        parts: &[ObjectPartReceipt],
    ) -> Result<CompletionMetadata, Self::Error>;

    async fn abort(&self, upload: &MultipartUpload) -> Result<(), Self::Error>;
}

/// Bounds the producer queues and multipart upload concurrency.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectWriterConfig {
    pub part_size: usize,
    pub batch_channel_capacity: usize,
    pub byte_channel_capacity: usize,
    pub max_in_flight_parts: usize,
}

impl Default for ObjectWriterConfig {
    fn default() -> Self {
        Self {
            part_size: 8 * 1024 * 1024,
            batch_channel_capacity: 2,
            byte_channel_capacity: 2,
            max_in_flight_parts: 2,
        }
    }
}

impl ObjectWriterConfig {
    pub fn validate(&self) -> Result<(), ObjectWriteError> {
        if self.part_size == 0 {
            return Err(ObjectWriteError::InvalidConfig(
                "part_size must be greater than zero".to_string(),
            ));
        }
        if self.batch_channel_capacity == 0 {
            return Err(ObjectWriteError::InvalidConfig(
                "batch_channel_capacity must be greater than zero".to_string(),
            ));
        }
        if self.byte_channel_capacity == 0 {
            return Err(ObjectWriteError::InvalidConfig(
                "byte_channel_capacity must be greater than zero".to_string(),
            ));
        }
        if self.max_in_flight_parts == 0 {
            return Err(ObjectWriteError::InvalidConfig(
                "max_in_flight_parts must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }

    /// Maximum encoded bytes owned by the transport pipeline, excluding
    /// Arrow/Parquet row-group buffers and buffers retained by a backend.
    ///
    /// This consists of the byte channel, in-flight upload bodies, and the
    /// producer's current part.
    pub fn transport_memory_bound_bytes(&self) -> usize {
        self.part_size.saturating_mul(
            self.byte_channel_capacity
                .saturating_add(self.max_in_flight_parts)
                .saturating_add(1),
        )
    }
}

#[derive(Debug, Error)]
pub enum ObjectWriteError {
    #[error("object key must not be empty")]
    InvalidObjectKey,
    #[error("invalid object writer configuration: {0}")]
    InvalidConfig(String),
    #[error("record batch stream failed: {0}")]
    Input(String),
    #[error("Parquet producer failed: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("backend {operation} failed: {source}")]
    Backend {
        operation: &'static str,
        #[source]
        source: BoxError,
    },
    #[error("object write was cancelled")]
    Cancelled,
    #[error("refusing to commit an object with no rows")]
    EmptyInput,
    #[error("object requires more than u32::MAX parts")]
    TooManyParts,
    #[error("row or byte statistics overflowed u64")]
    StatisticsOverflow,
    #[error("{task} task failed: {message}")]
    TaskJoin { task: &'static str, message: String },
}

impl ObjectWriteError {
    pub fn input(error: impl std::fmt::Display) -> Self {
        Self::Input(error.to_string())
    }

    fn backend<E>(operation: &'static str, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Backend {
            operation,
            source: Box::new(source),
        }
    }
}

/// Build a deterministic object key from a stable identity.
pub fn deterministic_object_key(
    prefix: &str,
    identity: &str,
    extension: &str,
) -> Result<String, ObjectWriteError> {
    let identity = identity.trim();
    if identity.is_empty() {
        return Err(ObjectWriteError::InvalidObjectKey);
    }

    let prefix = prefix.trim_matches('/');
    let extension = extension.trim_start_matches('.');
    let name = if extension.is_empty() {
        identity.to_string()
    } else {
        format!("{identity}.{extension}")
    };

    Ok(if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    })
}

/// A configured, single-use object write.
pub struct ObjectWriteSession<B> {
    backend: Arc<B>,
    request: ObjectWriteRequest,
    config: ObjectWriterConfig,
}

impl<B> ObjectWriteSession<B>
where
    B: ObjectWriteBackend,
{
    pub fn new(
        backend: Arc<B>,
        request: ObjectWriteRequest,
        config: ObjectWriterConfig,
    ) -> Result<Self, ObjectWriteError> {
        if request.object_key.trim().is_empty() {
            return Err(ObjectWriteError::InvalidObjectKey);
        }
        config.validate()?;
        Ok(Self {
            backend,
            request,
            config,
        })
    }

    pub fn request(&self) -> &ObjectWriteRequest {
        &self.request
    }

    pub fn config(&self) -> &ObjectWriterConfig {
        &self.config
    }

    /// Start the bounded Parquet producer and multipart consumer.
    ///
    /// The stream is fed through a bounded RecordBatch queue. Callers can apply
    /// ordering transforms before this boundary and supply the resulting
    /// `WriterProperties` without coupling this crate to sink/runtime layers.
    pub fn write_parquet<S>(
        self,
        schema: SchemaRef,
        writer_properties: WriterProperties,
        batches: S,
    ) -> ObjectWriteOperation
    where
        S: Stream<Item = Result<RecordBatch, ObjectWriteError>> + Send + 'static,
    {
        let (cancel, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(coordinate_write(
            self.backend,
            self.request,
            self.config,
            schema,
            writer_properties,
            batches,
            cancel.clone(),
            cancel_rx,
        ));
        ObjectWriteOperation { cancel, task }
    }
}

/// Awaitable write operation. Dropping it requests cancellation; the detached
/// coordinator then stops and joins producers before aborting the backend.
#[must_use = "dropping the operation cancels and aborts the object write"]
pub struct ObjectWriteOperation {
    cancel: watch::Sender<bool>,
    task: JoinHandle<Result<ObjectWriteReceipt, ObjectWriteError>>,
}

impl ObjectWriteOperation {
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

impl Future for ObjectWriteOperation {
    type Output = Result<ObjectWriteReceipt, ObjectWriteError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.task).poll(cx) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(error)) => Poll::Ready(Err(ObjectWriteError::TaskJoin {
                task: "coordinator",
                message: error.to_string(),
            })),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ObjectWriteOperation {
    fn drop(&mut self) {
        if !self.task.is_finished() {
            let _ = self.cancel.send(true);
        }
    }
}

struct ProducerStats {
    rows: u64,
}

async fn coordinate_write<B, S>(
    backend: Arc<B>,
    request: ObjectWriteRequest,
    config: ObjectWriterConfig,
    schema: SchemaRef,
    writer_properties: WriterProperties,
    batches: S,
    cancel: watch::Sender<bool>,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<ObjectWriteReceipt, ObjectWriteError>
where
    B: ObjectWriteBackend,
    S: Stream<Item = Result<RecordBatch, ObjectWriteError>> + Send + 'static,
{
    let upload = tokio::select! {
        biased;
        _ = wait_for_cancellation(&mut cancel_rx) => return Err(ObjectWriteError::Cancelled),
        result = backend.begin(&request) => result.map_err(|error| ObjectWriteError::backend("begin", error))?,
    };

    let result = write_started_upload(
        backend.clone(),
        &upload,
        &request,
        &config,
        schema,
        writer_properties,
        batches,
        cancel,
        cancel_rx,
    )
    .await;

    if result.is_err() {
        // There is one owner of the begun upload and one abort call site.
        let _ = backend.abort(&upload).await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn write_started_upload<B, S>(
    backend: Arc<B>,
    upload: &MultipartUpload,
    request: &ObjectWriteRequest,
    config: &ObjectWriterConfig,
    schema: SchemaRef,
    writer_properties: WriterProperties,
    batches: S,
    cancel: watch::Sender<bool>,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<ObjectWriteReceipt, ObjectWriteError>
where
    B: ObjectWriteBackend,
    S: Stream<Item = Result<RecordBatch, ObjectWriteError>> + Send + 'static,
{
    let (batch_tx, batch_rx) = mpsc::channel(config.batch_channel_capacity);
    let (byte_tx, mut byte_rx) = mpsc::channel(config.byte_channel_capacity);

    let feeder_cancel = cancel_rx.clone();
    let feeder: JoinHandle<Result<(), ObjectWriteError>> =
        tokio::spawn(feed_batches(batches, batch_tx, feeder_cancel));
    let part_size = config.part_size;
    let producer: JoinHandle<Result<ProducerStats, ObjectWriteError>> =
        tokio::task::spawn_blocking(move || {
            produce_parquet(schema, writer_properties, batch_rx, byte_tx, part_size)
        });

    let mut uploads = FuturesUnordered::new();
    let mut completed_parts = Vec::new();
    let mut next_part_number = 1_u32;
    let mut byte_channel_closed = false;

    let upload_result: Result<(), ObjectWriteError> = loop {
        if byte_channel_closed && uploads.is_empty() {
            break Ok(());
        }

        tokio::select! {
            biased;
            _ = wait_for_cancellation(&mut cancel_rx) => {
                break Err(ObjectWriteError::Cancelled);
            }
            uploaded = uploads.next(), if !uploads.is_empty() => {
                let Some((part_number, byte_count, result)): Option<(
                    u32,
                    u64,
                    Result<PartMetadata, B::Error>,
                )> = uploaded else {
                    continue;
                };
                let metadata = match result {
                    Ok(metadata) => metadata,
                    Err(error) => break Err(ObjectWriteError::backend("upload_part", error)),
                };
                completed_parts.push(ObjectPartReceipt {
                    part_number,
                    bytes: byte_count,
                    etag: metadata.etag,
                    checksum: metadata.checksum,
                    metadata: metadata.metadata,
                });
            }
            encoded = byte_rx.recv(), if !byte_channel_closed
                && uploads.len() < config.max_in_flight_parts => {
                match encoded {
                    Some(bytes) => {
                        let part_number = next_part_number;
                        next_part_number = match next_part_number.checked_add(1) {
                            Some(next) => next,
                            None => break Err(ObjectWriteError::TooManyParts),
                        };
                        let byte_count = bytes.len() as u64;
                        let backend = backend.clone();
                        let upload = upload.clone();
                        uploads.push(async move {
                            let result = backend
                                .upload_part(&upload, part_number, bytes)
                                .await;
                            (part_number, byte_count, result)
                        });
                    }
                    None => byte_channel_closed = true,
                }
            }
        }
    };

    if let Err(error) = upload_result {
        let _ = cancel.send(true);
        drop(byte_rx);
        drop(uploads);
        let _ = feeder.await;
        let _ = producer.await;
        return Err(error);
    }

    let feeder_result = feeder.await.map_err(|error| ObjectWriteError::TaskJoin {
        task: "RecordBatch feeder",
        message: error.to_string(),
    })?;
    let producer_result = producer.await.map_err(|error| ObjectWriteError::TaskJoin {
        task: "Parquet producer",
        message: error.to_string(),
    })?;
    feeder_result?;
    let stats = producer_result?;

    completed_parts.sort_unstable_by_key(|part| part.part_number);
    if stats.rows == 0 || completed_parts.is_empty() {
        return Err(ObjectWriteError::EmptyInput);
    }

    let bytes = completed_parts.iter().try_fold(0_u64, |total, part| {
        total
            .checked_add(part.bytes)
            .ok_or(ObjectWriteError::StatisticsOverflow)
    })?;
    let transport_chunk_count =
        u32::try_from(completed_parts.len()).map_err(|_| ObjectWriteError::TooManyParts)?;

    let completion = tokio::select! {
        biased;
        _ = wait_for_cancellation(&mut cancel_rx) => return Err(ObjectWriteError::Cancelled),
        result = backend.complete(upload, &completed_parts) => {
            result.map_err(|error| ObjectWriteError::backend("complete", error))?
        }
    };

    let mut backend_metadata = upload.metadata.clone();
    backend_metadata.extend(completion.metadata);
    Ok(ObjectWriteReceipt {
        version: 1,
        object_key: request.object_key.clone(),
        upload_id: upload.upload_id.clone(),
        rows: stats.rows,
        bytes,
        transport_chunk_count,
        parts: completed_parts,
        etag: completion.etag,
        checksum: completion.checksum,
        version_id: completion.version_id,
        backend_metadata,
    })
}

async fn feed_batches<S>(
    batches: S,
    batch_tx: mpsc::Sender<RecordBatch>,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<(), ObjectWriteError>
where
    S: Stream<Item = Result<RecordBatch, ObjectWriteError>> + Send + 'static,
{
    let mut batches = Box::pin(batches);
    loop {
        let next = tokio::select! {
            biased;
            _ = wait_for_cancellation(&mut cancel_rx) => return Ok(()),
            next = batches.next() => next,
        };
        let Some(batch) = next else {
            return Ok(());
        };
        let batch = batch?;
        let sent = tokio::select! {
            biased;
            _ = wait_for_cancellation(&mut cancel_rx) => return Ok(()),
            sent = batch_tx.send(batch) => sent,
        };
        if sent.is_err() {
            return Ok(());
        }
    }
}

fn produce_parquet(
    schema: SchemaRef,
    writer_properties: WriterProperties,
    mut batch_rx: mpsc::Receiver<RecordBatch>,
    byte_tx: mpsc::Sender<Bytes>,
    part_size: usize,
) -> Result<ProducerStats, ObjectWriteError> {
    let channel_writer = ChannelPartWriter::new(byte_tx, part_size);
    let mut parquet_writer = ArrowWriter::try_new(channel_writer, schema, Some(writer_properties))?;
    let mut rows = 0_u64;

    while let Some(batch) = batch_rx.blocking_recv() {
        rows = rows
            .checked_add(batch.num_rows() as u64)
            .ok_or(ObjectWriteError::StatisticsOverflow)?;
        parquet_writer.write(&batch)?;
    }
    parquet_writer.close()?;
    Ok(ProducerStats { rows })
}

async fn wait_for_cancellation(cancel_rx: &mut watch::Receiver<bool>) {
    if *cancel_rx.borrow() {
        return;
    }
    while cancel_rx.changed().await.is_ok() {
        if *cancel_rx.borrow() {
            return;
        }
    }
}

struct ChannelPartWriter {
    sender: mpsc::Sender<Bytes>,
    part_size: usize,
    buffer: BytesMut,
}

impl ChannelPartWriter {
    fn new(sender: mpsc::Sender<Bytes>, part_size: usize) -> Self {
        Self {
            sender,
            part_size,
            buffer: BytesMut::with_capacity(part_size),
        }
    }

    fn send_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let len = self.buffer.len();
        let bytes = self.buffer.split_to(len).freeze();
        self.sender
            .blocking_send(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "encoded byte consumer stopped"))
    }
}

impl Write for ChannelPartWriter {
    fn write(&mut self, mut input: &[u8]) -> io::Result<usize> {
        let input_len = input.len();
        while !input.is_empty() {
            let available = self.part_size - self.buffer.len();
            let copy_len = available.min(input.len());
            self.buffer.extend_from_slice(&input[..copy_len]);
            input = &input[copy_len..];
            if self.buffer.len() == self.part_size {
                self.send_buffer()?;
            }
        }
        Ok(input_len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send_buffer()
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use std::sync::{mpsc as std_mpsc, Mutex};
    use std::time::Duration;

    use arrow_array::{Int32Array, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use futures::stream;
    use parquet::basic::Compression;
    use tokio::time::{sleep, timeout};

    use super::*;

    #[derive(Debug, Error)]
    enum MockError {
        #[error("configured failure for part {0}")]
        Part(u32),
    }

    #[derive(Default)]
    struct MockBackend {
        begins: AtomicUsize,
        aborts: AtomicUsize,
        completes: AtomicUsize,
        uploads_started: AtomicUsize,
        uploads_in_flight: AtomicUsize,
        max_uploads_in_flight: AtomicUsize,
        fail_part: AtomicU32,
        delay_uploads: AtomicBool,
        block_uploads: AtomicBool,
        complete_parts: Mutex<Vec<u32>>,
        uploaded_sizes: Mutex<Vec<(u32, usize)>>,
    }

    struct InFlightGuard<'a>(&'a AtomicUsize);

    impl Drop for InFlightGuard<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ObjectWriteBackend for MockBackend {
        type Error = MockError;

        async fn begin(
            &self,
            _request: &ObjectWriteRequest,
        ) -> Result<MultipartUpload, Self::Error> {
            self.begins.fetch_add(1, Ordering::SeqCst);
            Ok(MultipartUpload {
                upload_id: "mock-upload".to_string(),
                metadata: BTreeMap::from([("region".to_string(), "test-1".to_string())]),
            })
        }

        async fn upload_part(
            &self,
            _upload: &MultipartUpload,
            part_number: u32,
            bytes: Bytes,
        ) -> Result<PartMetadata, Self::Error> {
            self.uploads_started.fetch_add(1, Ordering::SeqCst);
            self.uploaded_sizes
                .lock()
                .unwrap()
                .push((part_number, bytes.len()));
            let current = self.uploads_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_uploads_in_flight
                .fetch_max(current, Ordering::SeqCst);
            let _in_flight = InFlightGuard(&self.uploads_in_flight);

            if self.block_uploads.load(Ordering::SeqCst) {
                pending::<()>().await;
            }
            if self.delay_uploads.load(Ordering::SeqCst) {
                sleep(Duration::from_millis(1 + u64::from(part_number % 4))).await;
            }
            if self.fail_part.load(Ordering::SeqCst) == part_number {
                return Err(MockError::Part(part_number));
            }

            Ok(PartMetadata {
                etag: Some(format!("part-{part_number}-etag")),
                checksum: Some(format!("part-{part_number}-bytes-{}", bytes.len())),
                metadata: BTreeMap::new(),
            })
        }

        async fn complete(
            &self,
            _upload: &MultipartUpload,
            parts: &[ObjectPartReceipt],
        ) -> Result<CompletionMetadata, Self::Error> {
            self.completes.fetch_add(1, Ordering::SeqCst);
            *self.complete_parts.lock().unwrap() =
                parts.iter().map(|part| part.part_number).collect();
            Ok(CompletionMetadata {
                etag: Some("object-etag".to_string()),
                checksum: Some("object-checksum".to_string()),
                version_id: Some("object-version".to_string()),
                metadata: BTreeMap::from([("storage_class".to_string(), "mock".to_string())]),
            })
        }

        async fn abort(&self, _upload: &MultipartUpload) -> Result<(), Self::Error> {
            self.aborts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn test_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("value", DataType::Utf8, false),
        ]))
    }

    fn test_batch(rows: usize, payload_len: usize) -> RecordBatch {
        let schema = test_schema();
        let values = (0..rows)
            .map(|row| format!("{row:08}-{}", "x".repeat(payload_len)))
            .collect::<Vec<_>>();
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from_iter_values(0..rows as i32)),
                Arc::new(StringArray::from(values)),
            ],
        )
        .unwrap()
    }

    fn writer_properties() -> WriterProperties {
        WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_compression(Compression::UNCOMPRESSED)
            .build()
    }

    fn config(part_size: usize) -> ObjectWriterConfig {
        ObjectWriterConfig {
            part_size,
            batch_channel_capacity: 1,
            byte_channel_capacity: 2,
            max_in_flight_parts: 3,
        }
    }

    fn session(
        backend: Arc<MockBackend>,
        config: ObjectWriterConfig,
    ) -> ObjectWriteSession<MockBackend> {
        ObjectWriteSession::new(
            backend,
            ObjectWriteRequest::new("stable/prefix/apply-0001.parquet"),
            config,
        )
        .unwrap()
    }

    async fn write_batches(
        backend: Arc<MockBackend>,
        batches: Vec<RecordBatch>,
        config: ObjectWriterConfig,
    ) -> Result<ObjectWriteReceipt, ObjectWriteError> {
        session(backend, config)
            .write_parquet(
                test_schema(),
                writer_properties(),
                stream::iter(batches.into_iter().map(Ok)),
            )
            .await
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        timeout(Duration::from_secs(3), async {
            while !condition() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition was not reached");
    }

    #[test]
    fn deterministic_key_normalizes_only_separators() {
        assert_eq!(
            deterministic_object_key("/root/ns/", "apply-0001", ".parquet").unwrap(),
            "root/ns/apply-0001.parquet"
        );
        assert!(matches!(
            deterministic_object_key("root", " ", "parquet"),
            Err(ObjectWriteError::InvalidObjectKey)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uploads_concurrently_and_completes_in_part_order() {
        let backend = Arc::new(MockBackend::default());
        backend.delay_uploads.store(true, Ordering::SeqCst);

        let receipt = write_batches(backend.clone(), vec![test_batch(150, 80)], config(128))
            .await
            .unwrap();

        assert!(receipt.parts.len() > 3);
        assert!(backend.max_uploads_in_flight.load(Ordering::SeqCst) >= 2);
        let expected = (1..=receipt.parts.len() as u32).collect::<Vec<_>>();
        assert_eq!(*backend.complete_parts.lock().unwrap(), expected);
        assert_eq!(
            receipt
                .parts
                .iter()
                .map(|part| part.part_number)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn channel_backpressure_enforces_the_transport_bound() {
        let (byte_tx, mut byte_rx) = mpsc::channel(1);
        let (done_tx, done_rx) = std_mpsc::channel();
        let writer_thread = std::thread::spawn(move || {
            let mut writer = ChannelPartWriter::new(byte_tx, 4);
            writer.write_all(b"abcdefghijkl").unwrap();
            writer.flush().unwrap();
            done_tx.send(()).unwrap();
        });

        sleep(Duration::from_millis(25)).await;
        assert!(done_rx.try_recv().is_err(), "writer must block at capacity");
        assert_eq!(byte_rx.recv().await.unwrap(), Bytes::from_static(b"abcd"));
        sleep(Duration::from_millis(25)).await;
        assert!(done_rx.try_recv().is_err(), "writer must remain bounded");
        assert_eq!(byte_rx.recv().await.unwrap(), Bytes::from_static(b"efgh"));
        assert_eq!(byte_rx.recv().await.unwrap(), Bytes::from_static(b"ijkl"));
        writer_thread.join().unwrap();
        done_rx.recv().unwrap();

        let bounded = ObjectWriterConfig {
            part_size: 4,
            batch_channel_capacity: 1,
            byte_channel_capacity: 1,
            max_in_flight_parts: 1,
        };
        assert_eq!(bounded.transport_memory_bound_bytes(), 12);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uploads_a_small_final_part() {
        let backend = Arc::new(MockBackend::default());
        let one_megabyte = 1024 * 1024;
        let receipt = write_batches(backend, vec![test_batch(3, 8)], config(one_megabyte))
            .await
            .unwrap();

        assert_eq!(receipt.parts.len(), 1);
        assert!(receipt.parts[0].bytes > 0);
        assert!(receipt.parts[0].bytes < one_megabyte as u64);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn producer_failure_aborts_once() {
        let backend = Arc::new(MockBackend::default());
        let invalid_schema = Arc::new(Schema::new(vec![Field::new(
            "wrong",
            DataType::Int32,
            false,
        )]));
        let invalid_batch = RecordBatch::try_new(
            invalid_schema,
            vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
        )
        .unwrap();

        let result = session(backend.clone(), config(128))
            .write_parquet(
                test_schema(),
                writer_properties(),
                stream::iter(vec![Ok(invalid_batch)]),
            )
            .await;

        assert!(matches!(result, Err(ObjectWriteError::Parquet(_))));
        assert_eq!(backend.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(backend.completes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_failure_aborts_once() {
        let backend = Arc::new(MockBackend::default());
        backend.fail_part.store(2, Ordering::SeqCst);

        let result = write_batches(backend.clone(), vec![test_batch(100, 80)], config(128)).await;

        assert!(matches!(
            result,
            Err(ObjectWriteError::Backend {
                operation: "upload_part",
                ..
            })
        ));
        assert_eq!(backend.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(backend.completes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_stops_producer_and_aborts_once() {
        let backend = Arc::new(MockBackend::default());
        backend.block_uploads.store(true, Ordering::SeqCst);
        let operation = session(backend.clone(), config(128)).write_parquet(
            test_schema(),
            writer_properties(),
            stream::iter(vec![Ok(test_batch(150, 80))]),
        );

        wait_until(|| backend.uploads_started.load(Ordering::SeqCst) > 0).await;
        drop(operation);
        wait_until(|| backend.aborts.load(Ordering::SeqCst) == 1).await;

        assert_eq!(backend.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(backend.completes.load(Ordering::SeqCst), 0);
        assert_eq!(backend.uploads_in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receipt_statistics_are_deterministic() {
        let batch = test_batch(20, 24);
        let first = write_batches(
            Arc::new(MockBackend::default()),
            vec![batch.clone()],
            config(256),
        )
        .await
        .unwrap();
        let second = write_batches(Arc::new(MockBackend::default()), vec![batch], config(256))
            .await
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(first.rows, 20);
        assert_eq!(
            first.bytes,
            first.parts.iter().map(|part| part.bytes).sum::<u64>()
        );
        assert_eq!(first.transport_chunk_count as usize, first.parts.len());
        assert_eq!(first.etag.as_deref(), Some("object-etag"));
        assert_eq!(first.checksum.as_deref(), Some("object-checksum"));
        assert_eq!(
            first.backend_metadata.get("region").map(String::as_str),
            Some("test-1")
        );
        assert_eq!(
            first
                .backend_metadata
                .get("storage_class")
                .map(String::as_str),
            Some("mock")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_stream_never_commits_an_object() {
        let backend = Arc::new(MockBackend::default());
        let result = session(backend.clone(), config(128))
            .write_parquet(
                test_schema(),
                writer_properties(),
                stream::empty::<Result<RecordBatch, ObjectWriteError>>(),
            )
            .await;

        assert!(matches!(result, Err(ObjectWriteError::EmptyInput)));
        assert_eq!(backend.begins.load(Ordering::SeqCst), 1);
        assert_eq!(backend.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(backend.completes.load(Ordering::SeqCst), 0);
    }
}
