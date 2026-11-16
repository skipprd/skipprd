use skippr_runtime_sdk::SkipprConfig;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pgwire_replication::{Lsn, ReplicationClient, ReplicationConfig, ReplicationEvent};
use serde_derive::{Deserialize, Serialize};
use tokio_postgres::{NoTls, Row};
use tracing::{info, warn};

use crate::pgoutput::{self, PgColumn, PgOutputMessage};
use skippr_runtime_sdk::helpers::configuration::{runtime_child_config, Config};
use skippr_runtime_sdk::plugins::cdc::{
    before_image, source_capabilities, BeforeImage, MutationKind, PostgresCheckpoint, WalRowMeta,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceCdcMode, SourceExecutionContract, SourceOnceContract,
};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{
    load_checkpoint_payload, store_checkpoint_payload, submit_payload_batches_durable,
    take_row_count_batch, IngestBatch, SourceSyncContext,
};

#[derive(Debug, Deserialize, SkipprConfig, Serialize, Clone)]
pub struct DataSourcePostgresPluginConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    #[skippr(secret)]
    pub password: Option<String>,
    pub database: Option<String>,
    #[skippr(secret)]
    pub connection_string: Option<String>,
    pub tables: Option<Vec<String>>,
    pub query: Option<String>,
    pub batch_size_rows: Option<usize>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
    #[serde(default)]
    pub cdc_mode: SourceCdcMode,
    pub replication_slot_name: Option<String>,
    pub publication_name: Option<String>,
    #[serde(default)]
    pub cdc_idle_timeout_seconds: Option<u64>,
}

enum CdcFlush {
    Idle,
    Commit { lsn: u64 },
}

pub struct DataSourcePostgresPlugin {
    config: DataSourcePostgresPluginConfig,
}

impl DataSourcePostgresPlugin {
    pub fn with_runtime_config(config: DataSourcePostgresPluginConfig) -> Self {
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

    fn slot_name(&self) -> String {
        self.config
            .replication_slot_name
            .clone()
            .unwrap_or_else(|| "skippr_slot".to_string())
    }

    fn publication_name(&self) -> String {
        self.config
            .publication_name
            .clone()
            .unwrap_or_else(|| "skippr_publication".to_string())
    }

    fn cdc_mode(&self) -> SourceCdcMode {
        self.config.cdc_mode
    }

    fn checkpoint_key(slot_name: &str) -> String {
        format!("postgres:{slot_name}:lsn")
    }

    fn stored_resume_lsn(
        &self,
        ctx: &dyn SourceSyncContext,
        slot_name: &str,
    ) -> Result<Option<u64>, io::Error> {
        Ok(
            load_checkpoint_payload::<PostgresCheckpoint>(ctx, &Self::checkpoint_key(slot_name))?
                .map(|checkpoint| checkpoint.lsn),
        )
    }

    fn store_resume_checkpoint(
        &self,
        ctx: &dyn SourceSyncContext,
        slot_name: &str,
        lsn: u64,
    ) -> io::Result<()> {
        store_checkpoint_payload(
            ctx,
            &Self::checkpoint_key(slot_name),
            &PostgresCheckpoint {
                lsn,
                slot_name: slot_name.to_string(),
            },
        )
    }

    fn ingest_batches(
        &self,
        batches: Vec<IngestBatch>,
        ctx: &dyn SourceSyncContext,
    ) -> io::Result<()> {
        if batches.is_empty() {
            return Ok(());
        }
        submit_payload_batches_durable(ctx, batches).map(|_| ())
    }

    fn prepare_cdc_flush(mut batches: Vec<IngestBatch>, flush: CdcFlush) -> Vec<IngestBatch> {
        if let CdcFlush::Commit { lsn } = flush {
            Self::stamp_source_transaction(&mut batches, lsn);
        }
        batches
    }

    fn flush_cdc(
        &self,
        batches: Vec<IngestBatch>,
        flush: CdcFlush,
        ctx: &dyn SourceSyncContext,
    ) -> io::Result<()> {
        self.ingest_batches(Self::prepare_cdc_flush(batches, flush), ctx)
    }

    fn idle_timeout_stop(
        &self,
        idle: Duration,
        pending_batches: &mut Vec<IngestBatch>,
        ctx: &dyn SourceSyncContext,
    ) -> io::Result<()> {
        info!(
            "Runtime Postgres CDC idle timeout reached after {}s; stopping stream",
            idle.as_secs()
        );
        if !pending_batches.is_empty() {
            self.flush_cdc(std::mem::take(pending_batches), CdcFlush::Idle, ctx)?;
        }
        Ok(())
    }

    fn replica_before(cols: &[PgColumn], tuple: &[Option<String>]) -> BeforeImage {
        let owned = Self::replica_key_parts(cols, tuple);
        let parts: Vec<(&[u8], &[u8])> = owned
            .iter()
            .map(|(name, value)| (name.as_slice(), value.as_slice()))
            .collect();
        before_image(&parts, Self::tuple_to_json(cols, tuple).into_bytes())
    }

    fn update_before(
        cols: &[PgColumn],
        old_row: Option<&[Option<String>]>,
    ) -> io::Result<BeforeImage> {
        let old = old_row.ok_or_else(|| {
            io::Error::other(
                "Postgres CDC UPDATE is missing replica identity old row (require replica identity)",
            )
        })?;
        Ok(Self::replica_before(cols, old))
    }

    fn snapshot_cdc_rows(lsn_id: &[u8]) -> Vec<WalRowMeta> {
        vec![WalRowMeta::new(
            MutationKind::Snapshot,
            lsn_id.to_vec(),
            lsn_id.to_vec(),
        )]
    }

    fn cdc_batch(
        &self,
        ingest_config: &Config,
        table: &str,
        columns: &[PgColumn],
        tuple: &[Option<String>],
        mutation: MutationKind,
        lsn: u64,
        before: Option<BeforeImage>,
    ) -> IngestBatch {
        let namespace = format!("postgres.{}", table);
        let offset_key = OffsetKey::new(namespace.clone(), table.to_string());
        let json_str = Self::tuple_to_json(columns, tuple);
        let bytes = json_str.len();
        let lsn_id = lsn.to_be_bytes().to_vec();
        let mut meta = WalRowMeta::new(mutation, lsn_id.clone(), lsn_id);
        if let Some(before) = before {
            meta = meta.with_before(before);
        }
        IngestBatch::new_with_offset_pos(
            ingest_config,
            offset_key,
            json_str,
            bytes,
            lsn,
            format!("postgres://{}", table),
            Some(namespace),
            Some(vec![meta]),
        )
    }

    fn replica_key_parts(cols: &[PgColumn], tuple: &[Option<String>]) -> Vec<(Vec<u8>, Vec<u8>)> {
        cols.iter()
            .zip(tuple.iter())
            .filter_map(|(col, val)| {
                val.as_ref()
                    .map(|value| (col.name.as_bytes().to_vec(), value.as_bytes().to_vec()))
            })
            .collect()
    }

    fn replica_business_key(cols: &[PgColumn], tuple: &[Option<String>]) -> Vec<u8> {
        Self::replica_before(cols, tuple).key
    }

    fn stamp_source_transaction(batches: &mut [IngestBatch], commit_lsn: u64) {
        let id = commit_lsn.to_be_bytes().to_vec();
        for batch in batches {
            if let Some(rows) = batch.cdc_rows.as_mut() {
                for row in rows {
                    row.stamp_source_transaction_id(id.clone());
                }
            }
        }
    }

    async fn sync_snapshot(&mut self, ctx: Arc<dyn SourceSyncContext>) -> io::Result<()> {
        let ingest_config = runtime_child_config();
        let conn_str = self.connection_string();
        let (client, conn) = tokio_postgres::connect(&conn_str, NoTls)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                tracing::error!("Postgres snapshot connection error: {}", err);
            }
        });

        let tables = if let Some(ref configured) = self.config.tables {
            configured.clone()
        } else {
            client
                .query(
                    "SELECT tablename FROM pg_tables WHERE schemaname = 'public'",
                    &[],
                )
                .await
                .map_err(|err| io::Error::other(err.to_string()))?
                .iter()
                .map(|row| row.get::<_, String>(0))
                .collect()
        };

        let batch_size = self.config.batch_size_rows.unwrap_or(10_000);
        for table_name in &tables {
            let namespace = format!("postgres.{}", table_name);
            let offset_key = OffsetKey::new(namespace.clone(), table_name.clone());

            info!("Runtime Postgres snapshot: reading {}", table_name);
            let rows = client
                .query(&format!("SELECT * FROM {}", table_name), &[])
                .await
                .map_err(|err| io::Error::other(err.to_string()))?;

            let mut current_batch = Vec::new();
            for row in &rows {
                let json_str = Self::row_to_json(row);
                let bytes = json_str.len();
                current_batch.push(IngestBatch::new(
                    &ingest_config,
                    offset_key.clone(),
                    json_str,
                    bytes,
                    format!("postgres://{}", table_name),
                    Some(namespace.clone()),
                    None,
                ));

                if let Some(full) = take_row_count_batch(&mut current_batch, batch_size) {
                    self.ingest_batches(full, ctx.as_ref())?;
                }
            }

            if !current_batch.is_empty() {
                self.ingest_batches(current_batch, ctx.as_ref())?;
            }
        }

        info!("Runtime Postgres snapshot complete; CDC disabled");
        Ok(())
    }

    async fn sync_cdc(
        &mut self,
        ctx: Arc<dyn SourceSyncContext>,
        mode: SourceCdcMode,
    ) -> io::Result<()> {
        let conn_str = self.connection_string();
        let slot_name = self.slot_name();
        let pub_name = self.publication_name();

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
        let pub_rows = ddl_client.query(&pub_sql, &[]).await.map_err(|err| {
            io::Error::other(format!(
                "checking Postgres publication {pub_name} failed: {err:?}"
            ))
        })?;
        if pub_rows.is_empty() {
            let create_pub = format!("CREATE PUBLICATION {} FOR ALL TABLES", pub_name);
            ddl_client.execute(&create_pub, &[]).await.map_err(|err| {
                io::Error::other(format!(
                    "creating Postgres publication {pub_name} failed: {err:?}"
                ))
            })?;
            info!("Created publication {}", pub_name);
        }

        let ingest_config = runtime_child_config();
        let stored_lsn = self.stored_resume_lsn(ctx.as_ref(), &slot_name)?;
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
                let slot_row = ddl_client.query_one(&slot_sql, &[]).await.map_err(|err| {
                    io::Error::other(format!(
                        "creating Postgres logical replication slot {slot_name} failed: {err:?}"
                    ))
                })?;

                let consistent_point: String = slot_row.get("lsn");
                let lsn_val = parse_pg_lsn(&consistent_point);
                info!(
                    "Runtime Postgres CDC: created slot {} at LSN {} ({})",
                    slot_name, consistent_point, lsn_val
                );
                lsn_val
            }
        };

        let should_run_snapshot = mode.includes_initial_snapshot() && !resume_mode;
        let tables: Vec<String> = if should_run_snapshot {
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
        } else {
            match (resume_mode, mode) {
                (true, _) => {
                    info!("Runtime Postgres CDC: skipping snapshot (resuming from stored LSN)")
                }
                (false, SourceCdcMode::CdcOnly) => {
                    info!("Runtime Postgres CDC: cdc_only mode skips initial snapshot")
                }
                _ => {}
            }
            Vec::new()
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
                current_batch.push(IngestBatch::new_with_offset_pos(
                    &ingest_config,
                    offset_key.clone(),
                    json_str,
                    bytes,
                    snapshot_lsn,
                    format!("postgres://{}", table_name),
                    Some(namespace.clone()),
                    Some(Self::snapshot_cdc_rows(&lsn_id)),
                ));

                if let Some(full) = take_row_count_batch(&mut current_batch, batch_size) {
                    self.ingest_batches(full, ctx.as_ref())?;
                }
            }

            if !current_batch.is_empty() {
                self.ingest_batches(current_batch, ctx.as_ref())?;
            }
        }

        if should_run_snapshot {
            self.store_resume_checkpoint(ctx.as_ref(), &slot_name, snapshot_lsn)?;
        }

        info!("Runtime Postgres CDC bootstrap complete, starting logical replication");

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
            .map_err(|err| {
                io::Error::other(format!(
                    "connecting Postgres logical replication stream for slot {slot_name} failed: {err:?}"
                ))
            })?;
        let mut relation_map: HashMap<u32, (String, Vec<PgColumn>)> = HashMap::new();
        let mut pending_batches = Vec::new();
        let idle_timeout = self
            .config
            .cdc_idle_timeout_seconds
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs);
        let mut last_data = std::time::Instant::now();

        loop {
            let next_event = if let Some(idle_timeout) = idle_timeout {
                let Some(remaining) = idle_remaining(idle_timeout, last_data.elapsed()) else {
                    self.idle_timeout_stop(idle_timeout, &mut pending_batches, ctx.as_ref())?;
                    break;
                };
                match tokio::time::timeout(remaining, client.recv()).await {
                    Ok(result) => result.map_err(|err| io::Error::other(err.to_string()))?,
                    Err(_) => {
                        self.idle_timeout_stop(idle_timeout, &mut pending_batches, ctx.as_ref())?;
                        break;
                    }
                }
            } else {
                client
                    .recv()
                    .await
                    .map_err(|err| io::Error::other(err.to_string()))?
            };
            let Some(event) = next_event else {
                break;
            };
            match cdc_idle_kind(&event) {
                CdcIdleKind::Heartbeat => continue,
                CdcIdleKind::Stop => {
                    info!("Runtime Postgres replication stream stopped");
                    break;
                }
                CdcIdleKind::Data => last_data = std::time::Instant::now(),
            }
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
                                pending_batches.push(self.cdc_batch(
                                    &ingest_config,
                                    table,
                                    cols,
                                    &new_row,
                                    MutationKind::Insert,
                                    wal_end.as_u64(),
                                    None,
                                ));
                            }
                        }
                        PgOutputMessage::Update {
                            oid,
                            new_row,
                            old_row,
                        } => {
                            if let Some((table, cols)) = relation_map.get(&oid) {
                                let before = Self::update_before(cols, old_row.as_deref())?;
                                pending_batches.push(self.cdc_batch(
                                    &ingest_config,
                                    table,
                                    cols,
                                    &new_row,
                                    MutationKind::Update,
                                    wal_end.as_u64(),
                                    Some(before),
                                ));
                            }
                        }
                        PgOutputMessage::Delete { oid, old_row } => {
                            if let Some((table, cols)) = relation_map.get(&oid) {
                                let before = Self::replica_before(cols, &old_row);
                                pending_batches.push(self.cdc_batch(
                                    &ingest_config,
                                    table,
                                    cols,
                                    &old_row,
                                    MutationKind::Delete,
                                    wal_end.as_u64(),
                                    Some(before),
                                ));
                            }
                        }
                    }
                }
                ReplicationEvent::Commit { lsn, .. } => {
                    let lsn_u64: u64 = lsn.into();
                    self.flush_cdc(
                        std::mem::take(&mut pending_batches),
                        CdcFlush::Commit { lsn: lsn_u64 },
                        ctx.as_ref(),
                    )?;
                    client.update_applied_lsn(lsn);
                    if let Err(err) =
                        self.store_resume_checkpoint(ctx.as_ref(), &slot_name, lsn_u64)
                    {
                        warn!("trailing Postgres LSN store failed after WAL durability: {err}");
                    }
                }
                ReplicationEvent::KeepAlive { .. } | ReplicationEvent::StoppedAt { .. } => {
                    unreachable!("classified before this match")
                }
                _ => {}
            }
        }

        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourcePostgresPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        match self.cdc_mode() {
            SourceCdcMode::Snapshot => self.sync_snapshot(ctx).await,
            mode @ (SourceCdcMode::SnapshotThenCdc | SourceCdcMode::CdcOnly) => {
                self.sync_cdc(ctx, mode).await
            }
        }
    }

    fn execution_contract(&self) -> SourceExecutionContract {
        let mode = self.cdc_mode();
        if mode.includes_cdc_stream() {
            let once = if self
                .config
                .cdc_idle_timeout_seconds
                .filter(|s| *s > 0)
                .is_some()
            {
                SourceOnceContract::PluginIdleBounded
            } else {
                SourceOnceContract::HostIdleBounded
            };
            SourceExecutionContract::configurable_cdc(mode, &source_capabilities::POSTGRES, once)
        } else {
            SourceExecutionContract::configurable_cdc(
                mode,
                &source_capabilities::POSTGRES,
                SourceOnceContract::Finite,
            )
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CdcIdleKind {
    Data,
    Heartbeat,
    Stop,
}

fn cdc_idle_kind(event: &ReplicationEvent) -> CdcIdleKind {
    match event {
        ReplicationEvent::KeepAlive { .. } => CdcIdleKind::Heartbeat,
        ReplicationEvent::StoppedAt { .. } => CdcIdleKind::Stop,
        _ => CdcIdleKind::Data,
    }
}

fn idle_remaining(idle: Duration, since_data: Duration) -> Option<Duration> {
    idle.checked_sub(since_data).filter(|left| !left.is_zero())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(cdc_mode: SourceCdcMode) -> DataSourcePostgresPlugin {
        DataSourcePostgresPlugin::with_runtime_config(DataSourcePostgresPluginConfig {
            host: Some("localhost".to_string()),
            port: Some(5432),
            user: Some("postgres".to_string()),
            password: Some("postgres".to_string()),
            database: Some("postgres".to_string()),
            connection_string: None,
            tables: Some(vec!["people".to_string()]),
            query: None,
            batch_size_rows: None,
            format: None,
            batch_size_bytes: None,
            batch_size_seconds: None,
            cdc_mode,
            replication_slot_name: None,
            publication_name: None,
            cdc_idle_timeout_seconds: None,
        })
    }

    #[test]
    fn snapshot_then_cdc_reports_cdc_capability() {
        let plugin = plugin(SourceCdcMode::SnapshotThenCdc);
        assert_eq!(plugin.cdc_mode(), SourceCdcMode::SnapshotThenCdc);
        assert!(plugin.capability().is_some());
    }

    #[test]
    fn snapshot_mode_has_no_cdc_capability() {
        let plugin = plugin(SourceCdcMode::Snapshot);
        assert_eq!(plugin.cdc_mode(), SourceCdcMode::Snapshot);
        assert!(plugin.capability().is_none());
    }

    #[test]
    fn cdc_only_reports_cdc_capability() {
        let plugin = plugin(SourceCdcMode::CdcOnly);
        assert_eq!(plugin.cdc_mode(), SourceCdcMode::CdcOnly);
        assert!(plugin.capability().is_some());
    }

    fn keepalive() -> ReplicationEvent {
        ReplicationEvent::KeepAlive {
            wal_end: Lsn::from_u64(1),
            reply_requested: true,
            server_time_micros: 0,
        }
    }

    #[test]
    fn keepalive_does_not_reset_cdc_idle() {
        assert_eq!(cdc_idle_kind(&keepalive()), CdcIdleKind::Heartbeat);
        assert_eq!(
            cdc_idle_kind(&ReplicationEvent::StoppedAt {
                reached: Lsn::from_u64(1)
            }),
            CdcIdleKind::Stop
        );
        assert_eq!(
            cdc_idle_kind(&ReplicationEvent::Commit {
                lsn: Lsn::from_u64(2),
                end_lsn: Lsn::from_u64(2),
                commit_time_micros: 0,
            }),
            CdcIdleKind::Data
        );
    }

    #[test]
    fn idle_remaining_expires_while_heartbeats_arrive() {
        assert_eq!(
            idle_remaining(Duration::from_secs(5), Duration::from_secs(1)),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            idle_remaining(Duration::from_secs(5), Duration::from_secs(5)),
            None
        );
        assert_eq!(
            idle_remaining(Duration::from_secs(5), Duration::from_secs(6)),
            None
        );
    }

    #[test]
    fn commit_stamps_one_source_transaction_id_on_every_row() {
        let plugin = plugin(SourceCdcMode::CdcOnly);
        let cfg = runtime_child_config();
        let cols = [PgColumn {
            name: "id".into(),
            type_oid: 23,
        }];
        let batches = vec![
            plugin.cdc_batch(
                &cfg,
                "people",
                &cols,
                &[Some("1".into())],
                MutationKind::Insert,
                1,
                None,
            ),
            plugin.cdc_batch(
                &cfg,
                "people",
                &cols,
                &[Some("2".into())],
                MutationKind::Insert,
                2,
                None,
            ),
        ];
        let batches =
            DataSourcePostgresPlugin::prepare_cdc_flush(batches, CdcFlush::Commit { lsn: 99 });
        let ids: Vec<_> = batches
            .iter()
            .flat_map(|batch| {
                batch
                    .cdc_rows
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|row| row.source_transaction_id.clone())
            })
            .collect();
        let expected = 99u64.to_be_bytes().to_vec();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| id.as_ref() == Some(&expected)));
    }

    #[test]
    fn update_without_old_row_is_rejected() {
        let cols = [PgColumn {
            name: "id".into(),
            type_oid: 23,
        }];
        let err = DataSourcePostgresPlugin::update_before(&cols, None).unwrap_err();
        assert!(err.to_string().contains("missing replica identity old row"));
        let before =
            DataSourcePostgresPlugin::update_before(&cols, Some(&[Some("7".into())])).unwrap();
        assert_eq!(
            before.key,
            DataSourcePostgresPlugin::replica_business_key(&cols, &[Some("7".into())])
        );
    }

    #[test]
    fn delete_carries_the_old_row_as_before() {
        let cols = [PgColumn {
            name: "id".into(),
            type_oid: 23,
        }];
        let before = DataSourcePostgresPlugin::replica_before(&cols, &[Some("7".into())]);
        assert_eq!(
            before.key,
            DataSourcePostgresPlugin::replica_business_key(&cols, &[Some("7".into())])
        );
        assert!(!before.row.is_empty());
    }

    #[test]
    fn idle_flush_does_not_invent_a_source_transaction_id() {
        let plugin = plugin(SourceCdcMode::CdcOnly);
        let cfg = runtime_child_config();
        let cols = [PgColumn {
            name: "id".into(),
            type_oid: 23,
        }];
        let batch = plugin.cdc_batch(
            &cfg,
            "people",
            &cols,
            &[Some("1".into())],
            MutationKind::Insert,
            1,
            None,
        );
        let flushed = DataSourcePostgresPlugin::prepare_cdc_flush(vec![batch], CdcFlush::Idle);
        assert!(flushed[0].cdc_rows.as_ref().unwrap()[0]
            .source_transaction_id
            .is_none());
    }

    #[test]
    fn idle_then_commit_are_two_flushes() {
        let plugin = plugin(SourceCdcMode::CdcOnly);
        let cfg = runtime_child_config();
        let cols = [PgColumn {
            name: "id".into(),
            type_oid: 23,
        }];
        let idle_row = plugin.cdc_batch(
            &cfg,
            "people",
            &cols,
            &[Some("1".into())],
            MutationKind::Insert,
            1,
            None,
        );
        let commit_row = plugin.cdc_batch(
            &cfg,
            "people",
            &cols,
            &[Some("2".into())],
            MutationKind::Insert,
            2,
            None,
        );
        let idle = DataSourcePostgresPlugin::prepare_cdc_flush(vec![idle_row], CdcFlush::Idle);
        let commit = DataSourcePostgresPlugin::prepare_cdc_flush(
            vec![commit_row],
            CdcFlush::Commit { lsn: 50 },
        );
        assert!(idle[0].cdc_rows.as_ref().unwrap()[0]
            .source_transaction_id
            .is_none());
        assert_eq!(
            commit[0].cdc_rows.as_ref().unwrap()[0]
                .source_transaction_id
                .as_deref(),
            Some(50u64.to_be_bytes().as_slice())
        );
    }

    #[test]
    fn snapshot_rows_do_not_invent_a_source_transaction_id() {
        let rows = DataSourcePostgresPlugin::snapshot_cdc_rows(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mutation, MutationKind::Snapshot);
        assert!(rows[0].source_transaction_id.is_none());
    }

    #[test]
    fn snapshot_row_count_batches_split_at_the_cap() {
        let plugin = plugin(SourceCdcMode::Snapshot);
        let cfg = runtime_child_config();
        let cols = [PgColumn {
            name: "id".into(),
            type_oid: 23,
        }];
        let mut current = vec![
            plugin.cdc_batch(
                &cfg,
                "people",
                &cols,
                &[Some("1".into())],
                MutationKind::Insert,
                1,
                None,
            ),
            plugin.cdc_batch(
                &cfg,
                "people",
                &cols,
                &[Some("2".into())],
                MutationKind::Insert,
                2,
                None,
            ),
        ];
        let full = take_row_count_batch(&mut current, 2).unwrap();
        assert_eq!(full.len(), 2);
        assert!(current.is_empty());
        assert!(take_row_count_batch(&mut current, 2).is_none());
    }
}
