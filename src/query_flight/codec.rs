use std::sync::Arc;

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::TaskContext;
use datafusion::physical_plan::ExecutionPlan;
use datafusion_proto::physical_plan::DefaultPhysicalExtensionCodec;
use datafusion_proto::physical_plan::PhysicalExtensionCodec;
use datafusion_proto::physical_plan::PhysicalProtoConverterExtension;
use skippr_query_plan::{
    decode_tagged_node, encode_tagged_node, FlightSqlExec, IcebergScanNode, SkipprPhysicalCodec,
    PHYSICAL_KIND_CDC_RECONCILE, PHYSICAL_KIND_FLIGHT_SQL, PHYSICAL_KIND_ICEBERG,
};

use crate::sqlrt::cdc_reconcile::CdcReconcileExec;
use crate::sqlrt::iceberg_table::IcebergScanExec;
use crate::sqlrt::wal_table::WalScanExec;

/// Physical codec for QueryRuntime: Skippr leaves plus DataFusion defaults.
#[derive(Debug)]
pub struct SkipprHostCodec {
    flight: SkipprPhysicalCodec,
    inner: DefaultPhysicalExtensionCodec,
}

impl Default for SkipprHostCodec {
    fn default() -> Self {
        Self {
            flight: SkipprPhysicalCodec::default(),
            inner: DefaultPhysicalExtensionCodec {},
        }
    }
}

impl PhysicalExtensionCodec for SkipprHostCodec {
    fn try_decode(
        &self,
        buf: &[u8],
        inputs: &[Arc<dyn ExecutionPlan>],
        ctx: &TaskContext,
        proto_converter: &dyn PhysicalProtoConverterExtension,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match decode_tagged_node(buf) {
            Some((PHYSICAL_KIND_FLIGHT_SQL, _)) => self.flight.try_decode_flight(buf),
            Some((PHYSICAL_KIND_ICEBERG, payload)) => {
                let node = IcebergScanNode::decode_bytes(payload)?;
                IcebergScanExec::from_node(node)
                    .map(|exec| Arc::new(exec) as Arc<dyn ExecutionPlan>)
            }
            Some((PHYSICAL_KIND_CDC_RECONCILE, payload)) => {
                CdcReconcileExec::from_bytes(payload, inputs)
            }
            Some((kind, _)) => Err(DataFusionError::Internal(format!(
                "unknown Skippr physical node kind {kind}"
            ))),
            None => self.inner.try_decode(buf, inputs, ctx, proto_converter),
        }
    }

    fn try_encode(
        &self,
        node: Arc<dyn ExecutionPlan>,
        buf: &mut Vec<u8>,
        proto_converter: &dyn PhysicalProtoConverterExtension,
    ) -> Result<()> {
        if node.is::<FlightSqlExec>() {
            return self.flight.try_encode_flight(node, buf);
        }
        if let Some(iceberg) = node.downcast_ref::<IcebergScanExec>() {
            encode_tagged_node(
                PHYSICAL_KIND_ICEBERG,
                &iceberg.encode_node()?.encode_bytes()?,
                buf,
            );
            return Ok(());
        }
        if let Some(merge) = node.downcast_ref::<CdcReconcileExec>() {
            encode_tagged_node(PHYSICAL_KIND_CDC_RECONCILE, &merge.encode_bytes()?, buf);
            return Ok(());
        }
        if node.is::<WalScanExec>() {
            return Err(DataFusionError::Internal(
                "WalScanExec must not enter a distributed plan; WAL is FlightSqlExec to the owner"
                    .into(),
            ));
        }
        self.inner.try_encode(node, buf, proto_converter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion_proto::physical_plan::DefaultPhysicalProtoConverter;
    use skippr_query_plan::PHYSICAL_MAGIC;

    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]))
    }

    #[test]
    fn host_codec_round_trips_flight_sql() {
        let node = Arc::new(FlightSqlExec::with_authorization(
            "127.0.0.1:9".into(),
            b"SELECT 1".to_vec(),
            schema(),
            skippr_query_plan::basic_authorization("t", "w"),
        )) as Arc<dyn ExecutionPlan>;
        let codec = SkipprHostCodec::default();
        let conv = DefaultPhysicalProtoConverter {};
        let mut buf = Vec::new();
        codec.try_encode(node.clone(), &mut buf, &conv).unwrap();
        assert_eq!(&buf[..4], PHYSICAL_MAGIC);
        let decoded = codec
            .try_decode(&buf, &[], &TaskContext::default(), &conv)
            .unwrap();
        assert_eq!(decoded.name(), "FlightSqlExec");
    }

    #[test]
    fn host_codec_round_trips_iceberg_and_rejects_wal() {
        let iceberg = Arc::new(
            IcebergScanExec::from_node(IcebergScanNode {
                arrow_schema_ipc: skippr_query_plan::schema_to_ipc(schema().as_ref()).unwrap(),
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
        let conv = DefaultPhysicalProtoConverter {};
        let mut buf = Vec::new();
        codec.try_encode(iceberg, &mut buf, &conv).unwrap();
        assert_eq!(&buf[..4], PHYSICAL_MAGIC);
        let decoded = codec
            .try_decode(&buf, &[], &TaskContext::default(), &conv)
            .unwrap();
        assert_eq!(decoded.name(), "IcebergScanExec");

        let key = skippr_lease::PipelineKey::new("t", "w", "p").unwrap();
        let wal = Arc::new(
            WalScanExec::new(
                schema(),
                key,
                "ns".into(),
                vec![("seg".into(), 0)],
                None,
                false,
            )
            .unwrap(),
        ) as Arc<dyn ExecutionPlan>;
        buf.clear();
        let err = codec.try_encode(wal, &mut buf, &conv).unwrap_err();
        assert!(err
            .to_string()
            .contains("WalScanExec must not enter a distributed plan"));
    }

    #[test]
    fn host_codec_round_trips_cdc_reconcile() {
        use datafusion::physical_plan::empty::EmptyExec;

        let user = schema();
        let iceberg = Arc::new(EmptyExec::new(user.clone())) as Arc<dyn ExecutionPlan>;
        let wal = Arc::new(EmptyExec::new(
            crate::sqlrt::cdc_reconcile::cdc_sidecar_schema(&user),
        )) as Arc<dyn ExecutionPlan>;
        let merge = Arc::new(
            CdcReconcileExec::try_new(iceberg.clone(), wal.clone(), vec!["id".into()], user)
                .unwrap(),
        ) as Arc<dyn ExecutionPlan>;
        let codec = SkipprHostCodec::default();
        let conv = DefaultPhysicalProtoConverter {};
        let mut buf = Vec::new();
        codec.try_encode(merge, &mut buf, &conv).unwrap();
        assert_eq!(&buf[..4], PHYSICAL_MAGIC);
        let decoded = codec
            .try_decode(&buf, &[iceberg, wal], &TaskContext::default(), &conv)
            .unwrap();
        assert_eq!(decoded.name(), "CdcReconcileExec");
    }
}
