use std::collections::HashSet;
use std::sync::Arc;

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};
use futures::{StreamExt, TryStreamExt};
use iceberg::table::Table;
use iceberg::Catalog;

use skippr_query_plan::{schema_from_ipc, schema_to_ipc, IcebergScanNode};

use crate::wal_slice_ref::{
    decode_wal_slices, encode_wal_slices, walk_ancestry_wal_slices, WalSliceRef,
    SNAPSHOT_WAL_SLICES,
};

/// Read-only Iceberg scan over a pinned snapshot. Iceberg pipelines never fall
/// back to Parquet listing.
pub struct IcebergScanTableProvider {
    table: Option<Table>,
    schema: SchemaRef,
    snapshot_id: i64,
    catalog_json: String,
    catalog_ns: String,
    table_name: String,
    excluded: HashSet<WalSliceRef>,
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
        let slices = match table.metadata().snapshot_by_id(snapshot_id) {
            Some(_) => excluded_slices(&table, snapshot_id)?,
            None => HashSet::new(),
        };
        let arrow = if slices.is_empty() {
            arrow
        } else {
            let mut metadata = arrow.metadata().clone();
            metadata.insert(SNAPSHOT_WAL_SLICES.into(), encode_wal_slices(&slices));
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
            excluded: slices,
        })
    }

    pub fn from_scan_node(node: IcebergScanNode) -> Result<Self> {
        if node.arrow_schema_ipc.is_empty() {
            return Err(DataFusionError::Internal(
                "IcebergScan table missing schema".into(),
            ));
        }
        let schema = schema_from_ipc(&node.arrow_schema_ipc)?;
        let excluded = slices_from_schema(&schema)?;
        Ok(Self {
            table: None,
            schema,
            snapshot_id: node.snapshot_id,
            catalog_json: node.catalog_json,
            catalog_ns: node.catalog_ns,
            table_name: node.table_name,
            excluded,
        })
    }

    pub fn encode_scan_node(&self) -> Result<IcebergScanNode> {
        if self.catalog_json.is_empty() {
            return Err(DataFusionError::Internal(
                "IcebergScanTableProvider cannot enter a distributed plan without catalog_json"
                    .into(),
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

    pub fn excluded_slices(&self) -> Result<HashSet<WalSliceRef>> {
        Ok(self.excluded.clone())
    }
}

fn slices_from_schema(
    schema: &datafusion::arrow::datatypes::Schema,
) -> Result<HashSet<WalSliceRef>> {
    let value = schema
        .metadata()
        .get(SNAPSHOT_WAL_SLICES)
        .cloned()
        .unwrap_or_default();
    decode_wal_slices(&value)
        .map(|set| set.into_iter().collect())
        .map_err(|err| DataFusionError::Plan(err.to_string()))
}

#[async_trait::async_trait]
impl TableProvider for IcebergScanTableProvider {
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
        if self
            .table
            .as_ref()
            .is_some_and(|table| table.metadata().snapshot_by_id(self.snapshot_id).is_none())
        {
            return Ok(Arc::new(EmptyExec::new(schema)));
        }
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

    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(
            &Arc<dyn datafusion::physical_plan::PhysicalExpr>,
        ) -> Result<datafusion::common::tree_node::TreeNodeRecursion>,
    ) -> Result<datafusion::common::tree_node::TreeNodeRecursion> {
        Ok(datafusion::common::tree_node::TreeNodeRecursion::Continue)
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
        let select = if self.schema.fields().is_empty() {
            IcebergColumnSelect::Empty
        } else {
            IcebergColumnSelect::Named(
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
                select,
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum IcebergColumnSelect {
    Named(Vec<String>),
    Empty,
}

async fn stream_iceberg_into(
    table: Option<Table>,
    snapshot_id: i64,
    select: IcebergColumnSelect,
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
        select,
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

/// Iceberg snapshot scans bind column names against the snapshot schema.
/// After `replace_schema` (rename), current names must map to snapshot names
/// by field id so readers keep seeing evolved names.
pub(crate) fn snapshot_scan_names(
    current: &iceberg::spec::Schema,
    snapshot: &iceberg::spec::Schema,
    current_names: &[String],
) -> Vec<String> {
    current_names
        .iter()
        .map(|name| {
            current
                .field_id_by_name(name)
                .and_then(|id| snapshot.field_by_id(id))
                .map(|field| field.name.clone())
                .unwrap_or_else(|| name.clone())
        })
        .collect()
}

fn rename_batch_fields(batch: RecordBatch, names: &[String]) -> Result<RecordBatch> {
    if names.is_empty() || batch.num_columns() != names.len() {
        return Ok(batch);
    }
    let fields = batch
        .schema()
        .fields()
        .iter()
        .zip(names)
        .map(|(field, name)| {
            Arc::new(
                arrow::datatypes::Field::new(name, field.data_type().clone(), field.is_nullable())
                    .with_metadata(field.metadata().clone()),
            )
        })
        .collect::<Vec<_>>();
    RecordBatch::try_new(
        Arc::new(arrow::datatypes::Schema::new(fields)),
        batch.columns().to_vec(),
    )
    .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))
}

async fn iceberg_arrow_stream(
    table: Option<Table>,
    snapshot_id: i64,
    select: IcebergColumnSelect,
    catalog_json: String,
    catalog_ns: String,
    table_name: String,
) -> Result<crate::sqlrt::stream_plan::BatchStream> {
    let table = match table {
        Some(table) => table,
        None => reload_iceberg_table(&catalog_json, &catalog_ns, &table_name).await?,
    };
    let (select, current_names) = match select {
        IcebergColumnSelect::Named(names) => {
            let current = table.metadata().current_schema();
            let snap_schema = table
                .metadata()
                .snapshot_by_id(snapshot_id)
                .ok_or_else(|| {
                    DataFusionError::Plan(format!("Iceberg snapshot {snapshot_id} not found"))
                })?
                .schema(table.metadata())
                .map_err(|err| DataFusionError::External(Box::new(err)))?;
            let snap_names = snapshot_scan_names(current.as_ref(), snap_schema.as_ref(), &names);
            (IcebergColumnSelect::Named(snap_names), names)
        }
        IcebergColumnSelect::Empty => (IcebergColumnSelect::Empty, Vec::new()),
    };
    let mut builder = table.scan().snapshot_id(snapshot_id);
    builder = match select {
        IcebergColumnSelect::Named(names) => builder.select(names),
        IcebergColumnSelect::Empty => builder.select_empty(),
    };
    let stream = builder
        .build()
        .map_err(|err| DataFusionError::External(Box::new(err)))?
        .to_arrow()
        .await
        .map_err(|err| DataFusionError::External(Box::new(err)))?;
    Ok(Box::pin(
        stream
            .map_err(|err| DataFusionError::External(Box::new(err)))
            .and_then(move |batch| {
                let names = current_names.clone();
                async move { rename_batch_fields(batch, &names) }
            }),
    ))
}

pub(crate) async fn reload_iceberg_table(
    catalog_json: &str,
    catalog_ns: &str,
    table_name: &str,
) -> Result<Table> {
    if catalog_json.is_empty() || table_name.is_empty() {
        return Err(DataFusionError::Execution(
            "IcebergScanExec is missing catalog reload state".into(),
        ));
    }
    let catalog = crate::cluster::backend::reopen_iceberg_catalog(catalog_json)
        .await
        .map_err(DataFusionError::Plan)?;
    let ident = iceberg::TableIdent::from_strs([catalog_ns, table_name])
        .map_err(|err| DataFusionError::External(Box::new(err)))?;
    catalog
        .load_table(&ident)
        .await
        .map_err(|err| DataFusionError::External(Box::new(err)))
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

/// WAL slice refs recorded on Iceberg snapshot ancestry after a successful
/// grouped compaction. Live WAL must exclude these even when slice-done bits
/// have not yet reached the current Iceberg snapshot.
pub fn excluded_slices(table: &Table, snapshot_id: i64) -> Result<HashSet<WalSliceRef>> {
    walk_ancestry_wal_slices(snapshot_id, |id| {
        let snap = table.metadata().snapshot_by_id(id)?;
        let prop = snap
            .summary()
            .additional_properties
            .get(SNAPSHOT_WAL_SLICES)
            .cloned();
        Some((snap.parent_snapshot_id(), prop))
    })
    .map_err(|err| DataFusionError::Plan(err.to_string()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_projection_selects_no_iceberg_columns() {
        let src = include_str!("iceberg_table.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(prod.contains("IcebergColumnSelect::Empty"));
        assert!(prod.contains("builder.select_empty()"));
        assert!(!prod.contains("select_names: Option<Vec<String>>"));
        let execute = prod
            .split("fn execute(")
            .nth(1)
            .expect("IcebergScanExec::execute");
        let execute = execute
            .split("async fn stream_iceberg_into")
            .next()
            .unwrap();
        assert!(
            execute.contains("IcebergColumnSelect::Empty"),
            "COUNT(*) empty projection must not fall through to select_all"
        );
        assert!(
            !execute.contains("select_names"),
            "empty projection must not use Option<Vec> select_names"
        );
    }

    #[test]
    fn iceberg_provider_never_constructs_listing_table() {
        let src = include_str!("iceberg_table.rs");
        assert!(!src.contains(concat!("Listing", "Table::")));
        assert!(!src.contains(concat!("build_s3", "_df(")));
    }

    #[test]
    fn snapshot_scan_names_follow_field_id_across_rename() {
        use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
        let snapshot = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![
                NestedField::optional(1, "name", Type::Primitive(PrimitiveType::String)).into(),
                NestedField::optional(3, "category", Type::Primitive(PrimitiveType::String)).into(),
            ])
            .build()
            .unwrap();
        let current = Schema::builder()
            .with_schema_id(1)
            .with_fields(vec![
                NestedField::optional(1, "name", Type::Primitive(PrimitiveType::String)).into(),
                NestedField::optional(3, "category_code", Type::Primitive(PrimitiveType::String))
                    .into(),
            ])
            .build()
            .unwrap();
        assert_eq!(
            super::snapshot_scan_names(
                &current,
                &snapshot,
                &["category_code".into(), "name".into()]
            ),
            vec!["category".to_string(), "name".to_string()]
        );
    }

    #[test]
    fn exclude_set_is_slice_refs_on_ancestry() {
        let src = include_str!("iceberg_table.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(prod.contains("excluded_slices"));
        assert!(prod.contains("SNAPSHOT_WAL_SLICES"));
        assert!(!prod.contains("skippr.wal-segment-ids"));
        assert!(!prod.contains("compacted_wal_segment_ids"));
        assert!(!prod.contains("segment_ids_from_snapshot_properties"));
    }
}
