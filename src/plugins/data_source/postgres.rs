use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;
use tokio_postgres::{NoTls, Row};
use tracing::{info, warn};

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::cdc::{self, CheckpointEnvelope, MutationKind, SourceCapability, WalRowMeta};
use crate::plugins::data_source::pgoutput::{self, PgColumn, PgOutputMessage};
use crate::plugins::{DataSink, DataSource};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourcePostgresPluginConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub connection_string: Option<String>,
    pub tables: Option<Vec<String>>,
    pub query: Option<String>,
    pub batch_size_rows: Option<usize>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
    pub cdc_enabled: Option<bool>,
    pub replication_slot_name: Option<String>,
    pub publication_name: Option<String>,
}

impl From<DataSourcePluginConfig> for DataSourcePostgresPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Postgres(config) => config,
            _ => panic!("Invalid plugin type for Postgres input"),
        }
    }
}

pub struct DataSourcePostgresPlugin {
    ingest: Ingest,
    config: DataSourcePostgresPluginConfig,
}

impl DataSourcePostgresPlugin {
    pub async fn new() -> Self {
        let config: DataSourcePostgresPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(c) => c.into(),
                Err(_) => DataSourcePostgresPluginConfig {
                    host: Some(Config::getenv("POSTGRES_HOST", "localhost")),
                    port: Some(5432),
                    user: Some(Config::getenv("POSTGRES_USER", "postgres")),
                    password: Some(Config::getenv("POSTGRES_PASSWORD", "")),
                    database: Some(Config::getenv("POSTGRES_DATABASE", "")),
                    connection_string: None,
                    tables: None,
                    query: None,
                    batch_size_rows: None,
                    format: None,
                    batch_size_bytes: None,
                    batch_size_seconds: None,
                    cdc_enabled: None,
                    replication_slot_name: None,
                    publication_name: None,
                },
            };
        Self {
            ingest: Ingest::new(),
            config,
        }
    }

    fn connection_string(&self) -> String {
        if let Some(ref cs) = self.config.connection_string {
            return cs.clone();
        }
        format!(
            "host={} port={} user={} password={} dbname={}",
            self.config.host.as_deref().unwrap_or("localhost"),
            self.config.port.unwrap_or(5432),
            self.config.user.as_deref().unwrap_or("postgres"),
            self.config.password.as_deref().unwrap_or(""),
            self.config.database.as_deref().unwrap_or(""),
        )
    }

    fn row_to_json(row: &Row) -> String {
        let mut map = serde_json::Map::new();
        for (i, col) in row.columns().iter().enumerate() {
            let val: serde_json::Value = if let Ok(v) = row.try_get::<_, String>(i) {
                serde_json::Value::String(v)
            } else if let Ok(v) = row.try_get::<_, i64>(i) {
                serde_json::Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<_, i32>(i) {
                serde_json::Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<_, f64>(i) {
                serde_json::json!(v)
            } else if let Ok(v) = row.try_get::<_, bool>(i) {
                serde_json::Value::Bool(v)
            } else if let Ok(v) = row.try_get::<_, Option<String>>(i) {
                match v {
                    Some(s) => serde_json::Value::String(s),
                    None => serde_json::Value::Null,
                }
            } else {
                serde_json::Value::Null
            };
            map.insert(col.name().to_string(), val);
        }
        serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
    }

    fn is_cdc_enabled(&self) -> bool {
        self.config.cdc_enabled.unwrap_or(false)
    }

    fn tuple_to_json(columns: &[PgColumn], tuple: &[Option<String>]) -> String {
        let mut map = serde_json::Map::new();
        for (col, val) in columns.iter().zip(tuple.iter()) {
            let json_val = match val {
                Some(s) => serde_json::Value::String(s.clone()),
                None => serde_json::Value::Null,
            };
            map.insert(col.name.clone(), json_val);
        }
        serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
    }

    fn lsn_bytes(lsn: pgwire_replication::Lsn) -> Vec<u8> {
        lsn.as_u64().to_be_bytes().to_vec()
    }

    // -----------------------------------------------------------------------
    // Non-CDC (legacy) sync
    // -----------------------------------------------------------------------

    async fn sync_query(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let conn_str = self.connection_string();
        let (client, connection) = tokio_postgres::connect(&conn_str, NoTls)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!("Postgres connection error: {}", e);
            }
        });

        let queries: Vec<(String, String)> = if let Some(ref q) = self.config.query {
            vec![("query".to_string(), q.clone())]
        } else if let Some(ref tables) = self.config.tables {
            tables
                .iter()
                .map(|t| (t.clone(), format!("SELECT * FROM {}", t)))
                .collect()
        } else {
            let rows = client
                .query(
                    "SELECT tablename FROM pg_tables WHERE schemaname = 'public'",
                    &[],
                )
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            rows.iter()
                .map(|r| {
                    let name: String = r.get(0);
                    let q = format!("SELECT * FROM {}", name);
                    (name, q)
                })
                .collect()
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);

        for (table_name, query) in queries {
            let namespace = format!("postgres.{}", table_name);
            info!("Postgres input: querying {}", table_name);

            let rows = client
                .query(&query, &[])
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let offset_key = OffsetKey {
                namespace: namespace.clone(),
                partition: table_name.clone(),
            };

            let mut current_batch: Vec<IngestBatch> = Vec::new();

            for row in &rows {
                let json_str = Self::row_to_json(row);
                let bytes = json_str.len();
                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    source_uri: format!("postgres://{}", table_name),
                    namespace: Some(namespace.clone()),
                    cdc_rows: None,
                });

                if current_batch.len() >= batch_size {
                    let mut ingest_tasks = IngestTasks::new();
                    ingest_tasks.add(IngestTask::new(
                        std::mem::take(&mut current_batch),
                        offsets.clone(),
                        shared_output.clone(),
                    ));
                    self.ingest.ingest_file(
                        &Arc::new(ingest_tasks),
                        &offsets,
                        shared_output.clone(),
                    );
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
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // CDC sync via logical replication
    // -----------------------------------------------------------------------

    async fn sync_cdc(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        let conn_str = self.connection_string();
        let slot_name = self
            .config
            .replication_slot_name
            .clone()
            .unwrap_or_else(|| "skippr_slot".to_string());
        let pub_name = self
            .config
            .publication_name
            .clone()
            .unwrap_or_else(|| "skippr_publication".to_string());

        // --- DDL connection for setup + snapshot ---
        let (ddl_client, ddl_conn) = tokio_postgres::connect(&conn_str, NoTls)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        tokio::spawn(async move {
            if let Err(e) = ddl_conn.await {
                tracing::error!("Postgres DDL connection error: {}", e);
            }
        });

        // 1. Create publication if it does not exist
        let pub_sql = format!(
            "SELECT 1 FROM pg_publication WHERE pubname = '{}'",
            pub_name
        );
        let pub_rows = ddl_client
            .query(&pub_sql, &[])
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if pub_rows.is_empty() {
            let create_pub = format!("CREATE PUBLICATION {} FOR ALL TABLES", pub_name);
            ddl_client
                .execute(&create_pub, &[])
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            info!("Created publication {}", pub_name);
        }

        // 2. Reuse existing slot or create a new one.
        let checkpoint_key = format!("postgres:{}:lsn", slot_name);
        let stored_lsn = offsets.load_checkpoint(&checkpoint_key);
        let resume_mode = stored_lsn.is_some();

        let snapshot_lsn: u64 = if let Some(ref lsn_bytes) = stored_lsn {
            let arr: [u8; 8] = lsn_bytes.as_slice().try_into().unwrap_or([0u8; 8]);
            let lsn_val = u64::from_be_bytes(arr);
            info!(
                "Postgres CDC: resuming from stored LSN {} (slot {})",
                lsn_val, slot_name
            );
            lsn_val
        } else {
            // Check whether slot already exists
            let slot_exists_sql = format!(
                "SELECT confirmed_flush_lsn::text AS lsn FROM pg_replication_slots WHERE slot_name = '{}'",
                slot_name
            );
            let existing_rows = ddl_client
                .query(&slot_exists_sql, &[])
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            if let Some(row) = existing_rows.first() {
                let lsn_str: String = row.get("lsn");
                let lsn_val = parse_pg_lsn(&lsn_str);
                info!(
                    "Postgres CDC: reusing existing slot {} at LSN {} ({})",
                    slot_name, lsn_str, lsn_val
                );
                lsn_val
            } else {
                let slot_sql = format!(
                    "SELECT lsn::text AS lsn FROM pg_create_logical_replication_slot('{}', 'pgoutput')",
                    slot_name
                );
                let slot_row = ddl_client
                    .query_one(&slot_sql, &[])
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;

                let consistent_point: String = slot_row.get("lsn");
                let lsn_val = parse_pg_lsn(&consistent_point);
                info!(
                    "Postgres CDC: created slot {} at LSN {} ({})",
                    slot_name, consistent_point, lsn_val
                );
                lsn_val
            }
        };

        // 3. Initial snapshot (skipped on resume)
        let tables: Vec<String> = if resume_mode {
            info!("Postgres CDC: skipping snapshot (resuming from stored LSN)");
            Vec::new()
        } else {
            let table_rows = ddl_client
                .query(
                    "SELECT tablename FROM pg_tables WHERE schemaname = 'public'",
                    &[],
                )
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            if let Some(ref configured) = self.config.tables {
                configured.clone()
            } else {
                table_rows.iter().map(|r| r.get::<_, String>(0)).collect()
            }
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);
        let snapshot_lsn_typed = pgwire_replication::Lsn::from(snapshot_lsn);
        let lsn_id = Self::lsn_bytes(snapshot_lsn_typed);

        for table_name in &tables {
            let namespace = format!("postgres.{}", table_name);
            let offset_key = OffsetKey {
                namespace: namespace.clone(),
                partition: table_name.clone(),
            };

            info!("CDC snapshot: reading {}", table_name);
            let query = format!("SELECT * FROM {}", table_name);
            let rows = ddl_client
                .query(&query, &[])
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let mut current_batch: Vec<IngestBatch> = Vec::new();

            for row in &rows {
                let json_str = Self::row_to_json(row);
                let bytes = json_str.len();
                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    source_uri: format!("postgres://{}", table_name),
                    namespace: Some(namespace.clone()),
                    cdc_rows: Some(vec![WalRowMeta {
                        mutation: MutationKind::Snapshot,
                        event_id: lsn_id.clone(),
                        order_token: lsn_id.clone(),
                    }]),
                });

                if current_batch.len() >= batch_size {
                    let mut ingest_tasks = IngestTasks::new();
                    ingest_tasks.add(IngestTask::new(
                        std::mem::take(&mut current_batch),
                        offsets.clone(),
                        shared_output.clone(),
                    ));
                    self.ingest.ingest_file(
                        &Arc::new(ingest_tasks),
                        &offsets,
                        shared_output.clone(),
                    );
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
        }

        info!("CDC snapshot complete, switching to logical replication stream");

        // 4. Open replication stream via pgwire-replication
        use pgwire_replication::{ReplicationClient, ReplicationConfig, ReplicationEvent};

        let repl_config = ReplicationConfig {
            host: self
                .config
                .host
                .clone()
                .unwrap_or_else(|| "localhost".into()),
            port: self.config.port.unwrap_or(5432),
            user: self
                .config
                .user
                .clone()
                .unwrap_or_else(|| "postgres".into()),
            password: self.config.password.clone().unwrap_or_default(),
            database: self.config.database.clone().unwrap_or_default(),
            slot: slot_name.clone(),
            publication: pub_name.clone(),
            start_lsn: snapshot_lsn_typed,
            ..Default::default()
        };

        let mut client = ReplicationClient::connect(repl_config)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // OID → (schema.table, columns) relation cache
        let mut relation_map: HashMap<u32, (String, Vec<PgColumn>)> = HashMap::new();

        while let Some(event) = client
            .recv()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
        {
            match event {
                ReplicationEvent::XLogData { data, wal_end, .. } => {
                    let Some(msg) = pgoutput::parse(&data) else {
                        continue;
                    };

                    match msg {
                        PgOutputMessage::Relation {
                            oid,
                            schema,
                            name,
                            columns,
                        } => {
                            let qualified = if schema == "public" {
                                name.clone()
                            } else {
                                format!("{}.{}", schema, name)
                            };
                            relation_map.insert(oid, (qualified, columns));
                        }
                        PgOutputMessage::Insert { oid, new_row } => {
                            if let Some((table, cols)) = relation_map.get(&oid) {
                                let lsn_id = Self::lsn_bytes(wal_end);
                                self.emit_cdc_row(
                                    table,
                                    cols,
                                    &new_row,
                                    MutationKind::Insert,
                                    &lsn_id,
                                    &offsets,
                                    &shared_output,
                                );
                            }
                        }
                        PgOutputMessage::Update { oid, new_row, .. } => {
                            if let Some((table, cols)) = relation_map.get(&oid) {
                                let lsn_id = Self::lsn_bytes(wal_end);
                                self.emit_cdc_row(
                                    table,
                                    cols,
                                    &new_row,
                                    MutationKind::Update,
                                    &lsn_id,
                                    &offsets,
                                    &shared_output,
                                );
                            }
                        }
                        PgOutputMessage::Delete { oid, old_row } => {
                            if let Some((table, cols)) = relation_map.get(&oid) {
                                let lsn_id = Self::lsn_bytes(wal_end);
                                self.emit_cdc_row(
                                    table,
                                    cols,
                                    &old_row,
                                    MutationKind::Delete,
                                    &lsn_id,
                                    &offsets,
                                    &shared_output,
                                );
                            }
                        }
                    }
                }
                ReplicationEvent::Commit { lsn, .. } => {
                    client.update_applied_lsn(lsn);
                    let lsn_u64: u64 = lsn.into();
                    offsets.store_checkpoint(&checkpoint_key, &lsn_u64.to_be_bytes());
                }
                ReplicationEvent::KeepAlive { .. } => {}
                ReplicationEvent::StoppedAt { .. } => {
                    info!("Replication stream stopped");
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn emit_cdc_row(
        &self,
        table: &str,
        columns: &[PgColumn],
        tuple: &[Option<String>],
        mutation: MutationKind,
        lsn_id: &[u8],
        offsets: &Arc<Offsets>,
        shared_output: &Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        let namespace = format!("postgres.{}", table);
        let offset_key = OffsetKey {
            namespace: namespace.clone(),
            partition: table.to_string(),
        };

        let json_str = Self::tuple_to_json(columns, tuple);
        let bytes = json_str.len();

        let batch = IngestBatch {
            offset_key,
            data: json_str,
            bytes,
            source_uri: format!("postgres://{}", table),
            namespace: Some(namespace),
            cdc_rows: Some(vec![WalRowMeta {
                mutation,
                event_id: lsn_id.to_vec(),
                order_token: lsn_id.to_vec(),
            }]),
        };

        let mut ingest_tasks = IngestTasks::new();
        ingest_tasks.add(IngestTask::new(
            vec![batch],
            offsets.clone(),
            shared_output.clone(),
        ));
        self.ingest
            .ingest_file(&Arc::new(ingest_tasks), offsets, shared_output.clone());
    }
}

/// Parse a Postgres LSN string like "0/1696D50" into a u64.
fn parse_pg_lsn(lsn_str: &str) -> u64 {
    let parts: Vec<&str> = lsn_str.split('/').collect();
    if parts.len() != 2 {
        warn!("Invalid LSN format: {}", lsn_str);
        return 0;
    }
    let high = u64::from_str_radix(parts[0], 16).unwrap_or(0);
    let low = u64::from_str_radix(parts[1], 16).unwrap_or(0);
    (high << 32) | low
}

#[async_trait]
impl DataSource for DataSourcePostgresPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        if self.is_cdc_enabled() {
            self.sync_cdc(offsets, shared_output).await
        } else {
            self.sync_query(offsets, shared_output).await
        }
    }

    fn capability(&self) -> Option<&'static SourceCapability> {
        if self.is_cdc_enabled() {
            Some(&cdc::source_capabilities::POSTGRES)
        } else {
            None
        }
    }

    fn capture_bootstrap_anchor(&self) -> Option<CheckpointEnvelope> {
        // Only applicable when CDC is enabled and we've captured a snapshot LSN
        None // Will be populated when replication slot is created during sync_cdc
    }
}
