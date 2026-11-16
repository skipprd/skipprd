use crate::plugins::cdc::{MutationKind, WalPartKind, WalPartMeta, WalPartMetaError, WalRowMeta};

#[test]
fn cdc_one_model_matrix() {
    skippr_cdc::prove_matrix().unwrap();
}
use crate::sqlrt::cdc_reconcile::{attach_cdc_sidecar, reconcile_cdc};
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
    let err = reconcile_cdc(&[], &[wal], &Default::default(), &["id".into()]).unwrap_err();
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
    let err = reconcile_cdc(&[], &[wal], &Default::default(), &["id".into()]).unwrap_err();
    assert!(err.to_string().contains("unknown CDC mutation"), "{err}");
}

#[test]
fn b_cdc_rows_on_append_rejected() {
    let meta = WalPartMeta {
        kind: WalPartKind::Append,
        row_count: 1,
        rows: vec![WalRowMeta::new(
            MutationKind::Insert,
            b"e".to_vec(),
            vec![1],
        )],
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
    let err = reconcile_cdc(&[], &[wal], &Default::default(), &[]).unwrap_err();
    assert!(
        err.to_string().contains("requires business_key_columns"),
        "{err}"
    );
}

#[test]
fn b_source_transaction_id_range_does_not_split_a_batch() {
    use crate::sqlrt::read_boundary::pin_catalog_range;
    use skippr_cdc::{align_start, append_entry, RangeBoundary};
    let mut entries = Vec::new();
    append_entry(&mut entries, vec![RangeBoundary::Source(b"tx".to_vec())]);
    append_entry(&mut entries, vec![RangeBoundary::Source(b"tx".to_vec())]);
    let (start, tip) = pin_catalog_range(&entries, 2);
    assert_eq!(start, 1);
    assert_eq!(tip, 2);
    assert_eq!(align_start(&entries, 2), 1);
}

#[test]
fn b_idle_ingest_batch_ids_do_not_align() {
    use crate::sqlrt::read_boundary::pin_catalog_range;
    use skippr_cdc::{align_start, append_entry, RangeBoundary};
    let mut entries = Vec::new();
    append_entry(&mut entries, vec![RangeBoundary::Ingest(1)]);
    append_entry(&mut entries, vec![RangeBoundary::Ingest(2)]);
    let (start, tip) = pin_catalog_range(&entries, 2);
    assert_eq!(start, 2);
    assert_eq!(tip, 2);
    assert_eq!(align_start(&entries, 2), 2);
    assert_eq!(
        skippr_cdc::range_boundary(None, 1),
        RangeBoundary::Ingest(1)
    );
    assert_eq!(
        skippr_cdc::range_boundary(Some(b"tx"), 1),
        RangeBoundary::Source(b"tx".to_vec())
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
