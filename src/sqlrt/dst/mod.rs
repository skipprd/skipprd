//! Deterministic simulation World for Iceberg ∪ WAL CDC using production merge.
//!
//! The current table is `collapse_wal_batches` + `anti_join_lake`. There is no
//! second merge oracle.

#[cfg(test)]
mod alter;
#[cfg(test)]
mod append;
#[cfg(test)]
mod catalog;
#[cfg(test)]
mod cdc;
#[cfg(test)]
mod config;
#[cfg(test)]
mod copy_frames;
#[cfg(test)]
mod evolution;
#[cfg(test)]
mod failpoints;
#[cfg(test)]
mod inventory;
#[cfg(test)]
mod leases;
#[cfg(test)]
mod processor;
#[cfg(test)]
mod proptest_schedules;
#[cfg(test)]
mod query_pin;
#[cfg(test)]
mod regressions;
#[cfg(test)]
mod s3_list;
#[cfg(test)]
mod scripts;
#[cfg(test)]
mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use arrow::array::{Array, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use skippr_lease::{LeaseEpoch, NodeId, SegmentId};
use skippr_wal_catalog::{
    query_all, CatalogFence, CatalogKey, CatalogPrefix, CatalogRow, CatalogSeq, CatalogTxn,
    CompactionId, CompactionRow, CompactionState, MemoryWalCatalog, PayloadLocation, PutCond,
    SegmentRow, SeqRow, SliceBitmap, WalCatalogRead, WalCatalogStore,
};

use crate::failpoint::FailpointName;
use crate::plugins::cdc::MutationKind;
use crate::sqlrt::read_boundary::LakeSnapshot;

use super::cdc_reconcile::{
    anti_join_lake, attach_append_sidecar, attach_cdc_sidecar, collapse_wal_batches,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DstGroup {
    AppendIngest,
    CdcMerge,
    CatalogRecover,
    CopyFrames,
    Evolution,
    Alter,
    StreamTransform,
    QueryPin,
    Offsets,
    CloudCycleShape,
}

impl DstGroup {
    pub const ALL: [DstGroup; 10] = [
        DstGroup::AppendIngest,
        DstGroup::CdcMerge,
        DstGroup::CatalogRecover,
        DstGroup::CopyFrames,
        DstGroup::Evolution,
        DstGroup::Alter,
        DstGroup::StreamTransform,
        DstGroup::QueryPin,
        DstGroup::Offsets,
        DstGroup::CloudCycleShape,
    ];

    pub fn from_script_name(name: &str) -> Self {
        match name.split_once('_').map(|(prefix, _)| prefix) {
            Some("a") => DstGroup::AppendIngest,
            Some("b") => DstGroup::CdcMerge,
            Some("c") => DstGroup::CatalogRecover,
            Some("d") => DstGroup::CopyFrames,
            Some("e") => DstGroup::Evolution,
            Some("f") => DstGroup::Alter,
            Some("g") => DstGroup::StreamTransform,
            Some("h") => DstGroup::QueryPin,
            Some("i") => DstGroup::Offsets,
            Some("j") => DstGroup::CloudCycleShape,
            _ => panic!("DST script {name} must use a typed group prefix a_..j_"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRow {
    pub id: i64,
    pub name: String,
    pub amount: i64,
}

#[derive(Clone, Debug)]
pub struct WalEvent {
    pub row: UserRow,
    pub mutation: Option<MutationKind>,
    pub order: u8,
    pub seq: u64,
    pub ordinal: u32,
    pub row_i: u64,
}

#[derive(Clone, Debug)]
pub enum Op {
    IngestLake(UserRow),
    IngestWal(WalEvent),
    CompactThru(u64),
    Failpoint(FailpointName),
    Reopen,
    HoldPin,
    ReleasePin,
    ReleaseCompaction,
    DiskPause,
    DiskResume,
    DeadletterSkip,
    EvolveAddColumn(String),
}

const OPS_MAGIC: &[u8; 4] = b"DST1";
/// Untrusted `DST1` schedules must not `Vec::with_capacity` a mutated u32.
pub(crate) const MAX_SCHEDULE_OPS: usize = 1024;

fn schedule_seq_ok(seq: u64) -> bool {
    seq <= MAX_SCHEDULE_OPS as u64
}

/// Iceberg snapshot captured by compact / lake ingest. `S` in query pin `(S,E,W)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IcebergSnapshot {
    pub snapshot_id: i64,
    pub parent_snapshot_id: Option<i64>,
    pub compacted_thru: u64,
    pub rows: Vec<UserRow>,
}

/// Query-local pin: lake snapshot `S`, compacted prefix `E`, WAL `W`.
#[derive(Clone, Debug)]
pub struct QueryPin {
    pub lake: LakeSnapshot,
    pub compacted_thru: u64,
    pub wal: Vec<WalEvent>,
}

pub struct World {
    pub catalog: MemoryWalCatalog,
    pub fence: CatalogFence,
    pub payloads: BTreeMap<String, WalEvent>,
    pub snapshots: Vec<IcebergSnapshot>,
    pub compacted_thru: u64,
    pub catalog_committed: bool,
    pub offsets: u64,
    pub armed: Option<FailpointName>,
    pub held_pin: Option<QueryPin>,
    pub disk_paused: bool,
    pub deadletter_skip: bool,
    pub field_ids: BTreeMap<String, i32>,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    pub fn new() -> Self {
        let catalog = MemoryWalCatalog::new();
        let owner = NodeId::generate();
        let epoch = LeaseEpoch::new(1);
        catalog.set_writer_fence(owner, epoch);
        Self {
            catalog,
            fence: CatalogFence { owner, epoch },
            payloads: BTreeMap::new(),
            snapshots: Vec::new(),
            compacted_thru: 0,
            catalog_committed: true,
            offsets: 0,
            armed: None,
            held_pin: None,
            disk_paused: false,
            deadletter_skip: false,
            field_ids: BTreeMap::new(),
        }
    }
}

pub fn user_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("name", DataType::Utf8, true),
        Field::new("amount", DataType::Int64, true),
    ]))
}

pub fn batch_from_row(row: &UserRow) -> RecordBatch {
    RecordBatch::try_new(
        user_schema(),
        vec![
            Arc::new(Int64Array::from(vec![row.id])),
            Arc::new(StringArray::from(vec![row.name.as_str()])),
            Arc::new(Int64Array::from(vec![row.amount])),
        ],
    )
    .unwrap()
}

fn wal_batch(event: &WalEvent) -> RecordBatch {
    match event.mutation {
        None => attach_append_sidecar(
            batch_from_row(&event.row),
            event.seq,
            event.ordinal,
            event.row_i,
        )
        .unwrap(),
        Some(kind) => attach_cdc_sidecar(
            batch_from_row(&event.row),
            &[kind],
            &[vec![event.order]],
            event.seq,
            event.ordinal,
            event.row_i,
        )
        .unwrap(),
    }
}

pub fn apply(world: &mut World, op: Op) {
    match op {
        Op::IngestLake(row) => {
            let mut rows = lake_rows(world);
            rows.push(row);
            push_snapshot(world, rows);
        }
        Op::IngestWal(event) => {
            if world.deadletter_skip {
                world.deadletter_skip = false;
                return;
            }
            if world.armed == Some(FailpointName::BeforeCatalogCommit) || world.disk_paused {
                return;
            }
            if !commit_ingest(world, &event) {
                return;
            }
            if world.catalog_committed {
                world.offsets = world.payloads.len() as u64;
            }
        }
        Op::CompactThru(seq) => {
            if !schedule_seq_ok(seq) {
                return;
            }
            if matches!(
                world.armed,
                Some(FailpointName::HoldBeforeCompactionSink)
                    | Some(FailpointName::BeforeCompactionSink)
            ) {
                return;
            }
            let keys = vec!["id".to_string()];
            let events = catalog_events(world);
            let compacting: Vec<RecordBatch> = events
                .iter()
                .filter(|e| e.seq <= seq)
                .map(wal_batch)
                .collect();
            let lake_batches: Vec<RecordBatch> =
                lake_rows(world).iter().map(batch_from_row).collect();
            let mut next_lake = lake_rows(world);
            if !compacting.is_empty() {
                let collapsed = collapse_wal_batches(&compacting, &keys).unwrap();
                let kept = anti_join_lake(&lake_batches, &collapsed.changed_keys, &keys).unwrap();
                next_lake = rows_from_batches(&kept);
                next_lake.extend(rows_from_batches(&collapsed.survivor_batches));
            }
            world.compacted_thru = seq;
            push_snapshot(world, next_lake);
            let state = if world.armed == Some(FailpointName::AfterCompactionSink) {
                CompactionState::Sent
            } else {
                CompactionState::Acked
            };
            commit_compaction(world, seq, state);
        }
        Op::Failpoint(name) => apply_failpoint(world, name),
        Op::Reopen => {
            match world.armed {
                Some(FailpointName::HoldBeforeCompactionSink)
                | Some(FailpointName::BeforeCompactionSink) => {}
                _ => world.armed = None,
            }
            world.catalog_committed = true;
            world.offsets = world.payloads.len() as u64;
        }
        Op::HoldPin => {
            let lake = match world.snapshots.last() {
                Some(snap) => LakeSnapshot::At(snap.snapshot_id),
                None => LakeSnapshot::Empty,
            };
            world.held_pin = Some(QueryPin {
                lake,
                compacted_thru: world.compacted_thru,
                wal: live_wal(world),
            });
        }
        Op::ReleasePin => {
            world.held_pin = None;
        }
        Op::ReleaseCompaction => {
            if matches!(
                world.armed,
                Some(FailpointName::HoldBeforeCompactionSink)
                    | Some(FailpointName::BeforeCompactionSink)
            ) {
                world.armed = None;
            }
        }
        Op::DiskPause => world.disk_paused = true,
        Op::DiskResume => world.disk_paused = false,
        Op::DeadletterSkip => world.deadletter_skip = true,
        Op::EvolveAddColumn(name) => {
            let id = crate::lineage::deterministic_field_id("dst", std::slice::from_ref(&name));
            world.field_ids.entry(name).or_insert(id);
        }
    }
}

fn apply_failpoint(world: &mut World, name: FailpointName) {
    world.armed = Some(name);
    match name {
        FailpointName::BeforeCatalogCommit => world.catalog_committed = false,
        FailpointName::AfterCatalogCommit => {
            world.catalog_committed = true;
            world.offsets = 0;
        }
        FailpointName::AfterOffsetsPublished => {
            world.offsets = world.payloads.len() as u64;
        }
        FailpointName::BeforeCompactionSink
        | FailpointName::AfterCompactionSink
        | FailpointName::HoldBeforeCompactionSink => {}
    }
}

fn payload_sha(event: &WalEvent) -> [u8; 32] {
    let mut buf = Vec::new();
    buf.extend_from_slice(&event.row.id.to_le_bytes());
    buf.extend_from_slice(event.row.name.as_bytes());
    buf.extend_from_slice(&event.row.amount.to_le_bytes());
    buf.push(event.order);
    buf.push(match event.mutation {
        None => 0xff,
        Some(MutationKind::Snapshot) => 0,
        Some(MutationKind::Insert) => 1,
        Some(MutationKind::Update) => 2,
        Some(MutationKind::Delete) => 3,
    });
    CatalogRow::sha256(&buf)
}

fn segment_id_for(sha: &[u8; 32]) -> SegmentId {
    let hex: String = sha[..8].iter().map(|b| format!("{b:02x}")).collect();
    SegmentId::new(format!("p{hex}")).expect("payload segment id")
}

fn commit_ingest(world: &mut World, event: &WalEvent) -> bool {
    let sha = payload_sha(event);
    let segment_id = segment_id_for(&sha);
    let key = CatalogKey::Segment(segment_id.clone());
    let mut txn = CatalogTxn::new();
    match world.catalog.get(&key) {
        Ok(Some(CatalogRow::Segment(existing))) => {
            if txn
                .put(key, CatalogRow::Segment(existing), PutCond::SameSha)
                .is_err()
            {
                return false;
            }
        }
        Ok(None) | Ok(Some(_)) | Err(_) => {
            world
                .payloads
                .entry(segment_id.as_str().to_string())
                .or_insert_with(|| event.clone());
            let row = SegmentRow {
                segment_id,
                seq: CatalogSeq::new(event.seq),
                location: PayloadLocation::Disk {
                    path: format!("/dst/{}.seg", hex_prefix(&sha)),
                },
                payload_len: 1,
                payload_sha256: sha,
                slice_count: 1,
                done: SliceBitmap::new(1),
                offsets: vec![],
                checkpoints: vec![],
                epoch: world.fence.epoch,
                version: 0,
            };
            if txn
                .put(key, CatalogRow::Segment(row), PutCond::NotExists)
                .is_err()
            {
                return false;
            }
        }
    }
    let current_max = match world.catalog.get(&CatalogKey::Seq) {
        Ok(Some(CatalogRow::Seq(row))) => row.max_seq.get(),
        _ => 0,
    };
    if event.seq > current_max {
        if txn
            .put(
                CatalogKey::Seq,
                CatalogRow::Seq(SeqRow {
                    max_seq: CatalogSeq::new(event.seq),
                }),
                PutCond::SeqAdvance,
            )
            .is_err()
        {
            return false;
        }
    }
    world.catalog.commit(&txn, world.fence).is_ok()
}

fn hex_prefix(sha: &[u8; 32]) -> String {
    sha[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn lake_rows(world: &World) -> Vec<UserRow> {
    world
        .snapshots
        .last()
        .map(|snap| snap.rows.clone())
        .unwrap_or_default()
}

fn push_snapshot(world: &mut World, rows: Vec<UserRow>) {
    let snapshot_id = world.snapshots.len() as i64 + 1;
    let parent_snapshot_id = world.snapshots.last().map(|s| s.snapshot_id);
    world.snapshots.push(IcebergSnapshot {
        snapshot_id,
        parent_snapshot_id,
        compacted_thru: world.compacted_thru,
        rows,
    });
}

pub fn compaction_state(world: &World, seq: u64) -> Option<CompactionState> {
    let id = CompactionId::new(format!("c{seq}")).ok()?;
    match world.catalog.get(&CatalogKey::Compaction(id)).ok()? {
        Some(CatalogRow::Compaction(row)) => Some(row.state),
        _ => None,
    }
}

fn commit_compaction(world: &mut World, seq: u64, state: CompactionState) {
    let Ok(id) = CompactionId::new(format!("c{seq}")) else {
        return;
    };
    let mut txn = CatalogTxn::new();
    if txn
        .put(
            CatalogKey::Compaction(id.clone()),
            CatalogRow::Compaction(CompactionRow {
                id,
                state,
                body: vec![],
                version: 0,
            }),
            PutCond::NotExists,
        )
        .is_err()
    {
        return;
    }
    let _ = world.catalog.commit(&txn, world.fence);
}

fn catalog_events(world: &World) -> Vec<WalEvent> {
    let Ok(rows) = query_all(&world.catalog, CatalogPrefix::Segment) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for (_, row) in rows {
        if let CatalogRow::Segment(seg) = row {
            if let Some(event) = world.payloads.get(seg.segment_id.as_str()) {
                events.push(event.clone());
            }
        }
    }
    events.sort_by_key(|event| (event.seq, event.ordinal, event.row_i));
    events
}

pub fn live_wal(world: &World) -> Vec<WalEvent> {
    catalog_events(world)
        .into_iter()
        .filter(|e| e.seq > world.compacted_thru)
        .collect()
}

fn snapshot_rows(world: &World, lake: LakeSnapshot) -> Vec<UserRow> {
    match lake {
        LakeSnapshot::Empty => Vec::new(),
        LakeSnapshot::At(id) => world
            .snapshots
            .iter()
            .find(|snap| snap.snapshot_id == id)
            .map(|snap| snap.rows.clone())
            .unwrap_or_default(),
    }
}

fn merge_table(lake: &[UserRow], wal: &[WalEvent]) -> BTreeMap<i64, (String, i64)> {
    let keys = vec!["id".to_string()];
    let lake_batches: Vec<RecordBatch> = lake.iter().map(batch_from_row).collect();
    let wal_batches: Vec<RecordBatch> = wal.iter().map(wal_batch).collect();
    let collapsed = if wal_batches.is_empty() {
        None
    } else {
        Some(collapse_wal_batches(&wal_batches, &keys).unwrap())
    };
    let kept = match &collapsed {
        Some(c) => anti_join_lake(&lake_batches, &c.changed_keys, &keys).unwrap(),
        None => lake_batches,
    };
    let mut out = rows_from_batches(&kept);
    if let Some(c) = collapsed {
        out.extend(rows_from_batches(&c.survivor_batches));
    }
    let mut map = BTreeMap::new();
    for row in out {
        map.insert(row.id, (row.name, row.amount));
    }
    map
}

fn live_table(world: &World) -> BTreeMap<i64, (String, i64)> {
    merge_table(&lake_rows(world), &live_wal(world))
}

/// Production current table: collapse live WAL, anti-join lake.
/// A held query pin re-merges frozen `(S,E,W)` until `ReleasePin`.
pub fn current_table(world: &World) -> BTreeMap<i64, (String, i64)> {
    match &world.held_pin {
        Some(pin) => {
            let wal: Vec<WalEvent> = pin
                .wal
                .iter()
                .filter(|event| event.seq > pin.compacted_thru)
                .cloned()
                .collect();
            merge_table(&snapshot_rows(world, pin.lake.clone()), &wal)
        }
        None => live_table(world),
    }
}

pub fn count(world: &World) -> usize {
    current_table(world).len()
}

pub fn sum_amount(world: &World) -> i64 {
    current_table(world)
        .values()
        .map(|(_, amount)| amount)
        .sum()
}

fn rows_from_batches(batches: &[RecordBatch]) -> Vec<UserRow> {
    let mut rows = Vec::new();
    for batch in batches {
        let ids = batch
            .column_by_name("id")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let names = batch
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let amounts = batch
            .column_by_name("amount")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            if ids.is_null(i) {
                continue;
            }
            rows.push(UserRow {
                id: ids.value(i),
                name: names.value(i).to_string(),
                amount: amounts.value(i),
            });
        }
    }
    rows
}

pub fn run(ops: &[Op]) -> World {
    let mut world = World::new();
    for op in ops {
        apply(&mut world, op.clone());
    }
    world
}

pub fn pin_drained(world: &World) -> bool {
    live_wal(world).is_empty()
}

pub fn catalog_segments(world: &World) -> Vec<(CatalogKey, CatalogRow)> {
    query_all(&world.catalog, CatalogPrefix::Segment).unwrap_or_default()
}

pub fn encode_ops(ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::from(*OPS_MAGIC);
    out.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    for op in ops {
        encode_op(&mut out, op);
    }
    out
}

fn encode_op(out: &mut Vec<u8>, op: &Op) {
    match op {
        Op::IngestLake(row) => {
            out.push(0);
            encode_row(out, row);
        }
        Op::IngestWal(event) => {
            out.push(1);
            encode_row(out, &event.row);
            out.push(match event.mutation {
                None => 0xff,
                Some(MutationKind::Snapshot) => 0,
                Some(MutationKind::Insert) => 1,
                Some(MutationKind::Update) => 2,
                Some(MutationKind::Delete) => 3,
            });
            out.push(event.order);
            out.extend_from_slice(&event.seq.to_le_bytes());
            out.extend_from_slice(&event.ordinal.to_le_bytes());
            out.extend_from_slice(&event.row_i.to_le_bytes());
        }
        Op::CompactThru(seq) => {
            out.push(2);
            out.extend_from_slice(&seq.to_le_bytes());
        }
        Op::Failpoint(name) => {
            out.push(3);
            let idx = FailpointName::ALL
                .iter()
                .position(|n| *n == *name)
                .unwrap_or(0) as u8;
            out.push(idx);
        }
        Op::Reopen => out.push(4),
        Op::HoldPin => out.push(5),
        Op::ReleasePin => out.push(6),
        Op::ReleaseCompaction => out.push(7),
        Op::DiskPause => out.push(8),
        Op::DiskResume => out.push(9),
        Op::DeadletterSkip => out.push(10),
        Op::EvolveAddColumn(name) => {
            out.push(11);
            let bytes = name.as_bytes();
            let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&bytes[..len as usize]);
        }
    }
}

fn encode_row(out: &mut Vec<u8>, row: &UserRow) {
    out.extend_from_slice(&row.id.to_le_bytes());
    let bytes = row.name.as_bytes();
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
    out.extend_from_slice(&row.amount.to_le_bytes());
}

fn take_u8(bytes: &[u8], i: &mut usize) -> Option<u8> {
    let b = *bytes.get(*i)?;
    *i += 1;
    Some(b)
}

fn take_n<'a>(bytes: &'a [u8], i: &mut usize, n: usize) -> Option<&'a [u8]> {
    let end = i.checked_add(n)?;
    let slice = bytes.get(*i..end)?;
    *i = end;
    Some(slice)
}

fn take_i64(bytes: &[u8], i: &mut usize) -> Option<i64> {
    let raw = take_n(bytes, i, 8)?;
    Some(i64::from_le_bytes(raw.try_into().ok()?))
}

fn take_u64(bytes: &[u8], i: &mut usize) -> Option<u64> {
    let raw = take_n(bytes, i, 8)?;
    Some(u64::from_le_bytes(raw.try_into().ok()?))
}

fn take_u32(bytes: &[u8], i: &mut usize) -> Option<u32> {
    let raw = take_n(bytes, i, 4)?;
    Some(u32::from_le_bytes(raw.try_into().ok()?))
}

fn take_u16(bytes: &[u8], i: &mut usize) -> Option<u16> {
    let raw = take_n(bytes, i, 2)?;
    Some(u16::from_le_bytes(raw.try_into().ok()?))
}

fn decode_row(bytes: &[u8], i: &mut usize) -> Option<UserRow> {
    let id = take_i64(bytes, i)?;
    let name_len = take_u16(bytes, i)? as usize;
    let name = String::from_utf8(take_n(bytes, i, name_len)?.to_vec()).ok()?;
    let amount = take_i64(bytes, i)?;
    Some(UserRow { id, name, amount })
}

fn decode_ops(bytes: &[u8]) -> Option<Vec<Op>> {
    if bytes.len() < 8 || bytes.get(..4) != Some(OPS_MAGIC.as_slice()) {
        return None;
    }
    let mut i = 4;
    let count = take_u32(bytes, &mut i)? as usize;
    if count > MAX_SCHEDULE_OPS {
        return None;
    }
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        ops.push(decode_op(bytes, &mut i)?);
    }
    Some(ops)
}

fn decode_op(bytes: &[u8], i: &mut usize) -> Option<Op> {
    match take_u8(bytes, i)? {
        0 => Some(Op::IngestLake(decode_row(bytes, i)?)),
        1 => {
            let row = decode_row(bytes, i)?;
            let mutation = match take_u8(bytes, i)? {
                0xff => None,
                0 => Some(MutationKind::Snapshot),
                1 => Some(MutationKind::Insert),
                2 => Some(MutationKind::Update),
                3 => Some(MutationKind::Delete),
                _ => return None,
            };
            let order = take_u8(bytes, i)?;
            let seq = take_u64(bytes, i)?;
            if !schedule_seq_ok(seq) {
                return None;
            }
            let ordinal = take_u32(bytes, i)?;
            if !schedule_seq_ok(ordinal as u64) {
                return None;
            }
            let row_i = take_u64(bytes, i)?;
            if !schedule_seq_ok(row_i) {
                return None;
            }
            Some(Op::IngestWal(WalEvent {
                row,
                mutation,
                order,
                seq,
                ordinal,
                row_i,
            }))
        }
        2 => {
            let seq = take_u64(bytes, i)?;
            if !schedule_seq_ok(seq) {
                return None;
            }
            Some(Op::CompactThru(seq))
        }
        3 => {
            let idx = take_u8(bytes, i)? as usize;
            Some(Op::Failpoint(*FailpointName::ALL.get(idx)?))
        }
        4 => Some(Op::Reopen),
        5 => Some(Op::HoldPin),
        6 => Some(Op::ReleasePin),
        7 => Some(Op::ReleaseCompaction),
        8 => Some(Op::DiskPause),
        9 => Some(Op::DiskResume),
        10 => Some(Op::DeadletterSkip),
        11 => {
            let len = take_u16(bytes, i)? as usize;
            let name = String::from_utf8(take_n(bytes, i, len)?.to_vec()).ok()?;
            Some(Op::EvolveAddColumn(name))
        }
        _ => None,
    }
}

pub fn schedule_from_bytes(bytes: &[u8]) -> Vec<Op> {
    decode_ops(bytes).unwrap_or_default()
}

pub fn compacted_seqs(world: &World) -> BTreeSet<u64> {
    let thru = world.compacted_thru.min(MAX_SCHEDULE_OPS as u64);
    (1..=thru).collect()
}
