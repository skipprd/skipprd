use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use mongodb::bson::{doc, Document};
use mongodb::change_stream::event::OperationType;
use mongodb::options::{ClientOptions, FullDocumentType};
use mongodb::Client;
use serde_derive::Deserialize;
use tracing::{info, warn};

use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::cdc::{
    source_capabilities, CheckpointAuthority, CheckpointKind, MongodbCheckpoint, MutationKind,
    WalRowMeta,
};
use crate::plugins::{
    DataSink, DataSource, SourceCdcMode, SourceExecutionContract, SourceOnceContract,
};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceMongodbPluginConfig {
    pub connection_string: String,
    pub database: String,
    pub collection: String,
    pub filter: Option<String>,
    pub batch_size_rows: Option<usize>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
    #[serde(default)]
    pub cdc_mode: SourceCdcMode,
}

impl TryFrom<PluginConfigEntry> for DataSourceMongodbPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Mongodb")
    }
}

pub struct DataSourceMongodbPlugin {
    ingest: Ingest,
    config: DataSourceMongodbPluginConfig,
}

impl DataSourceMongodbPlugin {
    pub async fn new() -> Self {
        let config: DataSourceMongodbPluginConfig = match Config::get_pipeline_input_plugin_config()
        {
            Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
            Err(_) => DataSourceMongodbPluginConfig {
                connection_string: Config::getenv("MONGODB_CONNECTION_STRING", ""),
                database: Config::getenv("MONGODB_DATABASE", ""),
                collection: Config::getenv("MONGODB_COLLECTION", ""),
                filter: None,
                batch_size_rows: None,
                format: None,
                batch_size_bytes: Some(
                    Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000")
                        .parse()
                        .unwrap_or(1_024_000),
                ),
                batch_size_seconds: Some(
                    Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "600")
                        .parse()
                        .unwrap_or(600),
                ),
                cdc_mode: SourceCdcMode::Snapshot,
            },
        };
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    pub fn with_runtime_config(config: DataSourceMongodbPluginConfig) -> Self {
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    fn cdc_mode(&self) -> SourceCdcMode {
        self.config.cdc_mode
    }

    async fn sync_query(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        cdc_anchor: Option<&[u8]>,
    ) -> Result<(), std::io::Error> {
        let client_options = ClientOptions::parse(&self.config.connection_string)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let client = Client::with_options(client_options)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let db = client.database(&self.config.database);
        let collection = db.collection::<Document>(&self.config.collection);

        let filter_doc: Option<Document> = match &self.config.filter {
            Some(f) if !f.is_empty() => {
                let v: serde_json::Value =
                    serde_json::from_str(f).map_err(|e| std::io::Error::other(e.to_string()))?;
                let bson =
                    mongodb::bson::to_bson(&v).map_err(|e| std::io::Error::other(e.to_string()))?;
                Some(
                    bson.as_document()
                        .cloned()
                        .ok_or_else(|| std::io::Error::other("filter must be a JSON object"))?,
                )
            }
            _ => None,
        };

        let namespace = format!(
            "mongodb.{}.{}",
            self.config.database, self.config.collection
        );
        let offset_key = OffsetKey {
            namespace: namespace.clone(),
            partition: String::new(),
        };

        info!(
            "MongoDB: reading {}.{}",
            self.config.database, self.config.collection
        );

        let mut cursor = collection
            .find(filter_doc.unwrap_or_default())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let chunk_bytes = self.config.batch_size_bytes.unwrap_or(1_024_000) as usize;
        let mut current_batch: Vec<IngestBatch> = Vec::new();
        let mut current_bytes: usize = 0;
        let mut row_idx: u64 = 0;
        let source_uri = format!(
            "mongodb://{}/{}",
            self.config.database, self.config.collection
        );

        while let Some(doc) = cursor
            .try_next()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
        {
            let json_str =
                serde_json::to_string(&doc).map_err(|e| std::io::Error::other(e.to_string()))?;
            let bytes = json_str.len();

            let cdc_rows = if let Some(anchor) = cdc_anchor {
                let id_hash = if let Some(id_val) = doc.get("_id") {
                    md5::compute(format!("{}", id_val).as_bytes()).0.to_vec()
                } else {
                    format!(
                        "{}:{}:{}",
                        self.config.database, self.config.collection, row_idx
                    )
                    .into_bytes()
                };
                Some(vec![WalRowMeta {
                    mutation: MutationKind::Snapshot,
                    event_id: id_hash,
                    order_token: anchor.to_vec(),
                }])
            } else {
                None
            };

            current_batch.push(IngestBatch {
                offset_key: offset_key.clone(),
                data: json_str,
                bytes,
                source_uri: source_uri.clone(),
                namespace: Some(namespace.clone()),
                cdc_rows,
            });
            current_bytes += bytes;
            row_idx += 1;

            if current_bytes >= chunk_bytes {
                let mut ingest_tasks = IngestTasks::new();
                ingest_tasks.add(IngestTask::new(
                    std::mem::take(&mut current_batch),
                    offsets.clone(),
                    shared_output.clone(),
                ));
                self.ingest
                    .ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output.clone());
                current_bytes = 0;
            }
        }

        if !current_batch.is_empty() {
            let mut ingest_tasks = IngestTasks::new();
            ingest_tasks.add(IngestTask::new(
                current_batch,
                offsets.clone(),
                shared_output.clone(),
            ));
            self.ingest
                .ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output.clone());
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // CDC: anchored snapshot + change stream
    // -----------------------------------------------------------------------

    async fn sync_cdc(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
        mode: SourceCdcMode,
    ) -> Result<(), std::io::Error> {
        let client_options = ClientOptions::parse(&self.config.connection_string)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let client = Client::with_options(client_options)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let db = client.database(&self.config.database);
        let collection = db.collection::<Document>(&self.config.collection);

        let checkpoint_key = format!(
            "mongodb:{}:{}:resume_token",
            self.config.database, self.config.collection
        );
        let stored_token = offsets
            .load_checkpoint_payload::<MongodbCheckpoint>(&checkpoint_key)
            .map(|checkpoint| checkpoint.resume_token);
        let resume_mode = stored_token.is_some();

        if resume_mode {
            info!("MongoDB CDC: resuming from stored resume token");
        }

        // Capture the server's current operation time as the snapshot anchor
        // so snapshot order_tokens are comparable with change-stream cluster
        // times (both encoded as big-endian u64 BSON timestamps).
        let anchor_bytes = match db.run_command(doc! { "ping": 1 }).await {
            Ok(response) => {
                if let Ok(ts) = response.get_timestamp("operationTime") {
                    let t = (ts.time as u64) << 32 | (ts.increment as u64);
                    t.to_be_bytes().to_vec()
                } else {
                    chrono::Utc::now().timestamp().to_be_bytes().to_vec()
                }
            }
            Err(_) => chrono::Utc::now().timestamp().to_be_bytes().to_vec(),
        };

        // Determine the resume token: prefer stored checkpoint, then try
        // capturing a fresh one from a short-lived watch().
        let resume_token = if let Some(ref token_bytes) = stored_token {
            match serde_json::from_slice::<mongodb::change_stream::event::ResumeToken>(token_bytes)
            {
                Ok(token) => Some(token),
                Err(e) => {
                    warn!("MongoDB CDC: failed to deserialize stored resume token ({}), falling back to fresh snapshot", e);
                    None
                }
            }
        } else if mode.includes_initial_snapshot() {
            let stream = collection
                .watch()
                .full_document(FullDocumentType::UpdateLookup)
                .await
                .map_err(|e| {
                    std::io::Error::other(format!("MongoDB CDC requires change streams: {}", e))
                })?;
            stream
                .resume_token()
                .ok_or_else(|| {
                    std::io::Error::other("MongoDB CDC could not capture an initial resume token")
                })
                .map(Some)?
        } else {
            None
        };

        // Phase 1: Anchored snapshot (skipped on resume)
        if mode.includes_initial_snapshot() && !resume_mode {
            info!("MongoDB CDC: running initial snapshot");
            self.sync_query(offsets.clone(), shared_output.clone(), Some(&anchor_bytes))
                .await?;
        } else if mode == SourceCdcMode::CdcOnly && !resume_mode {
            info!("MongoDB CDC: cdc_only mode skips initial snapshot");
        }

        // Phase 2: Change stream from resume token
        info!("MongoDB CDC: starting change stream");
        let watch = collection
            .watch()
            .full_document(FullDocumentType::UpdateLookup);
        let mut stream = if let Some(token) = resume_token {
            watch
                .resume_after(token)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?
        } else {
            watch
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?
        };

        let namespace = format!(
            "mongodb.{}.{}",
            self.config.database, self.config.collection
        );
        let source_uri = format!(
            "mongodb://{}/{}",
            self.config.database, self.config.collection
        );
        let offset_key = OffsetKey {
            namespace: namespace.clone(),
            partition: String::new(),
        };

        while let Some(event) = stream
            .try_next()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
        {
            let (mutation, doc_json) = match event.operation_type {
                OperationType::Insert => match event.full_document {
                    Some(ref d) => (
                        MutationKind::Insert,
                        serde_json::to_string(d)
                            .map_err(|e| std::io::Error::other(e.to_string()))?,
                    ),
                    None => continue,
                },
                OperationType::Update | OperationType::Replace => match event.full_document {
                    Some(ref d) => (
                        MutationKind::Update,
                        serde_json::to_string(d)
                            .map_err(|e| std::io::Error::other(e.to_string()))?,
                    ),
                    None => continue,
                },
                OperationType::Delete => match event.document_key {
                    Some(ref key) => (
                        MutationKind::Delete,
                        serde_json::to_string(key)
                            .map_err(|e| std::io::Error::other(e.to_string()))?,
                    ),
                    None => continue,
                },
                _ => continue,
            };

            let event_id = serde_json::to_vec(&event.id).unwrap_or_default();

            let order_token = match event.cluster_time {
                Some(ts) => {
                    let t = (ts.time as u64) << 32 | (ts.increment as u64);
                    t.to_be_bytes().to_vec()
                }
                None => event_id.clone(),
            };

            let bytes = doc_json.len();
            let batch = IngestBatch {
                offset_key: offset_key.clone(),
                data: doc_json,
                bytes,
                source_uri: source_uri.clone(),
                namespace: Some(namespace.clone()),
                cdc_rows: Some(vec![WalRowMeta {
                    mutation,
                    event_id,
                    order_token,
                }]),
            };

            let mut ingest_tasks = IngestTasks::new();
            ingest_tasks.add(IngestTask::new(
                vec![batch],
                offsets.clone(),
                shared_output.clone(),
            ));
            self.ingest
                .ingest_file(&Arc::new(ingest_tasks), &offsets, shared_output.clone());

            if let Some(ref token) = stream.resume_token() {
                if let Ok(token_bytes) = serde_json::to_vec(token) {
                    let checkpoint = MongodbCheckpoint {
                        resume_token: token_bytes,
                    };
                    if let Err(err) = offsets.store_checkpoint_payload(
                        &checkpoint_key,
                        CheckpointAuthority::AdvisoryHint,
                        CheckpointKind::AdvisoryProgress,
                        1,
                        &checkpoint,
                    ) {
                        warn!("MongoDB CDC: failed to store checkpoint: {}", err);
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceMongodbPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        match self.cdc_mode() {
            SourceCdcMode::Snapshot => self.sync_query(offsets, shared_output, None).await,
            mode @ (SourceCdcMode::SnapshotThenCdc | SourceCdcMode::CdcOnly) => {
                self.sync_cdc(offsets, shared_output, mode).await
            }
        }
    }

    fn execution_contract(&self) -> SourceExecutionContract {
        let mode = self.cdc_mode();
        if mode.includes_cdc_stream() {
            SourceExecutionContract::configurable_cdc(
                mode,
                &source_capabilities::MONGODB,
                SourceOnceContract::HostIdleBounded,
            )
        } else {
            SourceExecutionContract::configurable_cdc(
                mode,
                &source_capabilities::MONGODB,
                SourceOnceContract::Finite,
            )
        }
    }
}
