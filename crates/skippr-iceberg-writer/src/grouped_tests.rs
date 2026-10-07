//! Grouped writes end to end against a filesystem catalog.

use super::*;
use arrow::array::Int64Array;
use arrow::datatypes::{DataType, Field};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use iceberg::io::FileIO;
use serde_json::json;
use skippr_iceberg_catalog_fs::FsCatalog;
use skippr_runtime_sdk::plugins::{
    GroupedBatchReader, GroupedBatchReaderConfig, GroupedSinkWriteContext, LiveWalSegments,
    SinkCallResult,
};
use skippr_runtime_sdk::protocol::RuntimeWalPartRef;

skippr_runtime_sdk::declare_sink_spec!(
    GroupedTestSpec,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::SKIPPRLAKE,
    skippr_runtime_sdk::plugins::TransactionalTableCommit
);

pub(crate) const NAMESPACE: &str = "orders";

/// Counts trait-level `load_table` calls; the catalog's own internal reads
/// inside `update_table` are not routed through it.
#[derive(Debug)]
pub(crate) struct CountingCatalog {
    inner: FsCatalog,
    loads: std::sync::atomic::AtomicUsize,
}

impl CountingCatalog {
    pub fn loads(&self) -> usize {
        self.loads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Catalog for CountingCatalog {
    async fn list_namespaces(
        &self,
        parent: Option<&NamespaceIdent>,
    ) -> iceberg::Result<Vec<NamespaceIdent>> {
        self.inner.list_namespaces(parent).await
    }
    async fn create_namespace(
        &self,
        namespace: &NamespaceIdent,
        properties: HashMap<String, String>,
    ) -> iceberg::Result<iceberg::Namespace> {
        self.inner.create_namespace(namespace, properties).await
    }
    async fn get_namespace(
        &self,
        namespace: &NamespaceIdent,
    ) -> iceberg::Result<iceberg::Namespace> {
        self.inner.get_namespace(namespace).await
    }
    async fn namespace_exists(&self, namespace: &NamespaceIdent) -> iceberg::Result<bool> {
        self.inner.namespace_exists(namespace).await
    }
    async fn update_namespace(
        &self,
        namespace: &NamespaceIdent,
        properties: HashMap<String, String>,
    ) -> iceberg::Result<()> {
        self.inner.update_namespace(namespace, properties).await
    }
    async fn drop_namespace(&self, namespace: &NamespaceIdent) -> iceberg::Result<()> {
        self.inner.drop_namespace(namespace).await
    }
    async fn list_tables(&self, namespace: &NamespaceIdent) -> iceberg::Result<Vec<TableIdent>> {
        self.inner.list_tables(namespace).await
    }
    async fn create_table(
        &self,
        namespace: &NamespaceIdent,
        creation: TableCreation,
    ) -> iceberg::Result<iceberg::table::Table> {
        self.inner.create_table(namespace, creation).await
    }
    async fn load_table(&self, table: &TableIdent) -> iceberg::Result<iceberg::table::Table> {
        self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.load_table(table).await
    }
    async fn drop_table(&self, table: &TableIdent) -> iceberg::Result<()> {
        self.inner.drop_table(table).await
    }
    async fn table_exists(&self, table: &TableIdent) -> iceberg::Result<bool> {
        self.inner.table_exists(table).await
    }
    async fn rename_table(&self, src: &TableIdent, dest: &TableIdent) -> iceberg::Result<()> {
        self.inner.rename_table(src, dest).await
    }
    async fn register_table(
        &self,
        table: &TableIdent,
        metadata_location: String,
    ) -> iceberg::Result<iceberg::table::Table> {
        self.inner.register_table(table, metadata_location).await
    }
    async fn update_table(
        &self,
        commit: iceberg::TableCommit,
    ) -> iceberg::Result<iceberg::table::Table> {
        self.inner.update_table(commit).await
    }
}

pub(crate) struct Harness {
    pub dir: tempfile::TempDir,
    pub counting: Arc<CountingCatalog>,
    pub catalog: Arc<dyn Catalog>,
    pub writer: IcebergWriter<GroupedTestSpec>,
}

impl Harness {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let counting = Arc::new(CountingCatalog {
            inner: FsCatalog::new(Self::warehouse_of(&dir), FileIO::new_with_fs()).unwrap(),
            loads: Default::default(),
        });
        let catalog: Arc<dyn Catalog> = counting.clone();
        catalog
            .create_namespace(&NamespaceIdent::new("lake".into()), HashMap::new())
            .await
            .unwrap();
        let writer = Self::writer_for(&dir, Arc::clone(&catalog)).await;
        Self {
            dir,
            counting,
            catalog,
            writer,
        }
    }

    fn warehouse_of(dir: &tempfile::TempDir) -> String {
        format!("file://{}", dir.path().display())
    }

    pub async fn with_clock(now_ms: fn() -> i64) -> Self {
        let mut harness = Self::new().await;
        harness.writer.now_ms = now_ms;
        harness
    }

    /// Another writer process on the same table, with its own table cache.
    pub async fn peer_writer(&self) -> IcebergWriter<GroupedTestSpec> {
        Self::writer_for(&self.dir, Arc::clone(&self.catalog)).await
    }

    async fn writer_for(
        dir: &tempfile::TempDir,
        catalog: Arc<dyn Catalog>,
    ) -> IcebergWriter<GroupedTestSpec> {
        let warehouse = Self::warehouse_of(dir);
        let writer = IcebergWriter::<GroupedTestSpec>::new(
            RuntimeExecutionContext {
                pipeline_name: "p".to_string(),
                workspace_name: "w".to_string(),
                data_dir: dir.path().display().to_string(),
                execution_mode: Default::default(),
                output_layout: Default::default(),
                inject_fields: BTreeMap::new(),
            },
            RuntimeBinding::Primary,
            "lake".to_string(),
            catalog,
            IcebergWriterConfig {
                policies: SinkWritePolicySupport {
                    supports_merge_by_key: true,
                    supports_replace_partition: true,
                    supports_replace_table: true,
                },
                table_namespace: "lake".to_string(),
                location_root: format!("{warehouse}/lake"),
            },
        )
        .await
        .unwrap();
        let metadata: OutputMetadata = serde_json::from_value(json!({
            "out_field_name": "",
            "determined_type": "record",
            "determined_type_values": "",
            "fields": {
                "id": {
                    "out_field_name": "id",
                    "determined_type": "long",
                    "determined_type_values": "",
                    "field_id": 1,
                    "schema_id": 1,
                    "lineage_id": "orders:id",
                    "nullable": false,
                    "default_value": null,
                    "fields": {}
                }
            }
        }))
        .unwrap();
        writer
            .install_schema_snapshot(&RuntimeSchemaState {
                version: 1,
                namespaces: BTreeMap::from([(NAMESPACE.to_string(), metadata)]),
                namespace_versions: BTreeMap::from([(NAMESPACE.to_string(), 1)]),
            })
            .await
            .unwrap();
        writer
    }

    pub fn ident(&self) -> TableIdent {
        TableIdent::new(
            NamespaceIdent::new("lake".into()),
            self.writer.table_name(NAMESPACE),
        )
    }

    pub async fn table(&self) -> iceberg::table::Table {
        self.catalog.load_table(&self.ident()).await.unwrap()
    }

    /// One grouped compaction named `key` over WAL segment `seg-{key}`.
    pub async fn write(&self, key: &str, ids: std::ops::Range<i64>) -> SinkCallResult {
        self.write_with(&self.writer, key, ids).await
    }

    pub async fn write_with(
        &self,
        writer: &IcebergWriter<GroupedTestSpec>,
        key: &str,
        ids: std::ops::Range<i64>,
    ) -> SinkCallResult {
        self.write_fenced(writer, key, ids, None).await
    }

    pub async fn write_fenced(
        &self,
        writer: &IcebergWriter<GroupedTestSpec>,
        key: &str,
        ids: std::ops::Range<i64>,
        live: Option<LiveWalSegments>,
    ) -> SinkCallResult {
        self.write_rows(writer, key, ids.collect(), live, None, None)
            .await
    }

    pub async fn write_rows(
        &self,
        writer: &IcebergWriter<GroupedTestSpec>,
        key: &str,
        ids: Vec<i64>,
        live: Option<LiveWalSegments>,
        source_contract: Option<&SourceNamespaceContract>,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> SinkCallResult {
        let wal_refs = vec![RuntimeWalPartRef {
            segment_id: format!("seg-{key}"),
            source: format!("wal://seg-{key}"),
            start: 0,
            len: 1,
            sink_ref: "data_sinks.lake".to_string(),
            namespace: NAMESPACE.to_string(),
            partition: String::new(),
            time: None,
            schema_fingerprint: "fp".to_string(),
            cdc_meta_hash: None,
        }];
        let ctx = GroupedSinkWriteContext::try_from(SinkWriteContext {
            filename: format!("namespace={NAMESPACE}"),
            compaction_id: format!("compaction-{key}"),
            idempotency_key: format!("apply-{key}"),
            wal_refs,
            write_semantics: skippr_runtime_sdk::plugins::SinkWriteSemantics::ExactOnce,
            schema_fingerprint: "fp".to_string(),
            cdc_ctx,
            source_contract,
        })
        .unwrap()
        .with_live_wal_segments(live);
        let schema = Arc::new(ArrowSchema::new(vec![Field::new(
            "id",
            DataType::Int64,
            false,
        )]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from_iter_values(ids))],
        )
        .unwrap();
        let stream: SendableRecordBatchStream = Box::pin(RecordBatchStreamAdapter::new(
            schema,
            futures::stream::iter(vec![Ok(batch)]),
        ));
        let reader = GroupedBatchReader::new(
            stream,
            ctx.grouping_key.clone(),
            GroupedBatchReaderConfig::default(),
        );
        writer.sync_grouped_call_result(reader, ctx).await.unwrap()
    }
}

#[tokio::test]
async fn grouped_write_reports_bytes_objects_and_phase_latency() {
    let harness = Harness::new().await;

    let applied = harness.write("1", 0..3).await;

    assert_eq!(applied.outcome, SinkWriteOutcome::Applied);
    assert_eq!(applied.stats.rows, None);
    assert_eq!(applied.stats.objects, Some(1));
    assert!(applied.stats.bytes.is_some_and(|bytes| bytes > 0));
    assert!(applied.stats.upload_duration_ms.is_some());
    assert!(applied.stats.commit_duration_ms.is_some());

    let replay = harness.write("1", 0..3).await;
    assert_eq!(replay.outcome, SinkWriteOutcome::AlreadyApplied);
    assert_eq!(replay.stats, SinkWriteStats::default());
}

#[tokio::test]
async fn warm_grouped_commit_does_not_reload_the_table() {
    let harness = Harness::new().await;
    harness.write("1", 0..3).await;

    let before = harness.counting.loads();
    let applied = harness.write("2", 3..6).await;

    assert_eq!(applied.outcome, SinkWriteOutcome::Applied);
    assert_eq!(harness.counting.loads(), before);
    assert_eq!(harness.table().await.metadata().snapshots().count(), 2);
}

#[tokio::test]
async fn stale_cached_table_reloads_and_keeps_both_commits() {
    let harness = Harness::new().await;
    harness.write("1", 0..3).await;
    let peer = harness.peer_writer().await;
    assert_eq!(
        harness.write_with(&peer, "2", 3..6).await.outcome,
        SinkWriteOutcome::Applied
    );

    let applied = harness.write("3", 6..9).await;

    assert_eq!(applied.outcome, SinkWriteOutcome::Applied);
    let table = harness.table().await;
    assert_eq!(table.metadata().snapshots().count(), 3);
    let summary = table.metadata().current_snapshot().unwrap().summary();
    assert_eq!(
        summary
            .additional_properties
            .get("total-records")
            .map(String::as_str),
        Some("9")
    );
}

pub(crate) async fn scanned_ids(table: &iceberg::table::Table) -> Vec<i64> {
    use futures::TryStreamExt;
    let batches: Vec<RecordBatch> = table
        .scan()
        .select_all()
        .build()
        .unwrap()
        .to_arrow()
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    let mut ids: Vec<i64> = batches
        .iter()
        .flat_map(|batch| {
            batch
                .column_by_name("id")
                .unwrap()
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values()
                .to_vec()
        })
        .collect();
    ids.sort_unstable();
    ids
}

pub(crate) async fn manifest_count(table: &iceberg::table::Table) -> usize {
    table
        .metadata()
        .current_snapshot()
        .unwrap()
        .load_manifest_list(table.file_io(), table.metadata())
        .await
        .unwrap()
        .entries()
        .len()
}

#[tokio::test]
async fn five_hundred_grouped_commits_keep_the_manifest_list_bounded() {
    let harness = Harness::new().await;
    for i in 0..500_i64 {
        let applied = harness.write(&i.to_string(), i..i + 1).await;
        assert_eq!(applied.outcome, SinkWriteOutcome::Applied);
    }

    let table = harness.table().await;
    let manifests = manifest_count(&table).await;
    assert!(manifests <= 101, "manifests={manifests}");
    assert_eq!(scanned_ids(&table).await, (0..500).collect::<Vec<_>>());
}

fn a_day_and_an_hour_from_now() -> i64 {
    wall_clock_ms() + 25 * 60 * 60 * 1000
}

fn live(segment_ids: &[&str]) -> Option<LiveWalSegments> {
    Some(LiveWalSegments::new(
        segment_ids.iter().map(|id| id.to_string()),
    ))
}

fn snapshot_segment_ids(table: &iceberg::table::Table) -> Vec<String> {
    let mut ids: Vec<String> = table
        .metadata()
        .snapshots()
        .filter_map(|snapshot| {
            snapshot
                .summary()
                .additional_properties
                .get(SNAPSHOT_WAL_SEGMENT_IDS)
                .cloned()
        })
        .collect();
    ids.sort();
    ids
}

fn metadata_files(harness: &Harness, prefix: &str) -> usize {
    let dir = harness
        .dir
        .path()
        .join("lake")
        .join(harness.writer.table_name(NAMESPACE))
        .join("metadata");
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(prefix)
        })
        .count()
}

#[tokio::test]
async fn expiry_keeps_live_wal_and_newest_snapshots_and_every_row() {
    let harness = Harness::with_clock(a_day_and_an_hour_from_now).await;
    for i in 0..130_i64 {
        let applied = harness
            .write_fenced(&harness.writer, &i.to_string(), i..i + 1, live(&["seg-5"]))
            .await;
        assert_eq!(applied.outcome, SinkWriteOutcome::Applied);
    }

    let table = harness.table().await;
    let snapshots = table.metadata().snapshots().count();
    assert!(snapshots > 100 && snapshots < 130, "snapshots={snapshots}");
    let mut sequence_numbers: Vec<i64> = table
        .metadata()
        .snapshots()
        .map(|snapshot| snapshot.sequence_number())
        .collect();
    sequence_numbers.sort_unstable();
    let newest = &sequence_numbers[sequence_numbers.len() - 100..];
    assert_eq!(newest[99] - newest[0], 99, "the newest 100 are all kept");
    let segments = snapshot_segment_ids(&table);
    assert!(segments.contains(&"seg-5".to_string()));
    assert!(segments.contains(&"seg-129".to_string()));
    assert!(!segments.contains(&"seg-0".to_string()));
    assert_eq!(metadata_files(&harness, "snap-"), snapshots);
    assert_eq!(scanned_ids(&table).await, (0..130).collect::<Vec<_>>());
}

#[tokio::test]
async fn crash_window_replay_of_a_live_segment_is_already_committed_after_expiry() {
    let harness = Harness::with_clock(a_day_and_an_hour_from_now).await;
    for i in 0..130_i64 {
        harness
            .write_fenced(&harness.writer, &i.to_string(), i..i + 1, live(&["seg-5"]))
            .await;
    }
    let (_, manifest) = harness
        .writer
        .idempotency_manifest_location(NAMESPACE, "apply-5")
        .unwrap()
        .parts();
    std::fs::remove_file(manifest).unwrap();

    let replay = harness
        .write_fenced(&harness.peer_writer().await, "5", 5..6, live(&["seg-5"]))
        .await;

    assert_eq!(replay.outcome, SinkWriteOutcome::AlreadyApplied);
    assert_eq!(
        scanned_ids(&harness.table().await).await,
        (0..130).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn expiry_without_a_fence_keeps_all_history() {
    let harness = Harness::with_clock(a_day_and_an_hour_from_now).await;
    for i in 0..120_i64 {
        harness.write(&i.to_string(), i..i + 1).await;
    }

    assert_eq!(snapshot_segment_ids(&harness.table().await).len(), 120);
}

async fn referenced_manifests(table: &iceberg::table::Table) -> std::collections::BTreeSet<String> {
    let mut paths = std::collections::BTreeSet::new();
    for snapshot in table.metadata().snapshots() {
        let list = snapshot
            .load_manifest_list(table.file_io(), table.metadata())
            .await
            .unwrap();
        for manifest in list.entries() {
            let name = manifest.manifest_path.rsplit('/').next().unwrap();
            paths.insert(name.to_string());
        }
    }
    paths
}

#[tokio::test]
async fn expiry_with_merged_manifests_deletes_exactly_the_unreferenced_manifests() {
    let harness = Harness::with_clock(a_day_and_an_hour_from_now).await;
    for i in 0..260_i64 {
        harness
            .write_fenced(&harness.writer, &i.to_string(), i..i + 1, live(&[]))
            .await;
    }

    let table = harness.table().await;
    let on_disk: std::collections::BTreeSet<String> = std::fs::read_dir(
        harness
            .dir
            .path()
            .join("lake")
            .join(harness.writer.table_name(NAMESPACE))
            .join("metadata"),
    )
    .unwrap()
    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
    .filter(|name| name.ends_with(".avro") && !name.starts_with("snap-"))
    .collect();
    assert!(table.metadata().snapshots().count() < 260);
    assert_eq!(on_disk, referenced_manifests(&table).await);
    let metadata_window = table.metadata().metadata_log().len() + 1;
    assert!(metadata_window <= 101, "metadata_window={metadata_window}");
    assert_eq!(metadata_files(&harness, "version-hint"), 1);
    assert_eq!(metadata_files(&harness, "v"), metadata_window + 1);
    assert_eq!(metadata_files(&harness, "00"), metadata_window);
    assert_eq!(scanned_ids(&table).await, (0..260).collect::<Vec<_>>());
}

#[tokio::test]
#[ignore = "long-history run: cargo test -p skippr-iceberg-writer two_thousand -- --ignored --nocapture"]
async fn two_thousand_commit_history_keeps_metadata_flat() {
    const COMMITS: i64 = 2_000;
    const WINDOW: usize = 200;
    let harness = Harness::with_clock(a_day_and_an_hour_from_now).await;
    let mut samples: Vec<(u64, u128)> = Vec::new();
    for i in 0..COMMITS {
        let started = std::time::Instant::now();
        harness
            .write_fenced(&harness.writer, &i.to_string(), i..i + 1, live(&[]))
            .await;
        let commit_ms = started.elapsed().as_millis();
        let table = harness.table().await;
        let location = table.metadata_location().unwrap();
        let bytes = std::fs::metadata(location.trim_start_matches("file://"))
            .unwrap()
            .len();
        samples.push((bytes, commit_ms));
    }
    let windows: Vec<(u64, f64)> = samples
        .chunks(WINDOW)
        .map(|window| {
            let bytes = window.iter().map(|(bytes, _)| *bytes).max().unwrap();
            let ms = window.iter().map(|(_, ms)| *ms as f64).sum::<f64>() / window.len() as f64;
            (bytes, ms)
        })
        .collect();
    for (index, (bytes, ms)) in windows.iter().enumerate() {
        println!(
            "commits {:>4}..{:>4}: max metadata.json {:>7} bytes, mean commit {:>6.1} ms",
            index * WINDOW,
            (index + 1) * WINDOW,
            bytes,
            ms
        );
    }
    let table = harness.table().await;
    println!(
        "final: snapshots={} manifests={} live data files={}",
        table.metadata().snapshots().count(),
        manifest_count(&table).await,
        table
            .metadata()
            .current_snapshot()
            .unwrap()
            .summary()
            .additional_properties
            .get("total-data-files")
            .unwrap()
    );
    let steady = windows[1].0;
    let last = windows.last().unwrap().0;
    assert!(
        last <= steady + steady / 4,
        "metadata grew from {steady} to {last} bytes"
    );
    assert_eq!(scanned_ids(&table).await, (0..COMMITS).collect::<Vec<_>>());
}
