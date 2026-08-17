use std::any::Any;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;

use arrow_flight::sql::ProstMessageExt;
use datafusion::arrow::datatypes::{Schema, SchemaRef};
use datafusion::arrow::ipc::reader::StreamReader;
use datafusion::arrow::ipc::writer::StreamWriter;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};
use datafusion_proto::physical_plan::{DefaultPhysicalExtensionCodec, PhysicalExtensionCodec};
use futures::{Stream, StreamExt, TryStreamExt};
use prost::Message;

pub const PHYSICAL_MAGIC: &[u8; 4] = b"SKPR";
pub const PHYSICAL_KIND_FLIGHT_SQL: u8 = 1;
pub const PHYSICAL_KIND_ICEBERG: u8 = 2;
/// Live WAL used to occupy kind 3; that leaf is forbidden in Ballista plans.
pub const PHYSICAL_KIND_UNION: u8 = 4;

pub fn encode_tagged_node(kind: u8, payload: &[u8], buf: &mut Vec<u8>) {
    buf.extend_from_slice(PHYSICAL_MAGIC);
    buf.push(kind);
    buf.extend_from_slice(payload);
}

pub fn decode_tagged_node(buf: &[u8]) -> Option<(u8, &[u8])> {
    if buf.len() < 5 || &buf[..4] != PHYSICAL_MAGIC {
        return None;
    }
    Some((buf[4], &buf[5..]))
}

pub fn encode_flight_sql_command(msg: &impl ProstMessageExt) -> Vec<u8> {
    msg.as_any().encode_to_vec()
}

#[derive(Clone, PartialEq, Message)]
pub struct FlightSqlExecNode {
    #[prost(string, tag = "1")]
    pub endpoint: String,
    #[prost(bytes = "vec", tag = "2")]
    pub prepared_handle: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub arrow_schema_ipc: Vec<u8>,
    #[prost(string, tag = "4")]
    pub authorization: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct IcebergScanNode {
    #[prost(bytes = "vec", tag = "1")]
    pub arrow_schema_ipc: Vec<u8>,
    #[prost(int64, tag = "2")]
    pub snapshot_id: i64,
    #[prost(uint64, repeated, tag = "3")]
    pub projection: Vec<u64>,
    #[prost(uint64, optional, tag = "4")]
    pub limit: Option<u64>,
    #[prost(string, tag = "5")]
    pub catalog_json: String,
    #[prost(string, tag = "6")]
    pub catalog_ns: String,
    #[prost(string, tag = "7")]
    pub table_name: String,
}

impl FlightSqlExecNode {
    pub fn encode_bytes(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.encode(&mut buf)
            .map_err(|err| DataFusionError::Internal(err.to_string()))?;
        Ok(buf)
    }

    pub fn decode_bytes(buf: &[u8]) -> Result<Self> {
        Self::decode(buf).map_err(|err| {
            DataFusionError::Internal(format!("truncated or invalid FlightSql node: {err}"))
        })
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct UnionTableNode {
    #[prost(bytes = "vec", tag = "1")]
    pub iceberg_provider: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub wal_provider: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub arrow_schema_ipc: Vec<u8>,
}

/// Physical node that executes a Flight SQL statement on `endpoint` (`host:port`).
#[derive(Debug, Clone)]
pub struct FlightSqlExec {
    pub endpoint: String,
    pub prepared_handle: Vec<u8>,
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
    authorization: String,
}

pub fn validate_flight_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.is_empty()
        || endpoint.starts_with("http://")
        || endpoint.starts_with("https://")
        || !endpoint.contains(':')
    {
        return Err(DataFusionError::Execution(
            "FlightSqlExec endpoint must be host:port".into(),
        ));
    }
    Ok(())
}

impl FlightSqlExec {
    pub fn new(endpoint: String, prepared_handle: Vec<u8>, schema: SchemaRef) -> Self {
        Self::with_authorization(endpoint, prepared_handle, schema, String::new())
    }

    pub fn with_authorization(
        endpoint: String,
        prepared_handle: Vec<u8>,
        schema: SchemaRef,
        authorization: String,
    ) -> Self {
        let properties = Arc::new(PlanProperties::new(
            datafusion::physical_expr::EquivalenceProperties::new(schema.clone()),
            datafusion::physical_plan::Partitioning::UnknownPartitioning(1),
            datafusion::physical_plan::execution_plan::EmissionType::Incremental,
            datafusion::physical_plan::execution_plan::Boundedness::Bounded,
        ));
        Self {
            endpoint,
            prepared_handle,
            schema,
            properties,
            authorization,
        }
    }

    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

impl IcebergScanNode {
    pub fn encode_bytes(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.encode(&mut buf)
            .map_err(|err| DataFusionError::Internal(err.to_string()))?;
        Ok(buf)
    }

    pub fn decode_bytes(buf: &[u8]) -> Result<Self> {
        Self::decode(buf).map_err(|err| {
            DataFusionError::Internal(format!("truncated or invalid IcebergScan node: {err}"))
        })
    }
}

impl UnionTableNode {
    pub fn encode_bytes(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.encode(&mut buf)
            .map_err(|err| DataFusionError::Internal(err.to_string()))?;
        Ok(buf)
    }

    pub fn decode_bytes(buf: &[u8]) -> Result<Self> {
        Self::decode(buf).map_err(|err| {
            DataFusionError::Internal(format!("truncated or invalid UnionTable node: {err}"))
        })
    }
}

impl DisplayAs for FlightSqlExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "FlightSqlExec(endpoint={})", self.endpoint)
    }
}

impl ExecutionPlan for FlightSqlExec {
    fn name(&self) -> &str {
        "FlightSqlExec"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        Vec::new()
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        if let Err(err) = validate_flight_endpoint(&self.endpoint) {
            return Err(err);
        }
        let sql = String::from_utf8(self.prepared_handle.clone()).map_err(|err| {
            DataFusionError::Execution(format!("FlightSqlExec handle is not UTF-8: {err}"))
        })?;
        let schema = self.schema.clone();
        let endpoint = self.endpoint.clone();
        let authorization = self.authorization.clone();
        let handle = tokio::runtime::Handle::try_current().map_err(|err| {
            DataFusionError::Execution(format!("FlightSqlExec requires a Tokio runtime: {err}"))
        })?;
        let (tx, rx) = futures::channel::mpsc::channel(8);
        let stream_schema = schema.clone();
        handle.spawn(async move {
            stream_statement_into(&endpoint, &sql, &authorization, stream_schema, tx).await;
        });
        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, rx)))
    }
}

fn align_batch(batch: RecordBatch, schema: &SchemaRef) -> Result<RecordBatch> {
    use datafusion::arrow::array::RecordBatchOptions;
    use datafusion::arrow::compute::kernels::cast::cast;

    if schema.fields().is_empty() {
        return RecordBatch::try_new_with_options(
            schema.clone(),
            Vec::new(),
            &RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
        )
        .map_err(|err| DataFusionError::ArrowError(Box::new(err), None));
    }
    let same_layout = batch.num_columns() == schema.fields().len()
        && batch
            .schema()
            .fields()
            .iter()
            .zip(schema.fields())
            .all(|(src, dst)| src.name() == dst.name() && src.data_type() == dst.data_type());
    if same_layout {
        return RecordBatch::try_new(schema.clone(), batch.columns().to_vec())
            .map_err(|err| DataFusionError::ArrowError(Box::new(err), None));
    }
    let mut columns = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        let col = batch.column_by_name(field.name()).ok_or_else(|| {
            DataFusionError::Execution(format!(
                "Flight SQL batch missing column '{}'",
                field.name()
            ))
        })?;
        let col = if col.data_type() == field.data_type() {
            col.clone()
        } else {
            cast(col, field.data_type())
                .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))?
        };
        columns.push(col);
    }
    RecordBatch::try_new(schema.clone(), columns)
        .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))
}

async fn stream_statement_into(
    endpoint: &str,
    sql: &str,
    authorization: &str,
    schema: SchemaRef,
    mut tx: futures::channel::mpsc::Sender<Result<RecordBatch, DataFusionError>>,
) {
    use futures::SinkExt;
    match stream_statement_batches_with_auth(endpoint, sql, authorization).await {
        Ok(mut stream) => {
            while let Some(item) = stream.next().await {
                let item = item.and_then(|batch| align_batch(batch, &schema));
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        }
        Err(err) => {
            let _ = tx.send(Err(err)).await;
        }
    }
}

pub fn schema_to_ipc(schema: &Schema) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut writer = StreamWriter::try_new(&mut buf, schema)
        .map_err(|err| DataFusionError::Internal(err.to_string()))?;
    writer
        .finish()
        .map_err(|err| DataFusionError::Internal(err.to_string()))?;
    Ok(buf)
}

pub fn schema_from_ipc(bytes: &[u8]) -> Result<SchemaRef> {
    let reader = StreamReader::try_new(Cursor::new(bytes.to_vec()), None)
        .map_err(|err| DataFusionError::Internal(err.to_string()))?;
    Ok(reader.schema())
}

pub fn basic_authorization(tenant: &str, workspace: &str) -> String {
    use base64::Engine;
    let user = format!("{tenant}/{workspace}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(user.as_bytes())
    )
}

pub fn session_authorization() -> Result<String, DataFusionError> {
    let tenant = std::env::var("SKIPPR_QUERY_TENANT").unwrap_or_default();
    let workspace = std::env::var("SKIPPR_QUERY_WORKSPACE").unwrap_or_default();
    if tenant.trim().is_empty() || workspace.trim().is_empty() {
        return Err(DataFusionError::Execution(
            "SKIPPR_QUERY_TENANT and SKIPPR_QUERY_WORKSPACE are required".into(),
        ));
    }
    Ok(basic_authorization(tenant.trim(), workspace.trim()))
}

fn resolve_authorization(explicit: &str) -> Result<String, DataFusionError> {
    if !explicit.trim().is_empty() {
        return Ok(explicit.to_string());
    }
    session_authorization()
}

fn cluster_client_tls() -> Result<tonic::transport::ClientTlsConfig, DataFusionError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca = std::env::var("SKIPPR_CLUSTER_TLS_CA").map_err(|_| {
        DataFusionError::Execution("SKIPPR_CLUSTER_TLS_CA is required for cluster TLS".into())
    })?;
    let cert = std::env::var("SKIPPR_CLUSTER_TLS_CERT").map_err(|_| {
        DataFusionError::Execution("SKIPPR_CLUSTER_TLS_CERT is required for cluster TLS".into())
    })?;
    let key = std::env::var("SKIPPR_CLUSTER_TLS_KEY").map_err(|_| {
        DataFusionError::Execution("SKIPPR_CLUSTER_TLS_KEY is required for cluster TLS".into())
    })?;
    if ca.trim().is_empty() || cert.trim().is_empty() || key.trim().is_empty() {
        return Err(DataFusionError::Execution(
            "cluster TLS PEM env vars must not be empty".into(),
        ));
    }
    Ok(tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(ca))
        .identity(tonic::transport::Identity::from_pem(cert, key))
        .domain_name("skippr-cluster"))
}

fn flight_channel(endpoint: &str) -> Result<tonic::transport::Endpoint, DataFusionError> {
    validate_flight_endpoint(endpoint)?;
    let url = format!("https://{endpoint}");
    tonic::transport::Endpoint::from_shared(url)
        .map_err(|err| DataFusionError::Execution(err.to_string()))?
        .tls_config(cluster_client_tls()?)
        .map_err(|err| DataFusionError::Execution(err.to_string()))
}

fn attach_session(
    client: &mut arrow_flight::sql::client::FlightSqlServiceClient<tonic::transport::Channel>,
    authorization: &str,
) -> Result<(), DataFusionError> {
    if authorization.trim().is_empty() {
        return Err(DataFusionError::Execution(
            "Flight SQL session Authorization is required".into(),
        ));
    }
    client.set_header("authorization", authorization);
    Ok(())
}

pub async fn stream_statement_batches(
    endpoint: &str,
    sql: &str,
) -> Result<Pin<Box<dyn Stream<Item = Result<RecordBatch, DataFusionError>> + Send>>, DataFusionError>
{
    stream_statement_batches_with_auth(endpoint, sql, &session_authorization()?).await
}

pub async fn stream_statement_batches_with_auth(
    endpoint: &str,
    sql: &str,
    authorization: &str,
) -> Result<Pin<Box<dyn Stream<Item = Result<RecordBatch, DataFusionError>> + Send>>, DataFusionError>
{
    let channel = flight_channel(endpoint)?
        .connect()
        .await
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    let mut client = arrow_flight::sql::client::FlightSqlServiceClient::new(channel);
    attach_session(&mut client, &resolve_authorization(authorization)?)?;
    let info = client
        .execute(sql.to_string(), None)
        .await
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    let ticket = info
        .endpoint
        .first()
        .and_then(|ep| ep.ticket.clone())
        .ok_or_else(|| DataFusionError::Execution("Flight SQL response missing ticket".into()))?;
    let stream = client
        .do_get(ticket)
        .await
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    Ok(Box::pin(stream.map_err(|err| {
        DataFusionError::Execution(err.to_string())
    })))
}

pub async fn fetch_statement_schema(
    endpoint: &str,
    sql: &str,
) -> Result<SchemaRef, DataFusionError> {
    fetch_statement_schema_with_auth(endpoint, sql, &session_authorization()?).await
}

pub async fn fetch_statement_schema_with_auth(
    endpoint: &str,
    sql: &str,
    authorization: &str,
) -> Result<SchemaRef, DataFusionError> {
    let channel = flight_channel(endpoint)?
        .connect()
        .await
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    let mut client = arrow_flight::sql::client::FlightSqlServiceClient::new(channel);
    attach_session(&mut client, &resolve_authorization(authorization)?)?;
    let info = client
        .execute(sql.to_string(), None)
        .await
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    info.try_decode_schema()
        .map(Arc::new)
        .map_err(|err| DataFusionError::Execution(err.to_string()))
}

pub async fn fetch_statement_batches(
    endpoint: &str,
    sql: &str,
) -> Result<Vec<RecordBatch>, DataFusionError> {
    fetch_statement_batches_with_auth(endpoint, sql, &session_authorization()?).await
}

pub async fn fetch_statement_batches_with_auth(
    endpoint: &str,
    sql: &str,
    authorization: &str,
) -> Result<Vec<RecordBatch>, DataFusionError> {
    stream_statement_batches_with_auth(endpoint, sql, authorization)
        .await?
        .try_collect()
        .await
}

pub async fn fetch_statement_batches_unauthenticated(
    endpoint: &str,
    sql: &str,
) -> Result<Vec<RecordBatch>, DataFusionError> {
    let channel = flight_channel(endpoint)?
        .connect()
        .await
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    let mut client = arrow_flight::sql::client::FlightSqlServiceClient::new(channel);
    let info = client
        .execute(sql.to_string(), None)
        .await
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    let ticket = info
        .endpoint
        .first()
        .and_then(|ep| ep.ticket.clone())
        .ok_or_else(|| DataFusionError::Execution("Flight SQL response missing ticket".into()))?;
    client
        .do_get(ticket)
        .await
        .map_err(|err| DataFusionError::Execution(err.to_string()))?
        .map_err(|err| DataFusionError::Execution(err.to_string()))
        .try_collect()
        .await
}

#[derive(Debug)]
pub struct SkipprPhysicalCodec {
    inner: DefaultPhysicalExtensionCodec,
}

impl Default for SkipprPhysicalCodec {
    fn default() -> Self {
        Self {
            inner: DefaultPhysicalExtensionCodec {},
        }
    }
}

impl SkipprPhysicalCodec {
    pub fn try_encode_flight(&self, node: Arc<dyn ExecutionPlan>, buf: &mut Vec<u8>) -> Result<()> {
        let Some(flight) = node.as_any().downcast_ref::<FlightSqlExec>() else {
            return Err(DataFusionError::Internal(
                "SkipprPhysicalCodec only encodes FlightSqlExec; other nodes use Ballista's codec"
                    .into(),
            ));
        };
        let encoded = FlightSqlExecNode {
            endpoint: flight.endpoint.clone(),
            prepared_handle: flight.prepared_handle.clone(),
            arrow_schema_ipc: schema_to_ipc(flight.schema.as_ref())?,
            authorization: {
                if flight.authorization.trim().is_empty() {
                    return Err(DataFusionError::Internal(
                        "FlightSqlExec requires session Authorization".into(),
                    ));
                }
                flight.authorization.clone()
            },
        };
        let mut payload = Vec::new();
        encoded
            .encode(&mut payload)
            .map_err(|err| DataFusionError::Internal(err.to_string()))?;
        encode_tagged_node(PHYSICAL_KIND_FLIGHT_SQL, &payload, buf);
        Ok(())
    }

    pub fn try_decode_flight(&self, buf: &[u8]) -> Result<Arc<dyn ExecutionPlan>> {
        let Some((PHYSICAL_KIND_FLIGHT_SQL, payload)) = decode_tagged_node(buf) else {
            return Err(DataFusionError::Internal(
                "not a tagged FlightSqlExec node".into(),
            ));
        };
        let node = FlightSqlExecNode::decode(payload).map_err(|err| {
            DataFusionError::Internal(format!("truncated or invalid FlightSqlExec node: {err}"))
        })?;
        if node.endpoint.is_empty() || node.arrow_schema_ipc.is_empty() {
            return Err(DataFusionError::Internal(
                "FlightSqlExec node missing required fields".into(),
            ));
        }
        if node.authorization.trim().is_empty() {
            return Err(DataFusionError::Internal(
                "FlightSqlExec node missing session Authorization".into(),
            ));
        }
        validate_flight_endpoint(&node.endpoint).map_err(|err| {
            DataFusionError::Internal(format!("FlightSqlExec node endpoint: {err}"))
        })?;
        let schema = schema_from_ipc(&node.arrow_schema_ipc)?;
        Ok(Arc::new(FlightSqlExec::with_authorization(
            node.endpoint,
            node.prepared_handle,
            schema,
            node.authorization,
        )))
    }
}

impl PhysicalExtensionCodec for SkipprPhysicalCodec {
    fn try_decode(
        &self,
        buf: &[u8],
        inputs: &[Arc<dyn ExecutionPlan>],
        ctx: &TaskContext,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if decode_tagged_node(buf).map(|(kind, _)| kind) == Some(PHYSICAL_KIND_FLIGHT_SQL) {
            return self.try_decode_flight(buf);
        }
        self.inner.try_decode(buf, inputs, ctx)
    }

    fn try_encode(&self, node: Arc<dyn ExecutionPlan>, buf: &mut Vec<u8>) -> Result<()> {
        if node.as_any().downcast_ref::<FlightSqlExec>().is_some() {
            return self.try_encode_flight(node, buf);
        }
        self.inner.try_encode(node, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};

    fn test_auth() -> String {
        basic_authorization("t", "w")
    }

    #[test]
    fn flight_sql_exec_codec_round_trips_schema() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let node = Arc::new(FlightSqlExec::with_authorization(
            "127.0.0.1:50051".into(),
            b"SELECT 1".to_vec(),
            schema.clone(),
            test_auth(),
        ));
        let codec = SkipprPhysicalCodec::default();
        let mut buf = Vec::new();
        codec.try_encode_flight(node.clone(), &mut buf).unwrap();
        assert_eq!(&buf[..4], PHYSICAL_MAGIC);
        let decoded = codec.try_decode_flight(&buf).unwrap();
        let flight = decoded.as_any().downcast_ref::<FlightSqlExec>().unwrap();
        assert_eq!(flight.endpoint, "127.0.0.1:50051");
        assert_eq!(flight.prepared_handle, b"SELECT 1");
        assert_eq!(flight.schema(), schema);
    }

    #[test]
    fn align_batch_projects_and_keeps_count_rows() {
        use datafusion::arrow::array::StringArray;
        let full = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("payload", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            full,
            vec![
                Arc::new(StringArray::from(vec!["a", "b"])),
                Arc::new(StringArray::from(vec!["x", "y"])),
            ],
        )
        .unwrap();
        let id_only = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let projected = align_batch(batch.clone(), &id_only).unwrap();
        assert_eq!(projected.num_columns(), 1);
        assert_eq!(projected.num_rows(), 2);
        assert_eq!(projected.schema(), id_only);

        let empty = Arc::new(Schema::empty());
        let counted = align_batch(batch, &empty).unwrap();
        assert_eq!(counted.num_columns(), 0);
        assert_eq!(counted.num_rows(), 2);
    }

    #[test]
    fn truncated_buffer_fails() {
        let codec = SkipprPhysicalCodec::default();
        let err = codec.try_decode_flight(&[0x01, 0x02]).unwrap_err();
        assert!(err.to_string().contains("not a tagged") || err.to_string().contains("invalid"));
    }

    #[test]
    fn empty_prost_defaults_are_rejected() {
        let codec = SkipprPhysicalCodec::default();
        let mut buf = Vec::new();
        encode_tagged_node(PHYSICAL_KIND_FLIGHT_SQL, &[], &mut buf);
        assert!(codec.try_decode_flight(&buf).is_err());
    }

    #[test]
    fn union_table_node_round_trips() {
        let node = UnionTableNode {
            iceberg_provider: b"ice".to_vec(),
            wal_provider: b"wal".to_vec(),
            arrow_schema_ipc: schema_to_ipc(&Schema::empty()).unwrap(),
        };
        let bytes = node.encode_bytes().unwrap();
        let decoded = UnionTableNode::decode_bytes(&bytes).unwrap();
        assert_eq!(decoded.iceberg_provider, b"ice");
        assert_eq!(decoded.wal_provider, b"wal");
    }

    #[test]
    fn clustered_flight_channel_is_always_https() {
        let src = include_str!("lib.rs");
        assert!(src.contains("https://{endpoint}"));
        assert!(!src.contains("format!(\"http://{endpoint}\")"));
        assert!(src.contains("domain_name(\"skippr-cluster\")"));
    }

    #[test]
    fn execute_rejects_non_host_port() {
        let schema = Arc::new(Schema::empty());
        let node = FlightSqlExec::with_authorization(
            "http://127.0.0.1:0".into(),
            b"SELECT 1".to_vec(),
            schema,
            test_auth(),
        );
        let ctx = TaskContext::default();
        let err = match node.execute(0, Arc::new(ctx)) {
            Ok(_) => panic!("expected host:port rejection"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("host:port"));
    }

    #[test]
    fn encode_rejects_missing_authorization() {
        let schema = Arc::new(Schema::empty());
        let node = Arc::new(FlightSqlExec::new(
            "127.0.0.1:50051".into(),
            b"SELECT 1".to_vec(),
            schema,
        ));
        let codec = SkipprPhysicalCodec::default();
        let mut buf = Vec::new();
        assert!(codec.try_encode_flight(node, &mut buf).is_err());
    }

    #[tokio::test]
    async fn execute_against_closed_port_is_execution_error() {
        let schema = Arc::new(Schema::empty());
        let node = FlightSqlExec::with_authorization(
            "127.0.0.1:1".into(),
            b"SELECT 1".to_vec(),
            schema,
            test_auth(),
        );
        let stream = node.execute(0, Arc::new(TaskContext::default())).unwrap();
        let err = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap_err();
        assert!(matches!(err, DataFusionError::Execution(_)));
    }
}
