use crate::plugins::cdc::{MutationKind, WalPartKind, WalPartMeta, WalPartMetaError, WalRowMeta};
use crate::sqlrt::cdc_reconcile::{attach_cdc_sidecar, collapse_wal_batches};
use crate::sqlrt::dst::{batch_from_row, UserRow};
use arrow::array::{Int64Array, StringArray, UInt8Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use std::sync::Arc;

#[test]
fn b_wal_null_business_key_fail_closed() {
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
fn b_unknown_mutation_sidecar_fail_closed() {
    let wal = attach_cdc_sidecar(
        batch_from_row(&UserRow {
            id: 1,
            name: "a".into(),
            amount: 1,
        }),
        &[MutationKind::Insert],
        &[vec![1]],
        1,
        0,
        0,
    )
    .unwrap();
    let mut cols = wal.columns().to_vec();
    cols[3] = Arc::new(UInt8Array::from(vec![99u8]));
    let wal = RecordBatch::try_new(wal.schema(), cols).unwrap();
    let err = collapse_wal_batches(&[wal], &["id".into()]).unwrap_err();
    assert!(err.to_string().contains("unknown CDC mutation"), "{err}");
}

#[test]
fn b_cdc_rows_on_append_rejected() {
    let meta = WalPartMeta {
        kind: WalPartKind::Append,
        row_count: 1,
        rows: vec![WalRowMeta {
            mutation: MutationKind::Insert,
            event_id: b"e".to_vec(),
            order_token: vec![1],
        }],
    };
    assert_eq!(
        meta.validate().unwrap_err(),
        WalPartMetaError::CdcRowsOnAppend
    );
}

#[test]
fn b_projection_without_pk_fail_closed() {
    let wal = attach_cdc_sidecar(
        batch_from_row(&UserRow {
            id: 1,
            name: "a".into(),
            amount: 1,
        }),
        &[MutationKind::Insert],
        &[vec![1]],
        1,
        0,
        0,
    )
    .unwrap();
    let err = collapse_wal_batches(&[wal], &[]).unwrap_err();
    assert!(
        err.to_string().contains("requires business_key_columns"),
        "{err}"
    );
}

#[test]
fn j_debezium_op_maps_to_mutation_kind() {
    assert_eq!(MutationKind::from_debezium_op("c"), MutationKind::Insert);
    assert_eq!(MutationKind::from_debezium_op("u"), MutationKind::Update);
    assert_eq!(MutationKind::from_debezium_op("d"), MutationKind::Delete);
    assert_eq!(MutationKind::from_debezium_op("r"), MutationKind::Snapshot);
    assert_eq!(MutationKind::from_debezium_op("x"), MutationKind::Insert);
}
