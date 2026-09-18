use skippr_runtime_sdk::SkipprConfig;
use crate::helpers::configuration::DataSinkPluginConfig;
use async_trait::async_trait;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::execution::SendableRecordBatchStream;
use futures::{stream::FuturesUnordered, StreamExt};
use lapin::{
    options::*, publisher_confirm::Confirmation, types::FieldTable, BasicProperties, Channel,
    Connection, ConnectionProperties, ExchangeKind,
};
use serde_derive::Deserialize;
use skippr_runtime_sdk::plugins::{
    AtLeastOnceMessageDelivery, DataSink, GroupedBatchReader, GroupedSinkWriteContext,
    SinkWriteContext, SinkWriteOutcome,
};
use std::{future::Future, io, pin::Pin, sync::Arc};
use tokio::sync::Mutex;
use tracing::info;

const DEFAULT_MAX_IN_FLIGHT: usize = 32;
const DEFAULT_MAX_IN_FLIGHT_BYTES: usize = 4 * 1024 * 1024;
const MAX_IN_FLIGHT_ENV: &str = "SKIPPR_AMQP_MAX_IN_FLIGHT";
const MAX_IN_FLIGHT_BYTES_ENV: &str = "SKIPPR_AMQP_MAX_IN_FLIGHT_BYTES";

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSinkAmqpPluginConfig {
    #[skippr(secret)]
    pub connection_string: String,
    pub exchange: String,
    pub routing_key: Option<String>,
    pub exchange_type: Option<String>,
    pub format: Option<String>,
    #[serde(default)]
    pub max_in_flight: Option<usize>,
    #[serde(default)]
    pub max_in_flight_bytes: Option<usize>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkAmqpPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Amqp")
    }
}

#[derive(Clone, Copy, Debug)]
struct PublishLimits {
    max_in_flight: usize,
    max_in_flight_bytes: usize,
}

impl PublishLimits {
    fn from_config(config: &DataSinkAmqpPluginConfig) -> Self {
        Self {
            max_in_flight: configured_limit(
                MAX_IN_FLIGHT_ENV,
                config.max_in_flight,
                DEFAULT_MAX_IN_FLIGHT,
            ),
            max_in_flight_bytes: configured_limit(
                MAX_IN_FLIGHT_BYTES_ENV,
                config.max_in_flight_bytes,
                DEFAULT_MAX_IN_FLIGHT_BYTES,
            ),
        }
    }
}

fn configured_limit(env_name: &str, configured: Option<usize>, default: usize) -> usize {
    std::env::var_os(env_name)
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .or(configured.filter(|value| *value > 0))
        .unwrap_or(default)
}

type PendingConfirm = Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'static>>;
type SizedConfirm = Pin<Box<dyn Future<Output = (usize, io::Result<()>)> + Send + 'static>>;

#[async_trait]
trait MessagePublisher: Send + Sync {
    /// Queue one publish in channel order and return its broker confirm future.
    async fn publish(&self, payload: Vec<u8>) -> io::Result<PendingConfirm>;
}

struct LapinSession {
    generation: u64,
    _connection: Connection,
    channel: Channel,
}

#[derive(Default)]
struct LapinPublisherState {
    generation: u64,
    session: Option<LapinSession>,
}

struct LapinPublisher {
    connection_string: String,
    exchange: String,
    routing_key: String,
    exchange_kind: ExchangeKind,
    state: Arc<Mutex<LapinPublisherState>>,
}

#[derive(Clone)]
struct PublisherSession {
    generation: u64,
    channel: Channel,
}

impl LapinPublisher {
    fn new(config: &DataSinkAmqpPluginConfig) -> Self {
        let exchange_kind = match config.exchange_type.as_deref() {
            Some("fanout") => ExchangeKind::Fanout,
            Some("topic") => ExchangeKind::Topic,
            Some("headers") => ExchangeKind::Headers,
            _ => ExchangeKind::Direct,
        };
        Self {
            connection_string: config.connection_string.clone(),
            exchange: config.exchange.clone(),
            routing_key: config.routing_key.clone().unwrap_or_default(),
            exchange_kind,
            state: Arc::new(Mutex::new(LapinPublisherState::default())),
        }
    }

    async fn session(&self) -> io::Result<PublisherSession> {
        let mut state = self.state.lock().await;
        if let Some(session) = state.session.as_ref() {
            return Ok(PublisherSession {
                generation: session.generation,
                channel: session.channel.clone(),
            });
        }

        let connection =
            Connection::connect(&self.connection_string, ConnectionProperties::default())
                .await
                .map_err(amqp_error)?;
        let channel = connection.create_channel().await.map_err(amqp_error)?;
        channel
            .exchange_declare(
                &self.exchange,
                self.exchange_kind.clone(),
                ExchangeDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(amqp_error)?;
        channel
            .confirm_select(ConfirmSelectOptions::default())
            .await
            .map_err(amqp_error)?;

        state.generation = state.generation.saturating_add(1);
        let generation = state.generation;
        state.session = Some(LapinSession {
            generation,
            _connection: connection,
            channel: channel.clone(),
        });
        Ok(PublisherSession {
            generation,
            channel,
        })
    }

    async fn invalidate(&self, generation: u64) {
        invalidate_session(&self.state, generation).await;
    }
}

async fn invalidate_session(state: &Arc<Mutex<LapinPublisherState>>, generation: u64) {
    let mut state = state.lock().await;
    if state
        .session
        .as_ref()
        .is_some_and(|session| session.generation == generation)
    {
        state.session = None;
    }
}

fn amqp_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[async_trait]
impl MessagePublisher for LapinPublisher {
    async fn publish(&self, payload: Vec<u8>) -> io::Result<PendingConfirm> {
        let session = self.session().await?;
        let confirm = match session
            .channel
            .basic_publish(
                &self.exchange,
                &self.routing_key,
                BasicPublishOptions::default(),
                &payload,
                BasicProperties::default().with_content_type("application/json".into()),
            )
            .await
        {
            Ok(confirm) => confirm,
            Err(error) => {
                self.invalidate(session.generation).await;
                return Err(amqp_error(error));
            }
        };

        let state = Arc::clone(&self.state);
        Ok(Box::pin(async move {
            let result = match confirm.await {
                Ok(Confirmation::Ack(_)) => Ok(()),
                Ok(Confirmation::Nack(_)) => Err(io::Error::other(
                    "AMQP broker negatively acknowledged publish",
                )),
                Ok(Confirmation::NotRequested) => {
                    Err(io::Error::other("AMQP publisher confirm was not requested"))
                }
                Err(error) => Err(amqp_error(error)),
            };
            if result.is_err() {
                invalidate_session(&state, session.generation).await;
            }
            result
        }))
    }
}

#[derive(Clone, Copy)]
struct EnvelopeContext<'a> {
    cdc_ctx: Option<&'a skippr_runtime_sdk::plugins::cdc::SyncContext>,
    compaction_id: Option<&'a str>,
    wal_refs: &'a [skippr_runtime_sdk::protocol::RuntimeWalPartRef],
}

impl<'a> EnvelopeContext<'a> {
    fn legacy(cdc_ctx: Option<&'a skippr_runtime_sdk::plugins::cdc::SyncContext>) -> Self {
        Self {
            cdc_ctx,
            compaction_id: None,
            wal_refs: &[],
        }
    }

    fn from_write_context(ctx: &'a SinkWriteContext<'_>) -> Self {
        Self {
            cdc_ctx: ctx.cdc_ctx,
            compaction_id: (!ctx.compaction_id.is_empty()).then_some(ctx.compaction_id.as_str()),
            wal_refs: &ctx.wal_refs,
        }
    }
}

fn encode_row(
    batch: &RecordBatch,
    row_idx: usize,
    row_offset: usize,
    ctx: EnvelopeContext<'_>,
) -> io::Result<Vec<u8>> {
    let mut map = serde_json::Map::new();
    for (col_idx, field) in batch.schema().fields().iter().enumerate() {
        let value = arrow::util::display::array_value_to_string(batch.column(col_idx), row_idx)
            .unwrap_or_else(|_| "null".to_string());
        map.insert(field.name().clone(), serde_json::Value::String(value));
    }

    if let Some(cdc_ctx) = ctx.cdc_ctx {
        if let Some(row_meta) = cdc_ctx.part_meta.rows.get(row_offset + row_idx) {
            let mutation = match row_meta.mutation {
                skippr_runtime_sdk::plugins::cdc::MutationKind::Snapshot => "snapshot",
                skippr_runtime_sdk::plugins::cdc::MutationKind::Insert => "insert",
                skippr_runtime_sdk::plugins::cdc::MutationKind::Update => "update",
                skippr_runtime_sdk::plugins::cdc::MutationKind::Delete => "delete",
            };
            map.insert(
                "_skippr_mutation".to_string(),
                serde_json::Value::String(mutation.to_string()),
            );
            map.insert(
                "_skippr_event_id".to_string(),
                serde_json::Value::String(hex::encode(&row_meta.event_id)),
            );
            map.insert(
                "_skippr_order_token".to_string(),
                serde_json::Value::String(hex::encode(&row_meta.order_token)),
            );
        }
    }

    if let Some(compaction_id) = ctx.compaction_id {
        map.insert(
            "_skippr_compaction_id".to_string(),
            serde_json::Value::String(compaction_id.to_string()),
        );
    }
    if !ctx.wal_refs.is_empty() {
        let fingerprint =
            skippr_runtime_sdk::sink_idempotency::canonical_wal_refs_fingerprint(ctx.wal_refs);
        map.insert(
            "_skippr_wal_refs_fingerprint".to_string(),
            serde_json::Value::String(fingerprint),
        );
        map.insert(
            "_skippr_wal_ref_count".to_string(),
            serde_json::Value::Number((ctx.wal_refs.len() as u64).into()),
        );
        if let [wal_ref] = ctx.wal_refs {
            map.insert(
                "_skippr_wal_ref".to_string(),
                serde_json::json!({
                    "segment_id": wal_ref.segment_id,
                    "source": wal_ref.source,
                    "start": wal_ref.start,
                    "len": wal_ref.len,
                    "sink_ref": wal_ref.sink_ref,
                    "namespace": wal_ref.namespace,
                    "partition": wal_ref.partition,
                    "time": wal_ref.time,
                    "schema_fingerprint": wal_ref.schema_fingerprint,
                    "cdc_meta_hash": wal_ref.cdc_meta_hash.map(hex::encode),
                }),
            );
        }
    }

    serde_json::to_vec(&map).map_err(amqp_error)
}

pub struct DataSinkAmqpPlugin {
    exchange: String,
    publisher: Arc<dyn MessagePublisher>,
    limits: PublishLimits,
}

skippr_runtime_sdk::declare_sink_spec!(
    AmqpSinkSpec,
    DataSinkAmqpPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::AMQP,
    skippr_runtime_sdk::plugins::AtLeastOnceMessageDelivery
);

impl DataSinkAmqpPlugin {
    async fn publish_stream(
        &self,
        mut stream: SendableRecordBatchStream,
        envelope: EnvelopeContext<'_>,
    ) -> io::Result<u64> {
        let mut in_flight = FuturesUnordered::<SizedConfirm>::new();
        let mut in_flight_bytes = 0usize;
        let mut message_count = 0u64;
        let mut row_offset = 0usize;

        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(amqp_error)?;
            for row_idx in 0..batch.num_rows() {
                let payload = encode_row(&batch, row_idx, row_offset, envelope)?;
                let payload_bytes = payload.len();

                while !in_flight.is_empty()
                    && (in_flight.len() >= self.limits.max_in_flight
                        || in_flight_bytes.saturating_add(payload_bytes)
                            > self.limits.max_in_flight_bytes)
                {
                    let (confirmed_bytes, result) = in_flight
                        .next()
                        .await
                        .expect("non-empty publisher confirm set");
                    in_flight_bytes = in_flight_bytes.saturating_sub(confirmed_bytes);
                    result?;
                }

                let confirm = self.publisher.publish(payload).await?;
                in_flight_bytes = in_flight_bytes.saturating_add(payload_bytes);
                in_flight.push(Box::pin(async move { (payload_bytes, confirm.await) }));
                message_count = message_count.saturating_add(1);
            }
            row_offset = row_offset.saturating_add(batch.num_rows());
        }

        while let Some((confirmed_bytes, result)) = in_flight.next().await {
            in_flight_bytes = in_flight_bytes.saturating_sub(confirmed_bytes);
            result?;
        }
        debug_assert_eq!(in_flight_bytes, 0);
        Ok(message_count)
    }

    async fn write_stream(
        &self,
        stream: SendableRecordBatchStream,
        envelope: EnvelopeContext<'_>,
    ) -> io::Result<()> {
        use skippr_runtime_sdk::metrics::counters;
        counters::inc_uploads_in_flight();
        let result = self.publish_stream(stream, envelope).await;
        counters::dec_uploads_in_flight();

        if let Ok(message_count) = result {
            info!(
                "AMQP: confirmed {} messages on exchange '{}'",
                message_count, self.exchange
            );
            Ok(())
        } else {
            result.map(|_| ())
        }
    }
}

#[async_trait]
impl DataSink for DataSinkAmqpPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        _filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        self.write_stream(stream, EnvelopeContext::legacy(cdc_ctx))
            .await
    }

    async fn sync_with_context(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<(), std::io::Error> {
        ctx.validate_grouped::<AtLeastOnceMessageDelivery>()
            .map_err(|error| io::Error::new(io::ErrorKind::Unsupported, error))?;
        let envelope = EnvelopeContext::from_write_context(&ctx);
        self.write_stream(stream, envelope).await
    }

    async fn sync_grouped(
        &self,
        reader: GroupedBatchReader,
        ctx: GroupedSinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        let write_ctx = ctx.to_sink_write_context();
        write_ctx
            .validate_grouped::<AtLeastOnceMessageDelivery>()
            .map_err(|error| io::Error::new(io::ErrorKind::Unsupported, error))?;
        let (stream, _progress) = reader.into_stream();
        let envelope = EnvelopeContext::from_write_context(&write_ctx);
        self.write_stream(stream, envelope).await?;
        Ok(SinkWriteOutcome::Applied)
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::AMQP
    }
}

impl DataSinkAmqpPlugin {
    pub async fn new_with_config(_buffer_name: String, config: DataSinkAmqpPluginConfig) -> Self {
        let limits = PublishLimits::from_config(&config);
        let publisher = Arc::new(LapinPublisher::new(&config));
        Self {
            exchange: config.exchange,
            publisher,
            limits,
        }
    }

    #[cfg(test)]
    fn with_publisher(publisher: Arc<dyn MessagePublisher>, limits: PublishLimits) -> Self {
        Self {
            exchange: "test-exchange".to_string(),
            publisher,
            limits,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::{
        array::Int64Array,
        datatypes::{DataType, Field, Schema},
    };
    use datafusion::{
        arrow::datatypes::SchemaRef, error::DataFusionError, physical_plan::RecordBatchStream,
    };
    use skippr_runtime_sdk::{
        plugins::cdc::{MutationKind, SyncContext, WalPartMeta, WalRowMeta},
        protocol::RuntimeWalPartRef,
    };
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };
    use tokio::sync::oneshot;

    struct TestBatchStream {
        schema: SchemaRef,
        batches: std::vec::IntoIter<RecordBatch>,
    }

    impl futures::Stream for TestBatchStream {
        type Item = Result<RecordBatch, DataFusionError>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.batches.next().map(Ok))
        }
    }

    impl RecordBatchStream for TestBatchStream {
        fn schema(&self) -> SchemaRef {
            Arc::clone(&self.schema)
        }
    }

    fn batch(values: &[i64]) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(values.to_vec()))]).unwrap()
    }

    fn stream(values: &[i64]) -> SendableRecordBatchStream {
        let batch = batch(values);
        Box::pin(TestBatchStream {
            schema: batch.schema(),
            batches: vec![batch].into_iter(),
        })
    }

    #[derive(Default)]
    struct ControlledState {
        published: Vec<Vec<u8>>,
        pending: BTreeMap<usize, oneshot::Sender<io::Result<()>>>,
    }

    #[derive(Default)]
    struct ControlledPublisher {
        state: Arc<Mutex<ControlledState>>,
        next_id: AtomicUsize,
        current_in_flight: Arc<AtomicUsize>,
        current_bytes: Arc<AtomicUsize>,
        max_in_flight: AtomicUsize,
        max_bytes: AtomicUsize,
    }

    struct InFlightGuard {
        current_in_flight: Arc<AtomicUsize>,
        current_bytes: Arc<AtomicUsize>,
        bytes: usize,
    }

    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            self.current_in_flight.fetch_sub(1, Ordering::SeqCst);
            self.current_bytes.fetch_sub(self.bytes, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl MessagePublisher for ControlledPublisher {
        async fn publish(&self, payload: Vec<u8>) -> io::Result<PendingConfirm> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let bytes = payload.len();
            let (sender, receiver) = oneshot::channel();
            {
                let mut state = self.state.lock().await;
                state.published.push(payload);
                state.pending.insert(id, sender);
            }

            let in_flight = self.current_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            let in_flight_bytes = self.current_bytes.fetch_add(bytes, Ordering::SeqCst) + bytes;
            self.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
            self.max_bytes.fetch_max(in_flight_bytes, Ordering::SeqCst);

            let guard = InFlightGuard {
                current_in_flight: Arc::clone(&self.current_in_flight),
                current_bytes: Arc::clone(&self.current_bytes),
                bytes,
            };
            Ok(Box::pin(async move {
                let _guard = guard;
                receiver
                    .await
                    .map_err(|_| io::Error::other("publisher disconnected"))?
            }))
        }
    }

    impl ControlledPublisher {
        async fn wait_for_published(&self, expected: usize) {
            for _ in 0..10_000 {
                if self.state.lock().await.published.len() >= expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("timed out waiting for {expected} published messages");
        }

        async fn complete(&self, id: usize, result: io::Result<()>) {
            let sender = self
                .state
                .lock()
                .await
                .pending
                .remove(&id)
                .unwrap_or_else(|| panic!("missing confirm {id}"));
            let _ = sender.send(result);
        }

        async fn published(&self) -> Vec<Vec<u8>> {
            self.state.lock().await.published.clone()
        }
    }

    fn plugin(
        publisher: Arc<ControlledPublisher>,
        max_in_flight: usize,
        max_in_flight_bytes: usize,
    ) -> Arc<DataSinkAmqpPlugin> {
        Arc::new(DataSinkAmqpPlugin::with_publisher(
            publisher,
            PublishLimits {
                max_in_flight,
                max_in_flight_bytes,
            },
        ))
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn all_out_of_order_confirms_gate_success() {
        let publisher = Arc::new(ControlledPublisher::default());
        let plugin = plugin(Arc::clone(&publisher), 3, usize::MAX);
        let task = tokio::spawn({
            let plugin = Arc::clone(&plugin);
            async move { plugin.sync(stream(&[1, 2, 3]), String::new(), None).await }
        });

        publisher.wait_for_published(3).await;
        publisher.complete(2, Ok(())).await;
        settle().await;
        assert!(!task.is_finished());
        publisher.complete(0, Ok(())).await;
        settle().await;
        assert!(!task.is_finished());
        publisher.complete(1, Ok(())).await;
        task.await.unwrap().unwrap();

        let ids = publisher
            .published()
            .await
            .iter()
            .map(|payload| {
                serde_json::from_slice::<serde_json::Value>(payload).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, ["1", "2", "3"]);
    }

    #[tokio::test]
    async fn publish_window_bounds_message_count_and_encoded_bytes() {
        let count_publisher = Arc::new(ControlledPublisher::default());
        let count_plugin = plugin(Arc::clone(&count_publisher), 2, usize::MAX);
        let count_task = tokio::spawn({
            let plugin = Arc::clone(&count_plugin);
            async move {
                plugin
                    .sync(stream(&[1, 2, 3, 4]), String::new(), None)
                    .await
            }
        });

        count_publisher.wait_for_published(2).await;
        settle().await;
        assert_eq!(count_publisher.published().await.len(), 2);
        count_publisher.complete(0, Ok(())).await;
        count_publisher.wait_for_published(3).await;
        count_publisher.complete(1, Ok(())).await;
        count_publisher.wait_for_published(4).await;
        count_publisher.complete(2, Ok(())).await;
        count_publisher.complete(3, Ok(())).await;
        count_task.await.unwrap().unwrap();
        assert_eq!(count_publisher.max_in_flight.load(Ordering::SeqCst), 2);

        let bytes_publisher = Arc::new(ControlledPublisher::default());
        let byte_limit = encode_row(&batch(&[1]), 0, 0, EnvelopeContext::legacy(None))
            .unwrap()
            .len();
        let bytes_plugin = plugin(Arc::clone(&bytes_publisher), 10, byte_limit);
        let bytes_task = tokio::spawn({
            let plugin = Arc::clone(&bytes_plugin);
            async move { plugin.sync(stream(&[1, 2]), String::new(), None).await }
        });

        bytes_publisher.wait_for_published(1).await;
        settle().await;
        assert_eq!(bytes_publisher.published().await.len(), 1);
        bytes_publisher.complete(0, Ok(())).await;
        bytes_publisher.wait_for_published(2).await;
        bytes_publisher.complete(1, Ok(())).await;
        bytes_task.await.unwrap().unwrap();
        assert_eq!(bytes_publisher.max_in_flight.load(Ordering::SeqCst), 1);
        assert!(
            bytes_publisher.max_bytes.load(Ordering::SeqCst) <= byte_limit,
            "encoded bytes exceeded the configured window"
        );
    }

    #[tokio::test]
    async fn nack_fails_write_without_publishing_later_rows() {
        let publisher = Arc::new(ControlledPublisher::default());
        let plugin = plugin(Arc::clone(&publisher), 2, usize::MAX);
        let task = tokio::spawn({
            let plugin = Arc::clone(&plugin);
            async move {
                plugin
                    .sync(stream(&[1, 2, 3, 4]), String::new(), None)
                    .await
            }
        });

        publisher.wait_for_published(2).await;
        publisher
            .complete(1, Err(io::Error::other("broker nack")))
            .await;
        let error = task.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("broker nack"));
        assert_eq!(publisher.published().await.len(), 2);
    }

    #[tokio::test]
    async fn persistent_publisher_is_reused_for_disconnect_retry() {
        let publisher = Arc::new(ControlledPublisher::default());
        let plugin = plugin(Arc::clone(&publisher), 1, usize::MAX);
        let first = tokio::spawn({
            let plugin = Arc::clone(&plugin);
            async move { plugin.sync(stream(&[7]), String::new(), None).await }
        });

        publisher.wait_for_published(1).await;
        publisher
            .complete(0, Err(io::Error::other("channel closed")))
            .await;
        assert!(first.await.unwrap().is_err());

        let retry = tokio::spawn({
            let plugin = Arc::clone(&plugin);
            async move { plugin.sync(stream(&[7]), String::new(), None).await }
        });
        publisher.wait_for_published(2).await;
        publisher.complete(1, Ok(())).await;
        retry.await.unwrap().unwrap();
        assert_eq!(publisher.published().await.len(), 2);
    }

    fn wal_ref(segment_id: &str) -> RuntimeWalPartRef {
        RuntimeWalPartRef {
            segment_id: segment_id.to_string(),
            source: "postgres".to_string(),
            start: 12,
            len: 34,
            sink_ref: "primary".to_string(),
            namespace: "public.users".to_string(),
            partition: "p=1".to_string(),
            time: Some(99),
            schema_fingerprint: "schema".to_string(),
            cdc_meta_hash: Some([0xab; 32]),
        }
    }

    #[test]
    fn grouped_envelope_preserves_wal_and_mixed_cdc_metadata() {
        let batch = batch(&[10, 11, 12, 13]);
        let mutations = [
            MutationKind::Snapshot,
            MutationKind::Insert,
            MutationKind::Update,
            MutationKind::Delete,
        ];
        let rows = mutations
            .iter()
            .enumerate()
            .map(|(index, mutation)| WalRowMeta {
                mutation: *mutation,
                event_id: vec![index as u8 + 1],
                order_token: vec![0, index as u8 + 10],
            })
            .collect::<Vec<_>>();
        let cdc = SyncContext {
            part_meta: WalPartMeta::cdc(rows, 4).unwrap(),
            contract: None,
        };
        let wal_refs = [wal_ref("segment-a"), wal_ref("segment-b")];
        let envelope = EnvelopeContext {
            cdc_ctx: Some(&cdc),
            compaction_id: Some("compaction-19"),
            wal_refs: &wal_refs,
        };
        let expected_fingerprint =
            skippr_runtime_sdk::sink_idempotency::canonical_wal_refs_fingerprint(&wal_refs);

        let expected_mutations = ["snapshot", "insert", "update", "delete"];
        for (index, expected_mutation) in expected_mutations.iter().enumerate() {
            let value: serde_json::Value =
                serde_json::from_slice(&encode_row(&batch, index, 0, envelope).unwrap()).unwrap();
            assert_eq!(value["id"], (index as i64 + 10).to_string());
            assert_eq!(value["_skippr_mutation"], *expected_mutation);
            assert_eq!(
                value["_skippr_event_id"],
                format!("{:02x}", index as u8 + 1)
            );
            assert_eq!(
                value["_skippr_order_token"],
                format!("00{:02x}", index as u8 + 10)
            );
            assert_eq!(value["_skippr_compaction_id"], "compaction-19");
            assert_eq!(value["_skippr_wal_refs_fingerprint"], expected_fingerprint);
            assert_eq!(value["_skippr_wal_ref_count"], 2);
            assert!(value.get("_skippr_wal_refs").is_none());
            assert!(value.get("_skippr_wal_ref").is_none());
        }
    }
}
