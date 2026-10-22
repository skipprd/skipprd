//! Fuzz composed Arrow ops: CDC sidecar alignment and fail-closed errors.

use super::*;
use crate::helpers::configuration::TransformOp;
use crate::plugins::cdc::{MutationKind, WalRowMeta};
use arrow::array::{Int64Array, StringArray};
use arrow_schema::{DataType, Field, Schema};
use proptest::prelude::*;
use std::sync::Arc;

fn stream_test_cases() -> u32 {
    match std::env::var("SKIPPR_STREAM_TEST_PROFILE").ok().as_deref() {
        Some("smoke") => 8,
        _ => 32,
    }
}

fn batch_amounts(ids: &[&str], amounts: &[i64]) -> RecordBatch {
    let schema = Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("amount", DataType::Int64, false),
    ]);
    RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(StringArray::from(ids.to_vec())),
            Arc::new(Int64Array::from(amounts.to_vec())),
        ],
    )
    .unwrap()
}

fn cdc_for(n: usize) -> Vec<WalRowMeta> {
    (0..n)
        .map(|i| WalRowMeta {
            mutation: MutationKind::Insert,
            event_id: format!("e{i}").into_bytes(),
            order_token: vec![i as u8],
        })
        .collect()
}

fn arb_op() -> impl Strategy<Value = TransformOp> {
    prop_oneof![
        (0i64..20).prop_map(|n| TransformOp {
            filter: Some(format!("amount > {n}")),
            ..Default::default()
        }),
        Just(TransformOp {
            project: Some(vec!["id".into(), "amount".into()]),
            ..Default::default()
        }),
        (1i64..5).prop_map(|k| TransformOp {
            derive: Some(
                [("scaled".into(), format!("amount * {k}"))]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        }),
        Just(TransformOp {
            rename: Some(
                [("amount".into(), "amount_usd".into())]
                    .into_iter()
                    .collect()
            ),
            ..Default::default()
        }),
        Just(TransformOp {
            cast: Some([("amount".into(), "float64".into())].into_iter().collect()),
            ..Default::default()
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(stream_test_cases()))]

    #[test]
    fn fuzz_ops_keep_cdc_aligned_or_fail_closed(
        amounts in proptest::collection::vec(-5i64..40, 1..8),
        ops in proptest::collection::vec(arb_op(), 0..4),
    ) {
        let ids: Vec<String> = (0..amounts.len()).map(|i| format!("id{i}")).collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let batch = batch_amounts(&id_refs, &amounts);
        let cdc = cdc_for(amounts.len());
        match apply_configured_ops(&ops, vec![batch], Some(cdc.clone())) {
            Ok((batches, out_cdc)) => {
                let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
                if ops.is_empty() {
                    prop_assert_eq!(rows, amounts.len());
                    prop_assert_eq!(out_cdc.as_ref().map(Vec::len), Some(cdc.len()));
                } else if let Some(cdc_out) = out_cdc {
                    prop_assert_eq!(cdc_out.len(), rows);
                }
            }
            Err(_) => {}
        }
    }

    #[test]
    fn fuzz_unknown_column_is_an_error(
        name in "[a-z]{3,8}",
    ) {
        prop_assume!(name != "id" && name != "amount");
        let batch = batch_amounts(&["a"], &[1]);
        let ops = vec![TransformOp {
            project: Some(vec![name]),
            ..Default::default()
        }];
        prop_assert!(apply_configured_ops(&ops, vec![batch], None).is_err());
    }
}
