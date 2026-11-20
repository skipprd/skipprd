use async_trait::async_trait;
use dashmap::DashSet;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use serde_derive::Deserialize;
use skippr_runtime_sdk::SkipprConfig;
use tracing::{error, info, warn};

use skippr_runtime_sdk::plugins::alter_ddl::{
    dest_table_name, publish_warehouse_table, DestPublishCache, InstalledDestSchema,
    PublishedBatchColumns, WarehouseDialect,
};
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::sink_compat::BufferChunker;

static ENSURED_DATASETS: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
const BIGQUERY_CDC_LOAD_JOB_BLOCKER: &str =
    "the connector currently exposes query jobs only and has no CDC-enriched load-job path";

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSinkBigqueryPluginConfig {
    pub project: String,
    pub dataset: String,
    pub location: Option<String>,
    #[skippr(secret_path)]
    pub credentials_path: Option<String>,
    /// Query/model only; ignored at ingest.
    #[serde(default)]
    #[allow(dead_code)]
    pub max_concurrency: Option<usize>,
    /// Query/model only; ignored at ingest.
    #[serde(default)]
    #[allow(dead_code)]
    pub discovery_cache_ttl_secs: Option<u64>,
}

pub struct BigqueryCdcBackend;

impl super::cdc_apply::CdcApplyBackend for BigqueryCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "BYTES";

    fn binary_literal(hex: &str) -> String {
        format!("FROM_HEX('{hex}')")
    }

    fn string_literal(value: &str) -> String {
        super::cdc_apply::bigquery_string_literal(value)
    }

    fn tx_begin() -> &'static str {
        "BEGIN TRANSACTION;\n"
    }

    fn tx_commit() -> &'static str {
        "\nCOMMIT TRANSACTION;"
    }

    fn merge_keyword() -> &'static str {
        "MERGE"
    }

    fn ddl_add_order_token_column(fq_table: &str) -> String {
        format!(
            "ALTER TABLE {fq_table} ADD COLUMN IF NOT EXISTS `_skippr_order_token` {};\n\
             ALTER TABLE {fq_table} ADD COLUMN IF NOT EXISTS `_skippr_tie_seq` INT64;\n\
             ALTER TABLE {fq_table} ADD COLUMN IF NOT EXISTS `_skippr_tie_ordinal` INT64;\n\
             ALTER TABLE {fq_table} ADD COLUMN IF NOT EXISTS `_skippr_tie_row` INT64",
            Self::ORDER_TOKEN_TYPE,
        )
    }

    fn ddl_create_tombstone_table(
        fq_tombstone_table: &str,
        business_key_cols: &[(String, &str)],
    ) -> String {
        let mut column_defs = business_key_cols
            .iter()
            .map(|(name, target_type)| {
                format!("`{}` {target_type} NOT NULL", name.replace('`', "\\`"))
            })
            .collect::<Vec<_>>();
        column_defs.push(format!(
            "`_skippr_order_token` {} NOT NULL",
            Self::ORDER_TOKEN_TYPE
        ));
        column_defs.push("`_skippr_tie_seq` INT64 NOT NULL".to_string());
        column_defs.push("`_skippr_tie_ordinal` INT64 NOT NULL".to_string());
        column_defs.push("`_skippr_tie_row` INT64 NOT NULL".to_string());
        let primary_key = business_key_cols
            .iter()
            .map(|(name, _)| format!("`{}`", name.replace('`', "\\`")))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "CREATE TABLE IF NOT EXISTS {fq_tombstone_table} ({}, PRIMARY KEY ({primary_key}) NOT ENFORCED)",
            column_defs.join(", ")
        )
    }
}

const JOB_POLL_MAX: u32 = 120;
const JOB_POLL_INTERVAL_MS: u64 = 500;

pub struct DataSinkBigqueryPlugin {
    config: DataSinkBigqueryPluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
    client: reqwest::Client,
    token: tokio::sync::RwLock<Option<(String, std::time::Instant)>>,
    installed: tokio::sync::RwLock<InstalledDestSchema>,
    publish_cache: DestPublishCache,
}

skippr_runtime_sdk::declare_sink_spec!(
    BigquerySinkSpec,
    DataSinkBigqueryPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::BIGQUERY,
    skippr_runtime_sdk::plugins::FinalStateIdempotentApply,
    skippr_runtime_sdk::plugins::AlterSupport::Warehouse(
        skippr_runtime_sdk::plugins::alter_ddl::WarehouseDialect::BigQuery
    )
);

impl DataSinkBigqueryPlugin {
    pub async fn new_with_config(
        buffer_name: String,
        config: DataSinkBigqueryPluginConfig,
    ) -> Self {
        Self {
            config,
            buffer_name,
            client: reqwest::Client::new(),
            token: tokio::sync::RwLock::new(None),
            installed: tokio::sync::RwLock::new(InstalledDestSchema::default()),
            publish_cache: DestPublishCache::default(),
        }
    }

    fn fq_table(&self, namespace: &str) -> String {
        format!(
            "`{}.{}.{}`",
            self.config.project,
            self.config.dataset,
            dest_table_name(namespace)
        )
    }

    async fn publish_namespace(&self, namespace: &str) -> Result<(), std::io::Error> {
        let (version, metadata) = self.installed.read().await.get(namespace)?;
        self.ensure_dataset().await?;
        publish_warehouse_table(
            &self.publish_cache,
            self,
            WarehouseDialect::BigQuery,
            &self.fq_table(namespace),
            namespace,
            version,
            &metadata,
        )
        .await
    }

    async fn batch_columns(
        &self,
        namespace: &str,
        batch: &datafusion::arrow::datatypes::Schema,
    ) -> Result<PublishedBatchColumns, std::io::Error> {
        self.installed.read().await.warehouse_batch_columns(
            WarehouseDialect::BigQuery,
            namespace,
            batch,
        )
    }

    fn resolve_credentials_path(&self) -> Result<String, String> {
        self.config
            .credentials_path
            .as_ref()
            .map(|path| path.trim())
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                "BigQuery requires a non-empty `credentials_path` in plugin config".to_string()
            })
    }

    async fn authenticate(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        {
            let guard = self.token.read().await;
            if let Some((ref tok, expiry)) = *guard {
                if expiry > std::time::Instant::now() {
                    return Ok(tok.clone());
                }
            }
        }

        let cred_path = self.resolve_credentials_path()?;
        info!(target: "bigquery", "authenticating via service account: {}", cred_path);

        let cred_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&cred_path)
                .map_err(|e| format!("failed to read credentials at {}: {}", cred_path, e))?,
        )?;

        let client_email = cred_json["client_email"]
            .as_str()
            .ok_or("Missing client_email in service account JSON")?;
        let private_key_pem = cred_json["private_key"]
            .as_str()
            .ok_or("Missing private_key in service account JSON")?;
        let token_uri = cred_json["token_uri"]
            .as_str()
            .unwrap_or("https://oauth2.googleapis.com/token");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        let claims = serde_json::json!({
            "iss": client_email,
            "scope": "https://www.googleapis.com/auth/bigquery",
            "aud": token_uri,
            "iat": now,
            "exp": now + 3600,
        });

        let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
            .map_err(|e| format!("failed to create JWT encoding key: {}", e))?;
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let jwt = jsonwebtoken::encode(&header, &claims, &encoding_key)
            .map_err(|e| format!("failed to sign JWT: {}", e))?;

        let resp = self
            .client
            .post(token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &jwt),
            ])
            .send()
            .await?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;

        if !status.is_success() {
            let msg = body["error_description"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(format!("BigQuery OAuth2 HTTP {}: {}", status, msg).into());
        }

        let access_token = body["access_token"]
            .as_str()
            .ok_or("No access_token in OAuth2 response")?
            .to_string();

        {
            let mut guard = self.token.write().await;
            *guard = Some((
                access_token.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(3500),
            ));
        }

        info!(target: "bigquery", "authenticated via service account JWT");
        Ok(access_token)
    }

    async fn execute_sql(
        &self,
        sql: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        self.execute_sql_limited(sql, 0).await
    }

    async fn execute_sql_limited(
        &self,
        sql: &str,
        max_results: i64,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let token = self.authenticate().await?;
        let url = format!(
            "https://bigquery.googleapis.com/bigquery/v2/projects/{}/queries",
            self.config.project
        );

        let mut payload = serde_json::json!({
            "query": sql,
            "useLegacySql": false,
            "timeoutMs": 120_000,
            "maxResults": max_results,
        });

        if let Some(ref loc) = self.config.location {
            payload["location"] = serde_json::json!(loc);
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;

        if !status.is_success() {
            let msg = body
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("BigQuery API HTTP {}: {}", status, msg).into());
        }

        if let Some(errors) = body.get("errors").and_then(|e| e.as_array()) {
            if !errors.is_empty() {
                let msg = errors[0]["message"]
                    .as_str()
                    .unwrap_or("unknown query error");
                return Err(format!("BigQuery query error: {}", msg).into());
            }
        }

        if body.get("jobComplete").and_then(|v| v.as_bool()) == Some(false) {
            return self.poll_job(&body, &token).await;
        }

        Ok(body)
    }

    async fn poll_job(
        &self,
        initial_body: &serde_json::Value,
        token: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let job_id = initial_body
            .pointer("/jobReference/jobId")
            .and_then(|v| v.as_str())
            .ok_or("Async response missing jobReference.jobId")?;

        let location_param = self
            .config
            .location
            .as_deref()
            .map(|l| format!("&location={}", l))
            .unwrap_or_default();

        let poll_url = format!(
            "https://bigquery.googleapis.com/bigquery/v2/projects/{}/queries/{}?timeoutMs=30000{}",
            self.config.project, job_id, location_param
        );

        for attempt in 1..=JOB_POLL_MAX {
            tokio::time::sleep(std::time::Duration::from_millis(JOB_POLL_INTERVAL_MS)).await;

            let resp = self
                .client
                .get(&poll_url)
                .header("Authorization", format!("Bearer {}", token))
                .send()
                .await?;

            let body: serde_json::Value = resp.json().await?;

            if let Some(errors) = body.get("errors").and_then(|e| e.as_array()) {
                if !errors.is_empty() {
                    let msg = errors[0]["message"]
                        .as_str()
                        .unwrap_or("unknown query error");
                    return Err(format!("BigQuery query error: {}", msg).into());
                }
            }

            if body.get("jobComplete").and_then(|v| v.as_bool()) == Some(true) {
                return Ok(body);
            }

            if attempt % 20 == 0 {
                warn!(
                    "BigQuery job {} still running after {}s",
                    job_id,
                    (attempt as u64 * JOB_POLL_INTERVAL_MS) / 1000
                );
            }
        }

        Err(format!(
            "BigQuery job {} timed out after {}s",
            job_id,
            (JOB_POLL_MAX as u64 * JOB_POLL_INTERVAL_MS) / 1000
        )
        .into())
    }

    async fn ensure_dataset(&self) -> Result<(), std::io::Error> {
        let key = format!("{}.{}", self.config.project, self.config.dataset);
        if !ENSURED_DATASETS.insert(key.clone()) {
            return Ok(());
        }

        let location_opt = self
            .config
            .location
            .as_deref()
            .map(|l| format!(" OPTIONS(location='{}')", l))
            .unwrap_or_default();

        let ddl = format!(
            "CREATE SCHEMA IF NOT EXISTS `{}.{}`{}",
            self.config.project, self.config.dataset, location_opt
        );
        info!("BigQuery DDL: {}", ddl);
        if let Err(e) = self.execute_sql(&ddl).await {
            ENSURED_DATASETS.remove(&key);
            error!("BigQuery CREATE SCHEMA failed: {}", e);
            return Err(std::io::Error::other(format!(
                "BigQuery CREATE SCHEMA: {}",
                e
            )));
        }

        Ok(())
    }

    async fn inner_sync(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::metrics::counters;
        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = dest_table_name(&namespace);
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let fq_table = self.fq_table(&namespace);
        self.ensure_dataset().await?;
        counters::inc_uploads_in_flight();

        let col_list = col_defs
            .columns()
            .iter()
            .map(|(name, _)| WarehouseDialect::BigQuery.quote_ident(name))
            .collect::<Vec<_>>()
            .join(", ");

        let mut total_rows = 0usize;
        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("stream error: {}", e))
            })?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }

            let value_rows = (0..num_rows)
                .map(|row| crate::cdc_apply::sql_values_row::<BigqueryCdcBackend>(&batch, row))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    counters::dec_uploads_in_flight();
                    std::io::Error::from(e)
                })?;

            let insert_sql = format!(
                "INSERT INTO {} ({}) VALUES {}",
                fq_table,
                col_list,
                value_rows.join(", ")
            );

            match self.execute_sql(&insert_sql).await {
                Ok(_) => {
                    total_rows += num_rows;
                    counters::add_parquet_rows(num_rows as u64);
                    info!(
                        "Inserted {} rows into {} (total: {})",
                        num_rows, table_name, total_rows
                    );
                }
                Err(e) => {
                    error!("BigQuery INSERT failed: {}", e);
                    counters::dec_uploads_in_flight();
                    return Err(std::io::Error::other(format!("BigQuery INSERT: {}", e)));
                }
            }
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "BigQuery sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }

    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &skippr_runtime_sdk::plugins::cdc::SyncContext,
    ) -> Result<(), std::io::Error> {
        use super::cdc_apply::{
            append_record_batch_to_cdc_apply, collapse_apply_batch, ddl_add_order_token_column,
            ddl_create_tombstone_table, guarded_warehouse_cdc_row_sql, warehouse_bulk_cdc_sql,
            CdcApplyBatch, CdcApplyColumn, CdcWarehouseDialect,
        };
        use skippr_runtime_sdk::metrics::counters;

        tracing::debug!(
            target: "bigquery",
            blocker = BIGQUERY_CDC_LOAD_JOB_BLOCKER,
            "using bounded query staging with guarded overflow"
        );
        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "bigquery", "CDC context without contract or business keys; falling back to append");
                return self.inner_sync(stream, filename).await;
            }
        };

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = dest_table_name(&namespace);
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let key_types = col_defs.key_types(&contract.business_key_columns)?;
        let fq_table = self.fq_table(&namespace);
        self.ensure_dataset().await?;
        counters::inc_uploads_in_flight();

        let tombstone_table = format!(
            "`{}.{}._skippr_tombstones_{}`",
            self.config.project, self.config.dataset, table_name
        );

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<BigqueryCdcBackend>(&fq_table);
            if let Err(e) = self.execute_sql(&order_col_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(std::io::Error::other(format!("BigQuery CDC DDL: {}", e)));
            }

            let tombstone_ddl =
                ddl_create_tombstone_table::<BigqueryCdcBackend>(&tombstone_table, &key_types);
            if let Err(e) = self.execute_sql(&tombstone_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(std::io::Error::other(format!("BigQuery CDC DDL: {}", e)));
            }

            info!(target: "bigquery", "CDC DDL applied for {}", fq_table);
        }

        let mut row_offset = 0usize;
        let mut apply_batch = CdcApplyBatch {
            columns: col_defs
                .columns()
                .iter()
                .map(|(name, target_type)| CdcApplyColumn {
                    name: name.clone(),
                    target_type: (*target_type).to_string(),
                })
                .collect(),
            business_key_columns: key_types.iter().map(|(name, _)| name.clone()).collect(),
            rows: Vec::new(),
        };

        let ties = ctx.decoded_ties().map_err(|e| {
            counters::dec_uploads_in_flight();
            std::io::Error::other(e)
        })?;
        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("stream error: {}", e))
            })?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }
            append_record_batch_to_cdc_apply(
                &mut apply_batch,
                &batch,
                &ctx.part_meta.rows,
                &ties,
                row_offset,
            )
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::from(e)
            })?;
            row_offset += num_rows;
        }

        let total_rows = apply_batch.rows.len();
        if total_rows > 0 {
            match warehouse_bulk_cdc_sql(
                CdcWarehouseDialect::BigQuery,
                &fq_table,
                &tombstone_table,
                &apply_batch,
            ) {
                Ok(sql) => {
                    self.execute_sql(&sql.transactional_script(CdcWarehouseDialect::BigQuery))
                        .await
                        .map_err(|error| {
                            counters::dec_uploads_in_flight();
                            std::io::Error::other(format!("BigQuery bulk CDC apply: {error}"))
                        })?;
                }
                Err(error) if error.is_warehouse_stage_limit() => {
                    warn!(
                        target: "bigquery",
                        "CDC chunk exceeds bounded staging envelope; using guarded row apply: {}",
                        error
                    );
                    let collapsed = collapse_apply_batch(&apply_batch);
                    for row in &collapsed.rows {
                        let statement = guarded_warehouse_cdc_row_sql::<BigqueryCdcBackend>(
                            CdcWarehouseDialect::BigQuery,
                            &fq_table,
                            &tombstone_table,
                            &collapsed,
                            row,
                        )?;
                        self.execute_sql(&statement).await.map_err(|error| {
                            counters::dec_uploads_in_flight();
                            std::io::Error::other(format!("BigQuery guarded CDC apply: {error}"))
                        })?;
                    }
                }
                Err(error) => {
                    counters::dec_uploads_in_flight();
                    return Err(std::io::Error::other(error.to_string()));
                }
            }
            counters::add_parquet_rows(total_rows as u64);
            info!(
                target: "bigquery",
                "CDC safely applied {} rows to {}",
                total_rows,
                table_name
            );
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "BigQuery CDC sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }
}

#[async_trait]
impl DataSink for DataSinkBigqueryPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        let namespace = BufferChunker::decode_file_namespace(&filename);
        self.publish_namespace(&namespace).await?;
        match cdc_ctx {
            Some(ctx) => self.sync_cdc(stream, filename, ctx).await,
            None => self.inner_sync(stream, filename).await,
        }
    }

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<skippr_runtime_sdk::plugins::SinkWriteOutcome, std::io::Error> {
        let schema = reader.schema();
        while let Some(chunk) = reader.next_chunk().await? {
            let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
            let chunk_ctx = ctx.chunk_sink_write_context_with_cdc(
                chunk.chunk_index,
                chunk.chunk_index == 0 && chunk.final_chunk,
                chunk_cdc.as_ref(),
            );
            self.sync(
                chunk.into_stream(schema.clone()),
                chunk_ctx.filename,
                chunk_ctx.cdc_ctx,
            )
            .await?;
        }
        Ok(skippr_runtime_sdk::plugins::SinkWriteOutcome::Applied)
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::BIGQUERY
    }

    fn installed_dest_schema(&self) -> Option<&tokio::sync::RwLock<InstalledDestSchema>> {
        Some(&self.installed)
    }

    fn alter_support(&self) -> skippr_runtime_sdk::plugins::AlterSupport {
        skippr_runtime_sdk::plugins::AlterSupport::Warehouse(
            skippr_runtime_sdk::plugins::alter_ddl::WarehouseDialect::BigQuery,
        )
    }

    async fn apply_alter(
        &self,
        namespace: &str,
        _metadata: &skippr_runtime_sdk::discover::OutputMetadata,
        alter: &skippr_runtime_sdk::discover::schema_alter::CommittedAlter,
    ) -> Result<(), std::io::Error> {
        skippr_runtime_sdk::plugins::alter_ddl::apply_warehouse_alter(
            self,
            WarehouseDialect::BigQuery,
            &self.fq_table(namespace),
            namespace,
            alter,
        )
        .await
    }
}

#[async_trait]
impl skippr_runtime_sdk::plugins::alter_ddl::WarehouseExec for DataSinkBigqueryPlugin {
    async fn columns(&self, namespace: &str) -> std::io::Result<Vec<String>> {
        let table_name = dest_table_name(namespace);
        let sql = format!(
            "SELECT column_name FROM `{project}.{dataset}.INFORMATION_SCHEMA.COLUMNS` WHERE table_name = {table}",
            project = self.config.project.replace('`', ""),
            dataset = self.config.dataset.replace('`', ""),
            table = crate::cdc_apply::bigquery_string_literal(&table_name),
        );
        let body = self
            .execute_sql_limited(&sql, 10_000)
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        skippr_runtime_sdk::plugins::alter_ddl::json_result_names(&body)
    }

    async fn execute(&self, sql: &str) -> std::io::Result<()> {
        self.execute_sql(sql)
            .await
            .map(|_| ())
            .map_err(|err| std::io::Error::other(err.to_string()))
    }

    async fn query_first(&self, sql: &str) -> std::io::Result<Option<String>> {
        let body = self
            .execute_sql_limited(sql, 10_000)
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        skippr_runtime_sdk::plugins::alter_ddl::json_result_first(&body)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bigquery_bulk_cdc_sql_quotes_keys_and_keeps_temp_ddl_atomic() {
        let batch = crate::cdc_apply::warehouse_sql_test_batch();
        let sql = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::BigQuery,
            "`project.dataset.users`",
            "`project.dataset._skippr_tombstones_users`",
            &batch,
        )
        .unwrap();
        let script = sql.transactional_script(crate::cdc_apply::CdcWarehouseDialect::BigQuery);

        assert!(script.starts_with("BEGIN TRANSACTION;"));
        assert!(script.contains("CREATE TEMP TABLE"));
        assert!(script.contains("FROM_HEX"));
        assert!(
            script.contains(
                "ON target.`tenant_id` = source.`tenant_id` AND target.`id` = source.`id`"
            ),
            "{script}"
        );
        assert_eq!(script.matches("MERGE INTO").count(), 5);
        assert!(script.contains("DROP TABLE IF EXISTS"));
        assert!(script.ends_with("COMMIT TRANSACTION;"));
    }

    #[test]
    fn bigquery_splits_loads_and_falls_back_above_encoded_limit() {
        assert!(crate::cdc_apply::CdcWarehouseDialect::BigQuery
            .validate_stage_row_count(10_000)
            .is_ok());
        assert!(crate::cdc_apply::CdcWarehouseDialect::BigQuery
            .validate_stage_row_count(10_001)
            .unwrap_err()
            .is_warehouse_stage_limit());
        assert!(crate::cdc_apply::CdcWarehouseDialect::BigQuery
            .validate_stage_row_count(100_000)
            .unwrap_err()
            .is_warehouse_stage_limit());
        let split_batch = crate::cdc_apply::warehouse_sql_test_batch_with_rows(501, 8);
        let split = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::BigQuery,
            "`project.dataset.users`",
            "`project.dataset._skippr_tombstones_users`",
            &split_batch,
        )
        .unwrap();
        assert_eq!(
            split
                .setup_statements
                .iter()
                .filter(|statement| statement.starts_with("INSERT INTO"))
                .count(),
            2
        );
        assert!(
            split
                .transactional_script(crate::cdc_apply::CdcWarehouseDialect::BigQuery)
                .len()
                <= 900 * 1024
        );

        let oversized = crate::cdc_apply::warehouse_sql_test_batch_with_rows(2, 200 * 1024);
        let error = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::BigQuery,
            "`project.dataset.users`",
            "`project.dataset._skippr_tombstones_users`",
            &oversized,
        )
        .unwrap_err();
        assert!(error.is_warehouse_stage_limit());
        let guarded = crate::cdc_apply::guarded_warehouse_cdc_sql::<super::BigqueryCdcBackend>(
            crate::cdc_apply::CdcWarehouseDialect::BigQuery,
            "`project.dataset.users`",
            "`project.dataset._skippr_tombstones_users`",
            &oversized,
        )
        .unwrap();
        assert_eq!(guarded.len(), 2);
        assert!(super::BIGQUERY_CDC_LOAD_JOB_BLOCKER.contains("query jobs only"));
    }

    #[test]
    fn bigquery_insert_row_escapes_with_backslash_and_encodes_binary() {
        use datafusion::arrow::array::{ArrayRef, BinaryArray, Date32Array, StringArray};
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::arrow::record_batch::RecordBatch;
        use std::sync::Arc;

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("day", DataType::Date32, true),
                Field::new("name", DataType::Utf8, true),
                Field::new("raw", DataType::Binary, true),
            ])),
            vec![
                Arc::new(Date32Array::from(vec![Some(19_723)])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("it's")])),
                Arc::new(BinaryArray::from_vec(vec![b"ab".as_slice()])),
            ],
        )
        .unwrap();
        assert_eq!(
            crate::cdc_apply::sql_values_row::<super::BigqueryCdcBackend>(&batch, 0).unwrap(),
            r"('2024-01-01', 'it\'s', FROM_HEX('6162'))"
        );
    }
}
