//! CDC collapse of uncompacted WAL onto a pinned Iceberg snapshot.
//!
//! Query calls `from_wal` then `collapse`. A WAL row hides a lake row only when
//! it beats the stored incumbent. The incumbent is `winner_of_set` of the row
//! slot and the tombstone slot. Append WAL rows stay outside `from_wal`.

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryArray, Int32Array, Int64Array, RecordBatch, StringArray, UInt32Array,
    UInt64Array, UInt8Array,
};
use arrow::compute::take;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};
use futures::{StreamExt, TryStreamExt};

use crate::plugins::cdc::{
    beats, collapse, encode_business_key, stored_incumbent, MutationKind, StoredWinner, Tie,
};
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
    columns: Vec<String>,
}

impl KeyEncoder {
    fn new(schema: &Schema, columns: &[String]) -> Result<Self> {
        for c in columns {
            schema.field_with_name(c).map_err(|_| {
                DataFusionError::Plan(format!("CDC business key column '{c}' is missing"))
            })?;
        }
        Ok(Self {
            columns: columns.to_vec(),
        })
    }

    fn encode(&self, batch: &RecordBatch, nulls: NullKey) -> Result<Vec<Option<Vec<u8>>>> {
        let cols = self
            .columns
            .iter()
            .map(|c| {
                batch.column_by_name(c).cloned().ok_or_else(|| {
                    DataFusionError::Plan(format!("CDC business key column '{c}' is missing"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        (0..batch.num_rows())
            .map(|i| match (cols.iter().any(|c| c.is_null(i)), &nulls) {
                (true, NullKey::Reject) => Err(DataFusionError::Execution(
                    "CDC WAL row has a null business key".into(),
                )),
                (true, NullKey::PassThrough) => Ok(None),
                (false, _) => {
                    let mut parts = Vec::with_capacity(self.columns.len());
                    let mut owned = Vec::with_capacity(self.columns.len());
                    for (name, col) in self.columns.iter().zip(cols.iter()) {
                        let value = arrow::util::display::array_value_to_string(col.as_ref(), i)
                            .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))?;
                        owned.push((name.as_bytes().to_vec(), value.into_bytes()));
                    }
                    for (name, value) in &owned {
                        parts.push((name.as_slice(), value.as_slice()));
                    }
                    Ok(Some(encode_business_key(&parts)))
                }
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

/// One stored incumbent: the slot that `beats` the other, or the row when equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LakeIncumbent {
    pub winner: StoredWinner,
    pub tombstone: bool,
}

/// Visible rows after `from_wal` then `collapse`, plus the resulting incumbents.
#[derive(Debug)]
pub struct ReconcileResult {
    pub batches: Vec<RecordBatch>,
    pub incumbents: BTreeMap<Vec<u8>, LakeIncumbent>,
}

/// Row slot vs tombstone slot. A strictly stronger tombstone hides the row.
pub fn incumbents_from_slots(
    row: &BTreeMap<Vec<u8>, StoredWinner>,
    tombstone: &BTreeMap<Vec<u8>, StoredWinner>,
) -> BTreeMap<Vec<u8>, LakeIncumbent> {
    let mut keys: Vec<Vec<u8>> = row.keys().cloned().collect();
    for key in tombstone.keys() {
        if !row.contains_key(key) {
            keys.push(key.clone());
        }
    }
    let mut out = BTreeMap::new();
    for key in keys {
        if let Some((winner, is_tomb)) = stored_incumbent(row.get(&key), tombstone.get(&key)) {
            out.insert(
                key,
                LakeIncumbent {
                    winner,
                    tombstone: is_tomb,
                },
            );
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Payload {
    Lake { batch: usize, row: usize },
    Wal { batch: usize, row: usize },
}

#[derive(Clone)]
struct Fold {
    winner: Option<StoredWinner>,
    tombstone: bool,
    payload: Option<Payload>,
}

/// Collapse WAL onto lake with `from_wal` then `collapse`. Appends stay appends.
pub fn reconcile_cdc(
    lake: &[RecordBatch],
    wal: &[RecordBatch],
    incumbents: &BTreeMap<Vec<u8>, LakeIncumbent>,
    business_key_columns: &[String],
) -> Result<ReconcileResult> {
    if business_key_columns.is_empty() {
        return Err(DataFusionError::Plan(
            "reconcile_cdc requires business_key_columns".into(),
        ));
    }
    if lake.is_empty() && wal.is_empty() {
        return Ok(ReconcileResult {
            batches: Vec::new(),
            incumbents: incumbents.clone(),
        });
    }
    let user_schema = user_schema_of(lake, wal)?;
    let mut fold: BTreeMap<Vec<u8>, Fold> = BTreeMap::new();
    for (key, inc) in incumbents {
        fold.insert(
            key.clone(),
            Fold {
                winner: Some(inc.winner.clone()),
                tombstone: inc.tombstone,
                payload: None,
            },
        );
    }
    let mut null_lake: Vec<(usize, usize)> = Vec::new();
    if !lake.is_empty() {
        let enc = KeyEncoder::new(lake[0].schema().as_ref(), business_key_columns)?;
        for (batch_idx, batch) in lake.iter().enumerate() {
            let keys = enc.encode(batch, NullKey::PassThrough)?;
            for row in 0..batch.num_rows() {
                match &keys[row] {
                    None => null_lake.push((batch_idx, row)),
                    Some(key) => {
                        let payload = Payload::Lake {
                            batch: batch_idx,
                            row,
                        };
                        match fold.get_mut(key) {
                            Some(state) if state.tombstone => {}
                            Some(state) => state.payload = Some(payload),
                            None => {
                                fold.insert(
                                    key.clone(),
                                    Fold {
                                        winner: None,
                                        tombstone: false,
                                        payload: Some(payload),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    let mut append_rows: Vec<(usize, usize)> = Vec::new();
    if !wal.is_empty() {
        let enc = KeyEncoder::new(wal[0].schema().as_ref(), business_key_columns)?;
        for (batch_idx, batch) in wal.iter().enumerate() {
            let mutation = uint8_col(batch, CDC_MUTATION_COL)?;
            let order = binary_col(batch, CDC_ORDER_COL)?;
            let seq = uint64_col(batch, CDC_SEQ_COL)?;
            let ord = uint32_col(batch, CDC_ORD_COL)?;
            let row_idx = uint64_col(batch, CDC_ROW_COL)?;
            let keys = enc.encode(batch, NullKey::Reject)?;
            for row in 0..batch.num_rows() {
                match decode_mutation(mutation.value(row))? {
                    None => append_rows.push((batch_idx, row)),
                    Some(kind) => {
                        let Some(key) = keys[row].clone() else {
                            return Err(DataFusionError::Execution(
                                "CDC WAL row has a null business key".into(),
                            ));
                        };
                        let order = order.value(row);
                        if order.is_empty() {
                            return Err(DataFusionError::Execution(
                                "CDC WAL row has an empty order token".into(),
                            ));
                        }
                        let incoming = StoredWinner {
                            order: order.to_vec(),
                            tie: Tie {
                                seq: seq.value(row),
                                ordinal: ord.value(row),
                                row: row_idx.value(row),
                            },
                        };
                        let prior = fold.get(&key).cloned();
                        let collapsed = collapse(
                            &incoming,
                            kind == MutationKind::Delete,
                            prior.as_ref().and_then(|s| s.winner.as_ref()),
                            prior.as_ref().map(|s| s.tombstone).unwrap_or(false),
                            key.clone(),
                        );
                        fold.insert(
                            key,
                            match collapsed {
                                skippr_cdc::Collapse::Upsert(winner) => Fold {
                                    payload: if prior.as_ref().and_then(|s| s.winner.as_ref())
                                        == Some(&winner)
                                    {
                                        prior.and_then(|s| s.payload)
                                    } else {
                                        Some(Payload::Wal {
                                            batch: batch_idx,
                                            row,
                                        })
                                    },
                                    winner: Some(winner),
                                    tombstone: false,
                                },
                                skippr_cdc::Collapse::Tombstone { winner, .. } => Fold {
                                    winner: Some(winner),
                                    tombstone: true,
                                    payload: None,
                                },
                            },
                        );
                    }
                }
            }
        }
    }
    let mut lake_keep = null_lake;
    let mut wal_keep = append_rows;
    for state in fold.values() {
        match state.payload {
            Some(Payload::Lake { batch, row }) => lake_keep.push((batch, row)),
            Some(Payload::Wal { batch, row }) => wal_keep.push((batch, row)),
            None => {}
        }
    }
    lake_keep.sort_unstable();
    wal_keep.sort_unstable();
    let mut batches = Vec::new();
    for batch in take_rows(lake, &lake_keep)? {
        let projected = project_batch(batch, &user_schema)?;
        if projected.num_rows() > 0 || user_schema.fields().is_empty() {
            batches.push(projected);
        }
    }
    for batch in take_rows(wal, &wal_keep)? {
        let projected = project_batch(batch, &user_schema)?;
        if projected.num_rows() > 0 || user_schema.fields().is_empty() {
            batches.push(projected);
        }
    }
    let incumbents = fold
        .into_iter()
        .filter_map(|(key, state)| {
            state.winner.map(|winner| {
                (
                    key,
                    LakeIncumbent {
                        winner,
                        tombstone: state.tombstone,
                    },
                )
            })
        })
        .collect();
    Ok(ReconcileResult {
        batches,
        incumbents,
    })
}

fn user_schema_of(lake: &[RecordBatch], wal: &[RecordBatch]) -> Result<SchemaRef> {
    if let Some(batch) = lake.first() {
        return Ok(batch.schema());
    }
    let Some(batch) = wal.first() else {
        return Err(DataFusionError::Plan(
            "reconcile_cdc needs a lake or WAL batch".into(),
        ));
    };
    let fields: Vec<Field> = batch
        .schema()
        .fields()
        .iter()
        .filter(|f| !is_cdc_sidecar_field(f.name()))
        .map(|f| f.as_ref().clone())
        .collect();
    Ok(Arc::new(Schema::new(fields)))
}

/// Stored Iceberg row-slot winners. Hex `_skippr_order_token` plus tie columns.
/// A lake row without those columns has no stored winner.
pub fn lake_winners_from_batches(
    batches: &[RecordBatch],
    business_key_columns: &[String],
) -> Result<BTreeMap<Vec<u8>, StoredWinner>> {
    if batches.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut encoder: Option<KeyEncoder> = None;
    let mut winners = BTreeMap::new();
    for batch in batches {
        let Some(token_col) = batch.column_by_name("_skippr_order_token") else {
            continue;
        };
        let token = token_col
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Execution("_skippr_order_token must be Utf8".into()))?;
        let seq = required_i64_col(batch, "_skippr_tie_seq")?;
        let ord = required_i32_col(batch, "_skippr_tie_ordinal")?;
        let row_idx = required_i64_col(batch, "_skippr_tie_row")?;
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
        for row in 0..batch.num_rows() {
            let Some(key) = keys[row].clone() else {
                continue;
            };
            if token.is_null(row) {
                continue;
            }
            let hex_token = token.value(row);
            if hex_token.is_empty()
                || hex_token.len() % 2 != 0
                || !hex_token.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(DataFusionError::Execution(format!(
                    "CDC order token '{hex_token}' is not fixed-width hex"
                )));
            }
            let order = hex::decode(hex_token).map_err(|err| {
                DataFusionError::Execution(format!("CDC order token '{hex_token}': {err}"))
            })?;
            let incoming = StoredWinner {
                order,
                tie: Tie {
                    seq: seq.value(row) as u64,
                    ordinal: ord.value(row) as u32,
                    row: row_idx.value(row) as u64,
                },
            };
            let replace = winners
                .get(&key)
                .map(|seen| beats(&incoming, seen))
                .unwrap_or(true);
            if replace {
                winners.insert(key, incoming);
            }
        }
    }
    Ok(winners)
}

fn lake_key_set(
    batches: &[RecordBatch],
    business_key_columns: &[String],
) -> Result<std::collections::BTreeSet<Vec<u8>>> {
    let mut keys = std::collections::BTreeSet::new();
    if batches.is_empty() {
        return Ok(keys);
    }
    let enc = KeyEncoder::new(batches[0].schema().as_ref(), business_key_columns)?;
    for batch in batches {
        for key in enc.encode(batch, NullKey::PassThrough)? {
            if let Some(key) = key {
                keys.insert(key);
            }
        }
    }
    Ok(keys)
}

/// Side-file winners plus lake token columns. A stored key with no lake row
/// is a tombstone. A stored key that still has a lake row is the row slot.
pub fn incumbents_from_stored(
    lake: &[RecordBatch],
    stored: &BTreeMap<Vec<u8>, StoredWinner>,
    business_key_columns: &[String],
) -> Result<BTreeMap<Vec<u8>, LakeIncumbent>> {
    let mut row = lake_winners_from_batches(lake, business_key_columns)?;
    let live = lake_key_set(lake, business_key_columns)?;
    let mut tomb = BTreeMap::new();
    for (key, winner) in stored {
        if live.contains(key) {
            row.entry(key.clone()).or_insert_with(|| winner.clone());
        } else {
            tomb.insert(key.clone(), winner.clone());
        }
    }
    Ok(incumbents_from_slots(&row, &tomb))
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

fn required_i64_col<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a Int64Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
        .ok_or_else(|| DataFusionError::Execution(format!("CDC lake row is missing {name}")))
}

fn required_i32_col<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a Int32Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<Int32Array>())
        .ok_or_else(|| DataFusionError::Execution(format!("CDC lake row is missing {name}")))
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

/// Serving-node merge: sidecar is the Arrow spelling of `from_wal`; this node
/// calls `collapse` against the stored incumbent (lake row ∪ side-file tomb).
#[derive(Debug)]
pub struct CdcReconcileExec {
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
    iceberg: Arc<dyn ExecutionPlan>,
    wal: Arc<dyn ExecutionPlan>,
    business_key_columns: Vec<String>,
    stored: BTreeMap<Vec<u8>, StoredWinner>,
}

impl CdcReconcileExec {
    pub fn try_new(
        iceberg: Arc<dyn ExecutionPlan>,
        wal: Arc<dyn ExecutionPlan>,
        business_key_columns: Vec<String>,
        schema: SchemaRef,
        stored: BTreeMap<Vec<u8>, StoredWinner>,
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
            stored,
        })
    }

    pub fn encode_bytes(&self) -> Result<Vec<u8>> {
        let node = CdcReconcileNode {
            arrow_schema_ipc: skippr_query_plan::schema_to_ipc(self.schema.as_ref())?,
            business_key_columns: self.business_key_columns.clone(),
            stored: self
                .stored
                .iter()
                .map(|(key, winner)| {
                    (
                        key.clone(),
                        winner.order.clone(),
                        winner.tie.seq,
                        winner.tie.ordinal,
                        winner.tie.row,
                    )
                })
                .collect(),
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
        let stored = node
            .stored
            .into_iter()
            .map(|(key, order, seq, ordinal, row)| {
                (
                    key,
                    StoredWinner {
                        order,
                        tie: Tie { seq, ordinal, row },
                    },
                )
            })
            .collect();
        Ok(Arc::new(Self::try_new(
            children[0].clone(),
            children[1].clone(),
            node.business_key_columns,
            schema,
            stored,
        )?))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CdcReconcileNode {
    arrow_schema_ipc: Vec<u8>,
    business_key_columns: Vec<String>,
    stored: Vec<(Vec<u8>, Vec<u8>, u64, u32, u64)>,
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
            self.stored.clone(),
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
        let stored = self.stored.clone();
        let schema = self.schema.clone();
        let fut = async move {
            let consumer =
                datafusion::execution::memory_pool::MemoryConsumer::new("CdcReconcileExec");
            let reservation = consumer.register(&pool);
            let wal_batches = wal.try_collect::<Vec<_>>().await?;
            for batch in &wal_batches {
                reservation.try_grow(batch.get_array_memory_size())?;
            }
            let lake_batches = iceberg.try_collect::<Vec<_>>().await?;
            for batch in &lake_batches {
                reservation.try_grow(batch.get_array_memory_size())?;
            }
            let incumbents = incumbents_from_stored(&lake_batches, &stored, &keys)?;
            let mut out = reconcile_cdc(&lake_batches, &wal_batches, &incumbents, &keys)?.batches;
            if schema.fields().is_empty() && out.is_empty() {
                out.push(RecordBatch::new_empty(schema.clone()));
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

    fn stored(order: u8, seq: u64, ordinal: u32, row: u64) -> StoredWinner {
        StoredWinner {
            order: vec![order],
            tie: Tie { seq, ordinal, row },
        }
    }

    fn id_key(id: i64) -> Vec<u8> {
        encode_business_key(&[(b"id", id.to_string().as_bytes())])
    }

    fn n_rows(batches: &[RecordBatch]) -> usize {
        batches.iter().map(|b| b.num_rows()).sum()
    }

    fn names(batches: &[RecordBatch]) -> Vec<String> {
        let mut out = Vec::new();
        for batch in batches {
            let col = batch
                .column_by_name("name")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for i in 0..col.len() {
                out.push(col.value(i).to_string());
            }
        }
        out
    }

    fn merge(
        lake: &[RecordBatch],
        wal: &[RecordBatch],
        incumbents: &BTreeMap<Vec<u8>, LakeIncumbent>,
    ) -> ReconcileResult {
        reconcile_cdc(lake, wal, incumbents, &["id".into()]).unwrap()
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
            CdcReconcileExec::try_new(
                iceberg,
                wal_exec,
                vec!["id".into()],
                user_schema(),
                BTreeMap::new(),
            )
            .unwrap(),
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
        let out = merge(&lake, &wal, &BTreeMap::new());
        assert_eq!(n_rows(&out.batches), 1);
        let amount = out.batches[0]
            .column_by_name("amount")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(amount.value(0), 100);
    }

    #[test]
    fn snapshot_wal_fills_empty_lake() {
        let wal = vec![
            cdc_row(1, "alpha", 100, MutationKind::Snapshot, 1, 1, 0, 0),
            cdc_row(2, "beta", 50, MutationKind::Snapshot, 1, 1, 1, 0),
        ];
        let out = merge(&[], &wal, &BTreeMap::new());
        assert_eq!(n_rows(&out.batches), 2);
    }

    #[test]
    fn insert_only_wal_unions_with_lake() {
        let lake = vec![row(1, "alpha", 10)];
        let wal = vec![cdc_row(2, "beta", 20, MutationKind::Insert, 1, 1, 0, 0)];
        let out = merge(&lake, &wal, &BTreeMap::new());
        assert_eq!(n_rows(&out.batches), 2);
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
        let out = merge(&lake, &wal, &BTreeMap::new());
        assert_eq!(n_rows(&out.batches), 1);
        assert_eq!(names(&out.batches), vec!["alpha-prime".to_string()]);
    }

    #[test]
    fn delete_removes_key() {
        let lake = vec![row(1, "alpha", 10), row(2, "beta", 20)];
        let wal = vec![cdc_row(2, "beta", 20, MutationKind::Delete, 1, 1, 0, 0)];
        let out = merge(&lake, &wal, &BTreeMap::new());
        assert_eq!(n_rows(&out.batches), 1);
        assert_eq!(names(&out.batches), vec!["alpha".to_string()]);
    }

    #[test]
    fn many_updates_keep_max_order_token() {
        let wal = vec![
            cdc_row(1, "a", 1, MutationKind::Update, 1, 1, 0, 0),
            cdc_row(1, "b", 2, MutationKind::Update, 3, 1, 1, 0),
            cdc_row(1, "c", 3, MutationKind::Update, 2, 1, 2, 0),
        ];
        let out = merge(&[], &wal, &BTreeMap::new());
        assert_eq!(names(&out.batches), vec!["b".to_string()]);
    }

    #[test]
    fn update_then_delete_absent() {
        let wal = vec![
            cdc_row(1, "a", 1, MutationKind::Update, 1, 1, 0, 0),
            cdc_row(1, "a", 1, MutationKind::Delete, 2, 1, 1, 0),
        ];
        let out = merge(&[], &wal, &BTreeMap::new());
        assert_eq!(n_rows(&out.batches), 0);
        assert_eq!(out.incumbents.len(), 1);
        assert!(out.incumbents.values().next().unwrap().tombstone);
    }

    #[test]
    fn delete_then_reinsert() {
        let wal = vec![
            cdc_row(1, "old", 1, MutationKind::Delete, 1, 1, 0, 0),
            cdc_row(1, "new", 9, MutationKind::Insert, 2, 1, 1, 0),
        ];
        let out = merge(&[], &wal, &BTreeMap::new());
        assert_eq!(names(&out.batches), vec!["new".to_string()]);
    }

    #[test]
    fn wal_does_not_hide_a_stronger_lake_token() {
        let lake = vec![row(1, "alpha", 100)];
        let wal = vec![cdc_row(1, "stale", 1, MutationKind::Update, 2, 1, 0, 0)];
        let mut row_slot = BTreeMap::new();
        row_slot.insert(id_key(1), stored(10, 1, 0, 0));
        let out = merge(
            &lake,
            &wal,
            &incumbents_from_slots(&row_slot, &BTreeMap::new()),
        );
        assert_eq!(n_rows(&out.batches), 1);
        assert_eq!(names(&out.batches), vec!["alpha".to_string()]);
    }

    #[tokio::test]
    async fn exec_side_file_tombstone_hides_older_wal_upsert() {
        let iceberg = Arc::new(StreamingBatchesExec::from_batches(user_schema(), vec![]));
        let wal = vec![cdc_row(1, "stale", 1, MutationKind::Insert, 2, 2, 0, 0)];
        let wal_exec = Arc::new(StreamingBatchesExec::from_batches(wal[0].schema(), wal));
        let mut side = BTreeMap::new();
        side.insert(id_key(1), stored(10, 1, 0, 0));
        let merge = Arc::new(
            CdcReconcileExec::try_new(iceberg, wal_exec, vec!["id".into()], user_schema(), side)
                .unwrap(),
        );
        let ctx = SessionContext::new();
        let batches =
            datafusion::physical_plan::common::collect(merge.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap();
        assert_eq!(n_rows(&batches), 0);
    }

    #[test]
    fn tombstone_incumbent_blocks_older_wal_upsert() {
        let wal = vec![cdc_row(1, "stale", 1, MutationKind::Insert, 2, 2, 0, 0)];
        let mut tomb = BTreeMap::new();
        tomb.insert(id_key(1), stored(10, 1, 0, 0));
        let out = merge(&[], &wal, &incumbents_from_slots(&BTreeMap::new(), &tomb));
        assert_eq!(n_rows(&out.batches), 0);
        assert!(out.incumbents[&id_key(1)].tombstone);
        assert_eq!(out.incumbents[&id_key(1)].winner.order, vec![10]);
    }

    #[test]
    fn row_10_tombstone_4_delete_7_keeps_lake_row() {
        let lake = vec![row(1, "alpha", 100)];
        let wal = vec![cdc_row(1, "alpha", 100, MutationKind::Delete, 7, 2, 0, 0)];
        let mut row_slot = BTreeMap::new();
        row_slot.insert(id_key(1), stored(10, 1, 0, 0));
        let mut tomb = BTreeMap::new();
        tomb.insert(id_key(1), stored(4, 1, 0, 0));
        let out = merge(&lake, &wal, &incumbents_from_slots(&row_slot, &tomb));
        assert_eq!(n_rows(&out.batches), 1);
        assert_eq!(names(&out.batches), vec!["alpha".to_string()]);
        let inc = &out.incumbents[&id_key(1)];
        assert!(!inc.tombstone);
        assert_eq!(inc.winner.order, vec![10]);
    }

    #[test]
    fn append_survives_a_same_key_delete() {
        let wal = vec![
            cdc_row(1, "gone", 1, MutationKind::Delete, 2, 1, 0, 0),
            attach_append_sidecar(row(1, "extra", 9), 1, 1, 0).unwrap(),
        ];
        let out = merge(&[], &wal, &BTreeMap::new());
        assert_eq!(n_rows(&out.batches), 1);
        assert_eq!(names(&out.batches), vec!["extra".to_string()]);
    }

    #[test]
    fn equal_tokens_tie_break_is_deterministic() {
        let wal = vec![
            cdc_row(1, "a", 1, MutationKind::Update, 5, 1, 0, 0),
            cdc_row(1, "b", 2, MutationKind::Update, 5, 2, 0, 0),
        ];
        let out = merge(&[], &wal, &BTreeMap::new());
        assert_eq!(names(&out.batches), vec!["b".to_string()]);
    }

    #[test]
    fn append_rows_do_not_suppress_lake() {
        let lake = vec![row(1, "alpha", 10)];
        let wal = vec![attach_append_sidecar(row(2, "extra", 1), 1, 0, 0).unwrap()];
        let out = merge(&lake, &wal, &BTreeMap::new());
        assert_eq!(n_rows(&out.batches), 2);
    }

    #[tokio::test]
    async fn sum_matches_dst_current_table() {
        use crate::sqlrt::dst::{current_table, run, Op, UserRow, WalEvent};
        let ops = vec![
            Op::IngestLake(UserRow {
                id: 1,
                name: "alpha".into(),
                amount: 100,
            }),
            Op::IngestLake(UserRow {
                id: 2,
                name: "beta".into(),
                amount: 50,
            }),
            Op::IngestWal(WalEvent {
                row: UserRow {
                    id: 1,
                    name: "alpha-prime".into(),
                    amount: 120,
                },
                mutation: Some(MutationKind::Update),
                order: 1,
                seq: 1,
                ordinal: 0,
                row_i: 0,
            }),
            Op::IngestWal(WalEvent {
                row: UserRow {
                    id: 3,
                    name: "gamma".into(),
                    amount: 30,
                },
                mutation: Some(MutationKind::Insert),
                order: 1,
                seq: 2,
                ordinal: 1,
                row_i: 0,
            }),
        ];
        let expect: i64 = current_table(&run(&ops))
            .values()
            .map(|(_, amount)| *amount)
            .sum();
        let lake = vec![row(1, "alpha", 100), row(2, "beta", 50)];
        let wal = vec![
            cdc_row(1, "alpha-prime", 120, MutationKind::Update, 1, 1, 0, 0),
            cdc_row(3, "gamma", 30, MutationKind::Insert, 1, 1, 1, 0),
        ];
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
        let out = reconcile_cdc(&[], &wal, &BTreeMap::new(), &["id".into()]).unwrap();
        assert_eq!(n_rows(&out.batches), 1);
        assert_eq!(names(&out.batches), vec!["new".to_string()]);
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
        let out = reconcile_cdc(&[], &wal, &BTreeMap::new(), &["a".into(), "b".into()]).unwrap();
        assert_eq!(n_rows(&out.batches), 2);
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
        let out = merge(&[lake], &wal, &BTreeMap::new());
        assert_eq!(n_rows(&out.batches), 2);
        assert!(names(&out.batches).contains(&"orphan".to_string()));
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
        let err = reconcile_cdc(&[], &[wal], &BTreeMap::new(), &["id".into()]).unwrap_err();
        assert!(err.to_string().contains("null business key"), "{err}");
    }

    #[test]
    fn unknown_mutation_code_is_error() {
        let mut wal = cdc_row(1, "a", 1, MutationKind::Insert, 1, 1, 0, 0);
        let mut cols = wal.columns().to_vec();
        cols[3] = Arc::new(UInt8Array::from(vec![99u8]));
        wal = RecordBatch::try_new(wal.schema(), cols).unwrap();
        let err = reconcile_cdc(&[], &[wal], &BTreeMap::new(), &["id".into()]).unwrap_err();
        assert!(err.to_string().contains("unknown CDC mutation"), "{err}");
    }

    #[test]
    fn reconcile_cdc_requires_keys() {
        let wal = vec![cdc_row(1, "a", 1, MutationKind::Insert, 1, 1, 0, 0)];
        let err = reconcile_cdc(&[], &wal, &BTreeMap::new(), &[]).unwrap_err();
        assert!(
            err.to_string().contains("requires business_key_columns"),
            "{err}"
        );
    }

    #[test]
    fn lake_malformed_order_token_is_error() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
            Field::new("amount", DataType::Int64, true),
            Field::new("_skippr_order_token", DataType::Utf8, true),
            Field::new("_skippr_tie_seq", DataType::Int64, true),
            Field::new("_skippr_tie_ordinal", DataType::Int32, true),
            Field::new("_skippr_tie_row", DataType::Int64, true),
        ]));
        let lake = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1])),
                Arc::new(StringArray::from(vec!["a"])),
                Arc::new(Int64Array::from(vec![1])),
                Arc::new(StringArray::from(vec!["zz"])),
                Arc::new(Int64Array::from(vec![1])),
                Arc::new(Int32Array::from(vec![0])),
                Arc::new(Int64Array::from(vec![0])),
            ],
        )
        .unwrap();
        let err = lake_winners_from_batches(&[lake], &["id".into()]).unwrap_err();
        assert!(err.to_string().contains("not fixed-width hex"), "{err}");
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
        assert!(!prod.contains("collapse_wal_batches"));
        assert!(!prod.contains("anti_join_lake"));
        assert!(!prod.contains("lake_keys_beaten"));
        assert!(!prod.contains("wal_keys_visible"));
        assert!(!prod.contains("drop_unseen_winners"));
        assert!(!prod.contains("zero_winner"));
        assert!(prod.contains("CDC_SEQ_COL"));
        assert!(prod.contains("_skippr_cdc_seq"));
        assert!(prod.contains("struct KeyEncoder"));
        assert!(prod.contains("fn decode_mutation"));
        assert!(prod.contains("NullKey::Reject"));
        assert!(prod.contains("Distribution::SinglePartition"));
        assert!(prod.contains("MemoryConsumer"));
        assert!(prod.contains("collapse("));
        assert!(prod.contains("stored_incumbent"));
        assert!(prod.contains("fn reconcile_cdc"));
        assert!(prod.contains("fn incumbents_from_slots"));
        assert!(prod.contains("fn incumbents_from_stored"));
        assert!(!prod.contains("CdcEvent::from_wal"));
        assert!(!prod.contains("collapse_event"));
    }
}
