use crate::helpers::configuration::{
    query_namespace_business_keys, CdcPipelineConfig, Config, Pipeline, Transform,
};
use crate::pipeline_type::PipelineType;
use crate::plugins::cdc::MutationKind;
use crate::plugins::cdc::{
    derive_and_validate, sink_capabilities, source_capabilities, CompatibilityResult,
    EffectiveGuarantee, SourceOrderModel,
};
use crate::sqlrt::cdc_reconcile::{anti_join_lake, attach_cdc_sidecar, collapse_wal_batches};
use crate::sqlrt::dst::{batch_from_row, UserRow};
use std::collections::HashMap;

#[test]
fn d_copy_cannot_set_flatten_or_aggregates() {
    let mut config = Config::new();
    let mut pipeline = Pipeline::default();
    pipeline.r#type = Some(PipelineType::Copy);
    pipeline.transform = Some(Transform {
        flatten_events: Some(true),
        ..Default::default()
    });
    config.pipelines.insert("p".into(), pipeline);
    let violations = config.bind_pipeline("p").get_config_dependency_violations();
    assert!(
        violations
            .iter()
            .any(|v| v.contains("type: copy cannot set transform.flatten_events")),
        "{violations:?}"
    );
}

#[test]
fn g_copy_cannot_set_ops_aggregates() {
    let mut config = Config::new();
    let mut pipeline = Pipeline::default();
    pipeline.r#type = Some(PipelineType::Copy);
    pipeline.transform = Some(Transform {
        ops: Some(vec![crate::helpers::configuration::TransformOp {
            filter: Some("amount > 0".into()),
            ..Default::default()
        }]),
        ..Default::default()
    });
    config.pipelines.insert("p".into(), pipeline);
    let violations = config.bind_pipeline("p").get_config_dependency_violations();
    assert!(
        violations
            .iter()
            .any(|v| v.contains("type: copy cannot set transform.ops")),
        "{violations:?}"
    );
}

#[test]
fn b_mixed_namespaces_yaml_cdc_keys() {
    let pipeline: Pipeline = serde_yaml::from_str(
        r#"
type: cdc
cdc:
  default:
    business_key_columns: [id]
  namespaces:
    orders:
      business_key_columns: [order_id, line]
"#,
    )
    .unwrap();
    assert_eq!(pipeline.r#type, Some(PipelineType::Cdc));
    let cdc = pipeline.cdc.unwrap();
    let contracts = cdc.resolve_contracts(&HashMap::new());
    assert_eq!(
        contracts
            .for_namespace("orders")
            .unwrap()
            .business_key_columns,
        vec!["order_id".to_string(), "line".to_string()]
    );
    assert_eq!(
        query_namespace_business_keys(PipelineType::Cdc, Some(&cdc), &HashMap::new(), "other",),
        vec!["id".to_string()]
    );
}

#[test]
fn b_missing_cdc_keys_fail_closed() {
    let cdc = CdcPipelineConfig::default();
    let keys = query_namespace_business_keys(PipelineType::Cdc, Some(&cdc), &HashMap::new(), "ns");
    assert!(keys.is_empty());
    let err = collapse_wal_batches(&[], &keys).unwrap_err();
    assert!(
        err.to_string().contains("requires business_key_columns"),
        "{err}"
    );
    match derive_and_validate(
        &source_capabilities::POSTGRES,
        &sink_capabilities::SKIPPRLAKE,
        "ns",
        &[],
    ) {
        CompatibilityResult::Incompatible(reasons) => {
            assert!(reasons.iter().any(|r| r.contains("business_key_columns")));
        }
        other => panic!("empty keys must fail exact-once closed, got {other:?}"),
    }
}

#[test]
fn b_unsupported_order_rejected_for_exact_once() {
    assert_eq!(
        source_capabilities::SQS.order_model,
        SourceOrderModel::UnsupportedForFinalState
    );
    match derive_and_validate(
        &source_capabilities::SQS,
        &sink_capabilities::SKIPPRLAKE,
        "events",
        &["id".into()],
    ) {
        CompatibilityResult::Compatible(EffectiveGuarantee::ExactOnceFinalState) => {
            panic!("UnsupportedForFinalState must not grant ExactOnceFinalState")
        }
        CompatibilityResult::Compatible(EffectiveGuarantee::CdcEncoded)
        | CompatibilityResult::Incompatible(_) => {}
    }
}

#[test]
fn b_composite_business_keys() {
    let keys = vec!["id".to_string(), "name".to_string()];
    let a = UserRow {
        id: 1,
        name: "left".into(),
        amount: 1,
    };
    let b = UserRow {
        id: 1,
        name: "right".into(),
        amount: 2,
    };
    let batches = vec![
        attach_cdc_sidecar(
            batch_from_row(&a),
            &[MutationKind::Insert],
            &[vec![1]],
            1,
            0,
            0,
        )
        .unwrap(),
        attach_cdc_sidecar(
            batch_from_row(&b),
            &[MutationKind::Insert],
            &[vec![1]],
            2,
            0,
            0,
        )
        .unwrap(),
    ];
    let collapsed = collapse_wal_batches(&batches, &keys).unwrap();
    let lake = anti_join_lake(&[], &collapsed.changed_keys, &keys).unwrap();
    let n: usize = lake.iter().map(|batch| batch.num_rows()).sum::<usize>()
        + collapsed
            .survivor_batches
            .iter()
            .map(|batch| batch.num_rows())
            .sum::<usize>();
    assert_eq!(n, 2, "composite keys keep both (id,name) identities");
}

#[test]
fn h_unknown_pipeline_fail_closed() {
    let config = Config::new();
    let err = config.validate_pipeline("no-such-pipeline").unwrap_err();
    assert!(
        err.contains("pipeline 'no-such-pipeline' is not defined"),
        "{err}"
    );
}
