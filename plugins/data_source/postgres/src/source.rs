use std::collections::HashMap;
use std::io;

use serde_derive::{Deserialize, Serialize};
use tokio::io::AsyncWrite;
use tokio_postgres::{NoTls, Row};
use tracing::{info, warn};

use crate::pgoutput::{self, PgColumn, PgOutputMessage};
use skippr::helpers::offsets::OffsetKey;
use skippr::ingest_work::IngestBatch;
use skippr::plugins::cdc::{
    CheckpointAuthority, CheckpointEnvelope, CheckpointKind, MutationKind, PostgresCheckpoint,
    WalRowMeta,
};
use skippr::runtime_plugins::framing::write_plugin_frame;
use skippr::runtime_plugins::protocol::{
    PluginFrame, RuntimeCheckpointUpdate, SourceEvent, SourceStartRequest,
};

#[derive(Debug, Deserialize, Serialize, Clone)]
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

#[derive(Clone, Debug)]
struct RuntimePostgresSource {
    config: DataSourcePostgresPluginConfig,
}

impl RuntimePostgresSource {
    fn new(config: DataSourcePostgresPluginConfig) -> Self {
        Self { config }
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

    async fn emit_batches<W>(&self, writer: &mut W, batches: Vec<IngestBatch>) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        write_plugin_frame(
            writer,
            &PluginFrame::SourceEvent(SourceEvent::IngestBatches { batches }),
        )
        .await
    }

    async fn emit_checkpoint<W>(
        &self,
        writer: &mut W,
        key: &str,
        kind: CheckpointKind,
        authority: CheckpointAuthority,
        lsn: u64,
        include_legacy_bytes: bool,
        slot_name: &str,
    ) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let payload = PostgresCheckpoint {
            lsn,
            slot_name: slot_name.to_string(),
        };
        let envelope = CheckpointEnvelope::from_payload(authority, kind, 1, &payload)
            .map_err(|err| io::Error::other(err.to_string()))?;
        let update = RuntimeCheckpointUpdate {
            key: key.to_string(),
            envelope,
            legacy_payload_bytes: include_legacy_bytes.then(|| lsn.to_be_bytes().to_vec()),
        };
        write_plugin_frame(
            writer,
            &PluginFrame::SourceEvent(SourceEvent::CheckpointUpdate(update)),
        )
        .await
    }

    fn parse_resume_lsn(start: &SourceStartRequest) -> Option<u64> {
        if let Some(resume) = start.resume_checkpoint.as_ref() {
            if let Ok(payload) = resume.into_payload::<PostgresCheckpoint>() {
                return Some(payload.lsn);
            }
        }
        start.legacy_resume_bytes.as_ref().map(|bytes| {
            let arr: [u8; 8] = bytes.as_slice().try_into().unwrap_or([0u8; 8]);
            u64::from_be_bytes(arr)
        })
    }

    pub async fn run<W>(&self, writer: &mut W, start: SourceStartRequest) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
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
        let checkpoint_key = format!("postgres:{slot_name}:lsn");
        let bootstrap_anchor_key = format!("postgres:{slot_name}:bootstrap_anchor");

        let (ddl_client, ddl_conn) = tokio_postgres::connect(&conn_str, NoTls)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        tokio::spawn(async move {
            if let Err(err) = ddl_conn.await {
                tracing::error!("Postgres DDL connection error: {}", err);
            }
        });

        let pub_sql = format!(
            "SELECT 1 FROM pg_publication WHERE pubname = '{}'",
            pub_name
        );
        let pub_rows = ddl_client
            .query(&pub_sql, &[])
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        if pub_rows.is_empty() {
            let create_pub = format!("CREATE PUBLICATION {} FOR ALL TABLES", pub_name);
            ddl_client
                .execute(&create_pub, &[])
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;
            info!("Created publication {}", pub_name);
        }

        let stored_lsn = Self::parse_resume_lsn(&start);
        let resume_mode = stored_lsn.is_some();

        let snapshot_lsn: u64 = if let Some(lsn_val) = stored_lsn {
            info!(
                "Runtime Postgres CDC: resuming from stored LSN {} (slot {})",
                lsn_val, slot_name
            );
            lsn_val
        } else {
            let slot_exists_sql = format!(
                "SELECT confirmed_flush_lsn::text AS lsn FROM pg_replication_slots WHERE slot_name = '{}'",
                slot_name
            );
            let existing_rows = ddl_client
                .query(&slot_exists_sql, &[])
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;

            if let Some(row) = existing_rows.first() {
                let lsn_str: String = row.get("lsn");
                let lsn_val = parse_pg_lsn(&lsn_str);
                info!(
                    "Runtime Postgres CDC: reusing existing slot {} at LSN {} ({})",
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
                    .map_err(|err| io::Error::other(err.to_string()))?;

                let consistent_point: String = slot_row.get("lsn");
                let lsn_val = parse_pg_lsn(&consistent_point);
                info!(
                    "Runtime Postgres CDC: created slot {} at LSN {} ({})",
                    slot_name, consistent_point, lsn_val
                );
                lsn_val
            }
        };

        let tables: Vec<String> = if resume_mode {
            info!("Runtime Postgres CDC: skipping snapshot (resuming from stored LSN)");
            Vec::new()
        } else {
            let table_rows = ddl_client
                .query(
                    "SELECT tablename FROM pg_tables WHERE schemaname = 'public'",
                    &[],
                )
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;

            if let Some(ref configured) = self.config.tables {
                configured.clone()
            } else {
                table_rows
                    .iter()
                    .map(|row| row.get::<_, String>(0))
                    .collect()
            }
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);
        let snapshot_lsn_typed = pgwire_replication::Lsn::from(snapshot_lsn);
        let lsn_id = Self::lsn_bytes(snapshot_lsn_typed);

        for table_name in &tables {
            let namespace = format!("postgres.{}", table_name);
            let offset_key = OffsetKey::new(namespace.clone(), table_name.clone());

            info!("Runtime Postgres CDC snapshot: reading {}", table_name);
            let query = format!("SELECT * FROM {}", table_name);
            let rows = ddl_client
                .query(&query, &[])
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;

            let mut current_batch = Vec::new();
            for row in &rows {
                let json_str = Self::row_to_json(row);
                let bytes = json_str.len();
                current_batch.push(IngestBatch::new(
                    offset_key.clone(),
                    json_str,
                    bytes,
                    format!("postgres://{}", table_name),
                    Some(namespace.clone()),
                    Some(vec![WalRowMeta {
                        mutation: MutationKind::Snapshot,
                        event_id: lsn_id.clone(),
                        order_token: lsn_id.clone(),
                    }]),
                ));

                if current_batch.len() >= batch_size {
                    self.emit_batches(writer, std::mem::take(&mut current_batch))
                        .await?;
                }
            }

            if !current_batch.is_empty() {
                self.emit_batches(writer, current_batch).await?;
            }
        }

        if !resume_mode {
            self.emit_checkpoint(
                writer,
                &bootstrap_anchor_key,
                CheckpointKind::BootstrapAnchor,
                CheckpointAuthority::WalOwnership,
                snapshot_lsn,
                false,
                &slot_name,
            )
            .await?;
            self.emit_checkpoint(
                writer,
                &checkpoint_key,
                CheckpointKind::SourceResume,
                CheckpointAuthority::WalOwnership,
                snapshot_lsn,
                true,
                &slot_name,
            )
            .await?;
        }

        info!("Runtime Postgres CDC snapshot complete, starting logical replication");

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
            .map_err(|err| io::Error::other(err.to_string()))?;
        let mut relation_map: HashMap<u32, (String, Vec<PgColumn>)> = HashMap::new();

        while let Some(event) = client
            .recv()
            .await
            .map_err(|err| io::Error::other(err.to_string()))?
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
                                    writer,
                                    table,
                                    cols,
                                    &new_row,
                                    MutationKind::Insert,
                                    &lsn_id,
                                )
                                .await?;
                            }
                        }
                        PgOutputMessage::Update { oid, new_row, .. } => {
                            if let Some((table, cols)) = relation_map.get(&oid) {
                                let lsn_id = Self::lsn_bytes(wal_end);
                                self.emit_cdc_row(
                                    writer,
                                    table,
                                    cols,
                                    &new_row,
                                    MutationKind::Update,
                                    &lsn_id,
                                )
                                .await?;
                            }
                        }
                        PgOutputMessage::Delete { oid, old_row } => {
                            if let Some((table, cols)) = relation_map.get(&oid) {
                                let lsn_id = Self::lsn_bytes(wal_end);
                                self.emit_cdc_row(
                                    writer,
                                    table,
                                    cols,
                                    &old_row,
                                    MutationKind::Delete,
                                    &lsn_id,
                                )
                                .await?;
                            }
                        }
                    }
                }
                ReplicationEvent::Commit { lsn, .. } => {
                    client.update_applied_lsn(lsn);
                    let lsn_u64: u64 = lsn.into();
                    self.emit_checkpoint(
                        writer,
                        &checkpoint_key,
                        CheckpointKind::SourceResume,
                        CheckpointAuthority::WalOwnership,
                        lsn_u64,
                        true,
                        &slot_name,
                    )
                    .await?;
                }
                ReplicationEvent::KeepAlive { .. } => {}
                ReplicationEvent::StoppedAt { .. } => {
                    info!("Runtime Postgres replication stream stopped");
                    break;
                }
                _ => {}
            }
        }

        write_plugin_frame(writer, &PluginFrame::SourceEvent(SourceEvent::Completed)).await
    }

    async fn emit_cdc_row<W>(
        &self,
        writer: &mut W,
        table: &str,
        columns: &[PgColumn],
        tuple: &[Option<String>],
        mutation: MutationKind,
        lsn_id: &[u8],
    ) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let namespace = format!("postgres.{}", table);
        let offset_key = OffsetKey::new(namespace.clone(), table.to_string());
        let json_str = Self::tuple_to_json(columns, tuple);
        let bytes = json_str.len();
        let batch = IngestBatch::new(
            offset_key,
            json_str,
            bytes,
            format!("postgres://{}", table),
            Some(namespace),
            Some(vec![WalRowMeta {
                mutation,
                event_id: lsn_id.to_vec(),
                order_token: lsn_id.to_vec(),
            }]),
        );
        self.emit_batches(writer, vec![batch]).await
    }
}

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

pub async fn run_runtime_postgres_source<W>(
    writer: &mut W,
    config: DataSourcePostgresPluginConfig,
    start: SourceStartRequest,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    RuntimePostgresSource::new(config).run(writer, start).await
}
