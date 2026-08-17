use std::sync::Arc;

use ballista_core::serde::{BallistaLogicalExtensionCodec, BallistaPhysicalExtensionCodec};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::TableProvider;
use datafusion::common::TableReference;
use datafusion::datasource::file_format::FileFormatFactory;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::TaskContext;
use datafusion::logical_expr::{Extension, LogicalPlan};
use datafusion::physical_plan::ExecutionPlan;
use datafusion_proto::logical_plan::LogicalExtensionCodec;
use datafusion_proto::physical_plan::PhysicalExtensionCodec;
use skippr_query_ballista::{
    decode_tagged_node, encode_tagged_node, schema_from_ipc, schema_to_ipc, FlightSqlExec,
    FlightSqlExecNode, IcebergScanNode, SkipprPhysicalCodec, UnionTableNode,
    PHYSICAL_KIND_FLIGHT_SQL, PHYSICAL_KIND_ICEBERG, PHYSICAL_KIND_UNION,
};

use crate::sqlrt::flight_sql_table::FlightSqlTableProvider;
use crate::sqlrt::iceberg_table::{IcebergScanExec, IcebergScanTableProvider};
use crate::sqlrt::tables::IcebergWalUnionProvider;
use crate::sqlrt::wal_table::{WalScanExec, WalTableProvider};

/// Physical codec for Ballista: Skippr leaves plus Ballista shuffle nodes.
#[derive(Debug, Default)]
pub struct SkipprHostCodec {
    flight: SkipprPhysicalCodec,
    ballista: BallistaPhysicalExtensionCodec,
}

impl PhysicalExtensionCodec for SkipprHostCodec {
    fn try_decode(
        &self,
        buf: &[u8],
        inputs: &[Arc<dyn ExecutionPlan>],
        ctx: &TaskContext,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match decode_tagged_node(buf) {
            Some((PHYSICAL_KIND_FLIGHT_SQL, _)) => self.flight.try_decode_flight(buf),
            Some((PHYSICAL_KIND_ICEBERG, payload)) => {
                let node = IcebergScanNode::decode_bytes(payload)?;
                IcebergScanExec::from_node(node)
                    .map(|exec| Arc::new(exec) as Arc<dyn ExecutionPlan>)
            }
            Some((kind, _)) => Err(DataFusionError::Internal(format!(
                "unknown Skippr physical node kind {kind}"
            ))),
            None => self.ballista.try_decode(buf, inputs, ctx),
        }
    }

    fn try_encode(&self, node: Arc<dyn ExecutionPlan>, buf: &mut Vec<u8>) -> Result<()> {
        if node.as_any().downcast_ref::<FlightSqlExec>().is_some() {
            return self.flight.try_encode_flight(node, buf);
        }
        if let Some(iceberg) = node.as_any().downcast_ref::<IcebergScanExec>() {
            encode_tagged_node(
                PHYSICAL_KIND_ICEBERG,
                &iceberg.encode_node()?.encode_bytes()?,
                buf,
            );
            return Ok(());
        }
        if node.as_any().downcast_ref::<WalScanExec>().is_some() {
            return Err(DataFusionError::Internal(
                "WalScanExec must not enter a Ballista plan; WAL is FlightSqlExec to the owner"
                    .into(),
            ));
        }
        self.ballista.try_encode(node, buf)
    }
}

/// Logical codec so Ballista can serialize custom UNION / Flight SQL / Iceberg
/// table providers. WAL table providers fail closed.
#[derive(Debug, Default)]
pub struct SkipprLogicalCodec {
    inner: BallistaLogicalExtensionCodec,
}

impl SkipprLogicalCodec {
    fn encode_flight_sql(provider: &FlightSqlTableProvider, buf: &mut Vec<u8>) -> Result<()> {
        skippr_query_ballista::validate_flight_endpoint(&provider.endpoint)?;
        if provider.authorization.trim().is_empty() {
            return Err(DataFusionError::Internal(
                "FlightSqlTableProvider requires session Authorization".into(),
            ));
        }
        let node = FlightSqlExecNode {
            endpoint: provider.endpoint.clone(),
            prepared_handle: provider.sql.as_bytes().to_vec(),
            arrow_schema_ipc: schema_to_ipc(provider.schema().as_ref())?,
            authorization: provider.authorization.clone(),
        };
        encode_tagged_node(PHYSICAL_KIND_FLIGHT_SQL, &node.encode_bytes()?, buf);
        Ok(())
    }

    fn decode_flight_sql(payload: &[u8], schema: SchemaRef) -> Result<Arc<dyn TableProvider>> {
        let node = FlightSqlExecNode::decode_bytes(payload)?;
        skippr_query_ballista::validate_flight_endpoint(&node.endpoint)?;
        let sql = String::from_utf8(node.prepared_handle).map_err(|err| {
            DataFusionError::Internal(format!("FlightSql table SQL is not utf-8: {err}"))
        })?;
        let schema = if node.arrow_schema_ipc.is_empty() {
            schema
        } else {
            schema_from_ipc(&node.arrow_schema_ipc)?
        };
        if node.authorization.trim().is_empty() {
            return Err(DataFusionError::Internal(
                "FlightSql table missing session Authorization".into(),
            ));
        }
        Ok(Arc::new(FlightSqlTableProvider::with_session(
            node.endpoint,
            sql,
            schema,
            node.authorization,
        )))
    }
}

impl LogicalExtensionCodec for SkipprLogicalCodec {
    fn try_decode(
        &self,
        buf: &[u8],
        inputs: &[LogicalPlan],
        ctx: &TaskContext,
    ) -> Result<Extension> {
        self.inner.try_decode(buf, inputs, ctx)
    }

    fn try_encode(&self, node: &Extension, buf: &mut Vec<u8>) -> Result<()> {
        self.inner.try_encode(node, buf)
    }

    fn try_decode_table_provider(
        &self,
        buf: &[u8],
        table_ref: &TableReference,
        schema: SchemaRef,
        ctx: &TaskContext,
    ) -> Result<Arc<dyn TableProvider>> {
        match decode_tagged_node(buf) {
            Some((PHYSICAL_KIND_FLIGHT_SQL, payload)) => Self::decode_flight_sql(payload, schema),
            Some((PHYSICAL_KIND_ICEBERG, payload)) => {
                let node = IcebergScanNode::decode_bytes(payload)?;
                IcebergScanTableProvider::from_scan_node(node)
                    .map(|provider| Arc::new(provider) as Arc<dyn TableProvider>)
            }
            Some((PHYSICAL_KIND_UNION, payload)) => {
                let node = UnionTableNode::decode_bytes(payload)?;
                let iceberg = self.try_decode_table_provider(
                    &node.iceberg_provider,
                    table_ref,
                    schema.clone(),
                    ctx,
                )?;
                let wal =
                    self.try_decode_table_provider(&node.wal_provider, table_ref, schema, ctx)?;
                let union_schema = if node.arrow_schema_ipc.is_empty() {
                    return Err(DataFusionError::Internal(
                        "Union table missing schema".into(),
                    ));
                } else {
                    schema_from_ipc(&node.arrow_schema_ipc)?
                };
                Ok(Arc::new(IcebergWalUnionProvider {
                    iceberg,
                    wal,
                    schema: union_schema,
                }))
            }
            Some((kind, _)) => Err(DataFusionError::Internal(format!(
                "unknown Skippr logical table kind {kind}"
            ))),
            None => self
                .inner
                .try_decode_table_provider(buf, table_ref, schema, ctx),
        }
    }

    fn try_encode_table_provider(
        &self,
        table_ref: &TableReference,
        node: Arc<dyn TableProvider>,
        buf: &mut Vec<u8>,
    ) -> Result<()> {
        if node.as_any().downcast_ref::<WalTableProvider>().is_some() {
            return Err(DataFusionError::Internal(
                "WalTableProvider must not enter a Ballista plan; WAL is FlightSqlTableProvider to the owner"
                    .into(),
            ));
        }
        if let Some(flight) = node.as_any().downcast_ref::<FlightSqlTableProvider>() {
            return Self::encode_flight_sql(flight, buf);
        }
        if let Some(iceberg) = node.as_any().downcast_ref::<IcebergScanTableProvider>() {
            encode_tagged_node(
                PHYSICAL_KIND_ICEBERG,
                &iceberg.encode_scan_node()?.encode_bytes()?,
                buf,
            );
            return Ok(());
        }
        if let Some(union) = node.as_any().downcast_ref::<IcebergWalUnionProvider>() {
            let mut iceberg_provider = Vec::new();
            self.try_encode_table_provider(
                table_ref,
                union.iceberg.clone(),
                &mut iceberg_provider,
            )?;
            let mut wal_provider = Vec::new();
            self.try_encode_table_provider(table_ref, union.wal.clone(), &mut wal_provider)?;
            let encoded = UnionTableNode {
                iceberg_provider,
                wal_provider,
                arrow_schema_ipc: schema_to_ipc(union.schema.as_ref())?,
            };
            encode_tagged_node(PHYSICAL_KIND_UNION, &encoded.encode_bytes()?, buf);
            return Ok(());
        }
        self.inner.try_encode_table_provider(table_ref, node, buf)
    }

    fn try_decode_file_format(
        &self,
        buf: &[u8],
        ctx: &TaskContext,
    ) -> Result<Arc<dyn FileFormatFactory>> {
        self.inner.try_decode_file_format(buf, ctx)
    }

    fn try_encode_file_format(
        &self,
        buf: &mut Vec<u8>,
        node: Arc<dyn FileFormatFactory>,
    ) -> Result<()> {
        self.inner.try_encode_file_format(buf, node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use skippr_query_ballista::PHYSICAL_MAGIC;

    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]))
    }

    #[test]
    fn host_codec_round_trips_flight_sql() {
        let node = Arc::new(FlightSqlExec::with_authorization(
            "127.0.0.1:9".into(),
            b"SELECT 1".to_vec(),
            schema(),
            skippr_query_ballista::basic_authorization("t", "w"),
        )) as Arc<dyn ExecutionPlan>;
        let codec = SkipprHostCodec::default();
        let mut buf = Vec::new();
        codec.try_encode(node.clone(), &mut buf).unwrap();
        assert_eq!(&buf[..4], PHYSICAL_MAGIC);
        let decoded = codec
            .try_decode(&buf, &[], &TaskContext::default())
            .unwrap();
        assert_eq!(decoded.name(), "FlightSqlExec");
    }

    #[test]
    fn host_codec_round_trips_iceberg_and_rejects_wal() {
        let iceberg = Arc::new(
            IcebergScanExec::from_node(IcebergScanNode {
                arrow_schema_ipc: skippr_query_ballista::schema_to_ipc(schema().as_ref()).unwrap(),
                snapshot_id: 7,
                projection: vec![0],
                limit: Some(3),
                catalog_json: "{\"adapter\":\"dynamodb\"}".into(),
                catalog_ns: "default".into(),
                table_name: "events".into(),
            })
            .unwrap(),
        ) as Arc<dyn ExecutionPlan>;
        let codec = SkipprHostCodec::default();
        let mut buf = Vec::new();
        codec.try_encode(iceberg, &mut buf).unwrap();
        assert_eq!(&buf[..4], PHYSICAL_MAGIC);
        let decoded = codec
            .try_decode(&buf, &[], &TaskContext::default())
            .unwrap();
        assert_eq!(decoded.name(), "IcebergScanExec");

        let key = skippr_lease::PipelineKey::new("t", "w", "p").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        let wal = Arc::new(
            WalScanExec::new(schema(), key, "ns".into(), vec!["seg".into()], paths, None).unwrap(),
        ) as Arc<dyn ExecutionPlan>;
        buf.clear();
        let err = codec.try_encode(wal, &mut buf).unwrap_err();
        assert!(err
            .to_string()
            .contains("WalScanExec must not enter a Ballista plan"));
    }

    #[test]
    fn logical_codec_round_trips_union_flight_and_iceberg() {
        let schema = schema();
        let iceberg = Arc::new(
            IcebergScanTableProvider::from_scan_node(IcebergScanNode {
                arrow_schema_ipc: schema_to_ipc(schema.as_ref()).unwrap(),
                snapshot_id: 11,
                projection: Vec::new(),
                limit: None,
                catalog_json: "{\"adapter\":\"dynamodb\"}".into(),
                catalog_ns: "default".into(),
                table_name: "events".into(),
            })
            .unwrap(),
        ) as Arc<dyn TableProvider>;
        let wal = Arc::new(FlightSqlTableProvider::with_session(
            "127.0.0.1:9".into(),
            "SELECT 1".into(),
            schema.clone(),
            skippr_query_ballista::basic_authorization("t", "w"),
        )) as Arc<dyn TableProvider>;
        let union = Arc::new(IcebergWalUnionProvider {
            iceberg,
            wal,
            schema: schema.clone(),
        }) as Arc<dyn TableProvider>;
        let codec = SkipprLogicalCodec::default();
        let table_ref = TableReference::bare("hla_events");
        let ctx = TaskContext::default();
        let mut buf = Vec::new();
        codec
            .try_encode_table_provider(&table_ref, union, &mut buf)
            .unwrap();
        assert_eq!(&buf[..4], PHYSICAL_MAGIC);
        assert_eq!(buf[4], PHYSICAL_KIND_UNION);
        let decoded = codec
            .try_decode_table_provider(&buf, &table_ref, schema.clone(), &ctx)
            .unwrap();
        let union = decoded
            .as_any()
            .downcast_ref::<IcebergWalUnionProvider>()
            .expect("union provider");
        assert!(union
            .iceberg
            .as_any()
            .downcast_ref::<IcebergScanTableProvider>()
            .is_some());
        let flight = union
            .wal
            .as_any()
            .downcast_ref::<FlightSqlTableProvider>()
            .expect("flight wal");
        assert_eq!(flight.endpoint, "127.0.0.1:9");
        assert_eq!(flight.sql, "SELECT 1");
    }

    #[test]
    fn logical_codec_rejects_wal_table_provider() {
        let key = skippr_lease::PipelineKey::new("t", "w", "p").unwrap();
        let wal = Arc::new(WalTableProvider::live(
            schema(),
            key,
            "ns".into(),
            Vec::new(),
        )) as Arc<dyn TableProvider>;
        let codec = SkipprLogicalCodec::default();
        let mut buf = Vec::new();
        let err = codec
            .try_encode_table_provider(&TableReference::bare("wal"), wal, &mut buf)
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("WalTableProvider must not enter a Ballista plan"));
    }
}
