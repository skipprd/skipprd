use crate::compute::{
    admit_config, apply_py_records, apply_query_statement, commit_log, drop_copy_py, ir_hash_kind,
    journal_key, load_journal, ComputeAlteration, ComputeApplyError, ComputeKind, ComputeLog, Fqn,
    Ident, PipelineName, Relation, StatelessSelect,
};
use crate::helpers::configuration::{ComputeItem, Config, Pipeline, Skippr, Transform};
use crate::pipeline_type::PipelineType;
use crate::sqlrt::parser::Statement;
use skippr_wal_catalog::MemorySkipprStore;
use std::path::Path;

const ENRICH_PY: &str = include_str!("../../../tests/perf/py_site_lookup/enrich.py");
const LOOKUP_PY: &str = include_str!("../../../tests/perf/py_site_lookup/lookup.py");

fn py_bound(dir: &Path, items: Vec<ComputeItem>) -> Config {
    let mut config = Config::new();
    config.skippr = Some(Skippr {
        workspace: Some("ws".into()),
        ..Default::default()
    });
    config.config_dir = Some(dir.to_path_buf());
    let mut pipeline = Pipeline::default();
    pipeline.transform = Some(Transform {
        compute: Some(items),
        ..Default::default()
    });
    config.pipelines.insert("p".into(), pipeline);
    config.bind_pipeline("p")
}

fn py_item(path: &str, handler: &str) -> ComputeItem {
    ComputeItem::Py {
        name: "enrich".into(),
        path: path.into(),
        handler: handler.into(),
    }
}

fn copy_bound(dir: &Path, items: Vec<ComputeItem>) -> Config {
    let mut config = py_bound(dir, items);
    config.pipelines.get_mut("p").expect("p").r#type = Some(PipelineType::Copy);
    config
}

fn pipeline_table(namespace: &str) -> Relation {
    Relation::PipelineTable {
        pipeline: PipelineName::new("p").unwrap(),
        namespace: Ident::new(namespace).unwrap(),
    }
}

fn assert_py_howto(msg: &str) {
    assert!(msg.contains("DROP COMPUTE"), "{msg}");
    assert!(msg.contains("skipprd query"), "{msg}");
    assert!(msg.contains("skipprd sync"), "{msg}");
    assert!(msg.contains("destination"), "{msg}");
    assert!(!msg.contains("DROP and CREATE"), "{msg}");
    assert!(!msg.contains("ALTER COMPUTE to migrate"), "{msg}");
    assert!(!msg.contains("bucket"), "{msg}");
    assert!(!msg.contains("admits"), "{msg}");
}

fn empty_metadata(config: &Config) -> crate::discover::PipelineMetadata {
    crate::discover::PipelineMetadata::new(config)
}

#[test]
fn g_py_value_unknown_field() {
    let err = serde_yaml::from_str::<ComputeItem>(
        "type: py\nname: enrich\nvalue: \"def transform(record): return record\"\n",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown field"), "{err}");
}

#[test]
fn g_py_path_handler_admits() {
    let dir = crate::compute::write_py_dir(&[(
        "stamp.py",
        "def transform(record):\n    record[\"mapped\"] = True\n    return record\n",
    )]);
    let config = py_bound(dir.path(), vec![py_item("stamp.py", "transform")]);
    let metadata = empty_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let log = admit_config(&config, &store, fence, &metadata).unwrap();
    match &log.kinds()[0] {
        ComputeKind::Py {
            name,
            path,
            handler,
            digest,
        } => {
            assert_eq!(name, &Fqn::from_parts("p", "enrich").unwrap());
            assert_eq!(path, "stamp.py");
            assert_eq!(handler, "transform");
            assert!(!digest.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn g_py_missing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let config = py_bound(dir.path(), vec![py_item("missing.py", "transform")]);
    let metadata = empty_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let err = admit_config(&config, &store, fence, &metadata).unwrap_err();
    assert!(err.to_string().contains("python compute path"), "{err}");
}

#[test]
fn g_py_sibling_digest_changes() {
    let dir = crate::compute::write_py_dir(&[
        ("lookup.py", "SITE = \"north\"\n"),
        ("enrich.py", "def transform(record):\n    return record\n"),
    ]);
    let config = py_bound(dir.path(), vec![py_item("enrich.py", "transform")]);
    let metadata = empty_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let first = admit_config(&config, &store, fence, &metadata).unwrap();
    let first_hash = ir_hash_kind(&first.kinds()[0]).unwrap();
    std::fs::write(dir.path().join("lookup.py"), "SITE = \"south\"\n").unwrap();
    let err = admit_config(&config, &store, fence, &metadata).unwrap_err();
    assert_py_howto(&err.to_string());
    let _ = first_hash;
}

#[test]
fn g_py_runtime_digest_must_match_journal() {
    let dir = crate::compute::write_py_dir(&[
        ("lookup.py", "SITE = \"north\"\n"),
        ("enrich.py", "def transform(record):\n    return record\n"),
    ]);
    let config = py_bound(dir.path(), vec![py_item("enrich.py", "transform")]);
    let metadata = empty_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let log = admit_config(&config, &store, fence, &metadata).unwrap();
    let loads = crate::compute::py_loads(&config).unwrap();
    crate::compute::assert_py_matches_journal("p", &loads, &log).unwrap();
    std::fs::write(dir.path().join("lookup.py"), "SITE = \"south\"\n").unwrap();
    let after = crate::compute::py_loads(&config).unwrap();
    let err = crate::compute::assert_py_matches_journal("p", &after, &log).unwrap_err();
    assert_py_howto(&err);
}

#[test]
fn g_py_path_retarget_must_match_journal() {
    let body = "def transform(record):\n    return record\n";
    let dir = crate::compute::write_py_dir(&[("a.py", body), ("b.py", body)]);
    let config = py_bound(dir.path(), vec![py_item("a.py", "transform")]);
    let metadata = empty_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let log = admit_config(&config, &store, fence, &metadata).unwrap();
    let retarget = py_bound(dir.path(), vec![py_item("b.py", "transform")]);
    let loads = crate::compute::py_loads(&retarget).unwrap();
    let err = crate::compute::assert_py_matches_journal("p", &loads, &log).unwrap_err();
    assert_py_howto(&err);
}

#[test]
fn g_py_handler_rename_changes_digest() {
    let dir = crate::compute::write_py_dir(&[(
        "enrich.py",
        "def transform(record):\n    return record\n\ndef enrich_event(record):\n    return record\n",
    )]);
    let config = py_bound(dir.path(), vec![py_item("enrich.py", "transform")]);
    let metadata = empty_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    admit_config(&config, &store, fence, &metadata).unwrap();
    let renamed = py_bound(dir.path(), vec![py_item("enrich.py", "enrich_event")]);
    let err = admit_config(&renamed, &store, fence, &metadata).unwrap_err();
    assert_py_howto(&err.to_string());
}

#[test]
fn g_py_alter_compute_is_drop_create() {
    let apply = include_str!("../../compute/apply.rs");
    let gate = apply
        .split("async fn apply_alter_compute")
        .nth(1)
        .unwrap()
        .split("fn apply_alterations")
        .next()
        .unwrap();
    assert!(
        gate.contains("ComputeKind::Py"),
        "ALTER COMPUTE on Py must fail before the Stateless SET FILTER gate"
    );
    assert!(
        !gate.contains("is_table_backed()"),
        "Py must not share the Stateless SET FILTER gate via is_table_backed"
    );
    let py_reject = apply
        .split("fn py_alter_rejected")
        .nth(1)
        .unwrap()
        .split("async fn apply_alter_compute")
        .next()
        .unwrap();
    assert!(py_reject.contains("py_drop_howto"), "{py_reject}");
    assert!(!py_reject.contains("DROP and CREATE"), "{py_reject}");
}

#[test]
fn g_py_changed_message_tells_you_what_to_run() {
    let name = Fqn::from_parts("p", "enrich").unwrap();
    let msg = crate::compute::py_changed_message(&name);
    assert!(msg.contains("DROP COMPUTE p.enrich"), "{msg}");
    assert!(msg.contains("path, handler, or a sibling .py"), "{msg}");
    assert_py_howto(&msg);
}

#[test]
fn g_py_unflattened_arrow_blocks() {
    assert!(crate::compute::blocks_unflattened_arrow(true, false));
    assert!(!crate::compute::blocks_unflattened_arrow(true, true));
    assert_eq!(
        crate::compute::PY_UNFLATTENED_ARROW,
        "python compute requires JSON objects; Arrow ingest without flatten_events cannot run type: py"
    );
}

#[test]
fn g_copy_allows_py_compute() {
    let dir = crate::compute::write_py_dir(&[(
        "enrich.py",
        "def transform(record):\n    return record\n",
    )]);
    let config = copy_bound(dir.path(), vec![py_item("enrich.py", "transform")]);
    let violations = config.get_config_dependency_violations();
    assert!(
        violations
            .iter()
            .all(|v| !v.contains("transform.compute") && !v.contains("python compute")),
        "{violations:?}"
    );
}

#[test]
fn g_copy_yaml_sql_fails_violations() {
    let dir = tempfile::tempdir().unwrap();
    let config = copy_bound(
        dir.path(),
        vec![ComputeItem::Sql {
            name: "p.t".into(),
            value: "CREATE TABLE p.t AS SELECT 1".into(),
        }],
    );
    let violations = config.get_config_dependency_violations();
    assert!(
        violations
            .iter()
            .any(|v| v.contains("type: copy cannot set transform.compute sql or sdl")),
        "{violations:?}"
    );
}

#[tokio::test]
async fn g_copy_py_drop_then_readmit() {
    let dir = crate::compute::write_py_dir(&[
        ("lookup.py", "SITE = \"north\"\n"),
        ("enrich.py", "def transform(record):\n    return record\n"),
    ]);
    let config = copy_bound(dir.path(), vec![py_item("enrich.py", "transform")]);
    let metadata = empty_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let first = admit_config(&config, &store, fence, &metadata).unwrap();
    let name = Fqn::from_parts("p", "enrich").unwrap();
    let first_digest = match &first.get(&name).unwrap().kind {
        ComputeKind::Py { digest, .. } => digest.clone(),
        other => panic!("{other:?}"),
    };
    std::fs::write(dir.path().join("lookup.py"), "SITE = \"south\"\n").unwrap();
    let mismatch = admit_config(&config, &store, fence, &metadata).unwrap_err();
    assert_py_howto(&mismatch.to_string());
    drop_copy_py(&config, &store, fence, &metadata, &name)
        .await
        .unwrap();
    let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name()).unwrap();
    let after_drop = load_journal(&store, &key).unwrap();
    assert!(after_drop.get(&name).is_none(), "{after_drop:?}");
    let next = admit_config(&config, &store, fence, &metadata).unwrap();
    match &next.get(&name).unwrap().kind {
        ComputeKind::Py { digest, .. } => assert_ne!(digest, &first_digest),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn g_copy_create_alter_rejected() {
    let dir = crate::compute::write_py_dir(&[(
        "enrich.py",
        "def transform(record):\n    return record\n",
    )]);
    let config = copy_bound(dir.path(), vec![py_item("enrich.py", "transform")]);
    let name = Fqn::from_parts("p", "enrich").unwrap();
    let create_view = apply_query_statement(
        &config,
        &Statement::CreateView {
            name: name.clone(),
            as_select: "SELECT 1".into(),
        },
    )
    .await
    .unwrap_err();
    assert_copy_create_alter(&create_view, "p.enrich");
    let create_table = apply_query_statement(
        &config,
        &Statement::CreateTableAs {
            name: name.clone(),
            as_select: "SELECT 1".into(),
        },
    )
    .await
    .unwrap_err();
    assert_copy_create_alter(&create_table, "p.enrich");
    let alter = apply_query_statement(
        &config,
        &Statement::AlterCompute {
            name,
            alterations: vec![ComputeAlteration::SetFilter {
                expr: "true".into(),
            }],
        },
    )
    .await
    .unwrap_err();
    assert_copy_create_alter(&alter, "p.enrich");
}

fn assert_copy_create_alter(err: &ComputeApplyError, name: &str) {
    let msg = err.to_string();
    assert!(msg.contains("does not CREATE or ALTER"), "{msg}");
    assert!(msg.contains(name), "{msg}");
    assert!(msg.contains("SQL and SDL are not allowed on copy"), "{msg}");
    assert_py_howto(&msg);
}

#[tokio::test]
async fn g_copy_drop_sql_sdl_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = copy_bound(dir.path(), Vec::new());
    let metadata = empty_metadata(&config);
    let fence = skippr_wal_catalog::CatalogFence::local();
    let store = MemorySkipprStore::open(fence);
    let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name()).unwrap();
    let mut log = ComputeLog::default();
    log.insert(ComputeKind::Stateless {
        name: Fqn::from_parts("p", "filter").unwrap(),
        target: pipeline_table("orders"),
        select: StatelessSelect {
            filter: Some("true".into()),
            derive: Vec::new(),
        },
    })
    .unwrap();
    log.insert(ComputeKind::View {
        name: Fqn::from_parts("p", "totals").unwrap(),
        input: pipeline_table("orders"),
        select: StatelessSelect {
            filter: None,
            derive: Vec::new(),
        },
    })
    .unwrap();
    commit_log(&store, fence, &key, &log).unwrap();
    let sql_err = drop_copy_py(
        &config,
        &store,
        fence,
        &metadata,
        &Fqn::from_parts("p", "filter").unwrap(),
    )
    .await
    .unwrap_err();
    let sql_msg = sql_err.to_string();
    assert!(sql_msg.contains("p.filter"), "{sql_msg}");
    assert!(sql_msg.contains("is not type: py"), "{sql_msg}");
    assert!(
        sql_msg.contains("SQL and SDL are not allowed on copy"),
        "{sql_msg}"
    );
    let sdl_err = drop_copy_py(
        &config,
        &store,
        fence,
        &metadata,
        &Fqn::from_parts("p", "totals").unwrap(),
    )
    .await
    .unwrap_err();
    let sdl_msg = sdl_err.to_string();
    assert!(sdl_msg.contains("p.totals"), "{sdl_msg}");
    assert!(sdl_msg.contains("is not type: py"), "{sdl_msg}");
    let missing = drop_copy_py(
        &config,
        &store,
        fence,
        &metadata,
        &Fqn::from_parts("p", "gone").unwrap(),
    )
    .await
    .unwrap_err();
    let missing_msg = missing.to_string();
    assert!(missing_msg.contains("p.gone"), "{missing_msg}");
    assert!(
        missing_msg.contains("is not in the journal"),
        "{missing_msg}"
    );
}

#[test]
fn j_py_site_lookup_maps_site() {
    let dir = crate::compute::write_py_dir(&[("lookup.py", LOOKUP_PY), ("enrich.py", ENRICH_PY)]);
    let load = crate::compute::admit_item(dir.path(), "enrich.py", "transform").unwrap();
    let out = apply_py_records(
        &[load],
        vec![
            serde_json::json!({"device_id": "dev-1", "event_time": 0}),
            serde_json::json!({"device_id": "dev-1", "event_time": 100}),
            serde_json::json!({"truck": {"id": "asset-9"}}),
            serde_json::json!({"id": "none"}),
            serde_json::json!({"device_id": "dev-1", "detail": {"site": "kept"}}),
            serde_json::json!({"truck": {"site": "west"}}),
        ],
    )
    .unwrap();
    assert_eq!(out[0].as_ref().unwrap()["detail"]["site"], "north");
    assert_eq!(out[1].as_ref().unwrap()["detail"]["site"], "south");
    assert_eq!(out[2].as_ref().unwrap()["detail"]["site"], "east");
    assert!(out[3]
        .as_ref()
        .unwrap()
        .get("detail")
        .and_then(|d| d.get("site"))
        .is_none());
    assert_eq!(out[4].as_ref().unwrap()["detail"]["site"], "kept");
    assert_eq!(out[5].as_ref().unwrap()["detail"]["site"], "west");
}
