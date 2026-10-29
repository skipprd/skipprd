use crate::buffer::compaction_progress::GroupedCompactionPhase;
use crate::buffer::wal_catalog_import::gc_orphan_payloads;
use crate::buffer::wal_handle::{catalog_fence_from_guard, memory_catalog_for_test};
use crate::helpers::offset_store::SledOffsetStore;
use crate::helpers::offsets::{
    OffsetKey, OffsetTypes, RuntimeOffsetOperation, RuntimeOffsetRpcRequest,
};
use crate::ingest::exact_arrow::is_exact_arrow_eligible;
use crate::runtime_plugins::protocol::{
    HostFrame, PluginFrame, RuntimeExecutionContext, RuntimeExecutionMode,
    RuntimeSchemaRefreshRequest, RuntimeSourceConfig, SchemaDelta, SourceStartRequest,
};
use crate::runtime_plugins::wire::decode_payload;
use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SystemClock};
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn a_orphan_gc() {
    let dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "orphan").unwrap();
    let paths = PipelinePaths::legacy_disk(dir.path());
    let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
    let fence = catalog_fence_from_guard(&guard).unwrap();
    let catalog = memory_catalog_for_test(fence);
    let db = sled::open(dir.path().join("offsets")).unwrap();
    let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
        Arc::new(SledOffsetStore::new(db.open_tree("o").unwrap()));
    let handle =
        crate::buffer::wal_handle::PipelineWalHandle::new(key, paths, guard, catalog, offsets);
    std::fs::create_dir_all(handle.paths().segs.as_path()).unwrap();
    let tmp = handle.paths().segs.join("crash.seg.tmp");
    std::fs::write(&tmp, b"partial").unwrap();
    gc_orphan_payloads(&handle, &crate::helpers::configuration::Config::new()).unwrap();
    assert!(!tmp.exists(), "orphan .seg.tmp must be collected");
}

#[test]
fn a_quarantine_dir_without_handle() {
    let dir = tempfile::tempdir().unwrap();
    let key = PipelineKey::new("t", "w", "quarantine").unwrap();
    let paths = PipelinePaths::legacy_disk(dir.path());
    let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
    let fence = catalog_fence_from_guard(&guard).unwrap();
    let catalog = memory_catalog_for_test(fence);
    let db = sled::open(dir.path().join("offsets")).unwrap();
    let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
        Arc::new(SledOffsetStore::new(db.open_tree("o").unwrap()));
    let handle =
        crate::buffer::wal_handle::PipelineWalHandle::new(key, paths, guard, catalog, offsets);
    let qdir = handle.paths().root.join("segment_buffer/quarantine");
    std::fs::create_dir_all(&qdir).unwrap();
    assert!(qdir.ends_with("segment_buffer/quarantine"));
    assert_eq!(
        crate::buffer::wal_store::ingest_quarantine_dir(
            &crate::helpers::configuration::Config::new()
        ),
        None,
        "without a process ingest handle the quarantine dir is absent"
    );
}

fn exec_ctx() -> RuntimeExecutionContext {
    RuntimeExecutionContext {
        pipeline_name: "p".into(),
        workspace_name: "w".into(),
        data_dir: "/tmp".into(),
        execution_mode: RuntimeExecutionMode::Sync,
        output_layout: Default::default(),
        inject_fields: Default::default(),
        pipeline_type: crate::pipeline_type::PipelineType::Ingest,
    }
}

#[test]
fn d_host_frame_run_source_roundtrip() {
    let frame = HostFrame::RunSource(SourceStartRequest {
        context: exec_ctx(),
        config: RuntimeSourceConfig(
            crate::runtime_plugins::protocol::RuntimePluginConfigEnvelope {
                plugin_name: "file".into(),
                raw_config_json: "{}".into(),
            },
        ),
        once: true,
        source_ingest_window: Default::default(),
    });
    let bytes = bincode::serialize(&frame).unwrap();
    let decoded: HostFrame = decode_payload(&bytes).unwrap();
    let HostFrame::RunSource(got) = decoded else {
        panic!("expected RunSource");
    };
    assert!(got.once);
    assert_eq!(got.context.pipeline_name, "p");
}

#[test]
fn d_install_schema_delta() {
    let delta = SchemaDelta {
        version: 3,
        namespaces: Default::default(),
    };
    let bytes = bincode::serialize(&HostFrame::InstallSchemaDelta(delta.clone())).unwrap();
    let decoded: HostFrame = decode_payload(&bytes).unwrap();
    let HostFrame::InstallSchemaDelta(got) = decoded else {
        panic!("expected InstallSchemaDelta");
    };
    assert_eq!(got, delta);
}

#[test]
fn d_schema_state_refresh_required() {
    let refresh = RuntimeSchemaRefreshRequest {
        request_id: 9,
        namespace: "ns".into(),
        required_version: 4,
        installed_version: 2,
    };
    let bytes =
        bincode::serialize(&PluginFrame::SchemaStateRefreshRequired(refresh.clone())).unwrap();
    let decoded: PluginFrame = decode_payload(&bytes).unwrap();
    let PluginFrame::SchemaStateRefreshRequired(got) = decoded else {
        panic!("expected SchemaStateRefreshRequired");
    };
    assert_eq!(got, refresh);
    assert_eq!(got.request_id, 9);
}

#[test]
fn d_offset_request_rpc() {
    let req = RuntimeOffsetRpcRequest {
        request_id: 11,
        operation: RuntimeOffsetOperation::Validate {
            key: OffsetKey::new("ns", "p"),
            offset_type: OffsetTypes::Position,
            offset_value: 42,
        },
    };
    let bytes = bincode::serialize(&PluginFrame::OffsetRequest(req.clone())).unwrap();
    let decoded: PluginFrame = decode_payload(&bytes).unwrap();
    let PluginFrame::OffsetRequest(got) = decoded else {
        panic!("expected OffsetRequest");
    };
    assert_eq!(got, req);
}

#[test]
fn d_grouped_compaction_phase() {
    use crate::buffer::compaction_progress::{
        format_in_flight_grouped_compactions, sink_work_in_flight_count, GroupedCompactionTracker,
    };
    use std::time::Duration;
    let tracker = GroupedCompactionTracker::begin(
        "dst-grouped-phase",
        "ns",
        "sink.a",
        2,
        Duration::from_secs(900),
        1,
    );
    let building = format_in_flight_grouped_compactions();
    assert!(building.contains("building_wal_stream"), "{building}");
    tracker.set_phase(GroupedCompactionPhase::UploadingToSink);
    assert!(sink_work_in_flight_count() >= 1);
    let uploading = format_in_flight_grouped_compactions();
    assert!(uploading.contains("uploading_to_sink"), "{uploading}");
    tracker.finish();
}

#[test]
fn e_exact_arrow_eligible() {
    let mut fields = HashMap::new();
    fields.insert(
        "id".into(),
        crate::discover::Metadata::new_with_type(crate::discover::SkipprDataType::Integer, "id"),
    );
    assert!(is_exact_arrow_eligible(&fields, false));
}

#[test]
fn e_exact_arrow_flatten_fallback() {
    let mut fields = HashMap::new();
    fields.insert(
        "id".into(),
        crate::discover::Metadata::new_with_type(crate::discover::SkipprDataType::Integer, "id"),
    );
    assert!(!is_exact_arrow_eligible(&fields, true));
}

#[test]
fn e_exact_arrow_nested_ineligible() {
    let mut nested = crate::discover::Metadata::new_with_type(
        crate::discover::SkipprDataType::Record,
        "address",
    );
    nested.fields.insert(
        "city".into(),
        crate::discover::Metadata::new_with_type(crate::discover::SkipprDataType::String, "city"),
    );
    let mut fields = HashMap::new();
    fields.insert("address".into(), nested);
    assert!(!is_exact_arrow_eligible(&fields, false));
}

fn leased(
    dir: &std::path::Path,
    pipeline: &str,
) -> (
    std::sync::Arc<crate::buffer::wal_handle::PipelineWalHandle>,
    skippr_wal_catalog::CatalogFence,
) {
    let key = PipelineKey::new("t", "w", pipeline).unwrap();
    let paths = PipelinePaths::legacy_disk(dir);
    let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
    let fence = catalog_fence_from_guard(&guard).unwrap();
    let catalog = memory_catalog_for_test(fence);
    let db = sled::open(dir.join("offsets")).unwrap();
    let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
        Arc::new(SledOffsetStore::new(db.open_tree("o").unwrap()));
    let handle =
        crate::buffer::wal_handle::PipelineWalHandle::new(key, paths, guard, catalog, offsets);
    (handle, fence)
}

#[test]
fn a_payload_unreadable() {
    use skippr_wal_catalog::{CatalogKey, CatalogRow, CatalogTxn, PutCond};
    let dir = tempfile::tempdir().unwrap();
    let (handle, fence) = leased(dir.path(), "payload-unread");
    std::fs::create_dir_all(handle.paths().segs.as_path()).unwrap();
    let path = handle.paths().segs.join("gone.seg");
    std::fs::write(&path, b"not-segf").unwrap();
    let row = skippr_wal_catalog::SegmentRow {
        segment_id: skippr_lease::SegmentId::new("gone").unwrap(),
        seq: skippr_wal_catalog::CatalogSeq::new(1),
        location: skippr_wal_catalog::PayloadLocation::Disk {
            path: path.to_string_lossy().into_owned(),
        },
        payload_len: 1,
        payload_sha256: [9; 32],
        slice_count: 1,
        done: skippr_wal_catalog::SliceBitmap::new(1),
        offsets: vec![],
        checkpoints: vec![],
        epoch: fence.epoch,
        version: 0,
    };
    let key = CatalogKey::Segment(row.segment_id.clone());
    let mut txn = CatalogTxn::new();
    txn.put(key, CatalogRow::Segment(row), PutCond::NotExists)
        .unwrap();
    handle.catalog().commit(&txn, fence).unwrap();
    assert!(handle.payload_unreadable("gone"));
    assert!(handle.payload_unreadable("missing"));
}

#[test]
fn c_complete_slices_and_reclaim() {
    use skippr_wal_catalog::{CatalogKey, CatalogRow, CatalogTxn, PutCond};
    crate::processor::set_min_unconsumed_seq(u64::MAX);
    let dir = tempfile::tempdir().unwrap();
    let (handle, fence) = leased(dir.path(), "complete-reclaim");
    let id = skippr_lease::SegmentId::new("done-seg").unwrap();
    let row = skippr_wal_catalog::SegmentRow {
        segment_id: id.clone(),
        seq: skippr_wal_catalog::CatalogSeq::new(1),
        location: skippr_wal_catalog::PayloadLocation::Disk {
            path: "/tmp/done-seg.seg".into(),
        },
        payload_len: 1,
        payload_sha256: [3; 32],
        slice_count: 1,
        done: skippr_wal_catalog::SliceBitmap::new(1),
        offsets: vec![],
        checkpoints: vec![],
        epoch: fence.epoch,
        version: 0,
    };
    let mut txn = CatalogTxn::new();
    txn.put(
        CatalogKey::Segment(id.clone()),
        CatalogRow::Segment(row),
        PutCond::NotExists,
    )
    .unwrap();
    handle.catalog().commit(&txn, fence).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reclaimed = rt
        .block_on(handle.complete_slices_and_reclaim("done-seg", &[0]))
        .unwrap();
    assert!(
        reclaimed,
        "all-done segment below processor cursor reclaims"
    );
    assert!(handle
        .catalog()
        .get(&CatalogKey::Segment(id))
        .unwrap()
        .is_none());
}
