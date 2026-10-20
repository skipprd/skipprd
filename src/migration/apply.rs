use crate::discover::schema_alter;
use crate::helpers::configuration::Config;
use crate::helpers::offset_store::OffsetStore;
use crate::helpers::offsets::Offsets;
use crate::migration::file::{list_sql_files, AlterStmt, LoadedMigration};
use crate::migration::journal::{
    self, journal_key, MigrationApply, MigrationDirection, MigrationId, MigrationPhase,
};
use crate::migration::migrations_dir;
use crate::plugins::{AlterSupport, SchemaSink};
use crate::sqlrt::parser::AlterTableStatement;
use crate::METADATA;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

static SCHEMA_BARRIER: AtomicBool = AtomicBool::new(false);

pub fn schema_barrier_held() -> bool {
    SCHEMA_BARRIER.load(Ordering::SeqCst)
}

pub fn set_schema_barrier(held: bool) {
    SCHEMA_BARRIER.store(held, Ordering::SeqCst);
}

pub async fn apply_query_alter(
    config: &Config,
    stmt: &AlterTableStatement,
    sql: &str,
) -> Result<String, String> {
    let pipeline = format!("{}", stmt.pipeline).replace('"', "");
    let namespace = stmt
        .namespace
        .as_ref()
        .map(|ns| format!("{ns}").replace('"', ""))
        .unwrap_or_else(|| pipeline.clone());
    let bound = config.bind_pipeline(&pipeline);
    bound.init().await;
    let offsets = Offsets::init(&bound).map_err(|err| err.to_string())?;
    let store = journal_store(&offsets)?;
    let _lease = match bound.get_wal_storage() {
        crate::helpers::wal_storage::WalStorage::Disk
        | crate::helpers::wal_storage::WalStorage::S3 => {
            Some(crate::pipeline_backend::acquire_ingest_lease(&bound, &offsets).await?)
        }
        crate::helpers::wal_storage::WalStorage::Clustered => None,
    };
    let loaded = crate::migration::file::load_sql_file(
        MigrationId::query(crate::migration::MigrationChecksum::of(sql)).as_str(),
        sql,
    )
    .map_err(|err| err.to_string())?;
    apply_loaded(&bound, store, &loaded, MigrationDirection::Up).await?;
    Ok(format!("ALTER TABLE {pipeline}.{namespace} {:?}", stmt.op))
}

pub async fn apply_pending_files(
    config: &Config,
    store: &dyn OffsetStore,
    direction: MigrationDirection,
) -> Result<Vec<String>, String> {
    let dir = migrations_dir(config);
    let files = list_sql_files(&dir).map_err(|err| err.to_string())?;
    let pipeline = config.get_pipeline_name();
    let mut applied = Vec::new();
    for file in files {
        if file.up.iter().all(|stmt| stmt.pipeline != pipeline) {
            continue;
        }
        apply_loaded(config, store, &file, direction).await?;
        applied.push(file.id.as_str().to_string());
    }
    Ok(applied)
}

pub async fn apply_pending_with_ingest_lease(
    config: &Config,
    direction: MigrationDirection,
) -> Result<Vec<String>, String> {
    let offsets = Offsets::init(config).map_err(|err| err.to_string())?;
    let store = journal_store(&offsets)?;
    let _lease = match config.get_wal_storage() {
        crate::helpers::wal_storage::WalStorage::Disk
        | crate::helpers::wal_storage::WalStorage::S3 => {
            Some(crate::pipeline_backend::acquire_ingest_lease(config, &offsets).await?)
        }
        crate::helpers::wal_storage::WalStorage::Clustered => None,
    };
    apply_pending_files(config, store, direction).await
}

pub async fn apply_migration(
    config: &Config,
    store: &dyn OffsetStore,
    apply: &MigrationApply,
    stmts: &[AlterStmt],
) -> Result<(), String> {
    let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name());
    let log = journal::load(store, &key).map_err(|err| err.to_string())?;
    if journal::latest_phase(&log, apply).map_err(|err| err.to_string())?
        == Some(MigrationPhase::Committed)
    {
        return Ok(());
    }
    let plugin = config.primary_schema_sink().await?;
    apply_with(config, store, plugin.as_ref(), apply, stmts).await
}

fn journal_store(offsets: &Offsets) -> Result<&dyn OffsetStore, String> {
    offsets
        .bytes_store()
        .map(|store| store.as_ref())
        .ok_or_else(|| "migration journal requires a skippr store".to_string())
}

async fn apply_with(
    config: &Config,
    store: &dyn crate::helpers::offset_store::OffsetStore,
    plugin: &dyn SchemaSink,
    apply: &MigrationApply,
    stmts: &[AlterStmt],
) -> Result<(), String> {
    let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name());
    let log = journal::load(store, &key).map_err(|err| err.to_string())?;
    let phase = journal::latest_phase(&log, apply).map_err(|err| err.to_string())?;
    if phase == Some(MigrationPhase::Committed) {
        return Ok(());
    }

    set_schema_barrier(true);
    let _release = SchemaBarrierGuard;
    wait_sink_idle().await;
    if plugin.alter_support() != AlterSupport::Native {
        return Err(format!(
            "schema sink does not support migrations ({:?})",
            plugin.alter_support()
        ));
    }

    let mut metadata = config
        .get_metadata()
        .await
        .map_err(|_| format!("No existing schema for {}", config.get_pipeline_name()))?;
    let flatten = config.get_transform_flatten_events();

    if phase != Some(MigrationPhase::DestApplied) {
        let mut dest_ops = Vec::new();
        for stmt in stmts {
            let ns_meta = metadata.metadata.get_mut(&stmt.namespace).ok_or_else(|| {
                format!(
                    "Schema '{}' not found for pipeline '{}'",
                    stmt.namespace, stmt.pipeline
                )
            })?;
            let op = schema_alter::canonicalize_op(ns_meta, &stmt.op).map_err(|e| e.to_string())?;
            let field_id =
                schema_alter::field_id_for_op(ns_meta, &op).map_err(|e| e.to_string())?;
            schema_alter::apply(ns_meta, &op).map_err(|e| e.to_string())?;
            crate::ingest_work::Ingest::replace_arrow_schema(
                config,
                &stmt.namespace,
                ns_meta,
                flatten,
            )
            .map_err(|err| err.to_string())?;
            if phase != Some(MigrationPhase::WalRewritten) {
                crate::migration::wal::rewrite_live_wal(config, &stmt.namespace, &op, field_id)?;
            }
            dest_ops.push((
                stmt.namespace.clone(),
                crate::discover::OutputMetadata::from_metadata(ns_meta),
                op,
                field_id,
            ));
        }
        if phase != Some(MigrationPhase::WalRewritten) {
            journal::commit(store, &key, apply, MigrationPhase::WalRewritten)
                .map_err(|err| err.to_string())?;
        }
        for (namespace, output, op, field_id) in dest_ops {
            plugin
                .apply_alter(&namespace, &output, &op, field_id)
                .await
                .map_err(|err| err.to_string())?;
        }
        journal::commit(store, &key, apply, MigrationPhase::DestApplied)
            .map_err(|err| err.to_string())?;
    }

    METADATA.store(Arc::new(metadata.clone()));
    config.persist_pipeline_metadata(&metadata).await?;
    journal::commit(store, &key, apply, MigrationPhase::Committed)
        .map_err(|err| err.to_string())?;
    Ok(())
}

async fn wait_sink_idle() {
    for _ in 0..1_000 {
        let sink_busy = crate::buffer::compaction_progress::sink_work_in_flight_count() > 0;
        let compact_busy = crate::metrics::counters::WAL_COMPACTIONS_IN_FLIGHT
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0;
        if !sink_busy && !compact_busy {
            return;
        }
        tokio::task::yield_now().await;
    }
}

async fn apply_loaded(
    config: &Config,
    store: &dyn OffsetStore,
    loaded: &LoadedMigration,
    direction: MigrationDirection,
) -> Result<(), String> {
    let stmts = match direction {
        MigrationDirection::Up => &loaded.up,
        MigrationDirection::Down => loaded
            .down
            .as_ref()
            .ok_or_else(|| format!("migration {} has no down()", loaded.id))?,
    };
    let apply = MigrationApply {
        id: loaded.id.clone(),
        checksum: loaded.checksum.clone(),
        direction,
    };
    apply_migration(config, store, &apply, stmts).await
}

struct SchemaBarrierGuard;

impl Drop for SchemaBarrierGuard {
    fn drop(&mut self) {
        set_schema_barrier(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::{PartitionKey, SegmentFile, WalPayload};
    use crate::discover::schema_alter::SchemaAlterOp;
    use crate::discover::{Metadata, PipelineMetadata, SkipprDataType};
    use crate::helpers::configuration::{Pipeline, Skippr};
    use crate::helpers::offset_store::SledOffsetStore;
    use crate::helpers::wal_storage::ElStorageMode;
    use crate::migration::file::load_sql_file;
    use crate::migration::journal;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::reader::StreamReader;
    use async_trait::async_trait;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::io::{Read, Seek};
    use std::sync::Mutex;
    use std::time::SystemTime;

    struct RecordingSink {
        support: AlterSupport,
        calls: Mutex<Vec<(String, SchemaAlterOp, i32)>>,
        saw_barrier: Mutex<bool>,
    }

    impl RecordingSink {
        fn native() -> Self {
            Self {
                support: AlterSupport::Native,
                calls: Mutex::new(Vec::new()),
                saw_barrier: Mutex::new(false),
            }
        }

        fn unsupported() -> Self {
            Self {
                support: AlterSupport::Unsupported,
                calls: Mutex::new(Vec::new()),
                saw_barrier: Mutex::new(false),
            }
        }
    }

    #[async_trait]
    impl SchemaSink for RecordingSink {
        fn alter_support(&self) -> AlterSupport {
            self.support
        }

        async fn apply_alter(
            &self,
            namespace: &str,
            _metadata: &crate::discover::OutputMetadata,
            op: &SchemaAlterOp,
            field_id: i32,
        ) -> Result<(), std::io::Error> {
            *self.saw_barrier.lock().unwrap() = schema_barrier_held();
            self.calls
                .lock()
                .unwrap()
                .push((namespace.to_string(), op.clone(), field_id));
            Ok(())
        }

        async fn sync_schema(
            &self,
            _namespace: &str,
            _metadata: &crate::discover::OutputMetadata,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }
    }

    fn ns_meta() -> Metadata {
        let mut root = Metadata::new().unwrap();
        let mut region = Metadata::new_with_type(SkipprDataType::String, "region");
        region.field_id = 7;
        root.fields.insert("region".into(), region);
        root
    }

    fn fixture_config(root: &std::path::Path) -> Config {
        std::fs::create_dir_all(root).unwrap();
        let mut config = Config::new();
        config.skippr = Some(Skippr {
            workspace: Some("migtest".into()),
            skipprd_el_storage_mode: Some(ElStorageMode::Local),
            ..Skippr::default()
        });
        config.pipelines.insert(
            "trips".into(),
            Pipeline {
                data_dir: Some(root.display().to_string()),
                ..Pipeline::default()
            },
        );
        let config = config.bind_pipeline("trips");
        assert_eq!(
            config.get_storage_mode(),
            ElStorageMode::Local,
            "migration tests must not touch S3"
        );
        config
    }

    fn store() -> SledOffsetStore {
        let db = sled::Config::new().temporary(true).open().unwrap();
        SledOffsetStore::new(db.open_tree("migration").unwrap())
    }

    fn rename_apply() -> (MigrationApply, Vec<AlterStmt>) {
        let loaded = load_sql_file(
            "20261020_rename_region",
            "ALTER TABLE trips RENAME COLUMN region TO region_code;",
        )
        .unwrap();
        (
            MigrationApply {
                id: loaded.id.clone(),
                checksum: loaded.checksum.clone(),
                direction: MigrationDirection::Up,
            },
            loaded.up,
        )
    }

    async fn seed_metadata(config: &Config) {
        let mut metadata = PipelineMetadata::new(config);
        metadata.metadata.insert("trips".into(), ns_meta());
        config.persist_pipeline_metadata(&metadata).await.unwrap();
    }

    fn write_region_wal(config: &Config) -> std::path::PathBuf {
        let segs = std::path::PathBuf::from(config.get_data_dir()).join("segment_buffer/segs");
        let seg = SegmentFile::new(&segs, "live1").unwrap();
        let mut field_meta = HashMap::new();
        field_meta.insert("PARQUET:field_id".into(), "7".into());
        let field = Field::new("region", DataType::Utf8, true).with_metadata(field_meta);
        let schema = Schema::new(vec![field]);
        let batch = arrow::array::RecordBatch::try_new(
            Arc::new(schema),
            vec![Arc::new(arrow::array::StringArray::from(vec!["west"]))],
        )
        .unwrap();
        let key = PartitionKey {
            sink_ref: "data_outputs.test".into(),
            namespace: "trips".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        let mut batches = HashMap::new();
        batches.insert(key.clone(), WalPayload::Arrow(vec![batch]));
        let mut parts_meta = HashMap::new();
        parts_meta.insert(key, (0, SystemTime::now()));
        let (meta, _, _) = seg
            .write_snapshot(&HashMap::new(), &batches, &parts_meta, &HashMap::new())
            .unwrap();
        seg.write_commit_marker(&meta).unwrap();
        seg.path.clone()
    }

    fn wal_field_name(path: &std::path::Path) -> String {
        let admitted = SegmentFile::admit_owned_pair_path(path).unwrap();
        let mut file = std::fs::File::open(path).unwrap();
        file.seek(std::io::SeekFrom::Start(admitted.index[0].start))
            .unwrap();
        let mut take = (&mut file).take(admitted.index[0].len);
        let reader = StreamReader::try_new(&mut take, None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        batch.schema().field(0).name().to_string()
    }

    #[tokio::test]
    #[serial]
    async fn apply_uses_the_open_lease_store_instead_of_reopening_sled() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        seed_metadata(&config).await;
        let offsets = Offsets::init(&config).expect("lease store");
        let store = offsets.bytes_store().expect("journal store");
        let sink = RecordingSink::native();
        let (apply, stmts) = rename_apply();
        apply_with(&config, store.as_ref(), &sink, &apply, &stmts)
            .await
            .unwrap();
        assert_eq!(sink.calls.lock().unwrap().len(), 1);
        let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name());
        assert_eq!(
            journal::latest_phase(&journal::load(store.as_ref(), &key).unwrap(), &apply).unwrap(),
            Some(MigrationPhase::Committed)
        );
    }

    #[tokio::test]
    #[serial]
    async fn apply_rename_rewrites_wal_calls_dest_and_commits_journal() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        seed_metadata(&config).await;
        let wal = write_region_wal(&config);
        let store = store();
        let sink = RecordingSink::native();
        let (apply, stmts) = rename_apply();

        apply_with(&config, &store, &sink, &apply, &stmts)
            .await
            .unwrap();

        let calls = sink.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "trips");
        assert_eq!(calls[0].2, 7);
        assert!(
            *sink.saw_barrier.lock().unwrap(),
            "dest apply_alter must run under SchemaBarrier"
        );
        assert_eq!(wal_field_name(&wal), "region_code");

        let persisted = config.get_metadata().await.unwrap();
        assert!(persisted.metadata["trips"]
            .fields
            .contains_key("region_code"));
        assert!(!persisted.metadata["trips"].fields.contains_key("region"));

        let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name());
        assert_eq!(
            journal::latest_phase(&journal::load(&store, &key).unwrap(), &apply).unwrap(),
            Some(MigrationPhase::Committed)
        );
        assert!(!schema_barrier_held());
    }

    #[tokio::test]
    #[serial]
    async fn committed_apply_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        seed_metadata(&config).await;
        let store = store();
        let sink = RecordingSink::native();
        let (apply, stmts) = rename_apply();
        apply_with(&config, &store, &sink, &apply, &stmts)
            .await
            .unwrap();
        apply_with(&config, &store, &sink, &apply, &stmts)
            .await
            .unwrap();
        assert_eq!(sink.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn unsupported_sink_fail_closes_before_wal_or_dest() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        seed_metadata(&config).await;
        let wal = write_region_wal(&config);
        let store = store();
        let sink = RecordingSink::unsupported();
        let (apply, stmts) = rename_apply();

        let err = apply_with(&config, &store, &sink, &apply, &stmts)
            .await
            .unwrap_err();
        assert!(err.contains("does not support migrations"), "{err}");
        assert!(sink.calls.lock().unwrap().is_empty());
        assert_eq!(wal_field_name(&wal), "region");

        let persisted = config.get_metadata().await.unwrap();
        assert!(persisted.metadata["trips"].fields.contains_key("region"));
        let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name());
        assert_eq!(
            journal::latest_phase(&journal::load(&store, &key).unwrap(), &apply).unwrap(),
            None
        );
    }

    #[tokio::test]
    #[serial]
    async fn dest_applied_resume_skips_dest_and_commits() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        seed_metadata(&config).await;
        let store = store();
        let sink = RecordingSink::native();
        let (apply, stmts) = rename_apply();
        let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name());
        journal::commit(&store, &key, &apply, MigrationPhase::DestApplied).unwrap();

        apply_with(&config, &store, &sink, &apply, &stmts)
            .await
            .unwrap();

        assert!(
            sink.calls.lock().unwrap().is_empty(),
            "DestApplied resume must not re-run dest DDL"
        );
        assert_eq!(
            journal::latest_phase(&journal::load(&store, &key).unwrap(), &apply).unwrap(),
            Some(MigrationPhase::Committed)
        );
        let persisted = config.get_metadata().await.unwrap();
        assert!(persisted.metadata["trips"].fields.contains_key("region"));
    }
}
