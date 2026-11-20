use crate::discover::schema_alter::{
    self, AlterOrigin, CommittedAlter, FlattenMode, OutputName, SchemaAlterError,
};
use crate::discover::{Metadata, OutputMetadata, PipelineMetadata};
use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::migration::file::{list_sql_files, AlterStmt, LoadedMigration, MigrationFileError};
use crate::migration::gate::{self, GateError};
use crate::migration::journal::{
    self, journal_key, JournalError, MigrationApply, MigrationDirection, MigrationId,
    MigrationPhase,
};
use crate::migration::migrations_dir;
use crate::plugins::DataSink;
use crate::sqlrt::parser::AlterTableStatement;
use crate::METADATA;
use skippr_lease::DurableError;
use skippr_wal_catalog::{CatalogFence, SkipprStore};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error(transparent)]
    Alter(#[from] SchemaAlterError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Gate(#[from] GateError),
    #[error(transparent)]
    File(#[from] MigrationFileError),
    #[error("WAL catalog: {0}")]
    Durable(#[from] DurableError),
    #[error("WAL payload: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("type: copy cannot ALTER TABLE")]
    CopyPipeline,
    #[error("copy/non-Arrow WAL parts cannot be rewritten")]
    CopyPayload,
    #[error("WAL part exceeds its payload")]
    TruncatedPayload,
    #[error("migration {0} has no down()")]
    NoDown(MigrationId),
    #[error("migration journal requires a skippr store")]
    NoJournalStore,
    #[error("pipeline: {0}")]
    Pipeline(String),
}

pub async fn apply_query_alter(
    config: &Config,
    stmt: &AlterTableStatement,
    sql: &str,
) -> Result<String, MigrationError> {
    let pipeline = format!("{}", stmt.pipeline).replace('"', "");
    let namespace = stmt
        .namespace
        .as_ref()
        .map(|ns| format!("{ns}").replace('"', ""))
        .unwrap_or_else(|| pipeline.clone());
    let bound = config.bind_pipeline(&pipeline);
    bound.init().await;
    let offsets =
        Arc::new(Offsets::init(&bound).map_err(|err| MigrationError::Pipeline(err.to_string()))?);
    let loaded = crate::migration::file::load_sql_file(
        MigrationId::query(crate::migration::MigrationChecksum::of(sql)).as_str(),
        sql,
    )?;
    apply_loaded(&bound, &offsets, &loaded, MigrationDirection::Up).await?;
    Ok(format!("ALTER TABLE {pipeline}.{namespace} {:?}", stmt.op))
}

pub async fn apply_pending(
    config: &Config,
    direction: MigrationDirection,
) -> Result<Vec<String>, MigrationError> {
    let offsets =
        Arc::new(Offsets::init(config).map_err(|err| MigrationError::Pipeline(err.to_string()))?);
    apply_pending_on(config, &offsets, direction).await
}

pub async fn apply_pending_on(
    config: &Config,
    offsets: &Arc<Offsets>,
    direction: MigrationDirection,
) -> Result<Vec<String>, MigrationError> {
    let files = list_sql_files(&migrations_dir(config))?;
    let pipeline = config.get_pipeline_name();
    let mut applied = Vec::new();
    for file in files {
        if file.up.iter().all(|stmt| stmt.pipeline != pipeline) {
            continue;
        }
        apply_loaded(config, offsets, &file, direction).await?;
        applied.push(file.id.as_str().to_string());
    }
    Ok(applied)
}

async fn apply_loaded(
    config: &Config,
    offsets: &Arc<Offsets>,
    loaded: &LoadedMigration,
    direction: MigrationDirection,
) -> Result<(), MigrationError> {
    let stmts = match direction {
        MigrationDirection::Up => &loaded.up,
        MigrationDirection::Down => loaded
            .down
            .as_ref()
            .ok_or_else(|| MigrationError::NoDown(loaded.id.clone()))?,
    };
    let apply = MigrationApply {
        id: loaded.id.clone(),
        checksum: loaded.checksum.clone(),
        direction,
    };
    let pipeline_name = config.get_pipeline_name();
    let process_budget =
        crate::runtime_plugins::host::RuntimeSinkProcessBudget::for_pipeline(&pipeline_name, 1);
    let plugin = config
        .ensure_primary_data_sink(process_budget)
        .await
        .map_err(MigrationError::Pipeline)?;
    apply_with(config, offsets, plugin.as_ref(), &apply, stmts).await
}

fn journal_store(
    offsets: &Offsets,
) -> Result<(Arc<dyn SkipprStore>, CatalogFence), MigrationError> {
    let store = offsets.skippr_kv().ok_or(MigrationError::NoJournalStore)?;
    let fence = offsets
        .writer_fence()
        .ok_or(MigrationError::NoJournalStore)?;
    Ok((store, fence))
}

/// One statement resolved for this apply.
struct Planned {
    namespace: String,
    output: OutputMetadata,
    committed: CommittedAlter,
}

async fn apply_with(
    config: &Config,
    offsets: &Arc<Offsets>,
    plugin: &dyn DataSink,
    apply: &MigrationApply,
    stmts: &[AlterStmt],
) -> Result<(), MigrationError> {
    let (store, fence) = journal_store(offsets)?;
    let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name())?;
    let phase = journal::latest_phase(&journal::load(store.as_ref(), &key)?, apply)?;
    if phase == Some(MigrationPhase::Committed) {
        return Ok(());
    }
    if matches!(
        config.pipeline_type().map_err(MigrationError::Pipeline)?,
        crate::pipeline_type::PipelineType::Copy
    ) {
        return Err(MigrationError::CopyPipeline);
    }
    let support = plugin.alter_support();
    if !support.is_table() {
        return Err(SchemaAlterError::DestRejects(schema_alter::DestRejection::NotTable).into());
    }
    let pipeline =
        crate::pipeline_backend::pipeline_key(config).map_err(MigrationError::Pipeline)?;
    let _exclusive = gate::exclusive(&pipeline).await?;
    let _metadata_writes = crate::ingest_work::exclusive_metadata_writes().await;
    let mut metadata = config.get_metadata().await.map_err(|_| {
        MigrationError::Pipeline(format!("no schema for {}", config.get_pipeline_name()))
    })?;
    let origin = AlterOrigin {
        migration: apply.id.clone(),
        direction: apply.direction,
    };

    let already_logged = phase == Some(MigrationPhase::DestApplied)
        && metadata
            .alters
            .values()
            .any(|log| log.last().is_some_and(|tail| tail.origin == origin));
    if !already_logged {
        let planned = plan(config, &metadata, support, &origin, stmts)?;
        if phase.is_none() {
            let wal = alter_wal_handle(config, offsets, &pipeline).await?;
            crate::buffer::wal_writer::flush_and_drain(config, offsets.clone()).await?;
            for p in &planned {
                crate::migration::wal::rewrite_live_wal(
                    config,
                    &wal,
                    &p.namespace,
                    &p.committed.alter,
                    p.committed.seq,
                )?;
            }
            journal::commit(
                store.as_ref(),
                fence,
                &key,
                apply,
                MigrationPhase::WalRewritten,
            )?;
        }
        if phase != Some(MigrationPhase::DestApplied) {
            for p in &planned {
                plugin
                    .apply_alter(&p.namespace, &p.output, &p.committed)
                    .await?;
            }
            journal::commit(
                store.as_ref(),
                fence,
                &key,
                apply,
                MigrationPhase::DestApplied,
            )?;
        }
        for p in planned {
            metadata
                .alters
                .entry(p.namespace)
                .or_default()
                .push(p.committed);
        }
        METADATA.store(Arc::new(metadata.clone()));
        config
            .persist_pipeline_metadata(&metadata)
            .await
            .map_err(MigrationError::Pipeline)?;
    }
    journal::commit(
        store.as_ref(),
        fence,
        &key,
        apply,
        MigrationPhase::Committed,
    )?;
    Ok(())
}

/// Canonicalize every statement against the running output (log fold plus
/// earlier statements of this migration). No I/O.
fn plan(
    config: &Config,
    metadata: &PipelineMetadata,
    support: crate::plugins::AlterSupport,
    origin: &AlterOrigin,
    stmts: &[AlterStmt],
) -> Result<Vec<Planned>, MigrationError> {
    let mode = metadata.flatten_mode();
    let in_use = config_columns(config, mode)?;
    let mut working: HashMap<String, (Metadata, u64)> = HashMap::new();
    let mut planned = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        let (current, seq) = match working.remove(&stmt.namespace) {
            Some(entry) => entry,
            None => (
                metadata.output(&stmt.namespace)?,
                metadata.alter_log(&stmt.namespace).len() as u64,
            ),
        };
        let alter = schema_alter::canonicalize(mode, &current, &stmt.op, &in_use)?;
        schema_alter::dest_accepts(support, &alter)?;
        let folded = schema_alter::apply(current, &alter)?;
        planned.push(Planned {
            namespace: stmt.namespace.clone(),
            output: OutputMetadata::project(&stmt.namespace, &folded, mode),
            committed: CommittedAlter {
                origin: origin.clone(),
                seq,
                alter,
            },
        });
        working.insert(stmt.namespace.clone(), (folded, seq + 1));
    }
    Ok(planned)
}

/// Columns the pipeline config names: time fields, partition fields, and
/// aggregate keys. ALTER must not rename, drop, or merge them away.
fn config_columns(config: &Config, mode: FlattenMode) -> Result<Vec<OutputName>, SchemaAlterError> {
    let listed = [
        config.get_transform_batch_time_fields(),
        config.get_transform_batch_partition_fields(),
    ];
    listed
        .iter()
        .flat_map(|list| list.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .map(|name| OutputName::parse_sql(mode, &name))
        .collect()
}

/// The pipeline's WAL handle. A CLI apply with no running pipeline opens it
/// and replays startup so every live segment is in the catalog.
async fn alter_wal_handle(
    config: &Config,
    offsets: &Arc<Offsets>,
    pipeline: &skippr_lease::PipelineKey,
) -> Result<Arc<crate::buffer::wal_handle::PipelineWalHandle>, MigrationError> {
    if let Some(handle) = crate::buffer::wal_handle::wal_handle_for(pipeline) {
        return Ok(handle);
    }
    let handle = crate::buffer::wal_store::ensure_disk_wal_handle(config, offsets.clone())?;
    crate::buffer::wal_startup::wal_startup(&handle, config, false).await?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::{PartitionKey, SegmentFile, WalPayload};
    use crate::discover::schema_alter::CanonicalAlter;
    use crate::discover::SkipprDataType;
    use crate::helpers::configuration::{Pipeline, Skippr, Transform};
    use crate::helpers::wal_storage::ElStorageMode;
    use crate::migration::file::load_sql_file;
    use crate::plugins::alter_ddl::WarehouseDialect;
    use crate::plugins::AlterSupport;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::reader::StreamReader;
    use async_trait::async_trait;
    use datafusion::execution::SendableRecordBatchStream;
    use serial_test::serial;
    use std::io::{Read, Seek};
    use std::sync::Mutex;
    use std::time::SystemTime;

    struct RecordingSink {
        support: AlterSupport,
        fail_dest: bool,
        calls: Mutex<Vec<(String, CommittedAlter)>>,
        dest_names: Mutex<Vec<Vec<String>>>,
        saw_gate: Mutex<bool>,
    }

    impl RecordingSink {
        fn new(support: AlterSupport) -> Self {
            Self {
                support,
                fail_dest: false,
                calls: Mutex::new(Vec::new()),
                dest_names: Mutex::new(Vec::new()),
                saw_gate: Mutex::new(false),
            }
        }

        fn iceberg() -> Self {
            Self::new(AlterSupport::Iceberg)
        }

        fn calls(&self) -> Vec<(String, CommittedAlter)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DataSink for RecordingSink {
        async fn sync(
            &self,
            _stream: SendableRecordBatchStream,
            _filename: String,
            _cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
        ) -> Result<(), std::io::Error> {
            Ok(())
        }

        async fn sync_grouped(
            &self,
            mut reader: crate::plugins::GroupedBatchReader,
            _ctx: crate::plugins::GroupedSinkWriteContext<'_>,
        ) -> Result<crate::plugins::SinkWriteOutcome, std::io::Error> {
            while let Some(_chunk) = reader.next_chunk().await? {}
            Ok(crate::plugins::SinkWriteOutcome::Applied)
        }

        fn capability(&self) -> &'static crate::plugins::cdc::SinkCapability {
            &crate::plugins::cdc::sink_capabilities::SKIPPRLAKE
        }

        fn alter_support(&self) -> AlterSupport {
            self.support
        }

        async fn apply_alter(
            &self,
            namespace: &str,
            dest_meta: &OutputMetadata,
            alter: &CommittedAlter,
        ) -> Result<(), std::io::Error> {
            let key = skippr_lease::PipelineKey::new("default", "migtest", "trips").unwrap();
            *self.saw_gate.lock().unwrap() = gate::is_exclusive(&key);
            if self.fail_dest {
                return Err(std::io::Error::other("warehouse unavailable"));
            }
            self.dest_names.lock().unwrap().push(
                dest_meta
                    .child_fields()
                    .map(|(key, _)| key.clone())
                    .collect(),
            );
            self.calls
                .lock()
                .unwrap()
                .push((namespace.to_string(), alter.clone()));
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
        assert_eq!(config.get_storage_mode(), ElStorageMode::Local);
        config
    }

    fn loaded(id: &str, sql: &str) -> LoadedMigration {
        load_sql_file(id, sql).unwrap()
    }

    fn rename_region() -> LoadedMigration {
        loaded(
            "20261020_rename_region",
            "ALTER TABLE trips RENAME COLUMN region TO region_code;\n-- migrate:down\nALTER TABLE trips RENAME COLUMN region_code TO region;",
        )
    }

    fn apply_of(loaded: &LoadedMigration, direction: MigrationDirection) -> MigrationApply {
        MigrationApply {
            id: loaded.id.clone(),
            checksum: loaded.checksum.clone(),
            direction,
        }
    }

    async fn run(
        config: &Config,
        offsets: &Arc<Offsets>,
        sink: &RecordingSink,
        loaded: &LoadedMigration,
        direction: MigrationDirection,
    ) -> Result<(), MigrationError> {
        let stmts = match direction {
            MigrationDirection::Up => &loaded.up,
            MigrationDirection::Down => loaded.down.as_ref().unwrap(),
        };
        apply_with(config, offsets, sink, &apply_of(loaded, direction), stmts).await
    }

    async fn seed(config: &Config) -> Arc<Offsets> {
        crate::buffer::wal_handle::clear_wal_handles();
        let mut metadata = PipelineMetadata::new(config);
        metadata.metadata.insert("trips".into(), ns_meta());
        config.persist_pipeline_metadata(&metadata).await.unwrap();
        Arc::new(Offsets::init(config).unwrap())
    }

    fn phase(config: &Config, offsets: &Offsets, apply: &MigrationApply) -> Option<MigrationPhase> {
        let (store, _) = journal_store(offsets).unwrap();
        let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name()).unwrap();
        journal::latest_phase(&journal::load(store.as_ref(), &key).unwrap(), apply).unwrap()
    }

    fn commit_phase(
        config: &Config,
        offsets: &Offsets,
        apply: &MigrationApply,
        phase: MigrationPhase,
    ) {
        let (store, fence) = journal_store(offsets).unwrap();
        let key = journal_key(&config.get_workspace_name(), &config.get_pipeline_name()).unwrap();
        journal::commit(store.as_ref(), fence, &key, apply, phase).unwrap();
    }

    /// Host-shaped WAL: no field ids on the Arrow fields.
    async fn write_region_wal(config: &Config, offsets: &Arc<Offsets>) -> std::path::PathBuf {
        let handle =
            crate::buffer::wal_store::ensure_disk_wal_handle(config, offsets.clone()).unwrap();
        let batch = arrow::array::RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "region",
                DataType::Utf8,
                true,
            )])),
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
        handle
            .persist_segment(
                "live1",
                &HashMap::new(),
                &HashMap::from([(key, WalPayload::Arrow(vec![batch]))]),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        handle.live_disk_payload_paths().unwrap()[0].clone()
    }

    fn wal_field_name(path: &std::path::Path) -> String {
        let admitted = SegmentFile::read_index_path(path).unwrap();
        let mut file = std::fs::File::open(path).unwrap();
        file.seek(std::io::SeekFrom::Start(admitted.index[0].start))
            .unwrap();
        let take = (&mut file).take(admitted.index[0].len);
        let reader = StreamReader::try_new(take, None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        batch.schema().field(0).name().to_string()
    }

    fn live_wal(config: &Config) -> std::path::PathBuf {
        crate::buffer::wal_handle::wal_handle_for(
            &crate::pipeline_backend::pipeline_key(config).unwrap(),
        )
        .unwrap()
        .live_disk_payload_paths()
        .unwrap()[0]
            .clone()
    }

    #[tokio::test]
    #[serial]
    async fn rename_rewrites_host_wal_calls_dest_and_commits() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        let wal = write_region_wal(&config, &offsets).await;
        let sink = RecordingSink::iceberg();
        let migration = rename_region();

        run(&config, &offsets, &sink, &migration, MigrationDirection::Up)
            .await
            .unwrap();

        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "trips");
        assert_eq!(calls[0].1.seq, 0);
        let CanonicalAlter::Rename { column, to_leaf } = &calls[0].1.alter else {
            panic!("{calls:?}");
        };
        assert_eq!(
            (column.name.column(), column.id, to_leaf.as_str()),
            ("region", 7, "region_code")
        );
        assert!(
            *sink.saw_gate.lock().unwrap(),
            "dest apply must run under the gate"
        );

        let live = live_wal(&config);
        assert_ne!(live, wal);
        assert_eq!(wal_field_name(&live), "region_code");

        let persisted = config.get_metadata().await.unwrap();
        assert!(persisted.metadata["trips"].fields.contains_key("region"));
        assert_eq!(persisted.alters["trips"][0].alter, calls[0].1.alter);
        assert!(persisted
            .output("trips")
            .unwrap()
            .fields
            .contains_key("region_code"));
        assert_eq!(
            phase(
                &config,
                &offsets,
                &apply_of(&migration, MigrationDirection::Up)
            ),
            Some(MigrationPhase::Committed)
        );
        let key = crate::pipeline_backend::pipeline_key(&config).unwrap();
        assert!(!gate::is_exclusive(&key));
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn cli_apply_opens_the_wal_when_no_pipeline_runs() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        write_region_wal(&config, &offsets).await;
        crate::buffer::wal_handle::clear_wal_handles();
        let sink = RecordingSink::iceberg();
        run(
            &config,
            &offsets,
            &sink,
            &rename_region(),
            MigrationDirection::Up,
        )
        .await
        .unwrap();
        assert_eq!(wal_field_name(&live_wal(&config)), "region_code");
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn up_down_up_logs_every_apply() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        let sink = RecordingSink::iceberg();
        let migration = rename_region();
        let directions = [
            MigrationDirection::Up,
            MigrationDirection::Down,
            MigrationDirection::Up,
        ];
        for direction in directions {
            run(&config, &offsets, &sink, &migration, direction)
                .await
                .unwrap();
        }
        let persisted = config.get_metadata().await.unwrap();
        let log = &persisted.alters["trips"];
        assert_eq!(
            log.iter()
                .map(|c| (c.seq, c.origin.direction))
                .collect::<Vec<_>>(),
            vec![
                (0, MigrationDirection::Up),
                (1, MigrationDirection::Down),
                (2, MigrationDirection::Up)
            ]
        );
        let output = persisted.output("trips").unwrap();
        assert!(output.fields.contains_key("region_code"));
        assert!(!output.fields.contains_key("region"));
        assert_eq!(sink.calls().len(), 3);
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn second_statement_binds_the_first_statements_output() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        let sink = RecordingSink::iceberg();
        let migration = loaded(
            "20261020_rename_then_drop",
            "ALTER TABLE trips RENAME COLUMN region TO region_code;\nALTER TABLE trips DROP COLUMN region_code;",
        );
        run(&config, &offsets, &sink, &migration, MigrationDirection::Up)
            .await
            .unwrap();
        let persisted = config.get_metadata().await.unwrap();
        assert_eq!(
            persisted.alters["trips"]
                .iter()
                .map(|c| c.seq)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(persisted.output("trips").unwrap().fields.is_empty());
        assert_eq!(sink.calls().len(), 2);
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn flattened_dest_metadata_uses_underscore_names() {
        let root = tempfile::tempdir().unwrap();
        let mut config = fixture_config(root.path());
        let mut pipeline = config.pipelines.get("trips").cloned().unwrap();
        pipeline.transform = Some(Transform {
            flatten_events: Some(true),
            ..Default::default()
        });
        config.pipelines.insert("trips".into(), pipeline);
        let config = config.bind_pipeline("trips");
        crate::buffer::wal_handle::clear_wal_handles();
        let mut metadata = PipelineMetadata::new(&config);
        let mut detail = Metadata::new_with_type(SkipprDataType::Record, "detail");
        detail.field_id = 1;
        let mut truck = Metadata::new_with_type(SkipprDataType::String, "truck_reg");
        truck.field_id = 4;
        detail.fields.insert("truck_reg".into(), truck);
        let mut ns = Metadata::new().unwrap();
        ns.fields.insert("detail".into(), detail);
        metadata.metadata.insert("trips".into(), ns);
        config.persist_pipeline_metadata(&metadata).await.unwrap();
        let offsets = Arc::new(Offsets::init(&config).unwrap());
        let sink = RecordingSink::iceberg();
        let migration = loaded(
            "20261020_flatten_rename",
            "ALTER TABLE trips RENAME COLUMN detail_truck_reg TO truck_registration;",
        );
        run(&config, &offsets, &sink, &migration, MigrationDirection::Up)
            .await
            .unwrap();
        let dest = sink.dest_names.lock().unwrap();
        assert!(
            dest.iter()
                .any(|names| names.iter().any(|n| n == "truck_registration")),
            "{dest:?}"
        );
        assert!(
            dest.iter()
                .all(|names| !names.iter().any(|n| n == "detail")),
            "{dest:?}"
        );
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn committed_apply_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        let sink = RecordingSink::iceberg();
        for _ in 0..2 {
            run(
                &config,
                &offsets,
                &sink,
                &rename_region(),
                MigrationDirection::Up,
            )
            .await
            .unwrap();
        }
        assert_eq!(sink.calls().len(), 1);
        crate::buffer::wal_handle::clear_wal_handles();
    }

    async fn assert_untouched(
        config: &Config,
        offsets: &Offsets,
        wal: &std::path::Path,
        sink: &RecordingSink,
    ) {
        assert!(sink.calls().is_empty());
        assert_eq!(wal_field_name(wal), "region");
        let persisted = config.get_metadata().await.unwrap();
        assert!(persisted.alters.is_empty());
        assert_eq!(
            phase(
                config,
                offsets,
                &apply_of(&rename_region(), MigrationDirection::Up)
            ),
            None
        );
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn stream_sink_fails_before_wal_or_dest() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        let wal = write_region_wal(&config, &offsets).await;
        let sink = RecordingSink::new(AlterSupport::Unsupported);
        let err = run(
            &config,
            &offsets,
            &sink,
            &rename_region(),
            MigrationDirection::Up,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            MigrationError::Alter(SchemaAlterError::DestRejects(
                schema_alter::DestRejection::NotTable
            ))
        ));
        assert_untouched(&config, &offsets, &wal, &sink).await;
    }

    #[tokio::test]
    #[serial]
    async fn dest_rejection_fails_before_wal_or_dest() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        let wal = write_region_wal(&config, &offsets).await;
        let sink = RecordingSink::new(AlterSupport::Warehouse(WarehouseDialect::Redshift));
        let migration = loaded(
            "20261020_promote",
            "ALTER TABLE trips ALTER COLUMN region TYPE VARCHAR;",
        );
        let err = run(&config, &offsets, &sink, &migration, MigrationDirection::Up)
            .await
            .unwrap_err();
        assert!(matches!(err, MigrationError::Alter(_)), "{err}");
        assert!(sink.calls().is_empty());
        assert_eq!(wal_field_name(&wal), "region");
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn copy_pipeline_fails_before_wal_or_dest() {
        let root = tempfile::tempdir().unwrap();
        let mut config = fixture_config(root.path());
        config.pipelines.get_mut("trips").unwrap().r#type =
            Some(crate::pipeline_type::PipelineType::Copy);
        let config = config.bind_pipeline("trips");
        let offsets = seed(&config).await;
        let wal = write_region_wal(&config, &offsets).await;
        let sink = RecordingSink::iceberg();
        let err = run(
            &config,
            &offsets,
            &sink,
            &rename_region(),
            MigrationDirection::Up,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MigrationError::CopyPipeline));
        assert_untouched(&config, &offsets, &wal, &sink).await;
    }

    #[tokio::test]
    #[serial]
    async fn dest_failure_after_wal_resumes_from_wal_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        write_region_wal(&config, &offsets).await;
        let migration = rename_region();
        let up = apply_of(&migration, MigrationDirection::Up);
        let mut failing = RecordingSink::iceberg();
        failing.fail_dest = true;
        let err = run(
            &config,
            &offsets,
            &failing,
            &migration,
            MigrationDirection::Up,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MigrationError::Io(_)));
        assert_eq!(
            phase(&config, &offsets, &up),
            Some(MigrationPhase::WalRewritten)
        );
        assert_eq!(wal_field_name(&live_wal(&config)), "region_code");
        assert!(config.get_metadata().await.unwrap().alters.is_empty());

        let sink = RecordingSink::iceberg();
        run(&config, &offsets, &sink, &migration, MigrationDirection::Up)
            .await
            .unwrap();
        assert_eq!(sink.calls().len(), 1);
        assert_eq!(
            phase(&config, &offsets, &up),
            Some(MigrationPhase::Committed)
        );
        assert_eq!(wal_field_name(&live_wal(&config)), "region_code");
        assert_eq!(
            config.get_metadata().await.unwrap().alters["trips"].len(),
            1
        );
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn dest_applied_resume_appends_once_and_commits() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        let migration = rename_region();
        let up = apply_of(&migration, MigrationDirection::Up);
        commit_phase(&config, &offsets, &up, MigrationPhase::DestApplied);
        let sink = RecordingSink::iceberg();
        run(&config, &offsets, &sink, &migration, MigrationDirection::Up)
            .await
            .unwrap();
        assert!(
            sink.calls().is_empty(),
            "DestApplied resume must not re-run dest DDL"
        );
        assert_eq!(
            phase(&config, &offsets, &up),
            Some(MigrationPhase::Committed)
        );
        assert_eq!(
            config.get_metadata().await.unwrap().alters["trips"].len(),
            1
        );
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn dest_applied_resume_after_persist_does_not_append_twice() {
        let root = tempfile::tempdir().unwrap();
        let config = fixture_config(root.path());
        let offsets = seed(&config).await;
        let migration = rename_region();
        let up = apply_of(&migration, MigrationDirection::Up);
        let sink = RecordingSink::iceberg();
        run(&config, &offsets, &sink, &migration, MigrationDirection::Up)
            .await
            .unwrap();
        commit_phase(&config, &offsets, &up, MigrationPhase::DestApplied);
        run(&config, &offsets, &sink, &migration, MigrationDirection::Up)
            .await
            .unwrap();
        assert_eq!(
            config.get_metadata().await.unwrap().alters["trips"].len(),
            1
        );
        assert_eq!(sink.calls().len(), 1);
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn config_referenced_column_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let mut config = fixture_config(root.path());
        let mut pipeline = config.pipelines.get("trips").cloned().unwrap();
        pipeline.transform = Some(Transform {
            batch_partition_fields: Some("region".into()),
            ..Default::default()
        });
        config.pipelines.insert("trips".into(), pipeline);
        let config = config.bind_pipeline("trips");
        let offsets = seed(&config).await;
        let sink = RecordingSink::iceberg();
        let err = run(
            &config,
            &offsets,
            &sink,
            &rename_region(),
            MigrationDirection::Up,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            MigrationError::Alter(SchemaAlterError::ColumnInUse(ref column)) if column == "region"
        ));
        crate::buffer::wal_handle::clear_wal_handles();
    }

    #[tokio::test]
    #[serial]
    async fn flattened_dotted_config_column_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let mut config = fixture_config(root.path());
        let mut pipeline = config.pipelines.get("trips").cloned().unwrap();
        pipeline.transform = Some(Transform {
            flatten_events: Some(true),
            batch_partition_fields: Some("detail.region".into()),
            ..Default::default()
        });
        config.pipelines.insert("trips".into(), pipeline);
        let config = config.bind_pipeline("trips");
        let offsets = seed(&config).await;
        let sink = RecordingSink::iceberg();
        let err = run(
            &config,
            &offsets,
            &sink,
            &rename_region(),
            MigrationDirection::Up,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            MigrationError::Alter(SchemaAlterError::SqlFormMismatch { .. })
        ));
        crate::buffer::wal_handle::clear_wal_handles();
    }
}
