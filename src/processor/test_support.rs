//! Shared local-FS Skippr Store fixture, golden model, and CDC event helpers.

use super::*;
use crate::compute::{
    field_with_id, ComputeKind, EventTimeCol, EventWatermark, FieldId, Fqn, Ident, Measure,
    MeasureKind, NonEmpty, PipelineName, Relation, Tumble,
};
use arrow::array::{Int64Array, RecordBatch, StringArray};
use arrow::datatypes::DataType;
use arrow_schema::Schema;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub fn stream_test_cases() -> u32 {
    match std::env::var("SKIPPR_STREAM_TEST_PROFILE").ok().as_deref() {
        Some("smoke") => 8,
        _ => 64,
    }
}

pub fn invoice_kind() -> ComputeKind {
    ComputeKind::KeyedTable {
        name: Fqn::from_parts("events", "customer_invoice_totals").unwrap(),
        input: Relation::PipelineTable {
            pipeline: PipelineName::new("events").unwrap(),
            namespace: Ident::new("orders").unwrap(),
        },
        keys: NonEmpty::try_from_vec(vec![FieldId::new(1).unwrap()]).unwrap(),
        measures: NonEmpty::try_from_vec(vec![
            Measure {
                name: Ident::new("invoice_total").unwrap(),
                kind: MeasureKind::Sum(FieldId::new(2).unwrap()),
            },
            Measure {
                name: Ident::new("invoice_count").unwrap(),
                kind: MeasureKind::Count,
            },
        ])
        .unwrap(),
    }
}

pub fn hourly_kind() -> ComputeKind {
    ComputeKind::WindowedTable {
        name: Fqn::from_parts("events", "hourly_totals").unwrap(),
        input: Relation::PipelineTable {
            pipeline: PipelineName::new("events").unwrap(),
            namespace: Ident::new("orders").unwrap(),
        },
        keys: NonEmpty::try_from_vec(vec![FieldId::new(1).unwrap()]).unwrap(),
        window: Tumble {
            size: Duration::from_secs(3600),
            on: EventTimeCol::new(FieldId::new(4).unwrap()),
        },
        watermark: EventWatermark::new(
            EventTimeCol::new(FieldId::new(4).unwrap()),
            Duration::from_secs(300),
        ),
        measures: NonEmpty::try_from_vec(vec![Measure {
            name: Ident::new("invoice_total").unwrap(),
            kind: MeasureKind::Sum(FieldId::new(2).unwrap()),
        }])
        .unwrap(),
    }
}

pub fn window_batch(
    customers: &[&str],
    amounts: &[i64],
    ids: &[&str],
    times: &[&str],
) -> RecordBatch {
    let schema = Schema::new(vec![
        field_with_id("customer_id", DataType::Utf8, false, 1),
        field_with_id("amount", DataType::Int64, false, 2),
        field_with_id("invoice_id", DataType::Utf8, false, 3),
        field_with_id("created_at", DataType::Utf8, false, 4),
    ]);
    RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(StringArray::from(customers.to_vec())),
            Arc::new(Int64Array::from(amounts.to_vec())),
            Arc::new(StringArray::from(ids.to_vec())),
            Arc::new(StringArray::from(times.to_vec())),
        ],
    )
    .unwrap()
}

pub fn window_store_key(customer: &str, rfc3339: &str) -> String {
    let ts = chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .timestamp();
    let bucket = ts.div_euclid(3600) * 3600;
    format!("{customer}\u{1f}{bucket}")
}

pub fn invoice_batch(customers: &[&str], amounts: &[i64], ids: &[&str]) -> RecordBatch {
    let schema = Schema::new(vec![
        field_with_id("customer_id", DataType::Utf8, false, 1),
        field_with_id("amount", DataType::Int64, false, 2),
        field_with_id("invoice_id", DataType::Utf8, false, 3),
    ]);
    RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(StringArray::from(customers.to_vec())),
            Arc::new(Int64Array::from(amounts.to_vec())),
            Arc::new(StringArray::from(ids.to_vec())),
        ],
    )
    .unwrap()
}

pub fn invoice_before(
    customer: &str,
    amount: i64,
    invoice_id: &str,
) -> crate::plugins::cdc::BeforeImage {
    crate::plugins::cdc::BeforeImage {
        key: customer.as_bytes().to_vec(),
        row: serde_json::to_vec(&BTreeMap::from([
            ("customer_id".to_string(), Value::String(customer.into())),
            ("amount".to_string(), Value::from(amount)),
            ("invoice_id".to_string(), Value::String(invoice_id.into())),
        ]))
        .expect("before row"),
    }
}

pub fn insert_meta(id: &str) -> WalRowMeta {
    WalRowMeta::new(
        MutationKind::Insert,
        id.as_bytes().to_vec(),
        id.as_bytes().to_vec(),
    )
}

pub fn meta(kind: MutationKind, id: &str) -> WalRowMeta {
    WalRowMeta::new(kind, id.as_bytes().to_vec(), id.as_bytes().to_vec())
}

#[derive(Clone, Debug)]
pub struct SimEvent {
    pub customer: String,
    pub amount: i64,
    pub invoice_id: String,
    pub mutation: MutationKind,
    pub event_id: String,
    pub before_row: Option<BTreeMap<String, Value>>,
}

impl SimEvent {
    pub fn new(
        mutation: MutationKind,
        customer: impl Into<String>,
        amount: i64,
        invoice_id: impl Into<String>,
        event_id: impl Into<String>,
    ) -> Self {
        Self {
            customer: customer.into(),
            amount,
            invoice_id: invoice_id.into(),
            mutation,
            event_id: event_id.into(),
            before_row: None,
        }
    }

    pub fn with_before_row(mut self, row: BTreeMap<String, Value>) -> Self {
        self.before_row = Some(row);
        self
    }

    pub fn row(&self) -> BTreeMap<String, Value> {
        let mut row = BTreeMap::new();
        row.insert("customer_id".into(), Value::String(self.customer.clone()));
        row.insert("amount".into(), Value::from(self.amount));
        row.insert("invoice_id".into(), Value::String(self.invoice_id.clone()));
        row
    }

    pub fn batch(&self) -> RecordBatch {
        invoice_batch(&[&self.customer], &[self.amount], &[&self.invoice_id])
    }

    pub fn cdc(&self) -> WalRowMeta {
        let mut row = meta(self.mutation, &self.event_id);
        if let Some(before) = &self.before_row {
            row = row.with_before(crate::plugins::cdc::BeforeImage {
                key: before
                    .get("customer_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .as_bytes()
                    .to_vec(),
                row: serde_json::to_vec(before).expect("before row"),
            });
        }
        row
    }
}

#[derive(Clone, Debug, Default)]
pub struct Model {
    pub totals: BTreeMap<String, BTreeMap<String, f64>>,
    pub rows: BTreeMap<String, BTreeMap<String, Value>>,
    pub seen: BTreeSet<String>,
}

impl Model {
    pub fn apply(&mut self, spec: &ComputeKind, event: &SimEvent) {
        if !event.event_id.is_empty() && !self.seen.insert(event.event_id.clone()) {
            return;
        }
        let schema = event.batch().schema();
        if event.mutation == MutationKind::Update && event.before_row.is_none() {
            return;
        }
        let new_row = event.row();
        let new_key = encode_key_ids(spec, schema.as_ref(), &new_row).unwrap();
        let old = event.before_row.clone().or_else(|| {
            self.rows
                .get(&new_key)
                .cloned()
                .filter(|row| !row.is_empty())
        });
        let mut deltas = BTreeMap::new();
        let mut last_rows = BTreeMap::new();
        apply_mutation(
            spec,
            schema.as_ref(),
            event.mutation,
            &new_key,
            old.as_ref(),
            &new_row,
            &mut deltas,
            &mut last_rows,
        )
        .unwrap();
        for (key, values) in deltas {
            let slot = self.totals.entry(key).or_default();
            for (name, delta) in values {
                *slot.entry(name).or_insert(0.0) += delta;
            }
        }
        for (key, row) in last_rows {
            if row.is_empty() {
                self.rows.remove(&key);
            } else {
                self.rows.insert(key, row);
            }
        }
    }

    pub fn total(&self, key: &str, measure: &str) -> f64 {
        self.totals
            .get(key)
            .and_then(|m| m.get(measure))
            .copied()
            .unwrap_or(0.0)
    }
}

pub struct DiskFixture {
    pub dir: tempfile::TempDir,
    pub db_path: PathBuf,
    kv: Option<Arc<dyn skippr_wal_catalog::SkipprStore>>,
    state: Option<StreamState>,
    pub spec: ComputeKind,
}

impl DiskFixture {
    pub fn new() -> Self {
        Self::with_spec(invoice_kind())
    }

    pub fn with_spec(spec: ComputeKind) -> Self {
        let dir = tempfile::tempdir().expect("local tempdir");
        let db_path = dir.path().join("db");
        let (kv, state) = open_db(&db_path, spec.clone());
        assert!(
            db_path.exists(),
            "Skippr Store must persist on the local filesystem at {}",
            db_path.display()
        );
        Self {
            dir,
            db_path,
            kv: Some(kv),
            state: Some(state),
            spec,
        }
    }

    pub fn kv_for_test(&self) -> Arc<dyn skippr_wal_catalog::SkipprStore> {
        self.kv.clone().expect("test store")
    }

    pub fn wipe_compute(&self, id: &str) {
        let fence = skippr_wal_catalog::CatalogFence::local();
        let store = self.kv_for_test();
        let ops = crate::compute::prefix_deletes(store.as_ref(), "events", id).unwrap();
        if !ops.is_empty() {
            store.commit(&ops, fence).unwrap();
        }
    }

    pub fn apply(&self, event: &SimEvent, cursor: WalCursor) -> Result<WalCursor, String> {
        self.state.as_ref().unwrap().apply_events(
            &self.spec,
            cursor,
            &event.batch(),
            Some(&[event.cdc()]),
            0,
        )
    }

    pub fn apply_all(&self, events: &[SimEvent]) -> WalCursor {
        let mut cursor = WalCursor::default();
        for event in events {
            cursor = self.apply(event, cursor).unwrap();
        }
        cursor
    }

    pub fn reopen(mut self) -> Self {
        let spec = self.spec.clone();
        let db_path = self.db_path.clone();
        drop(self.state.take());
        drop(self.kv.take());
        let (kv, state) = open_db(&db_path, spec);
        self.kv = Some(kv);
        self.state = Some(state);
        self
    }

    pub fn release(mut self) -> (tempfile::TempDir, PathBuf) {
        let db_path = self.db_path.clone();
        drop(self.state.take());
        drop(self.kv.take());
        let dir = std::mem::replace(&mut self.dir, tempfile::tempdir().unwrap());
        (dir, db_path)
    }

    pub fn crash_one(self, event: &SimEvent, cursor: WalCursor, point: CrashPoint) -> (Self, bool) {
        struct ClearCrash;
        impl Drop for ClearCrash {
            fn drop(&mut self) {
                set_test_crash(None);
            }
        }
        let _guard = ClearCrash;
        set_test_crash(Some(point));
        let spec = self.spec.clone();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = self.state.as_ref().unwrap().apply_events(
                &spec,
                cursor,
                &event.batch(),
                Some(&[event.cdc()]),
                0,
            );
        }))
        .is_err();
        drop(_guard);
        (self.reopen(), panicked)
    }

    pub fn assert_matches(&self, model: &Model) {
        let keys: BTreeSet<String> = model
            .totals
            .keys()
            .cloned()
            .chain(["A", "B", "C", "D"].into_iter().map(str::to_string))
            .collect();
        for key in keys {
            for measure in ["invoice_total", "invoice_count"] {
                let actual = self
                    .state
                    .as_ref()
                    .unwrap()
                    .measure(self.spec.store_id().unwrap(), &key, measure)
                    .unwrap();
                let expected = model.total(&key, measure);
                assert!(
                    (actual - expected).abs() < 1e-9,
                    "measure {key}.{measure}: store={actual} model={expected}"
                );
            }
        }
    }
}

impl Drop for DiskFixture {
    fn drop(&mut self) {
        reset_reclaim_hold();
        set_test_crash(None);
    }
}

impl std::ops::Deref for DiskFixture {
    type Target = StreamState;
    fn deref(&self) -> &StreamState {
        self.state.as_ref().unwrap()
    }
}

pub fn open_db(
    path: &Path,
    spec: ComputeKind,
) -> (Arc<dyn skippr_wal_catalog::SkipprStore>, StreamState) {
    let fence = skippr_wal_catalog::CatalogFence::local();
    let kv: Arc<dyn skippr_wal_catalog::SkipprStore> = Arc::new(
        skippr_wal_catalog::FjallSkipprStore::open(path, fence)
            .unwrap_or_else(|err| panic!("open Skippr Store at {}: {err}", path.display())),
    );
    let state = StreamState::open(Arc::clone(&kv), fence, "events", vec![spec]).unwrap();
    (kv, state)
}

pub fn crash_points() -> &'static [CrashPoint] {
    &[
        CrashPoint::BeforeTxn,
        CrashPoint::BeforeCommit,
        CrashPoint::DuringTxn,
        CrashPoint::AfterTxn,
    ]
}

pub fn commit_survives(point: CrashPoint) -> bool {
    matches!(point, CrashPoint::AfterTxn)
}
