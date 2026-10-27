//! CDC collapse of uncompacted WAL onto a pinned Iceberg snapshot.
//!
//! Latest `order_token` wins per business key. Equal tokens break ties by
//! `(seq, ordinal, row)`. Deletes suppress the key; insert/update/snapshot
//! after-images survive. Compacted leftovers are excluded by Iceberg `E` at
//! pin, not by in-merge snapshot dropping. Append WAL rows have no keys to
//! suppress. v1 has no before-image: a mutated business key is a new row
//! unless the source emitted delete(old)+insert(new).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryArray, RecordBatch, UInt32Array, UInt64Array, UInt8Array,
};
use arrow::compute::take;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::row::{OwnedRow, RowConverter, SortField};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};
use futures::{StreamExt, TryStreamExt};

use crate::plugins::cdc::MutationKind;
use crate::sqlrt::wal_table::{align_exec_to_schema, project_batch};

pub const CDC_MUTATION_COL: &str = "_skippr_cdc_mutation";
pub const CDC_ORDER_COL: &str = "_skippr_cdc_order";
pub const CDC_SEQ_COL: &str = "_skippr_cdc_seq";
pub const CDC_ORD_COL: &str = "_skippr_cdc_ord";
pub const CDC_ROW_COL: &str = "_skippr_cdc_row";

const MUT_NONE: u8 = 0;
const MUT_SNAPSHOT: u8 = 1;
const MUT_INSERT: u8 = 2;
const MUT_UPDATE: u8 = 3;
const MUT_DELETE: u8 = 4;

pub fn mutation_to_u8(kind: MutationKind) -> u8 {
    match kind {
        MutationKind::Snapshot => MUT_SNAPSHOT,
        MutationKind::Insert => MUT_INSERT,
        MutationKind::Update => MUT_UPDATE,
        MutationKind::Delete => MUT_DELETE,
    }
}

fn decode_mutation(code: u8) -> Result<Option<MutationKind>> {
    match code {
        MUT_NONE => Ok(None),
        MUT_SNAPSHOT => Ok(Some(MutationKind::Snapshot)),
        MUT_INSERT => Ok(Some(MutationKind::Insert)),
        MUT_UPDATE => Ok(Some(MutationKind::Update)),
        MUT_DELETE => Ok(Some(MutationKind::Delete)),
        other => Err(DataFusionError::Internal(format!(
            "unknown CDC mutation code {other}"
        ))),
    }
}

enum NullKey {
    Reject,
    PassThrough,
}

struct KeyEncoder {
    converter: RowConverter,
    columns: Vec<String>,
}

impl KeyEncoder {
    fn new(schema: &Schema, columns: &[String]) -> Result<Self> {
        let fields = columns
            .iter()
            .map(|c| {
                schema
                    .field_with_name(c)
                    .map(|f| SortField::new(f.data_type().clone()))
                    .map_err(|_| {
                        DataFusionError::Plan(format!("CDC business key column '{c}' is missing"))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            converter: RowConverter::new(fields)
                .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))?,
            columns: columns.to_vec(),
        })
    }

    fn encode(&self, batch: &RecordBatch, nulls: NullKey) -> Result<Vec<Option<OwnedRow>>> {
        let cols = self
            .columns
            .iter()
            .map(|c| {
                batch.column_by_name(c).cloned().ok_or_else(|| {
                    DataFusionError::Plan(format!("CDC business key column '{c}' is missing"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let rows = self
            .converter
            .convert_columns(&cols)
            .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))?;
        (0..batch.num_rows())
            .map(|i| match (cols.iter().any(|c| c.is_null(i)), &nulls) {
                (true, NullKey::Reject) => Err(DataFusionError::Execution(
                    "CDC WAL row has a null business key".into(),
                )),
                (true, NullKey::PassThrough) => Ok(None),
                (false, _) => Ok(Some(rows.row(i).owned())),
            })
            .collect()
    }
}

pub fn cdc_sidecar_fields() -> Vec<Field> {
    vec![
        Field::new(CDC_MUTATION_COL, DataType::UInt8, false),
        Field::new(CDC_ORDER_COL, DataType::Binary, false),
        Field::new(CDC_SEQ_COL, DataType::UInt64, false),
        Field::new(CDC_ORD_COL, DataType::UInt32, false),
        Field::new(CDC_ROW_COL, DataType::UInt64, false),
    ]
}

pub fn cdc_sidecar_schema(user: &SchemaRef) -> SchemaRef {
    let mut fields: Vec<Field> = user.fields().iter().map(|f| f.as_ref().clone()).collect();
    fields.extend(cdc_sidecar_fields());
    Arc::new(Schema::new(fields))
}

pub fn is_cdc_sidecar_field(name: &str) -> bool {
    matches!(
        name,
        CDC_MUTATION_COL | CDC_ORDER_COL | CDC_SEQ_COL | CDC_ORD_COL | CDC_ROW_COL
    )
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TieBreak {
    order_token: Vec<u8>,
    seq: u64,
    ordinal: u32,
    row: u64,
}

#[derive(Clone, Debug)]
struct Winner {
    tie: TieBreak,
    mutation: MutationKind,
    batch_idx: usize,
    row: usize,
}

#[derive(Debug)]
pub struct CollapseResult {
    pub changed_keys: HashSet<OwnedRow>,
    pub survivor_batches: Vec<RecordBatch>,
}

/// Collapse CDC WAL batches (user columns + sidecar) to latest-per-key survivors.
pub fn collapse_wal_batches(
    batches: &[RecordBatch],
    business_key_columns: &[String],
) -> Result<CollapseResult> {
    if business_key_columns.is_empty() {
        return Err(DataFusionError::Plan(
            "collapse_wal_batches requires business_key_columns".into(),
        ));
    }
    let mut winners: BTreeMap<OwnedRow, Winner> = BTreeMap::new();
    let mut append_rows: Vec<(usize, usize)> = Vec::new();
    let mut encoder: Option<KeyEncoder> = None;
    for (batch_idx, batch) in batches.iter().enumerate() {
        let mutation = uint8_col(batch, CDC_MUTATION_COL)?;
        let order = binary_col(batch, CDC_ORDER_COL)?;
        let seq = uint64_col(batch, CDC_SEQ_COL)?;
        let ord = uint32_col(batch, CDC_ORD_COL)?;
        let row_idx = uint64_col(batch, CDC_ROW_COL)?;
        let enc = match encoder.as_ref() {
            Some(enc) => enc,
            None => {
                encoder = Some(KeyEncoder::new(
                    batch.schema().as_ref(),
                    business_key_columns,
                )?);
                encoder.as_ref().expect("encoder just set")
            }
        };
        let keys = enc.encode(batch, NullKey::Reject)?;
        for row in 0..batch.num_rows() {
            match decode_mutation(mutation.value(row))? {
                None => {
                    append_rows.push((batch_idx, row));
                    continue;
                }
                Some(kind) => {
                    let Some(key) = keys[row].clone() else {
                        return Err(DataFusionError::Execution(
                            "CDC WAL row has a null business key".into(),
                        ));
                    };
                    let tie = TieBreak {
                        order_token: order.value(row).to_vec(),
                        seq: seq.value(row),
                        ordinal: ord.value(row),
                        row: row_idx.value(row),
                    };
                    let candidate = Winner {
                        tie,
                        mutation: kind,
                        batch_idx,
                        row,
                    };
                    match winners.get(&key) {
                        Some(existing) if existing.tie >= candidate.tie => {}
                        _ => {
                            winners.insert(key, candidate);
                        }
                    }
                }
            }
        }
    }
    let mut changed_keys = HashSet::new();
    let mut keep: Vec<(usize, usize)> = append_rows;
    for (key, winner) in &winners {
        match winner.mutation {
            MutationKind::Snapshot | MutationKind::Insert | MutationKind::Update => {
                changed_keys.insert(key.clone());
                keep.push((winner.batch_idx, winner.row));
            }
            MutationKind::Delete => {
                changed_keys.insert(key.clone());
            }
        }
    }
    keep.sort_unstable();
    Ok(CollapseResult {
        changed_keys,
        survivor_batches: take_rows(batches, &keep)?,
    })
}

pub fn anti_join_lake(
    lake: &[RecordBatch],
    changed_keys: &HashSet<OwnedRow>,
    business_key_columns: &[String],
) -> Result<Vec<RecordBatch>> {
    if business_key_columns.is_empty() {
        return Err(DataFusionError::Plan(
            "anti_join_lake requires business_key_columns".into(),
        ));
    }
    if changed_keys.is_empty() {
        return Ok(lake.to_vec());
    }
    let mut encoder: Option<KeyEncoder> = None;
    let mut out = Vec::new();
    for batch in lake {
        let enc = match encoder.as_ref() {
            Some(enc) => enc,
            None => {
                encoder = Some(KeyEncoder::new(
                    batch.schema().as_ref(),
                    business_key_columns,
                )?);
                encoder.as_ref().expect("encoder just set")
            }
        };
        let keys = enc.encode(batch, NullKey::PassThrough)?;
        let mut keep = Vec::new();
        for row in 0..batch.num_rows() {
            match &keys[row] {
                Some(key) if changed_keys.contains(key) => {}
                _ => keep.push(row),
            }
        }
        if keep.len() == batch.num_rows() {
            out.push(batch.clone());
        } else if !keep.is_empty() {
            out.extend(take_rows(
                std::slice::from_ref(batch),
                &keep.iter().map(|r| (0, *r)).collect::<Vec<_>>(),
            )?);
        }
    }
    Ok(out)
}

fn take_rows(batches: &[RecordBatch], keep: &[(usize, usize)]) -> Result<Vec<RecordBatch>> {
    if keep.is_empty() || batches.is_empty() {
        return Ok(Vec::new());
    }
    let schema = batches[0].schema();
    let mut grouped: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for (batch_idx, row) in keep {
        grouped.entry(*batch_idx).or_default().push(*row as u32);
    }
    let mut out = Vec::new();
    for (batch_idx, indices) in grouped {
        let batch = &batches[batch_idx];
        let idx = UInt32Array::from(indices);
        let cols: Result<Vec<ArrayRef>> = batch
            .columns()
            .iter()
            .map(|col| {
                take(col.as_ref(), &idx, None)
                    .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))
            })
            .collect();
        out.push(
            RecordBatch::try_new(schema.clone(), cols?)
                .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))?,
        );
    }
    Ok(out)
}

fn uint8_col<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt8Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<UInt8Array>())
        .ok_or_else(|| DataFusionError::Internal(format!("missing CDC column {name}")))
}

fn binary_col<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a BinaryArray> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<BinaryArray>())
        .ok_or_else(|| DataFusionError::Internal(format!("missing CDC column {name}")))
}

fn uint64_col<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt64Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .ok_or_else(|| DataFusionError::Internal(format!("missing CDC column {name}")))
}

fn uint32_col<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt32Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
        .ok_or_else(|| DataFusionError::Internal(format!("missing CDC column {name}")))
}

pub fn append_sidecar_none(seq: u64, ordinal: u32, row_base: u64, rows: usize) -> Vec<ArrayRef> {
    vec![
        Arc::new(UInt8Array::from(vec![MUT_NONE; rows])),
        Arc::new(BinaryArray::from_iter_values(std::iter::repeat_n(
            [].as_slice(),
            rows,
        ))),
        Arc::new(UInt64Array::from(vec![seq; rows])),
        Arc::new(UInt32Array::from(vec![ordinal; rows])),
        Arc::new(UInt64Array::from(
            (row_base..row_base + rows as u64).collect::<Vec<_>>(),
        )),
    ]
}

pub fn attach_cdc_sidecar(
    batch: RecordBatch,
    mutations: &[MutationKind],
    order_tokens: &[Vec<u8>],
    seq: u64,
    ordinal: u32,
    row_base: u64,
) -> Result<RecordBatch> {
    let rows = batch.num_rows();
    if mutations.len() != rows || order_tokens.len() != rows {
        return Err(DataFusionError::Internal(
            "CDC sidecar row count does not match Arrow batch".into(),
        ));
    }
    let mut cols: Vec<ArrayRef> = batch.columns().to_vec();
    cols.push(Arc::new(UInt8Array::from(
        mutations
            .iter()
            .copied()
            .map(mutation_to_u8)
            .collect::<Vec<_>>(),
    )));
    cols.push(Arc::new(BinaryArray::from_iter_values(
        order_tokens.iter().map(|t| t.as_slice()),
    )));
    cols.push(Arc::new(UInt64Array::from(vec![seq; rows])));
    cols.push(Arc::new(UInt32Array::from(vec![ordinal; rows])));
    cols.push(Arc::new(UInt64Array::from(
        (row_base..row_base + rows as u64).collect::<Vec<_>>(),
    )));
    let schema = cdc_sidecar_schema(&batch.schema());
    RecordBatch::try_new(schema, cols)
        .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))
}

pub fn attach_append_sidecar(
    batch: RecordBatch,
    seq: u64,
    ordinal: u32,
    row_base: u64,
) -> Result<RecordBatch> {
    let rows = batch.num_rows();
    let mut cols: Vec<ArrayRef> = batch.columns().to_vec();
    cols.extend(append_sidecar_none(seq, ordinal, row_base, rows));
    let schema = cdc_sidecar_schema(&batch.schema());
    RecordBatch::try_new(schema, cols)
        .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))
}

fn coalesce_single(plan: Arc<dyn ExecutionPlan>) -> Result<Arc<dyn ExecutionPlan>> {
    if plan.properties().output_partitioning().partition_count() <= 1 {
        Ok(plan)
    } else {
        Ok(Arc::new(
            datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec::new(plan),
        ))
    }
}

/// Serving-node merge: scan lake, collapse WAL, anti-join changed keys, union.
#[derive(Debug)]
pub struct CdcReconcileExec {
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
    iceberg: Arc<dyn ExecutionPlan>,
    wal: Arc<dyn ExecutionPlan>,
    business_key_columns: Vec<String>,
}

impl CdcReconcileExec {
    pub fn try_new(
        iceberg: Arc<dyn ExecutionPlan>,
        wal: Arc<dyn ExecutionPlan>,
        business_key_columns: Vec<String>,
        schema: SchemaRef,
    ) -> Result<Self> {
        if business_key_columns.is_empty() {
            return Err(DataFusionError::Plan(
                "CdcReconcileExec requires business_key_columns".into(),
            ));
        }
        if iceberg.schema().as_ref() != schema.as_ref() {
            return Err(DataFusionError::Plan(
                "CdcReconcileExec lake child schema must equal the user schema".into(),
            ));
        }
        let wal_schema = cdc_sidecar_schema(&schema);
        if wal.schema().as_ref() != wal_schema.as_ref() {
            return Err(DataFusionError::Plan(
                "CdcReconcileExec WAL child schema must equal cdc_sidecar_schema(user)".into(),
            ));
        }
        let iceberg = coalesce_single(iceberg)?;
        let wal = coalesce_single(wal)?;
        let properties = Arc::new(PlanProperties::new(
            datafusion::physical_expr::EquivalenceProperties::new(schema.clone()),
            datafusion::physical_plan::Partitioning::UnknownPartitioning(1),
            datafusion::physical_plan::execution_plan::EmissionType::Incremental,
            datafusion::physical_plan::execution_plan::Boundedness::Bounded,
        ));
        Ok(Self {
            schema,
            properties,
            iceberg,
            wal,
            business_key_columns,
        })
    }

    pub fn encode_bytes(&self) -> Result<Vec<u8>> {
        let node = CdcReconcileNode {
            arrow_schema_ipc: skippr_query_plan::schema_to_ipc(self.schema.as_ref())?,
            business_key_columns: self.business_key_columns.clone(),
        };
        bincode::serialize(&node)
            .map_err(|err| DataFusionError::Internal(format!("encode CdcReconcileExec: {err}")))
    }

    pub fn from_bytes(
        buf: &[u8],
        children: &[Arc<dyn ExecutionPlan>],
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if children.len() != 2 {
            return Err(DataFusionError::Internal(
                "CdcReconcileExec expects iceberg and WAL children".into(),
            ));
        }
        let node: CdcReconcileNode = bincode::deserialize(buf)
            .map_err(|err| DataFusionError::Internal(format!("decode CdcReconcileExec: {err}")))?;
        let schema = skippr_query_plan::schema_from_ipc(&node.arrow_schema_ipc)?;
        Ok(Arc::new(Self::try_new(
            children[0].clone(),
            children[1].clone(),
            node.business_key_columns,
            schema,
        )?))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CdcReconcileNode {
    arrow_schema_ipc: Vec<u8>,
    business_key_columns: Vec<String>,
}

impl DisplayAs for CdcReconcileExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "CdcReconcileExec(keys={})",
            self.business_key_columns.join(",")
        )
    }
}

impl ExecutionPlan for CdcReconcileExec {
    fn name(&self) -> &str {
        "CdcReconcileExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.iceberg, &self.wal]
    }

    fn required_input_distribution(&self) -> Vec<datafusion::physical_plan::Distribution> {
        vec![datafusion::physical_plan::Distribution::SinglePartition; 2]
    }

    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false; 2]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if children.len() != 2 {
            return Err(DataFusionError::Internal(
                "CdcReconcileExec expects two children".into(),
            ));
        }
        Ok(Arc::new(Self::try_new(
            children[0].clone(),
            children[1].clone(),
            self.business_key_columns.clone(),
            self.schema.clone(),
        )?))
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
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let started = std::time::Instant::now();
        let pool = Arc::clone(context.memory_pool());
        let wal = self.wal.execute(0, Arc::clone(&context))?;
        let iceberg = self.iceberg.execute(0, context)?;
        let keys = self.business_key_columns.clone();
        let schema = self.schema.clone();
        let fut = async move {
            let consumer =
                datafusion::execution::memory_pool::MemoryConsumer::new("CdcReconcileExec");
            let reservation = consumer.register(&pool);
            let wal_batches = wal.try_collect::<Vec<_>>().await?;
            for batch in &wal_batches {
                reservation.try_grow(batch.get_array_memory_size())?;
            }
            let collapsed = collapse_wal_batches(&wal_batches, &keys)?;
            let lake_batches = iceberg.try_collect::<Vec<_>>().await?;
            for batch in &lake_batches {
                reservation.try_grow(batch.get_array_memory_size())?;
            }
            let lake_kept = anti_join_lake(&lake_batches, &collapsed.changed_keys, &keys)?;
            let wal_kept = collapsed.survivor_batches;
            let mut out = Vec::new();
            for batch in lake_kept.into_iter().chain(wal_kept) {
                let projected = project_batch(batch, &schema)?;
                if projected.num_rows() > 0 || schema.fields().is_empty() {
                    out.push(projected);
                }
            }
            crate::helpers::perf::emit(
                "query_reconcile",
                serde_json::json!({
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                    "rows": out.iter().map(|b| b.num_rows()).sum::<usize>(),
                }),
            );
            Ok::<_, DataFusionError>(out)
        };
        let schema_out = self.schema.clone();
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            schema_out,
            futures::stream::once(fut)
                .map(|res| match res {
                    Ok(batches) => futures::stream::iter(batches.into_iter().map(Ok)).left_stream(),
                    Err(err) => futures::stream::once(async move { Err(err) }).right_stream(),
                })
                .flatten(),
        )))
    }
}

pub fn wrap_limit(
    plan: Arc<dyn ExecutionPlan>,
    schema: SchemaRef,
    limit: Option<usize>,
) -> Result<Arc<dyn ExecutionPlan>> {
    let plan = align_exec_to_schema(plan, schema)?;
    if let Some(limit) = limit {
        Ok(Arc::new(
            datafusion::physical_plan::limit::GlobalLimitExec::new(plan, 0, Some(limit)),
        ))
    } else {
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlrt::stream_plan::StreamingBatchesExec;
    use arrow::array::{Int64Array, StringArray};
    use datafusion::prelude::SessionContext;

    fn user_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
            Field::new("amount", DataType::Int64, true),
        ]))
    }

    fn row(id: i64, name: &str, amount: i64) -> RecordBatch {
        RecordBatch::try_new(
            user_schema(),
            vec![
                Arc::new(Int64Array::from(vec![id])),
                Arc::new(StringArray::from(vec![name])),
                Arc::new(Int64Array::from(vec![amount])),
            ],
        )
        .unwrap()
    }

    fn cdc_row(
        id: i64,
        name: &str,
        amount: i64,
        mutation: MutationKind,
        order: u8,
        seq: u64,
        ordinal: u32,
        row_i: u64,
    ) -> RecordBatch {
        attach_cdc_sidecar(
            row(id, name, amount),
            &[mutation],
            &[vec![order]],
            seq,
            ordinal,
            row_i,
        )
        .unwrap()
    }

    fn oracle(
        lake: &[(i64, &str, i64)],
        wal: &[(i64, &str, i64, MutationKind, u8)],
    ) -> BTreeMap<i64, (String, i64)> {
        let mut state: BTreeMap<i64, (String, i64, Vec<u8>)> = BTreeMap::new();
        for (id, name, amount) in lake {
            state.insert(*id, ((*name).to_string(), *amount, vec![]));
        }
        for (id, name, amount, mutation, order) in wal {
            let token = vec![*order];
            match state.get(id) {
                Some((_, _, existing))
                    if existing.as_slice() > token.as_slice() && !existing.is_empty() =>
                {
                    continue;
                }
                _ => {}
            }
            match mutation {
                MutationKind::Delete => {
                    state.remove(id);
                }
                _ => {
                    state.insert(*id, ((*name).to_string(), *amount, token));
                }
            }
        }
        state
            .into_iter()
            .map(|(id, (name, amount, _))| (id, (name, amount)))
            .collect()
    }

    async fn merge_sql(
        lake: Vec<RecordBatch>,
        wal: Vec<RecordBatch>,
        sql: &str,
    ) -> Vec<RecordBatch> {
        let iceberg = Arc::new(StreamingBatchesExec::from_batches(user_schema(), lake));
        let wal_schema = if wal.is_empty() {
            cdc_sidecar_schema(&user_schema())
        } else {
            wal[0].schema()
        };
        let wal_exec = Arc::new(StreamingBatchesExec::from_batches(wal_schema, wal));
        let merge = Arc::new(
            CdcReconcileExec::try_new(iceberg, wal_exec, vec!["id".into()], user_schema()).unwrap(),
        );
        let ctx = SessionContext::new();
        ctx.register_table(
            "t",
            Arc::new(
                datafusion::datasource::MemTable::try_new(user_schema(), vec![vec![]]).unwrap(),
            ),
        )
        .unwrap();
        // Execute merge directly, then run SQL on a memtable of the result.
        let batches =
            datafusion::physical_plan::common::collect(merge.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap();
        let table =
            datafusion::datasource::MemTable::try_new(user_schema(), vec![batches]).unwrap();
        let ctx = SessionContext::new();
        ctx.register_table("t", Arc::new(table)).unwrap();
        ctx.sql(sql).await.unwrap().collect().await.unwrap()
    }

    #[test]
    fn uncompacted_snapshot_replaces_lake() {
        let lake = vec![row(1, "alpha", 120)];
        let wal = vec![cdc_row(1, "alpha", 100, MutationKind::Snapshot, 1, 1, 0, 0)];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        let kept = anti_join_lake(&lake, &collapsed.changed_keys, &["id".into()]).unwrap();
        assert!(kept.iter().all(|b| b.num_rows() == 0) || kept.is_empty());
        let amount = collapsed.survivor_batches[0]
            .column_by_name("amount")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(amount.value(0), 100);
    }

    #[test]
    fn snapshot_wal_fills_empty_lake() {
        let lake: Vec<RecordBatch> = Vec::new();
        let wal = vec![
            cdc_row(1, "alpha", 100, MutationKind::Snapshot, 1, 1, 0, 0),
            cdc_row(2, "beta", 50, MutationKind::Snapshot, 1, 1, 1, 0),
        ];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        let kept = anti_join_lake(&lake, &collapsed.changed_keys, &["id".into()]).unwrap();
        assert_eq!(kept.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
        assert_eq!(
            collapsed
                .survivor_batches
                .iter()
                .map(|b| b.num_rows())
                .sum::<usize>(),
            2
        );
    }

    #[test]
    fn insert_only_wal_unions_with_lake() {
        let lake = vec![row(1, "alpha", 10)];
        let wal = vec![cdc_row(2, "beta", 20, MutationKind::Insert, 1, 1, 0, 0)];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        let kept = anti_join_lake(&lake, &collapsed.changed_keys, &["id".into()]).unwrap();
        assert_eq!(kept.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
        assert_eq!(
            collapsed
                .survivor_batches
                .iter()
                .map(|b| b.num_rows())
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn update_replaces_lake_row() {
        let lake = vec![row(1, "alpha", 100)];
        let wal = vec![cdc_row(
            1,
            "alpha-prime",
            120,
            MutationKind::Update,
            2,
            1,
            0,
            0,
        )];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        let kept = anti_join_lake(&lake, &collapsed.changed_keys, &["id".into()]).unwrap();
        assert!(kept.iter().all(|b| b.num_rows() == 0) || kept.is_empty());
        assert_eq!(
            collapsed
                .survivor_batches
                .iter()
                .map(|b| b.num_rows())
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn delete_removes_key() {
        let lake = vec![row(1, "alpha", 10), row(2, "beta", 20)];
        let wal = vec![cdc_row(2, "beta", 20, MutationKind::Delete, 1, 1, 0, 0)];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        let kept = anti_join_lake(&lake, &collapsed.changed_keys, &["id".into()]).unwrap();
        assert_eq!(kept.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
        assert!(
            collapsed.survivor_batches.iter().all(|b| b.num_rows() == 0)
                || collapsed.survivor_batches.is_empty()
        );
    }

    #[test]
    fn many_updates_keep_max_order_token() {
        let wal = vec![
            cdc_row(1, "a", 1, MutationKind::Update, 1, 1, 0, 0),
            cdc_row(1, "b", 2, MutationKind::Update, 3, 1, 1, 0),
            cdc_row(1, "c", 3, MutationKind::Update, 2, 1, 2, 0),
        ];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        let batch = &collapsed.survivor_batches[0];
        let name = batch
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name.value(0), "b");
    }

    #[test]
    fn update_then_delete_absent() {
        let wal = vec![
            cdc_row(1, "a", 1, MutationKind::Update, 1, 1, 0, 0),
            cdc_row(1, "a", 1, MutationKind::Delete, 2, 1, 1, 0),
        ];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        assert!(
            collapsed.survivor_batches.iter().all(|b| b.num_rows() == 0)
                || collapsed.survivor_batches.is_empty()
        );
        assert!(collapsed.changed_keys.len() == 1);
    }

    #[test]
    fn delete_then_reinsert() {
        let wal = vec![
            cdc_row(1, "old", 1, MutationKind::Delete, 1, 1, 0, 0),
            cdc_row(1, "new", 9, MutationKind::Insert, 2, 1, 1, 0),
        ];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        let name = collapsed.survivor_batches[0]
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name.value(0), "new");
    }

    #[test]
    fn equal_tokens_tie_break_is_deterministic() {
        let wal = vec![
            cdc_row(1, "a", 1, MutationKind::Update, 5, 1, 0, 0),
            cdc_row(1, "b", 2, MutationKind::Update, 5, 2, 0, 0),
        ];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        let name = collapsed.survivor_batches[0]
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name.value(0), "b");
    }

    #[test]
    fn append_rows_do_not_suppress_lake() {
        let lake = vec![row(1, "alpha", 10)];
        let wal = vec![attach_append_sidecar(row(2, "extra", 1), 1, 0, 0).unwrap()];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        assert!(collapsed.changed_keys.is_empty());
        let kept = anti_join_lake(&lake, &collapsed.changed_keys, &["id".into()]).unwrap();
        assert_eq!(kept.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
        assert_eq!(
            collapsed
                .survivor_batches
                .iter()
                .map(|b| b.num_rows())
                .sum::<usize>(),
            1
        );
    }

    #[tokio::test]
    async fn sum_matches_oracle() {
        let lake = vec![row(1, "alpha", 100), row(2, "beta", 50)];
        let wal = vec![
            cdc_row(1, "alpha-prime", 120, MutationKind::Update, 1, 1, 0, 0),
            cdc_row(3, "gamma", 30, MutationKind::Insert, 1, 1, 1, 0),
        ];
        let expect: i64 = oracle(
            &[(1, "alpha", 100), (2, "beta", 50)],
            &[
                (1, "alpha-prime", 120, MutationKind::Update, 1),
                (3, "gamma", 30, MutationKind::Insert, 1),
            ],
        )
        .values()
        .map(|(_, amount)| *amount)
        .sum();
        let batches = merge_sql(lake, wal, "SELECT SUM(amount) FROM t").await;
        let got = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0);
        assert_eq!(got, expect);
    }

    #[tokio::test]
    async fn projection_without_pk_still_reconciles() {
        let lake = vec![row(1, "alpha", 100)];
        let wal = vec![cdc_row(
            1,
            "alpha-prime",
            120,
            MutationKind::Update,
            1,
            1,
            0,
            0,
        )];
        let batches = merge_sql(lake, wal, "SELECT name FROM t").await;
        let name = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
        assert_eq!(name.value(0), "alpha-prime");
    }

    #[tokio::test]
    async fn after_image_only_predicate() {
        let lake = vec![row(1, "alpha", 100)];
        let wal = vec![cdc_row(
            1,
            "alpha-prime",
            120,
            MutationKind::Update,
            1,
            1,
            0,
            0,
        )];
        let batches = merge_sql(lake, wal, "SELECT name FROM t WHERE amount = 120").await;
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
        let batches = merge_sql(
            vec![row(1, "alpha", 100)],
            vec![cdc_row(
                1,
                "alpha-prime",
                120,
                MutationKind::Update,
                1,
                1,
                0,
                0,
            )],
            "SELECT name FROM t WHERE amount = 100",
        )
        .await;
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
    }

    fn uuid_batch(id: [u8; 16], name: &str) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::FixedSizeBinary(16), true),
                Field::new("name", DataType::Utf8, true),
            ])),
            vec![
                Arc::new(
                    arrow::array::FixedSizeBinaryArray::try_from_iter(std::iter::once(
                        id.as_slice(),
                    ))
                    .unwrap(),
                ),
                Arc::new(StringArray::from(vec![name])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn uuid_keys_collapse_to_one_winner() {
        let id = [7u8; 16];
        let wal = vec![
            attach_cdc_sidecar(
                uuid_batch(id, "old"),
                &[MutationKind::Update],
                &[vec![1]],
                1,
                0,
                0,
            )
            .unwrap(),
            attach_cdc_sidecar(
                uuid_batch(id, "new"),
                &[MutationKind::Update],
                &[vec![2]],
                2,
                1,
                0,
            )
            .unwrap(),
        ];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        assert_eq!(
            collapsed
                .survivor_batches
                .iter()
                .map(|b| b.num_rows())
                .sum::<usize>(),
            1
        );
        let name = collapsed.survivor_batches[0]
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name.value(0), "new");
    }

    #[test]
    fn composite_separator_keys_are_distinct() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Utf8, true),
            Field::new("b", DataType::Utf8, true),
            Field::new("name", DataType::Utf8, true),
        ]));
        let batch = |a: &str, b: &str, name: &str| {
            RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(StringArray::from(vec![a])),
                    Arc::new(StringArray::from(vec![b])),
                    Arc::new(StringArray::from(vec![name])),
                ],
            )
            .unwrap()
        };
        let wal = vec![
            attach_cdc_sidecar(
                batch("a\x1f", "b", "left"),
                &[MutationKind::Insert],
                &[vec![1]],
                1,
                0,
                0,
            )
            .unwrap(),
            attach_cdc_sidecar(
                batch("a", "\x1fb", "right"),
                &[MutationKind::Insert],
                &[vec![1]],
                1,
                1,
                0,
            )
            .unwrap(),
        ];
        let collapsed = collapse_wal_batches(&wal, &["a".into(), "b".into()]).unwrap();
        assert_eq!(
            collapsed
                .survivor_batches
                .iter()
                .map(|b| b.num_rows())
                .sum::<usize>(),
            2
        );
    }

    #[test]
    fn lake_null_business_key_is_kept() {
        let lake = RecordBatch::try_new(
            user_schema(),
            vec![
                Arc::new(Int64Array::from(vec![None])),
                Arc::new(StringArray::from(vec!["orphan"])),
                Arc::new(Int64Array::from(vec![1])),
            ],
        )
        .unwrap();
        let wal = vec![cdc_row(1, "a", 1, MutationKind::Insert, 1, 1, 0, 0)];
        let collapsed = collapse_wal_batches(&wal, &["id".into()]).unwrap();
        let kept = anti_join_lake(&[lake], &collapsed.changed_keys, &["id".into()]).unwrap();
        assert_eq!(kept.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
        let name = kept[0]
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name.value(0), "orphan");
    }

    #[test]
    fn wal_null_business_key_is_error() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![None])),
                Arc::new(StringArray::from(vec!["x"])),
            ],
        )
        .unwrap();
        let wal = attach_cdc_sidecar(batch, &[MutationKind::Insert], &[vec![1]], 1, 0, 0).unwrap();
        let err = collapse_wal_batches(&[wal], &["id".into()]).unwrap_err();
        assert!(err.to_string().contains("null business key"), "{err}");
    }

    #[test]
    fn unknown_mutation_code_is_error() {
        let mut wal = cdc_row(1, "a", 1, MutationKind::Insert, 1, 1, 0, 0);
        let mut cols = wal.columns().to_vec();
        cols[3] = Arc::new(UInt8Array::from(vec![99u8]));
        wal = RecordBatch::try_new(wal.schema(), cols).unwrap();
        let err = collapse_wal_batches(&[wal], &["id".into()]).unwrap_err();
        assert!(err.to_string().contains("unknown CDC mutation"), "{err}");
    }

    #[test]
    fn collapse_wal_batches_requires_keys() {
        let wal = vec![cdc_row(1, "a", 1, MutationKind::Insert, 1, 1, 0, 0)];
        let err = collapse_wal_batches(&wal, &[]).unwrap_err();
        assert!(
            err.to_string().contains("requires business_key_columns"),
            "{err}"
        );
        let lake = vec![row(1, "a", 1)];
        let err = anti_join_lake(&lake, &HashSet::new(), &[]).unwrap_err();
        assert!(
            err.to_string().contains("requires business_key_columns"),
            "{err}"
        );
    }

    #[test]
    fn cdc_merge_sot_deletes_legacy_key_and_seg_paths() {
        let src = include_str!("cdc_reconcile.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(!prod.contains("CDC_SEG_COL"));
        assert!(!prod.contains("_skippr_cdc_seg"));
        assert!(!prod.contains("fn row_business_key"));
        assert!(!prod.contains("fn scalar_key_bytes"));
        assert!(!prod.contains("drop_compacted_snapshot_leftover"));
        assert!(!prod.contains("snapshot_keys"));
        assert!(prod.contains("CDC_SEQ_COL"));
        assert!(prod.contains("_skippr_cdc_seq"));
        assert!(prod.contains("struct KeyEncoder"));
        assert!(prod.contains("fn decode_mutation"));
        assert!(prod.contains("NullKey::Reject"));
        assert!(prod.contains("Distribution::SinglePartition"));
        assert!(prod.contains("MemoryConsumer"));
    }
}
