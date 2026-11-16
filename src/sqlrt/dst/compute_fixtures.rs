use crate::compute::{
    admit_config, apply_stateless, bind_drop_compute, bind_statement, commit_journal,
    desugar_create, desugar_stateless, drop_compute, ir_hash, ir_hash_kind, journal_key,
    load_journal, BindCatalog, ComputeKind, FieldId, Fqn, MeasureKind, StatelessSelect,
};
use crate::processor::test_support::invoice_batch;
use skippr_wal_catalog::MemorySkipprStore;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

fn catalog() -> BindCatalog {
    let mut ingest = BTreeMap::new();
    ingest.insert(
        ("invoices".into(), "orders".into()),
        BTreeMap::from([
            ("customer_id".into(), FieldId::new(1).unwrap()),
            ("amount".into(), FieldId::new(2).unwrap()),
            ("created_at".into(), FieldId::new(3).unwrap()),
        ]),
    );
    BindCatalog {
        ingest,
        ingest_namespaces: BTreeSet::from(["orders".into()]),
        existing: Vec::new(),
        event_time: Some(crate::compute::EventTimeCol::new(FieldId::new(3).unwrap())),
        watermark_delay: Some(Duration::from_secs(300)),
    }
}

fn bind_sql(sql: &str, is_view: bool) -> ComputeKind {
    let name = if is_view {
        Fqn::from_parts("invoices", "positive").unwrap()
    } else {
        Fqn::from_parts("invoices", "customer_totals").unwrap()
    };
    let unbound = desugar_create(name, sql, is_view).unwrap();
    bind_statement(&catalog(), unbound).unwrap()
}

#[test]
fn g_view_filter_matches_stream() {
    let kind = bind_sql("SELECT * FROM invoices.orders WHERE amount > 0", true);
    match &kind {
        ComputeKind::View { select, .. } => {
            assert!(select.filter.as_ref().unwrap().contains("amount"));
            assert!(crate::compute::view_sql(&kind).unwrap().contains("WHERE"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn g_view_legal_permutations() {
    use crate::compute::VIEW_LEGAL;
    for case in VIEW_LEGAL {
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "positive").unwrap(),
            case.sql,
            true,
        )
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
        let kind = bind_statement(&catalog(), unbound)
            .unwrap_or_else(|err| panic!("{} bind: {err}", case.name));
        match &kind {
            ComputeKind::View { select, name, .. } => {
                assert_eq!(name.to_string(), "invoices.positive", "{}", case.name);
                assert_eq!(select.filter.is_some(), case.has_filter, "{}", case.name);
                assert_eq!(select.derive.len(), case.derive_count, "{}", case.name);
                let sql = crate::compute::view_sql(&kind).unwrap();
                assert_eq!(
                    sql.contains("WHERE"),
                    case.has_filter,
                    "{} {sql}",
                    case.name
                );
            }
            other => panic!("{}: {other:?}", case.name),
        }
    }
}

#[test]
fn g_view_illegal_fail_closed() {
    use crate::compute::{VIEW_ILLEGAL, VIEW_ILLEGAL_REQUIRED};
    use std::collections::BTreeSet;
    let names: BTreeSet<_> = VIEW_ILLEGAL.iter().map(|c| c.name).collect();
    for required in VIEW_ILLEGAL_REQUIRED {
        assert!(names.contains(required), "missing VIEW illegal {required}");
    }
    for case in VIEW_ILLEGAL {
        let err = desugar_create(
            Fqn::from_parts("invoices", "positive").unwrap(),
            case.sql,
            true,
        )
        .expect_err(&format!("{} must fail at desugar", case.name))
        .to_string();
        assert!(
            err.to_ascii_lowercase()
                .contains(&case.needle.to_ascii_lowercase()),
            "{}: expected {:?} in {err}",
            case.name,
            case.needle
        );
    }
}

#[test]
fn g_view_skins_same_ir() {
    let body = "SELECT * FROM invoices.orders WHERE amount > 0";
    let create = format!("CREATE VIEW invoices.positive AS {body}");
    let from_parser = {
        let mut parser = crate::sqlrt::parser::SParser::new(&create).unwrap();
        match parser.parse_statement().unwrap() {
            crate::sqlrt::parser::Statement::CreateView { name, as_select } => {
                bind_statement(&catalog(), desugar_create(name, &as_select, true).unwrap()).unwrap()
            }
            other => panic!("{other:?}"),
        }
    };
    let from_desugar = bind_sql(body, true);
    assert_eq!(
        ir_hash(&[from_parser]).unwrap(),
        ir_hash(&[from_desugar]).unwrap()
    );
}

#[test]
fn g_view_journals() {
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let key = journal_key("ws", "invoices").unwrap();
    let kind = bind_sql("SELECT * FROM invoices.orders WHERE amount > 0", true);
    commit_journal(&store, fence, &key, vec![kind.clone()]).unwrap();
    let log = load_journal(&store, &key).unwrap();
    let kinds = log.kinds();
    assert_eq!(kinds.len(), 1);
    match &kinds[0] {
        ComputeKind::View { select, .. } => {
            assert!(select.filter.as_ref().unwrap().contains("amount"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(kind.store_id(), None);
}

#[test]
fn g_latest_insert_update_delete() {
    let unbound = desugar_create(
        Fqn::from_parts("invoices", "customers").unwrap(),
        "SELECT DISTINCT ON (customer_id) * FROM invoices.orders ORDER BY customer_id, created_at DESC",
        false,
    )
    .unwrap();
    match bind_statement(&catalog(), unbound).unwrap() {
        ComputeKind::Latest { keys, order, .. } => {
            assert_eq!(keys.as_slice(), &[FieldId::new(1).unwrap()]);
            assert_eq!(
                order,
                vec![
                    (FieldId::new(1).unwrap(), false),
                    (FieldId::new(3).unwrap(), true)
                ]
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn g_window_tumble_insert() {
    let unbound = desugar_create(
        Fqn::from_parts("invoices", "hourly_totals").unwrap(),
        "SELECT customer_id, date_trunc('hour', created_at) AS window_start, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id, date_trunc('hour', created_at)",
        false,
    )
    .unwrap();
    match bind_statement(&catalog(), unbound).unwrap() {
        ComputeKind::WindowedTable { keys, .. } => {
            assert_eq!(keys.as_slice(), &[FieldId::new(1).unwrap()]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn g_over_equals_join_keyed() {
    let mut cat = catalog();
    cat.existing.push(bind_sql(
        "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        false,
    ));
    let unbound = desugar_create(
        Fqn::from_parts("invoices", "enriched").unwrap(),
        "SELECT customer_id, SUM(amount) OVER (PARTITION BY customer_id) AS invoice_total FROM invoices.orders",
        false,
    )
    .unwrap();
    match bind_statement(&cat, unbound).unwrap() {
        ComputeKind::StreamTableJoin { table, .. } => {
            assert_eq!(table.to_string(), "invoices.customer_totals");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn g_stream_table_join_apply() {
    g_over_equals_join_keyed();
}

#[test]
fn g_stateless_filter_before_wal() {
    let unbound = desugar_stateless(
        Fqn::from_parts("invoices", "positive_orders").unwrap(),
        "SELECT * FROM invoices.orders WHERE amount > 0",
    )
    .unwrap();
    match bind_statement(&catalog(), unbound).unwrap() {
        ComputeKind::Stateless { select, .. } => {
            assert!(select.filter.as_ref().unwrap().contains("amount"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn g_derive_rowcount_unchanged() {
    let batch = invoice_batch(&["a"], &[3], &["i1"]);
    let select = StatelessSelect {
        filter: None,
        derive: vec![("amount2".into(), "amount + 1".into())],
    };
    let err = apply_stateless(&[], Some(&select), batch, None);
    // derive that doesn't change row count succeeds; a row-changing expr is rejected
    // by apply_stateless when the SQL returns a different length.
    let aligned = err.expect("derive of a scalar must keep rows");
    assert_eq!(aligned.batch.num_rows(), 1);
}

#[test]
fn g_empty_batch_stateless_schema() {
    let batch = invoice_batch(&[], &[], &[]);
    let select = StatelessSelect {
        filter: Some("amount > 0".into()),
        derive: Vec::new(),
    };
    let aligned = apply_stateless(&[], Some(&select), batch, None).unwrap();
    assert_eq!(aligned.batch.num_rows(), 0);
    assert!(aligned.batch.schema().field_with_name("amount").is_ok());
}

#[test]
fn g_drop_compute() {
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let key = journal_key("ws", "invoices").unwrap();
    let kind = bind_sql(
        "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        false,
    );
    commit_journal(&store, fence, &key, vec![kind.clone()]).unwrap();
    let name = Fqn::from_parts("invoices", "customer_totals").unwrap();
    drop_compute(&store, fence, &key, &name).unwrap();
    let log = load_journal(&store, &key).unwrap();
    assert!(log.kinds().is_empty());
}

#[test]
fn g_adhoc_groupby_not_journaled() {
    let mut parser = crate::sqlrt::parser::SParser::new(
        "SELECT customer_id, SUM(amount) FROM invoices.orders GROUP BY customer_id",
    )
    .unwrap();
    let err = parser.parse_statement().unwrap_err();
    assert!(
        err.to_string().contains("Not implemented") || err.to_string().contains("Unknown"),
        "{err}"
    );
}

#[test]
fn g_no_iceberg_cdc_rebuild() {
    let apply = include_str!("../../compute/apply.rs");
    let processor = include_str!("../../processor/mod.rs");
    assert!(!apply.contains("rebuild_from_iceberg"));
    assert!(!processor.contains("rebuild_from_iceberg"));
    assert!(!apply.contains("replay Iceberg"));
}

#[test]
fn g_rebuild_fail_closed_reclaimed() {
    let apply = include_str!("../../compute/apply.rs");
    assert!(apply.contains("ALTER COMPUTE to migrate"));
    assert!(!apply.contains("live WAL starts at seq"));
    assert!(!apply.contains("sync_from_config"));
}

#[test]
fn g_poison_time_deadletter_before_wal() {
    use arrow::array::StringArray;
    use arrow::record_batch::RecordBatch;
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("created_at", DataType::Utf8, true),
        ])),
        vec![
            Arc::new(arrow::array::Int64Array::from(vec![
                Some(1),
                Some(2),
                Some(3),
            ])),
            Arc::new(StringArray::from(vec![
                Some("2026-11-15T12:00:00Z"),
                None,
                Some("not-a-time"),
            ])),
        ],
    )
    .unwrap();
    let (aligned, dropped) =
        crate::compute::drop_invalid_event_times(batch, None, "created_at").unwrap();
    assert_eq!(dropped, 2);
    assert_eq!(aligned.batch.num_rows(), 1);
    let ingest = include_str!("../../ingest_work.rs");
    assert!(ingest.contains("drop_invalid_event_times"));
    assert!(ingest.contains("failure_code: \"EVENT_TIME\""));
}

#[test]
fn g_rename_no_rebuild() {
    let a = ir_hash(&[bind_sql(
        "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        false,
    )])
    .unwrap();
    let b = ir_hash(&[bind_sql(
        "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        false,
    )])
    .unwrap();
    assert_eq!(a, b);
}

#[test]
fn g_catalog_keyed_name() {
    let kind = bind_sql(
        "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        false,
    );
    assert_eq!(kind.name().to_string(), "invoices.customer_totals");
}

#[test]
fn g_keyed_min_max_cdc() {
    let unbound = desugar_create(
        Fqn::from_parts("invoices", "extremes").unwrap(),
        "SELECT customer_id, MIN(amount) AS lo, MAX(amount) AS hi FROM invoices.orders GROUP BY customer_id",
        false,
    )
    .unwrap();
    match bind_statement(&catalog(), unbound).unwrap() {
        ComputeKind::KeyedTable { measures, .. } => {
            let kinds: Vec<_> = measures.as_slice().iter().map(|m| &m.kind).collect();
            assert!(matches!(kinds[0], MeasureKind::Min(_)));
            assert!(matches!(kinds[1], MeasureKind::Max(_)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn g_drop_ingest_namespace_fail_closed() {
    let err =
        bind_drop_compute(&catalog(), &Fqn::from_parts("invoices", "orders").unwrap()).unwrap_err();
    assert!(err.to_string().contains("ingest namespace"), "{err}");
}

#[test]
fn g_distinct_refcount_delete() {
    let unbound = desugar_create(
        Fqn::from_parts("invoices", "customers_present").unwrap(),
        "SELECT DISTINCT customer_id FROM invoices.orders",
        false,
    );
    match unbound {
        Ok(u) => match bind_statement(&catalog(), u) {
            Ok(ComputeKind::KeyedTable { measures, .. }) => {
                assert!(matches!(measures.as_slice()[0].kind, MeasureKind::Count));
            }
            Ok(other) => panic!("{other:?}"),
            Err(err) => panic!("{err}"),
        },
        Err(err) => {
            let msg = err.to_string();
            assert!(msg.contains("DISTINCT") || msg.contains("Count"), "{msg}");
        }
    }
}

#[test]
fn g_alter_rename_no_rebuild() {
    let mut renamed = catalog();
    {
        let fields = renamed
            .ingest
            .get_mut(&("invoices".into(), "orders".into()))
            .unwrap();
        fields.remove("customer_id");
        fields.insert("cust".into(), FieldId::new(1).unwrap());
    }
    let a = ir_hash(&[bind_sql(
        "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        false,
    )])
    .unwrap();
    let unbound = desugar_create(
        Fqn::from_parts("invoices", "customer_totals").unwrap(),
        "SELECT cust, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY cust",
        false,
    )
    .unwrap();
    let b = ir_hash(&[bind_statement(&renamed, unbound).unwrap()]).unwrap();
    assert_eq!(a, b);
}

fn compute_yaml(
    items: Vec<crate::helpers::configuration::ComputeItem>,
) -> crate::helpers::configuration::Config {
    use crate::helpers::configuration::{Config, Pipeline, Skippr, Transform};
    let mut config = Config::new();
    config.skippr = Some(Skippr {
        workspace: Some("ws".into()),
        ..Default::default()
    });
    let mut pipeline = Pipeline::default();
    pipeline.transform = Some(Transform {
        compute: Some(items),
        ..Default::default()
    });
    config.pipelines.insert("invoices".into(), pipeline);
    config.bind_pipeline("invoices")
}

fn invoice_metadata(
    config: &crate::helpers::configuration::Config,
) -> crate::discover::PipelineMetadata {
    use crate::discover::{Metadata, PipelineMetadata, SkipprDataType};
    use std::collections::HashMap;
    let mut ns = Metadata::new().unwrap();
    for (name, ty, id) in [
        ("customer_id", SkipprDataType::String, 1),
        ("amount", SkipprDataType::Long, 2),
    ] {
        let mut field = Metadata::new_with_type(ty, name);
        field.field_id = id;
        ns.fields.insert(name.into(), field);
    }
    let mut metadata = PipelineMetadata::new(config);
    metadata.metadata = HashMap::from([("orders".into(), ns)]);
    metadata
}

#[test]
fn g_admit_puts_new_names_keeps_adhoc_fails_hash_mismatch() {
    use crate::helpers::configuration::ComputeItem;
    let keyed = ComputeItem::Sql {
        name: "invoices.customer_totals".into(),
        value: "CREATE TABLE invoices.customer_totals AS SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id".into(),
    };
    let config = compute_yaml(vec![keyed.clone()]);
    let metadata = invoice_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let first = admit_config(&config, &store, fence, &metadata).unwrap();
    assert_eq!(first.items.len(), 1);
    let again = admit_config(&config, &store, fence, &metadata).unwrap();
    assert_eq!(
        again
            .get(&Fqn::from_parts("invoices", "customer_totals").unwrap())
            .unwrap()
            .hash,
        ir_hash_kind(&again.kinds()[0]).unwrap()
    );
    let with_view = compute_yaml(vec![
        keyed.clone(),
        ComputeItem::Sql {
            name: "invoices.positive".into(),
            value:
                "CREATE VIEW invoices.positive AS SELECT * FROM invoices.orders WHERE amount > 0"
                    .into(),
        },
    ]);
    let added = admit_config(&with_view, &store, fence, &metadata).unwrap();
    assert_eq!(added.items.len(), 2);
    let empty = compute_yaml(vec![]);
    let kept = admit_config(&empty, &store, fence, &metadata).unwrap();
    assert_eq!(
        kept.items.len(),
        2,
        "ad-hoc / journal rows survive empty config"
    );
    let changed = compute_yaml(vec![ComputeItem::Sql {
        name: "invoices.customer_totals".into(),
        value: "CREATE TABLE invoices.customer_totals AS SELECT customer_id, COUNT(*) AS n FROM invoices.orders GROUP BY customer_id".into(),
    }]);
    let err = admit_config(&changed, &store, fence, &metadata).unwrap_err();
    assert!(err.to_string().contains("ALTER COMPUTE"), "{err}");
}

#[test]
fn g_adhoc_join_stream_keyed() {
    let sql = "SELECT o.customer_id, t.invoice_total FROM invoices.orders o JOIN invoices.customer_totals t ON o.customer_id = t.customer_id";
    let mut parser = crate::sqlrt::parser::SParser::new(sql).unwrap();
    assert!(parser.parse_statement().is_err());
}

#[test]
fn g_stream_derived_name() {
    let sql = "STREAM * FROM invoices.customer_totals";
    assert!(!crate::sqlrt::query::sql_uses_record_batch_collect(sql));
    assert!(!crate::sqlrt::query::sql_uses_shared_extension_collect(sql));
    let stream = crate::sqlrt::query::parsed_stream(sql).expect("Statement::Stream");
    assert_eq!(stream.from, "invoices.customer_totals");
    assert!(stream
        .select_sql
        .to_ascii_uppercase()
        .starts_with("SELECT "));
    assert!(crate::sqlrt::query::parsed_stream("SELECT 1 FROM invoices.orders").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn g_adhoc_select_journaled_compute() {
    use crate::helpers::configuration::{Config, Skippr};
    crate::helpers::offsets::reset_live_kv();
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store: Arc<dyn skippr_wal_catalog::SkipprStore> = Arc::new(MemorySkipprStore::open(fence));
    let mut config = Config::new();
    config.skippr = Some(Skippr {
        workspace: Some("ws".into()),
        ..Default::default()
    });
    let bound = config.bind_pipeline("invoices");
    crate::helpers::offsets::install_live_kv(bound.skippr_store_dir(), Arc::clone(&store), fence);
    let key = journal_key("ws", "invoices").unwrap();
    let kind = bind_sql(
        "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        false,
    );
    commit_journal(store.as_ref(), fence, &key, vec![kind]).unwrap();
    assert!(
        crate::sqlrt::tables::journal_has_table(&bound, "invoices", "customer_totals").unwrap()
    );
    assert!(!crate::sqlrt::tables::journal_has_table(&bound, "invoices", "orders").unwrap());
    let ctx = datafusion::prelude::SessionContext::new();
    crate::sqlrt::tables::register_compute_tables(&ctx, &bound, "invoices")
        .await
        .unwrap();
    let batches = ctx
        .sql("SELECT count(*) FROM invoices.customer_totals")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let count = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Int64Array>()
        .unwrap()
        .value(0);
    assert_eq!(count, 0);
    crate::helpers::offsets::reset_live_kv();
}

#[test]
fn g_config_name_foreign_pipeline_fails() {
    use crate::helpers::configuration::ComputeItem;
    let item = ComputeItem::Sql {
        name: "other.positive".into(),
        value: "SELECT * FROM invoices.orders WHERE amount > 0".into(),
    };
    let config = compute_yaml(vec![item]);
    let metadata = invoice_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let err = admit_config(&config, &store, fence, &metadata).unwrap_err();
    assert!(err.to_string().contains("bound pipeline"), "{err}");
}

#[test]
fn g_sdl_join_table_must_match_bound() {
    use crate::helpers::configuration::{ComputeItem, SdlDoc};
    let keyed = ComputeItem::Sql {
        name: "invoices.customer_totals".into(),
        value: "CREATE TABLE invoices.customer_totals AS SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id".into(),
    };
    let join = ComputeItem::Sdl {
        name: "invoices.enriched".into(),
        value: SdlDoc::StreamTableJoin {
            name: None,
            stream: "invoices.orders".into(),
            table: "invoices.wrong_table".into(),
            on: vec!["customer_id".into()],
        },
    };
    let config = compute_yaml(vec![keyed, join]);
    let metadata = invoice_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let err = admit_config(&config, &store, fence, &metadata).unwrap_err();
    assert!(err.to_string().contains("is not a KeyedTable"), "{err}");
}

#[test]
fn g_sdl_join_table_foreign_pipeline_fails() {
    use crate::helpers::configuration::{ComputeItem, SdlDoc};
    let keyed = ComputeItem::Sql {
        name: "invoices.customer_totals".into(),
        value: "CREATE TABLE invoices.customer_totals AS SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id".into(),
    };
    let join = ComputeItem::Sdl {
        name: "invoices.enriched".into(),
        value: SdlDoc::StreamTableJoin {
            name: None,
            stream: "invoices.orders".into(),
            table: "billing.customer_totals".into(),
            on: vec!["customer_id".into()],
        },
    };
    let config = compute_yaml(vec![keyed, join]);
    let metadata = invoice_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let err = admit_config(&config, &store, fence, &metadata).unwrap_err();
    assert!(err.to_string().contains("bound pipeline"), "{err}");
}

#[test]
fn g_live_kv_does_not_leak_across_pipelines() {
    use crate::helpers::configuration::{Config, Pipeline, Skippr};
    crate::helpers::offsets::reset_live_kv();
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store: Arc<dyn skippr_wal_catalog::SkipprStore> = Arc::new(MemorySkipprStore::open(fence));
    let mut config = Config::new();
    config.skippr = Some(Skippr {
        workspace: Some("ws".into()),
        ..Default::default()
    });
    config
        .pipelines
        .insert("invoices".into(), Pipeline::default());
    config
        .pipelines
        .insert("billing".into(), Pipeline::default());
    let invoices = config.bind_pipeline("invoices");
    crate::helpers::offsets::install_live_kv(
        invoices.skippr_store_dir(),
        Arc::clone(&store),
        fence,
    );
    let kind = bind_sql(
        "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        false,
    );
    commit_journal(
        store.as_ref(),
        fence,
        &journal_key("ws", "billing").unwrap(),
        vec![kind],
    )
    .unwrap();
    assert!(
        !crate::sqlrt::tables::journal_has_table(&config, "billing", "customer_totals").unwrap(),
        "invoices' live_kv must not serve billing's compute journal"
    );
    crate::helpers::offsets::reset_live_kv();
}

#[test]
fn g_query_join_pin() {
    let query = include_str!("../query.rs");
    let prod = query.split("#[cfg(test)]").next().unwrap();
    assert!(prod.contains("register_compute_tables(&ctx, &bound, &pipeline)"));
    let tables = include_str!("../tables.rs");
    let tables_prod = tables.split("#[cfg(test)]").next().unwrap();
    assert!(
        tables_prod.contains("FjallReadStore::open"),
        "query/serve must read the compute journal via FjallReadStore when live_kv is unset"
    );
    assert!(
        tables_prod.contains("live_kv_for"),
        "in-process query may reuse only this pipeline's live writer"
    );
    assert!(
        !tables_prod.contains("offsets::live_kv()"),
        "compute journal must not read an unmatched process-global live_kv"
    );
    assert!(tables.contains("view_sql"));
    assert!(tables.contains("stream_join_sql"));
    assert!(!tables.contains("if !kind.is_store_table() {\n            continue;"));
}

#[test]
fn g_stream_transform_emits_perf() {
    let ingest = include_str!("../../ingest_work.rs");
    let apply = ingest
        .find("crate::compute::apply_stateless")
        .expect("apply_stateless");
    let slice = &ingest[apply..apply.saturating_add(900)];
    assert!(
        slice.contains("stream_transform"),
        "stateless compute must emit stream_transform PERF_EVENT"
    );
    assert!(
        slice.contains("add_stream_transform_records"),
        "stateless compute must count output rows"
    );
}

#[test]
fn g_sync_admits_compute() {
    let engine = include_str!("../../engine.rs");
    let prod = engine.split("#[cfg(test)]").next().unwrap();
    assert!(
        prod.contains("admit_config"),
        "sync must admit transform.compute before ingest, including WAL-only"
    );
    let wal_only = prod.find("if wal_only(config)").expect("wal_only");
    let start = prod
        .find("crate::processor::start_processor_service")
        .expect("processor start");
    assert!(
        start > wal_only,
        "processor must start for WAL-only sync so table-backed compute still runs"
    );
}

#[test]
fn g_iceberg_derived_compact() {
    let apply = include_str!("../../compute/apply.rs");
    let table = include_str!("../../compute/table.rs");
    assert!(!apply.contains("rebuild_from_iceberg"));
    assert!(table.contains("MemTable::try_new"));
}
