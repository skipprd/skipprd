use crate::buffer::wal_reader::PinReadError;
use crate::query_flight::service::sql_is_within_ceiling;
use skippr_lease::CONTROL_FRAME_MAX_BYTES;

#[test]
fn b_reclaim_between_getflight_and_doget_wal_pin_lost() {
    use crate::buffer::wal_reader::WalCatalogReader;
    use skippr_lease::PipelineKey;
    use skippr_wal_catalog::MemoryWalCatalog;
    use std::sync::Arc;
    let reader = WalCatalogReader::new(
        PipelineKey::new("t", "w", "p").unwrap(),
        Arc::new(MemoryWalCatalog::new()),
    );
    let err = reader.pinned_slice("missing", 0).unwrap_err();
    assert!(
        matches!(&err, PinReadError::Lost(lost) if lost.segment_id.as_str() == "missing"),
        "{err}"
    );
    assert!(err.to_string().contains("retry the query"));
}

#[test]
fn h_oversized_sql_fail_closed() {
    let sql = "x".repeat(CONTROL_FRAME_MAX_BYTES + 1);
    assert!(!sql_is_within_ceiling(&sql));
}

#[test]
fn h_lake_only_env_is_deleted() {
    use crate::plugins::cdc::MutationKind;
    use crate::sqlrt::dst::{pin_drained, run, Op, UserRow, WalEvent};
    let world = run(&[
        Op::IngestWal(WalEvent {
            row: UserRow {
                id: 1,
                name: "a".into(),
                amount: 1,
            },
            mutation: Some(MutationKind::Insert),
            order: 1,
            seq: 1,
            ordinal: 0,
            row_i: 0,
        }),
        Op::CompactThru(1),
    ]);
    assert!(
        pin_drained(&world),
        "lake-only is pin drained, not SKIPPR_QUERY_LAKE_ONLY"
    );
}

#[test]
fn h_minus_excluded() {
    use crate::sqlrt::read_boundary::minus_excluded;
    use crate::wal_slice_ref::WalSliceRef;
    use std::collections::HashSet;
    let keep = WalSliceRef::try_from_parts("keep", 0).unwrap();
    let drop = WalSliceRef::try_from_parts("drop", 8).unwrap();
    let excluded = HashSet::from([drop.clone()]);
    let live = vec![keep.clone(), drop.clone()];
    let got = minus_excluded(live, &excluded, |r| r.clone());
    assert_eq!(got, vec![keep]);
}

#[test]
fn h_ticket_slices_minus_excluded() {
    use crate::sqlrt::read_boundary::minus_excluded;
    use crate::wal_slice_ref::WalSliceRef;
    use skippr_query_plan::PinnedSliceRef;
    use std::collections::HashSet;
    let excluded = HashSet::from([WalSliceRef::try_from_parts("gone", 1).unwrap()]);
    let live = vec![
        PinnedSliceRef {
            segment_id: "keep".into(),
            start: 0,
            seq: 1,
            ordinal: 0,
        },
        PinnedSliceRef {
            segment_id: "gone".into(),
            start: 1,
            seq: 2,
            ordinal: 1,
        },
        PinnedSliceRef {
            segment_id: "tail".into(),
            start: 4,
            seq: 3,
            ordinal: 0,
        },
    ];
    let slices = minus_excluded(live, &excluded, |slice| {
        WalSliceRef::try_from_parts(&slice.segment_id, slice.start).unwrap()
    });
    assert_eq!(slices.len(), 2);
    assert_eq!(slices[0].segment_id, "keep");
    assert_eq!(slices[1].segment_id, "tail");
}

#[test]
fn h_foreign_tenant_ticket() {
    use crate::query_flight::service::pin_live_wal;
    use crate::query_flight::TenantScope;
    use skippr_query_plan::LiveWalPinRequest;
    let scope = TenantScope::new("t", "w").unwrap();
    let pin = LiveWalPinRequest {
        tenant: "other".into(),
        workspace: "w".into(),
        pipeline: "p".into(),
        namespace: "ns".into(),
    };
    let err = pin_live_wal(&scope, &pin).unwrap_err();
    assert!(
        err.message()
            .contains("live WAL pin tenant/workspace does not match session"),
        "{err}"
    );
}

#[test]
fn h_query_pin_retry_ceiling() {
    use crate::buffer::wal_reader::WalPinLost;
    use crate::sqlrt::wal_table::{is_wal_pin_lost, QUERY_PIN_RETRY_CEILING};
    use datafusion::error::DataFusionError;
    use skippr_lease::SegmentId;
    assert_eq!(QUERY_PIN_RETRY_CEILING, 3);
    let lost = DataFusionError::External(Box::new(WalPinLost {
        segment_id: SegmentId::new("gone").unwrap(),
        start: 0,
    }));
    assert!(is_wal_pin_lost(&lost));
    assert!(!is_wal_pin_lost(&DataFusionError::Plan(
        "WAL catalog for pipeline p is not open".into()
    )));
    let mut retries = 0u32;
    for attempt in 0..QUERY_PIN_RETRY_CEILING {
        if is_wal_pin_lost(&lost) && attempt + 1 < QUERY_PIN_RETRY_CEILING {
            retries += 1;
        }
    }
    assert_eq!(retries, QUERY_PIN_RETRY_CEILING - 1);
}

#[test]
fn h_illegal_sql() {
    use crate::query_flight::service::sql_is_within_ceiling;
    use skippr_lease::CONTROL_FRAME_MAX_BYTES;
    assert!(sql_is_within_ceiling("SELECT 1"));
    assert!(!sql_is_within_ceiling(
        &"x".repeat(CONTROL_FRAME_MAX_BYTES + 1)
    ));
    match crate::sqlrt::parser::SParser::new("ALTER TABLE t DROP COLUMN") {
        Err(_) => {}
        Ok(mut parser) => {
            assert!(parser.parse_statement().is_err());
        }
    }
}
