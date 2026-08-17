use std::any::Any;
use std::sync::Arc;

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};
use futures::{StreamExt, TryStreamExt};
use iceberg::table::Table;
use iceberg::Catalog;

use skippr_query_ballista::{schema_from_ipc, schema_to_ipc, IcebergScanNode};

/// Read-only Iceberg scan over a pinned snapshot. Iceberg pipelines never fall
/// back to Parquet listing.
pub struct IcebergScanTableProvider {
    table: Option<Table>,
    schema: SchemaRef,
    snapshot_id: i64,
    catalog_json: String,
    catalog_ns: String,
    table_name: String,
}

impl std::fmt::Debug for IcebergScanTableProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IcebergScanTableProvider")
            .field("snapshot_id", &self.snapshot_id)
            .finish()
    }
}

impl IcebergScanTableProvider {
    pub fn new(table: Table, snapshot_id: i64) -> Result<Self> {
        Self::new_with_catalog(
            table,
            snapshot_id,
            String::new(),
            String::new(),
            String::new(),
        )
    }

    pub fn new_with_catalog(
        table: Table,
        snapshot_id: i64,
        catalog_json: String,
        catalog_ns: String,
        table_name: String,
    ) -> Result<Self> {
        let schema = table.metadata().current_schema();
        let arrow = iceberg::arrow::schema_to_arrow_schema(schema)
            .map_err(|err| DataFusionError::External(Box::new(err)))?;
        let ids = compacted_wal_segment_ids(&table);
        let arrow = if ids.is_empty() {
            arrow
        } else {
            let mut metadata = arrow.metadata().clone();
            metadata.insert("skippr.wal-segment-ids".into(), ids.join(";"));
            datafusion::arrow::datatypes::Schema::new_with_metadata(
                arrow.fields().clone(),
                metadata,
            )
        };
        Ok(Self {
            table: Some(table),
            schema: Arc::new(arrow),
            snapshot_id,
            catalog_json,
            catalog_ns,
            table_name,
        })
    }

    pub fn from_scan_node(node: IcebergScanNode) -> Result<Self> {
        if node.arrow_schema_ipc.is_empty() {
            return Err(DataFusionError::Internal(
                "IcebergScan table missing schema".into(),
            ));
        }
        let schema = schema_from_ipc(&node.arrow_schema_ipc)?;
        Ok(Self {
            table: None,
            schema,
            snapshot_id: node.snapshot_id,
            catalog_json: node.catalog_json,
            catalog_ns: node.catalog_ns,
            table_name: node.table_name,
        })
    }

    pub fn encode_scan_node(&self) -> Result<IcebergScanNode> {
        if self.catalog_json.is_empty() {
            return Err(DataFusionError::Internal(
                "IcebergScanTableProvider cannot enter a Ballista plan without catalog_json".into(),
            ));
        }
        Ok(IcebergScanNode {
            arrow_schema_ipc: schema_to_ipc(self.schema.as_ref())?,
            snapshot_id: self.snapshot_id,
            projection: Vec::new(),
            limit: None,
            catalog_json: self.catalog_json.clone(),
            catalog_ns: self.catalog_ns.clone(),
            table_name: self.table_name.clone(),
        })
    }

    pub fn snapshot_id(&self) -> i64 {
        self.snapshot_id
    }

    pub fn compacted_segment_ids(&self) -> Vec<String> {
        match &self.table {
            Some(table) => compacted_wal_segment_ids(table),
            None => {
                let value = self
                    .schema
                    .metadata()
                    .get(SNAPSHOT_WAL_SEGMENT_IDS)
                    .cloned()
                    .unwrap_or_default()
                    .replace(';', ",");
                segment_ids_from_snapshot_properties(std::iter::once(value))
            }
        }
    }
}

#[async_trait::async_trait]
impl TableProvider for IcebergScanTableProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let schema = crate::sqlrt::wal_table::projected_schema(&self.schema, projection)?;
        match &self.table {
            Some(table) => IcebergScanExec::from_table(
                table.clone(),
                self.snapshot_id,
                schema,
                projection.cloned(),
                limit,
                self.catalog_json.clone(),
                self.catalog_ns.clone(),
                self.table_name.clone(),
            ),
            None => IcebergScanExec::from_node(IcebergScanNode {
                arrow_schema_ipc: schema_to_ipc(schema.as_ref())?,
                snapshot_id: self.snapshot_id,
                projection: projection
                    .map(|cols| cols.iter().map(|i| *i as u64).collect())
                    .unwrap_or_default(),
                limit: limit.map(|n| n as u64),
                catalog_json: self.catalog_json.clone(),
                catalog_ns: self.catalog_ns.clone(),
                table_name: self.table_name.clone(),
            }),
        }
        .map(|exec| Arc::new(exec) as Arc<dyn ExecutionPlan>)
    }
}

#[derive(Clone)]
pub struct IcebergScanExec {
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
    snapshot_id: i64,
    projection: Option<Vec<usize>>,
    limit: Option<usize>,
    catalog_json: String,
    catalog_ns: String,
    table_name: String,
    table: Option<Table>,
}

impl std::fmt::Debug for IcebergScanExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IcebergScanExec")
            .field("snapshot_id", &self.snapshot_id)
            .field("table_name", &self.table_name)
            .finish()
    }
}

impl IcebergScanExec {
    pub fn from_table(
        table: Table,
        snapshot_id: i64,
        schema: SchemaRef,
        projection: Option<Vec<usize>>,
        limit: Option<usize>,
        catalog_json: String,
        catalog_ns: String,
        table_name: String,
    ) -> Result<Self> {
        Ok(Self {
            properties: plan_properties(&schema),
            schema,
            snapshot_id,
            projection,
            limit,
            catalog_json,
            catalog_ns,
            table_name,
            table: Some(table),
        })
    }

    pub fn encode_node(&self) -> Result<IcebergScanNode> {
        Ok(IcebergScanNode {
            arrow_schema_ipc: schema_to_ipc(self.schema.as_ref())?,
            snapshot_id: self.snapshot_id,
            projection: self
                .projection
                .as_ref()
                .map(|p| p.iter().map(|i| *i as u64).collect())
                .unwrap_or_default(),
            limit: self.limit.map(|n| n as u64),
            catalog_json: self.catalog_json.clone(),
            catalog_ns: self.catalog_ns.clone(),
            table_name: self.table_name.clone(),
        })
    }

    pub fn from_node(node: IcebergScanNode) -> Result<Self> {
        if node.arrow_schema_ipc.is_empty() {
            return Err(DataFusionError::Internal(
                "IcebergScan node missing schema".into(),
            ));
        }
        let schema = schema_from_ipc(&node.arrow_schema_ipc)?;
        let projection = if node.projection.is_empty() {
            None
        } else {
            Some(node.projection.iter().map(|i| *i as usize).collect())
        };
        Ok(Self {
            properties: plan_properties(&schema),
            schema,
            snapshot_id: node.snapshot_id,
            projection,
            limit: node.limit.map(|n| n as usize),
            catalog_json: node.catalog_json,
            catalog_ns: node.catalog_ns,
            table_name: node.table_name,
            table: None,
        })
    }

    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

fn plan_properties(schema: &SchemaRef) -> Arc<PlanProperties> {
    Arc::new(PlanProperties::new(
        datafusion::physical_expr::EquivalenceProperties::new(schema.clone()),
        datafusion::physical_plan::Partitioning::UnknownPartitioning(1),
        datafusion::physical_plan::execution_plan::EmissionType::Incremental,
        datafusion::physical_plan::execution_plan::Boundedness::Bounded,
    ))
}

impl DisplayAs for IcebergScanExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "IcebergScanExec(table={}, snapshot={})",
            self.table_name, self.snapshot_id
        )
    }
}

impl ExecutionPlan for IcebergScanExec {
    fn name(&self) -> &str {
        "IcebergScanExec"
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
        let handle = tokio::runtime::Handle::try_current().map_err(|err| {
            DataFusionError::Execution(format!("IcebergScanExec requires a Tokio runtime: {err}"))
        })?;
        let snapshot_id = self.snapshot_id;
        let select_names: Option<Vec<String>> = if self.schema.fields().is_empty() {
            None
        } else {
            Some(
                self.schema
                    .fields()
                    .iter()
                    .map(|field| field.name().clone())
                    .collect(),
            )
        };
        let limit = self.limit;
        let table = self.table.clone();
        let catalog_json = self.catalog_json.clone();
        let catalog_ns = self.catalog_ns.clone();
        let table_name = self.table_name.clone();
        let out_schema = self.schema.clone();
        let (tx, rx) = futures::channel::mpsc::channel(8);
        handle.spawn(async move {
            stream_iceberg_into(
                table,
                snapshot_id,
                select_names,
                limit,
                catalog_json,
                catalog_ns,
                table_name,
                out_schema,
                tx,
            )
            .await;
        });
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            self.schema.clone(),
            rx,
        )))
    }
}

async fn stream_iceberg_into(
    table: Option<Table>,
    snapshot_id: i64,
    schema_names: Option<Vec<String>>,
    limit: Option<usize>,
    catalog_json: String,
    catalog_ns: String,
    table_name: String,
    out_schema: SchemaRef,
    mut tx: futures::channel::mpsc::Sender<Result<RecordBatch>>,
) {
    use futures::SinkExt;
    match iceberg_arrow_stream(
        table,
        snapshot_id,
        schema_names,
        catalog_json,
        catalog_ns,
        table_name,
    )
    .await
    {
        Ok(mut stream) => {
            let mut remaining = limit;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(batch) => {
                        let batch = match crate::sqlrt::wal_table::project_batch(batch, &out_schema)
                        {
                            Ok(batch) => batch,
                            Err(err) => {
                                let _ = tx.send(Err(err)).await;
                                break;
                            }
                        };
                        let batch = match remaining.as_mut() {
                            Some(left) if *left == 0 => break,
                            Some(left) if batch.num_rows() > *left => {
                                let sliced = batch.slice(0, *left);
                                *left = 0;
                                sliced
                            }
                            Some(left) => {
                                *left -= batch.num_rows();
                                batch
                            }
                            None => batch,
                        };
                        if tx.send(Ok(batch)).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err)).await;
                        break;
                    }
                }
            }
        }
        Err(err) => {
            let _ = tx.send(Err(err)).await;
        }
    }
}

async fn iceberg_arrow_stream(
    table: Option<Table>,
    snapshot_id: i64,
    schema_names: Option<Vec<String>>,
    catalog_json: String,
    catalog_ns: String,
    table_name: String,
) -> Result<crate::sqlrt::stream_plan::BatchStream> {
    let table = match table {
        Some(table) => table,
        None => reload_iceberg_table(&catalog_json, &catalog_ns, &table_name).await?,
    };
    let mut builder = table.scan().snapshot_id(snapshot_id);
    if let Some(names) = schema_names {
        if !names.is_empty() {
            builder = builder.select(names);
        }
    }
    let stream = builder
        .build()
        .map_err(|err| DataFusionError::External(Box::new(err)))?
        .to_arrow()
        .await
        .map_err(|err| DataFusionError::External(Box::new(err)))?;
    Ok(Box::pin(
        stream.map_err(|err| DataFusionError::External(Box::new(err))),
    ))
}

async fn reload_iceberg_table(
    catalog_json: &str,
    catalog_ns: &str,
    table_name: &str,
) -> Result<Table> {
    if catalog_json.is_empty() || table_name.is_empty() {
        return Err(DataFusionError::Execution(
            "IcebergScanExec is missing catalog reload state".into(),
        ));
    }
    let catalog_cfg: skippr_iceberg_catalog::IcebergCatalogConfig =
        serde_json::from_str(catalog_json)
            .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    let ident = iceberg::TableIdent::from_strs([catalog_ns, table_name])
        .map_err(|err| DataFusionError::External(Box::new(err)))?;
    #[cfg(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    ))]
    {
        match &catalog_cfg {
            skippr_iceberg_catalog::IcebergCatalogConfig::Skippr { .. } => {
                let catalog = crate::cluster::backend::open_skippr_catalog(&catalog_cfg)
                    .await
                    .map_err(DataFusionError::Plan)?;
                let (table, _) = load_pinned_iceberg_table(catalog, &ident).await?;
                Ok(table)
            }
            other => Err(DataFusionError::Plan(format!(
                "IcebergScanExec cannot reload catalog adapter '{}'",
                other.adapter_name()
            ))),
        }
    }
    #[cfg(not(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    )))]
    {
        let _ = (catalog_cfg, ident);
        Err(DataFusionError::Plan(
            "IcebergScanExec catalog reload requires offset-store-dynamodb or offset-store-cloud-tables".into(),
        ))
    }
}

pub async fn load_pinned_iceberg_table(
    catalog: Arc<dyn Catalog>,
    ident: &iceberg::TableIdent,
) -> Result<(Table, i64)> {
    let table = catalog
        .load_table(ident)
        .await
        .map_err(|err| DataFusionError::External(Box::new(err)))?;
    let snapshot_id = table
        .metadata()
        .current_snapshot()
        .map(|snap| snap.snapshot_id())
        .ok_or_else(|| DataFusionError::Plan("Iceberg table has no current snapshot".into()))?;
    Ok((table, snapshot_id))
}

const SNAPSHOT_WAL_SEGMENT_IDS: &str = "skippr.wal-segment-ids";

/// WAL segment ids recorded on Iceberg snapshot ancestry after a successful
/// grouped compaction. Live WAL must exclude these even when CompleteSlices
/// has not yet reached the current Iceberg snapshot.
pub fn compacted_wal_segment_ids(table: &Table) -> Vec<String> {
    segment_ids_from_snapshot_properties(table.metadata().snapshots().flat_map(|snapshot| {
        snapshot
            .summary()
            .additional_properties
            .get(SNAPSHOT_WAL_SEGMENT_IDS)
            .cloned()
    }))
}

pub fn segment_ids_from_snapshot_properties(
    values: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut ids = Vec::new();
    for value in values {
        for id in value.split(',') {
            let id = id.trim();
            if id.is_empty() {
                continue;
            }
            if !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    #[test]
    fn iceberg_provider_never_constructs_listing_table() {
        let src = include_str!("iceberg_table.rs");
        assert!(!src.contains(concat!("Listing", "Table::")));
        assert!(!src.contains(concat!("build_s3", "_df(")));
    }

    #[test]
    fn snapshot_property_segment_ids_are_deduped() {
        let ids = super::segment_ids_from_snapshot_properties([
            "seg-a,seg-b".to_string(),
            "seg-b,seg-c".to_string(),
            String::new(),
        ]);
        assert_eq!(
            ids,
            vec![
                "seg-a".to_string(),
                "seg-b".to_string(),
                "seg-c".to_string()
            ]
        );
    }
}
