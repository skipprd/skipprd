use crate::compute::{admit_config, apply_py_records, ir_hash_kind, ComputeKind, Fqn};
use crate::helpers::configuration::{ComputeItem, Config, Pipeline, Skippr, Transform};
use crate::pipeline_type::PipelineType;
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
    assert!(err.to_string().contains("DROP and CREATE"), "{err}");
    assert!(!err.to_string().contains("ALTER COMPUTE"), "{err}");
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
    assert!(err.contains("DROP and CREATE"), "{err}");
    assert!(!err.contains("ALTER COMPUTE"), "{err}");
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
    assert!(err.contains("DROP and CREATE"), "{err}");
    assert!(!err.contains("ALTER COMPUTE"), "{err}");
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
    assert!(err.to_string().contains("DROP and CREATE"), "{err}");
    assert!(!err.to_string().contains("ALTER COMPUTE"), "{err}");
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
    assert!(apply.contains("Py {name} is DROP and CREATE, not ALTER COMPUTE"));
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
    let mut config = py_bound(dir.path(), vec![py_item("enrich.py", "transform")]);
    config.pipelines.get_mut("p").expect("p").r#type = Some(PipelineType::Copy);
    let violations = config.get_config_dependency_violations();
    assert!(
        violations
            .iter()
            .all(|v| !v.contains("transform.compute") && !v.contains("python compute")),
        "{violations:?}"
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
