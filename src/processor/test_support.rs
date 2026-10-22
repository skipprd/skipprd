//! Shared local-FS Sled fixture, golden model, and CDC event helpers.

use super::*;
use arrow::array::{Int64Array, RecordBatch, StringArray};
use arrow::datatypes::DataType;
use arrow_schema::{Field, Schema};
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

pub fn invoice_agg() -> TransformAggregate {
    TransformAggregate {
        id: "customer_invoice_totals".into(),
        key_fields: vec!["customer_id".into()],
        measures: [
            (
                "invoice_total".into(),
                AggregateMeasure {
                    sum: Some("amount".into()),
                    ..Default::default()
                },
            ),
            (
                "invoice_count".into(),
                AggregateMeasure {
                    count: Some("*".into()),
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .collect(),
    }
}

pub fn invoice_batch(customers: &[&str], amounts: &[i64], ids: &[&str]) -> RecordBatch {
    let schema = Schema::new(vec![
        Field::new("customer_id", DataType::Utf8, false),
        Field::new("amount", DataType::Int64, false),
        Field::new("invoice_id", DataType::Utf8, false),
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

pub fn insert_meta(id: &str) -> WalRowMeta {
    WalRowMeta {
        mutation: MutationKind::Insert,
        event_id: id.as_bytes().to_vec(),
        order_token: id.as_bytes().to_vec(),
    }
}

pub fn meta(kind: MutationKind, id: &str) -> WalRowMeta {
    WalRowMeta {
        mutation: kind,
        event_id: id.as_bytes().to_vec(),
        order_token: id.as_bytes().to_vec(),
    }
}

#[derive(Clone, Debug)]
pub struct SimEvent {
    pub customer: String,
    pub amount: i64,
    pub invoice_id: String,
    pub mutation: MutationKind,
    pub event_id: String,
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
        }
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
        meta(self.mutation, &self.event_id)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Model {
    pub totals: BTreeMap<String, BTreeMap<String, f64>>,
    pub rows: BTreeMap<String, BTreeMap<String, Value>>,
    pub entity: BTreeMap<String, String>,
    pub seen: BTreeSet<String>,
}

impl Model {
    pub fn apply(&mut self, spec: &TransformAggregate, event: &SimEvent) {
        if !event.event_id.is_empty() && !self.seen.insert(event.event_id.clone()) {
            return;
        }
        let new_row = event.row();
        let new_key = encode_key(&spec.key_fields, &new_row).unwrap();
        let old = self.load_previous(spec, &new_key, &new_row);
        let mut deltas = BTreeMap::new();
        let mut last_rows = BTreeMap::new();
        apply_mutation(
            spec,
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
                if let Some(entity) = entity_identity(spec, &row) {
                    self.entity.insert(entity, key.clone());
                }
                self.rows.insert(key, row);
            }
        }
    }

    fn load_previous(
        &self,
        spec: &TransformAggregate,
        new_key: &str,
        new_row: &BTreeMap<String, Value>,
    ) -> Option<BTreeMap<String, Value>> {
        if let Some(entity) = entity_identity(spec, new_row) {
            if let Some(old_key) = self.entity.get(&entity) {
                if let Some(row) = self
                    .rows
                    .get(old_key)
                    .cloned()
                    .filter(|row| !row.is_empty())
                {
                    return Some(row);
                }
            }
        }
        self.rows
            .get(new_key)
            .cloned()
            .filter(|row| !row.is_empty())
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
    db: Option<sled::Db>,
    state: Option<StreamState>,
    pub spec: TransformAggregate,
}

impl DiskFixture {
    pub fn new() -> Self {
        Self::with_spec(invoice_agg())
    }

    pub fn with_spec(spec: TransformAggregate) -> Self {
        let dir = tempfile::tempdir().expect("local tempdir");
        let db_path = dir.path().join("db");
        let (db, state) = open_db(&db_path, spec.clone());
        assert!(
            db_path.exists(),
            "sled must persist on the local filesystem at {}",
            db_path.display()
        );
        Self {
            dir,
            db_path,
            db: Some(db),
            state: Some(state),
            spec,
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
        drop(self.db.take());
        let (db, state) = open_db(&db_path, spec);
        self.db = Some(db);
        self.state = Some(state);
        self
    }

    pub fn release(mut self) -> (tempfile::TempDir, PathBuf) {
        let db_path = self.db_path.clone();
        drop(self.state.take());
        drop(self.db.take());
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
                    .measure(&self.spec.id, &key, measure)
                    .unwrap();
                let expected = model.total(&key, measure);
                assert!(
                    (actual - expected).abs() < 1e-9,
                    "measure {key}.{measure}: sled={actual} model={expected}"
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

pub fn open_db(path: &Path, spec: TransformAggregate) -> (sled::Db, StreamState) {
    let db = open_sled_with_retry(path);
    let tree = db.open_tree(STREAM_STATE_TREE).unwrap();
    let state = StreamState::open(tree, "events", vec![spec]).unwrap();
    (db, state)
}

fn open_sled_with_retry(path: &Path) -> sled::Db {
    let mut last = None;
    for delay_ms in [0u64, 5, 10, 20, 40, 80, 160, 320] {
        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        match sled::Config::new().path(path).flush_every_ms(None).open() {
            Ok(db) => return db,
            Err(err) => last = Some(err),
        }
    }
    panic!("open local sled at {}: {:?}", path.display(), last);
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
