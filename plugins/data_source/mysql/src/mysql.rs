use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use mysql_async::prelude::*;
use mysql_async::{BinlogStreamRequest, Pool, Row, Value as MysqlValue};
use serde_derive::Deserialize;
use serde_json::{json, Map, Value};
use tracing::{error, info, warn};

use crate::helpers::plugin_config::PluginConfigEntry;
use skippr_runtime_sdk::plugins::cdc::{
    source_capabilities, MutationKind, MysqlCheckpoint, WalRowMeta,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceCdcMode, SourceExecutionContract, SourceOnceContract,
};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{
    load_checkpoint_payload, partition_already_closed, store_checkpoint_payload,
    submit_payload_batch_groups, submit_payload_batches, IngestBatch, SourceSyncContext,
};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceMysqlPluginConfig {
    pub connection_string: String,
    pub tables: Option<Vec<String>>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
    #[serde(default)]
    pub cdc_mode: SourceCdcMode,
    pub server_id: Option<u32>,
    #[serde(default)]
    pub cdc_idle_timeout_seconds: Option<u64>,
}

impl TryFrom<PluginConfigEntry> for DataSourceMysqlPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Mysql")
    }
}

pub struct DataSourceMysqlPlugin {
    pub(crate) config: DataSourceMysqlPluginConfig,
}

impl DataSourceMysqlPlugin {

    pub fn with_runtime_config(config: DataSourceMysqlPluginConfig) -> Self {
        Self { config }
    }

    async fn connect_pool(
        config: &DataSourceMysqlPluginConfig,
    ) -> Result<(Pool, mysql_async::Conn), mysql_async::Error> {
        let pool = Pool::new(config.connection_string.as_str());
        let conn = pool.get_conn().await?;
        Ok((pool, conn))
    }

    async fn discover_tables(
        conn: &mut mysql_async::Conn,
    ) -> Result<Vec<String>, mysql_async::Error> {
        let sql = "SELECT TABLE_SCHEMA, TABLE_NAME FROM INFORMATION_SCHEMA.TABLES \
                   WHERE TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_SCHEMA, TABLE_NAME";
        let rows: Vec<Row> = conn.query(sql).await?;
        let mut tables = Vec::new();
        for row in rows {
            let schema: String = row.get(0).unwrap_or_else(|| "".to_string());
            let table: String = row.get(1).unwrap_or_else(|| "".to_string());
            if !table.is_empty() {
                tables.push(format!("{}.{}", schema, table));
            }
        }
        Ok(tables)
    }

    async fn get_database_name(conn: &mut mysql_async::Conn) -> String {
        let sql = "SELECT DATABASE()";
        match conn.query_first::<Row, _>(sql).await {
            Ok(Some(row)) => match row.get::<Option<String>, _>(0) {
                Some(Some(s)) if !s.is_empty() => s,
                _ => "unknown".to_string(),
            },
            _ => "unknown".to_string(),
        }
    }

    fn escape_ident(ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn mysql_value_to_json(v: &MysqlValue) -> Value {
        match v {
            MysqlValue::NULL => Value::Null,
            MysqlValue::Bytes(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
            MysqlValue::Int(i) => json!(i),
            MysqlValue::UInt(u) => json!(u),
            MysqlValue::Float(f) => json!(f),
            MysqlValue::Double(d) => json!(d),
            MysqlValue::Date(y, mo, d, h, mi, s, micro) => Value::String(format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}",
                y, mo, d, h, mi, s, micro
            )),
            MysqlValue::Time(neg, days, h, mi, s, micro) => {
                let sign = if *neg { "-" } else { "" };
                Value::String(format!(
                    "{}{} {}:{:02}:{:02}.{:06}",
                    sign, days, h, mi, s, micro
                ))
            }
        }
    }

    fn row_to_json(row: &Row) -> String {
        let mut map = Map::new();
        for i in 0..row.len() {
            let name = row.columns_ref()[i].name_str().to_string();
            let value = row
                .as_ref(i)
                .map(Self::mysql_value_to_json)
                .unwrap_or(Value::Null);
            map.insert(name, value);
        }
        serde_json::to_string(&Value::Object(map)).unwrap_or_default()
    }

    fn cdc_mode(&self) -> SourceCdcMode {
        self.config.cdc_mode
    }

    /// Capture current binlog file and position from the primary.
    async fn get_binlog_position(
        conn: &mut mysql_async::Conn,
    ) -> Result<(String, u64), std::io::Error> {
        let row: Option<Row> = match conn.query_first("SHOW BINARY LOG STATUS").await {
            Ok(row) => row,
            Err(primary_err) => {
                warn!(
                    "SHOW BINARY LOG STATUS failed, falling back to SHOW MASTER STATUS: {}",
                    primary_err
                );
                conn.query_first("SHOW MASTER STATUS")
                    .await
                    .map_err(|fallback_err| {
                        std::io::Error::other(format!(
                            "SHOW BINARY LOG STATUS: {}; SHOW MASTER STATUS: {}",
                            primary_err, fallback_err
                        ))
                    })?
            }
        };
        let row = row.ok_or_else(|| {
            std::io::Error::other(
                "SHOW BINARY LOG STATUS/SHOW MASTER STATUS returned no rows; is binary logging enabled?",
            )
        })?;
        let file: String = row.get(0).unwrap_or_default();
        let position: u64 = row.get(1).unwrap_or(0);
        Ok((file, position))
    }

    /// Fetch column names for all user tables, keyed by `schema.table`.
    /// Ordered by ORDINAL_POSITION so indices match binlog row columns.
    async fn fetch_column_names(
        conn: &mut mysql_async::Conn,
    ) -> Result<HashMap<String, Vec<String>>, std::io::Error> {
        let sql = "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME \
                   FROM INFORMATION_SCHEMA.COLUMNS \
                   WHERE TABLE_SCHEMA NOT IN \
                     ('information_schema','performance_schema','mysql','sys') \
                   ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION";
        let rows: Vec<Row> = conn
            .query(sql)
            .await
            .map_err(|e| std::io::Error::other(format!("Column names query: {}", e)))?;
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rows {
            let schema: String = row.get(0).unwrap_or_default();
            let table: String = row.get(1).unwrap_or_default();
            let column: String = row.get(2).unwrap_or_default();
            map.entry(format!("{}.{}", schema, table))
                .or_default()
                .push(column);
        }
        Ok(map)
    }

    /// Build a lexicographically-sortable order token from binlog
    /// timestamp (seconds since epoch, upper 32 bits) and log position
    /// (lower 32 bits).
    fn binlog_order_token(timestamp: u32, position: u64) -> Vec<u8> {
        let combined = ((timestamp as u64) << 32) | (position & 0xFFFF_FFFF);
        combined.to_be_bytes().to_vec()
    }

    // -----------------------------------------------------------------------
    // CDC sync via binlog replication
    // -----------------------------------------------------------------------

    async fn sync_cdc(
        &mut self,
        ctx: Arc<dyn SourceSyncContext>,
        mode: SourceCdcMode,
    ) -> Result<(), std::io::Error> {
        info!("MySQL CDC: starting binlog replication");

        let server_id = self.config.server_id.unwrap_or(1);

        let (pool, mut conn) = Self::connect_pool(&self.config)
            .await
            .map_err(|e| std::io::Error::other(format!("MySQL connect: {}", e)))?;

        let db_name = Self::get_database_name(&mut conn).await;

        // Check for stored checkpoint to enable resume
        let checkpoint_key = format!("mysql:{}:binlog", db_name);
        let stored_checkpoint =
            load_checkpoint_payload::<MysqlCheckpoint>(ctx.as_ref(), &checkpoint_key);
        let resume_mode = stored_checkpoint.is_some();

        let (binlog_file, binlog_pos) = if let Some(ref ckpt) = stored_checkpoint {
            info!(
                "MySQL CDC: resuming from stored binlog position {}:{}",
                ckpt.binlog_file, ckpt.binlog_position
            );
            (ckpt.binlog_file.clone(), ckpt.binlog_position)
        } else {
            // 1. Capture binlog position before snapshot
            let (file, pos) = Self::get_binlog_position(&mut conn).await?;
            info!("MySQL CDC: captured binlog position {}:{}", file, pos);
            (file, pos)
        };

        // 2. Pre-fetch column names for binlog row → JSON conversion
        let column_map = Self::fetch_column_names(&mut conn).await?;

        // 3. Discover tables
        let should_run_snapshot = mode.includes_initial_snapshot() && !resume_mode;
        let tables = if should_run_snapshot {
            match &self.config.tables {
                Some(t) => t.clone(),
                None => Self::discover_tables(&mut conn)
                    .await
                    .map_err(|e| std::io::Error::other(format!("Discover tables: {}", e)))?,
            }
        } else {
            match (resume_mode, mode) {
                (true, _) => {
                    info!("MySQL CDC: skipping snapshot (resuming from stored binlog position)")
                }
                (false, SourceCdcMode::CdcOnly) => {
                    info!("MySQL CDC: cdc_only mode skips initial snapshot")
                }
                _ => {}
            }
            Vec::new()
        };

        // 4. Initial snapshot anchored to binlog position
        let anchor_token = Self::binlog_order_token(0, binlog_pos);
        let anchor_event_id = format!("{}:{}", binlog_file, binlog_pos).into_bytes();

        const SNAPSHOT_BATCH_ROWS: usize = 10_000;

        for table_fq in &tables {
            let parts: Vec<&str> = table_fq.splitn(2, '.').collect();
            let (schema, table) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                (db_name.as_str(), parts[0])
            };

            let offset_key = OffsetKey {
                namespace: format!("mysql:{}.{}.{}", db_name, schema, table),
                partition: table_fq.clone(),
            };

            if partition_already_closed(ctx.as_ref(), &offset_key) {
                info!("CDC snapshot: skipping already-ingested {}", table_fq);
                continue;
            }

            info!("CDC snapshot: reading {}", table_fq);

            let q_schema = Self::escape_ident(schema);
            let q_table = Self::escape_ident(table);
            let query_sql = format!("SELECT * FROM {}.{}", q_schema, q_table);

            let rows: Vec<Row> = match conn.query(query_sql.as_str()).await {
                Ok(r) => r,
                Err(e) => {
                    error!("Failed to query table {}: {}", table_fq, e);
                    continue;
                }
            };

            info!("CDC snapshot: {} rows from {}", rows.len(), table_fq);

            let mut current_batch: Vec<IngestBatch> = Vec::new();
            let mut batch_groups: Vec<Vec<IngestBatch>> = Vec::new();

            for row in &rows {
                let json_str = Self::row_to_json(row);
                let bytes = json_str.len();

                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    offset_pos: None,
                    source_uri: format!("mysql://{}/{}", db_name, table_fq),
                    namespace: Some(table.to_string()),
                    cdc_rows: Some(vec![WalRowMeta {
                        mutation: MutationKind::Snapshot,
                        event_id: anchor_event_id.clone(),
                        order_token: anchor_token.clone(),
                    }]),
                });

                if current_batch.len() >= SNAPSHOT_BATCH_ROWS {
                    batch_groups.push(std::mem::take(&mut current_batch));
                }
            }

            if !current_batch.is_empty() {
                batch_groups.push(current_batch);
            }

            if !batch_groups.is_empty() {
                submit_payload_batch_groups(ctx.as_ref(), batch_groups)?;
            }
        }

        if should_run_snapshot {
            store_checkpoint_payload(
                ctx.as_ref(),
                &checkpoint_key,
                &MysqlCheckpoint {
                    binlog_file: binlog_file.clone(),
                    binlog_position: binlog_pos,
                },
            )?;
        }

        info!("MySQL CDC: bootstrap complete, switching to binlog stream");

        // 5. Open binlog stream from captured position.
        //    get_binlog_stream() consumes the Conn, so acquire a fresh one.
        let conn2 = pool
            .get_conn()
            .await
            .map_err(|e| std::io::Error::other(format!("MySQL binlog connect: {}", e)))?;

        let request = BinlogStreamRequest::new(server_id)
            .with_filename(binlog_file.as_bytes())
            .with_pos(binlog_pos);

        let mut binlog_stream = conn2
            .get_binlog_stream(request)
            .await
            .map_err(|e| std::io::Error::other(format!("Binlog stream open: {}", e)))?;

        // MySQL binlog row-event type codes (protocol-stable).
        const WRITE_ROWS_V1: u8 = 23;
        const UPDATE_ROWS_V1: u8 = 24;
        const DELETE_ROWS_V1: u8 = 25;
        const WRITE_ROWS_V2: u8 = 30;
        const UPDATE_ROWS_V2: u8 = 31;
        const DELETE_ROWS_V2: u8 = 32;
        let idle_timeout = self
            .config
            .cdc_idle_timeout_seconds
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs);

        // 6. Process binlog events
        loop {
            let event_result = if let Some(idle_timeout) = idle_timeout {
                match tokio::time::timeout(idle_timeout, binlog_stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        info!(
                            "MySQL CDC idle timeout reached after {}s; stopping binlog stream",
                            idle_timeout.as_secs()
                        );
                        break;
                    }
                }
            } else {
                binlog_stream.next().await
            };
            let Some(event_result) = event_result else {
                break;
            };
            let event = match event_result {
                Ok(e) => e,
                Err(e) => {
                    error!("MySQL CDC: binlog event error: {}", e);
                    continue;
                }
            };

            let header = event.header();
            let timestamp = header.timestamp();
            let log_pos = header.log_pos();
            let evt_raw = header.event_type_raw();

            let mutation = match evt_raw {
                WRITE_ROWS_V1 | WRITE_ROWS_V2 => MutationKind::Insert,
                UPDATE_ROWS_V1 | UPDATE_ROWS_V2 => MutationKind::Update,
                DELETE_ROWS_V1 | DELETE_ROWS_V2 => MutationKind::Delete,
                _ => continue, // TableMapEvents, queries, etc. — skip
            };

            let event_data = match event.read_data() {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(e) => {
                    error!("MySQL CDC: failed to parse binlog event: {}", e);
                    continue;
                }
            };

            // Extract the RowsEvent payload (all row-event variants collapse
            // into EventData::RowsEvent in mysql_common).
            let rows_data = match event_data {
                mysql_async::binlog::events::EventData::RowsEvent(r) => r,
                _ => continue,
            };

            let table_id = rows_data.table_id();
            let tme = match binlog_stream.get_tme(table_id) {
                Some(t) => t,
                None => {
                    warn!(
                        "MySQL CDC: no cached TableMapEvent for table_id {}",
                        table_id
                    );
                    continue;
                }
            };

            let tme_db = tme.database_name().to_string();
            let tme_table = tme.table_name().to_string();
            let fq_table = format!("{}.{}", tme_db, tme_table);

            let columns = column_map.get(&fq_table);
            let order_token = Self::binlog_order_token(timestamp, log_pos as u64);
            let event_id = format!("{}:{}", binlog_file, log_pos).into_bytes();

            let offset_key = OffsetKey {
                namespace: format!("mysql:{}.{}.{}", db_name, tme_db, tme_table),
                partition: fq_table.clone(),
            };

            for row_result in rows_data.rows(tme) {
                let (before, after) = match row_result {
                    Ok(pair) => pair,
                    Err(e) => {
                        error!("MySQL CDC: row parse error: {}", e);
                        continue;
                    }
                };

                // INSERT/UPDATE use the after-image; DELETE uses before-image.
                let values = match mutation {
                    MutationKind::Delete => before,
                    _ => after,
                };
                let Some(values) = values else { continue };

                let mut map = Map::new();
                for i in 0..values.len() {
                    let col_name = columns
                        .and_then(|cols| cols.get(i))
                        .cloned()
                        .unwrap_or_else(|| format!("col_{}", i));
                    let json_val = values
                        .as_ref(i)
                        .and_then(|bv| MysqlValue::try_from(bv.clone()).ok())
                        .map(|v| Self::mysql_value_to_json(&v))
                        .unwrap_or(Value::Null);
                    map.insert(col_name, json_val);
                }
                let json_str = serde_json::to_string(&Value::Object(map)).unwrap_or_default();
                let bytes = json_str.len();

                let batch = IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    offset_pos: None,
                    source_uri: format!("mysql://{}/{}", db_name, fq_table),
                    namespace: Some(tme_table.clone()),
                    cdc_rows: Some(vec![WalRowMeta {
                        mutation,
                        event_id: event_id.clone(),
                        order_token: order_token.clone(),
                    }]),
                };

                submit_payload_batches(ctx.as_ref(), vec![batch])?;
            }

            store_checkpoint_payload(
                ctx.as_ref(),
                &checkpoint_key,
                &MysqlCheckpoint {
                    binlog_file: binlog_file.clone(),
                    binlog_position: log_pos as u64,
                },
            )?;
        }

        info!("MySQL CDC: binlog stream ended");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Snapshot/query path for non-CDC operation
    // -----------------------------------------------------------------------

    async fn sync_query(
        &mut self,
        ctx: Arc<dyn SourceSyncContext>,
        cdc_tag: bool,
    ) -> std::io::Result<()> {
        info!("MySQL input plugin starting sync");

        let (_pool, mut conn) = match Self::connect_pool(&self.config).await {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to connect to MySQL: {}", e);
                return Ok(());
            }
        };

        let db_name = Self::get_database_name(&mut conn).await;

        let tables = match &self.config.tables {
            Some(t) => t.clone(),
            None => match Self::discover_tables(&mut conn).await {
                Ok(t) => {
                    info!("Discovered {} tables from MySQL", t.len());
                    t
                }
                Err(e) => {
                    error!("Failed to discover MySQL tables: {}", e);
                    return Ok(());
                }
            },
        };

        const DEFAULT_BATCH_ROWS: usize = 10_000;
        let batch_size = DEFAULT_BATCH_ROWS;

        for table_fq in &tables {
            let parts: Vec<&str> = table_fq.splitn(2, '.').collect();
            let (schema, table) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                (db_name.as_str(), parts[0])
            };

            let namespace = format!("mysql.{}.{}.{}", db_name, schema, table);
            let offset_key = OffsetKey {
                namespace: format!("mysql:{}.{}.{}", db_name, schema, table),
                partition: table_fq.clone(),
            };

            if partition_already_closed(ctx.as_ref(), &offset_key) {
                info!("Skipping already-ingested table: {}", table_fq);
                continue;
            }

            info!("Ingesting table: {} -> namespace: {}", table_fq, namespace);

            let q_schema = Self::escape_ident(schema);
            let q_table = Self::escape_ident(table);
            let query_sql = format!("SELECT * FROM {}.{}", q_schema, q_table);

            let rows: Vec<Row> = match conn.query(query_sql.as_str()).await {
                Ok(r) => r,
                Err(e) => {
                    error!("Failed to query table {}: {}", table_fq, e);
                    continue;
                }
            };

            info!("Read {} rows from {}", rows.len(), table_fq);

            let mut current_batch: Vec<IngestBatch> = Vec::new();
            let mut batch_groups: Vec<Vec<IngestBatch>> = Vec::new();

            for (row_idx, row) in rows.iter().enumerate() {
                let json_str = Self::row_to_json(row);
                let bytes = json_str.len();

                let cdc_rows = if cdc_tag {
                    let event_id = format!("{}:{}", table_fq, row_idx).into_bytes();
                    Some(vec![WalRowMeta {
                        mutation: MutationKind::Snapshot,
                        event_id: event_id.clone(),
                        order_token: event_id,
                    }])
                } else {
                    None
                };

                current_batch.push(IngestBatch {
                    offset_key: offset_key.clone(),
                    data: json_str,
                    bytes,
                    offset_pos: None,
                    source_uri: format!("mysql://{}/{}", db_name, table_fq),
                    namespace: Some(table.to_string()),
                    cdc_rows,
                });

                if current_batch.len() >= batch_size {
                    batch_groups.push(std::mem::take(&mut current_batch));
                }
            }

            if !current_batch.is_empty() {
                batch_groups.push(current_batch);
            }

            if !batch_groups.is_empty() {
                submit_payload_batch_groups(ctx.as_ref(), batch_groups)?;
            }
        }

        info!("MySQL input plugin sync complete");
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceMysqlPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        match self.cdc_mode() {
            SourceCdcMode::Snapshot => {
                self.sync_query(ctx, false).await?;
                Ok(())
            }
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
            SourceExecutionContract::configurable_cdc(mode, &source_capabilities::MYSQL, once)
        } else {
            SourceExecutionContract::configurable_cdc(
                mode,
                &source_capabilities::MYSQL,
                SourceOnceContract::Finite,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binlog_order_token_lexicographic_ordering() {
        let earlier = DataSourceMysqlPlugin::binlog_order_token(1, 100);
        let later = DataSourceMysqlPlugin::binlog_order_token(2, 50);
        assert!(
            earlier < later,
            "timestamp=1,pos=100 must sort before timestamp=2,pos=50"
        );
    }

    #[test]
    fn test_binlog_order_token_same_timestamp_orders_by_position() {
        let a = DataSourceMysqlPlugin::binlog_order_token(100, 200);
        let b = DataSourceMysqlPlugin::binlog_order_token(100, 300);
        assert!(a < b, "same timestamp: lower position must sort first");
    }

    #[test]
    fn test_binlog_order_token_is_8_bytes() {
        let token = DataSourceMysqlPlugin::binlog_order_token(42, 99);
        assert_eq!(
            token.len(),
            8,
            "order token must be 8 bytes (u64 big-endian)"
        );
    }

    #[test]
    fn test_binlog_order_token_encodes_big_endian() {
        let token = DataSourceMysqlPlugin::binlog_order_token(1, 0);
        let expected: u64 = 1u64 << 32;
        assert_eq!(token, expected.to_be_bytes().to_vec());
    }
}
