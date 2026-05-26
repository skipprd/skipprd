use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_derive::{Deserialize, Serialize};
use tokio_postgres::{NoTls, Row};
use tracing::{info, warn};

use crate::pgoutput::{self, PgColumn, PgOutputMessage};
use skippr_runtime_sdk::plugins::cdc::{
    source_capabilities, MutationKind, PostgresCheckpoint, WalRowMeta,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceCdcMode, SourceExecutionContract, SourceOnceContract,
};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{
    load_checkpoint_payload, submit_payload_batches, IngestBatch, SourceSyncContext,
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
    #[serde(default)]
    pub cdc_mode: SourceCdcMode,
    pub replication_slot_name: Option<String>,
    pub publication_name: Option<String>,
    #[serde(default)]
    pub cdc_idle_timeout_seconds: Option<u64>,
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

    fn stored_resume_lsn(&self, ctx: &dyn SourceSyncContext, slot_name: &str) -> Option<u64> {
        load_checkpoint_payload::<PostgresCheckpoint>(ctx, &Self::checkpoint_key(slot_name))
            .map(|checkpoint| checkpoint.lsn)
    }

    fn store_resume_checkpoint(
        &self,
        ctx: &dyn SourceSyncContext,
        slot_name: &str,
        lsn: u64,
    ) -> io::Result<()> {
        let _ = (ctx, slot_name, lsn);
        Ok(())
    }

    fn ingest_batches(
        &self,
        batches: Vec<IngestBatch>,
        ctx: &dyn SourceSyncContext,
    ) -> io::Result<()> {
        if batches.is_empty() {
            return Ok(());
        }
        submit_payload_batches(ctx, batches).map(|_| ())
    }

    fn cdc_batch(
        &self,
        table: &str,
        columns: &[PgColumn],
        tuple: &[Option<String>],
        mutation: MutationKind,
        lsn_id: &[u8],
    ) -> IngestBatch {
        let namespace = format!("postgres.{}", table);
        let offset_key = OffsetKey::new(namespace.clone(), table.to_string());
        let json_str = Self::tuple_to_json(columns, tuple);
        let bytes = json_str.len();
        IngestBatch::new(
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
        )
    }

    async fn sync_snapshot(&mut self, ctx: Arc<dyn SourceSyncContext>) -> io::Result<()> {
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
                    offset_key.clone(),
                    json_str,
                    bytes,
                    format!("postgres://{}", table_name),
                    Some(namespace.clone()),
                    None,
                ));

                if current_batch.len() >= batch_size {
                    self.ingest_batches(std::mem::take(&mut current_batch), ctx.as_ref())?;
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

        let stored_lsn = self.stored_resume_lsn(ctx.as_ref(), &slot_name);
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
                    self.ingest_batches(std::mem::take(&mut current_batch), ctx.as_ref())?;
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

        loop {
            let next_event = if let Some(idle_timeout) = idle_timeout {
                match tokio::time::timeout(idle_timeout, client.recv()).await {
                    Ok(result) => result.map_err(|err| io::Error::other(err.to_string()))?,
                    Err(_) => {
                        info!(
                            "Runtime Postgres CDC idle timeout reached after {}s; stopping stream",
                            idle_timeout.as_secs()
                        );
                        if !pending_batches.is_empty() {
                            self.ingest_batches(
                                std::mem::take(&mut pending_batches),
                                ctx.as_ref(),
                            )?;
                        }
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
                                pending_batches.push(self.cdc_batch(
                                    table,
                                    cols,
                                    &new_row,
                                    MutationKind::Insert,
                                    &lsn_id,
                                ));
                            }
                        }
                        PgOutputMessage::Update { oid, new_row, .. } => {
                            if let Some((table, cols)) = relation_map.get(&oid) {
                                let lsn_id = Self::lsn_bytes(wal_end);
                                pending_batches.push(self.cdc_batch(
                                    table,
                                    cols,
                                    &new_row,
                                    MutationKind::Update,
                                    &lsn_id,
                                ));
                            }
                        }
                        PgOutputMessage::Delete { oid, old_row } => {
                            if let Some((table, cols)) = relation_map.get(&oid) {
                                let lsn_id = Self::lsn_bytes(wal_end);
                                pending_batches.push(self.cdc_batch(
                                    table,
                                    cols,
                                    &old_row,
                                    MutationKind::Delete,
                                    &lsn_id,
                                ));
                            }
                        }
                    }
                }
                ReplicationEvent::Commit { lsn, .. } => {
                    client.update_applied_lsn(lsn);
                    let lsn_u64: u64 = lsn.into();
                    self.ingest_batches(std::mem::take(&mut pending_batches), ctx.as_ref())?;
                    self.store_resume_checkpoint(ctx.as_ref(), &slot_name, lsn_u64)?;
                }
                ReplicationEvent::KeepAlive { .. } => {}
                ReplicationEvent::StoppedAt { .. } => {
                    info!("Runtime Postgres replication stream stopped");
                    break;
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
}
