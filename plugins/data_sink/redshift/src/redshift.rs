use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_redshiftdata::Client as RedshiftClient;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use serde_derive::Deserialize;
use skippr_runtime_sdk::SkipprConfig;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use dashmap::DashSet;
use once_cell::sync::Lazy;

use super::parquet_util::serialize_to_parquet;
use crate::helpers::configuration::DataSinkPluginConfig;
use skippr_runtime_sdk::plugins::alter_ddl::{
    dest_table_name, publish_warehouse_table, DestPublishCache, InstalledDestSchema,
    PublishedBatchColumns, WarehouseDialect,
};
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::sink_compat::BufferChunker;

static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
const REDSHIFT_CDC_S3_STAGE_BLOCKER: &str = "the current S3 COPY path serializes target-only \
    Parquet and cannot load CDC metadata into the same BatchExecuteStatement transaction";

pub struct RedshiftCdcBackend;

impl super::cdc_apply::CdcApplyBackend for RedshiftCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "VARBYTE";

    fn binary_literal(hex: &str) -> String {
        format!("FROM_HEX('{hex}')")
    }
}

#[derive(Debug, Deserialize, SkipprConfig, Clone)]
pub struct DataSinkRedshiftPluginConfig {
    pub cluster_identifier: Option<String>,
    pub workgroup_name: Option<String>,
    pub database: String,
    pub db_user: Option<String>,
    pub table: Option<String>,
    pub region: Option<String>,
    pub staging_s3_bucket: Option<String>,
    pub staging_s3_prefix: Option<String>,
    pub iam_role_arn: Option<String>,
    /// Query/model only; ignored at ingest.
    #[serde(default)]
    #[allow(dead_code)]
    pub schema: Option<String>,
}

impl TryFrom<DataSinkPluginConfig> for DataSinkRedshiftPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Redshift")
    }
}

pub struct DataSinkRedshiftPlugin {
    config: DataSinkRedshiftPluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
    redshift_client: RedshiftClient,
    s3_client: Option<S3Client>,
    order_fields: Vec<String>,
    installed: tokio::sync::RwLock<InstalledDestSchema>,
    publish_cache: DestPublishCache,
}

skippr_runtime_sdk::declare_sink_spec!(
    RedshiftSinkSpec,
    DataSinkRedshiftPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::REDSHIFT,
    skippr_runtime_sdk::plugins::FinalStateIdempotentApply,
    skippr_runtime_sdk::plugins::AlterSupport::Warehouse(
        skippr_runtime_sdk::plugins::alter_ddl::WarehouseDialect::Redshift
    )
);

impl DataSinkRedshiftPlugin {
    pub async fn new_with_config(
        buffer_name: String,
        config: DataSinkRedshiftPluginConfig,
        order_fields: Vec<String>,
    ) -> Self {
        let mut aws_builder = aws_config::defaults(BehaviorVersion::latest());
        if let Some(ref region) = config.region {
            aws_builder = aws_builder.region(aws_config::Region::new(region.clone()));
        }
        let aws_config = aws_builder.load().await;
        let redshift_client = RedshiftClient::new(&aws_config);
        let s3_client = if config.staging_s3_bucket.is_some() {
            Some(S3Client::new(&aws_config))
        } else {
            None
        };
        Self {
            config,
            buffer_name,
            redshift_client,
            s3_client,
            order_fields,
            installed: tokio::sync::RwLock::new(InstalledDestSchema::default()),
            publish_cache: DestPublishCache::default(),
        }
    }

    fn table_name(&self, namespace: &str) -> String {
        self.config
            .table
            .clone()
            .unwrap_or_else(|| dest_table_name(namespace))
    }

    fn fq_table(&self, namespace: &str) -> String {
        let table_name = self.table_name(namespace);
        match &self.config.schema {
            Some(schema) => format!("\"{}\".\"{}\"", schema, table_name),
            None => format!("\"{}\"", table_name),
        }
    }

    async fn publish_namespace(&self, namespace: &str) -> Result<(), std::io::Error> {
        let (version, metadata) = self.installed.read().await.get(namespace)?;
        publish_warehouse_table(
            &self.publish_cache,
            self,
            WarehouseDialect::Redshift,
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
            WarehouseDialect::Redshift,
            namespace,
            batch,
        )
    }

    async fn sync_append(
        &self,
        stream: SendableRecordBatchStream,
        filename: &str,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::metrics::counters;
        let namespace = BufferChunker::decode_file_namespace(filename);
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let fq_table = self.fq_table(&namespace);
        counters::inc_uploads_in_flight();
        let result = if self.config.staging_s3_bucket.is_some() {
            self.sync_via_s3(stream, filename, &fq_table, &col_defs)
                .await
        } else {
            self.sync_via_insert(stream, &fq_table, &col_defs).await
        };
        match &result {
            Ok(_) => {
                counters::add_upload(1);
                info!("Redshift sync complete for {}", fq_table);
            }
            Err(e) => error!("Redshift sync failed for {}: {}", fq_table, e),
        }
        counters::dec_uploads_in_flight();
        result
    }

    async fn execute_statement(&self, sql: &str) -> Result<String, std::io::Error> {
        let mut stmt = self
            .redshift_client
            .execute_statement()
            .database(&self.config.database)
            .sql(sql);
        if let Some(ref cluster) = self.config.cluster_identifier {
            stmt = stmt.cluster_identifier(cluster);
        }
        if let Some(ref wg) = self.config.workgroup_name {
            stmt = stmt.workgroup_name(wg);
        }
        if let Some(ref user) = self.config.db_user {
            stmt = stmt.db_user(user);
        }

        let result = stmt
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("Redshift execute: {}", e)))?;
        let statement_id = result.id().unwrap_or_default().to_string();

        loop {
            let desc = self
                .redshift_client
                .describe_statement()
                .id(&statement_id)
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let status = desc
                .status()
                .map(|s| s.as_str().to_string())
                .unwrap_or_default();
            match status.as_str() {
                "FINISHED" => return Ok(statement_id),
                "FAILED" | "ABORTED" => {
                    return Err(std::io::Error::other(format!(
                        "Redshift statement {}: {}",
                        status,
                        desc.error().unwrap_or_default()
                    )));
                }
                _ => sleep(Duration::from_secs(1)).await,
            }
        }
    }

    async fn execute_transaction(
        &self,
        statements: Vec<String>,
        idempotency_key: &str,
    ) -> Result<String, std::io::Error> {
        let mut request = self
            .redshift_client
            .batch_execute_statement()
            .database(&self.config.database)
            .set_sqls(Some(statements))
            .client_token(idempotency_key);
        if let Some(ref cluster) = self.config.cluster_identifier {
            request = request.cluster_identifier(cluster);
        }
        if let Some(ref workgroup) = self.config.workgroup_name {
            request = request.workgroup_name(workgroup);
        }
        if let Some(ref user) = self.config.db_user {
            request = request.db_user(user);
        }

        let result = request
            .send()
            .await
            .map_err(|error| std::io::Error::other(format!("Redshift batch execute: {error}")))?;
        let statement_id = result.id().unwrap_or_default().to_string();

        loop {
            let description = self
                .redshift_client
                .describe_statement()
                .id(&statement_id)
                .send()
                .await
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            match description.status().map(|status| status.as_str()) {
                Some("FINISHED") => return Ok(statement_id),
                Some("FAILED" | "ABORTED") => {
                    return Err(std::io::Error::other(format!(
                        "Redshift transaction {}: {}",
                        description
                            .status()
                            .map(|status| status.as_str())
                            .unwrap_or("UNKNOWN"),
                        description.error().unwrap_or_default()
                    )));
                }
                _ => sleep(Duration::from_secs(1)).await,
            }
        }
    }

    async fn sync_via_s3(
        &self,
        stream: SendableRecordBatchStream,
        filename: &str,
        table_name: &str,
        col_defs: &PublishedBatchColumns,
    ) -> Result<(), std::io::Error> {
        let s3_client = self.s3_client.as_ref().unwrap();
        let bucket = self.config.staging_s3_bucket.as_deref().unwrap();
        let prefix = self
            .config
            .staging_s3_prefix
            .as_deref()
            .unwrap_or("skippr-staging");

        let parquet_bytes = serialize_to_parquet(stream, &self.order_fields).await?;
        let row_count = parquet_bytes.num_rows as u64;

        let file_id = hex::encode(md5::compute(filename.as_bytes()).0);
        let s3_key = format!("{}/{}.parquet", prefix.trim_matches('/'), file_id);

        s3_client
            .put_object()
            .bucket(bucket)
            .key(&s3_key)
            .body(ByteStream::from(parquet_bytes.bytes))
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("S3 staging upload: {}", e)))?;

        info!(
            "Redshift: staged {} rows to s3://{}/{}",
            row_count, bucket, s3_key
        );

        let iam_role =
            self.config.iam_role_arn.as_deref().ok_or_else(|| {
                std::io::Error::other("Redshift: iam_role_arn required for S3 COPY")
            })?;

        let statements = s3_copy_statements(
            table_name,
            col_defs.columns(),
            &format!("s3://{bucket}/{s3_key}"),
            iam_role,
            &file_id,
        );
        self.execute_transaction(statements, &file_id).await?;
        info!(
            "Redshift: COPY INTO {} complete ({} rows)",
            table_name, row_count
        );

        skippr_runtime_sdk::metrics::counters::add_parquet_rows(row_count);
        Ok(())
    }

    async fn sync_via_insert(
        &self,
        mut stream: SendableRecordBatchStream,
        table_name: &str,
        col_defs: &PublishedBatchColumns,
    ) -> Result<(), std::io::Error> {
        let col_list = col_defs
            .columns()
            .iter()
            .map(|(name, _)| WarehouseDialect::Redshift.quote_ident(name))
            .collect::<Vec<_>>()
            .join(", ");

        let mut total_rows = 0usize;
        while let Some(batch_result) = stream.next().await {
            let batch =
                batch_result.map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }

            let value_rows = (0..num_rows)
                .map(|row| crate::cdc_apply::sql_values_row::<RedshiftCdcBackend>(&batch, row))
                .collect::<Result<Vec<_>, _>>()?;

            let insert_sql = format!(
                "INSERT INTO {} ({}) VALUES {}",
                table_name,
                col_list,
                value_rows.join(", ")
            );

            self.execute_statement(&insert_sql).await?;
            total_rows += num_rows;
            skippr_runtime_sdk::metrics::counters::add_parquet_rows(num_rows as u64);
            info!(
                "Redshift: inserted {} rows into {} (total: {})",
                num_rows, table_name, total_rows
            );
        }

        Ok(())
    }
}

impl DataSinkRedshiftPlugin {
    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &skippr_runtime_sdk::plugins::cdc::SyncContext,
    ) -> Result<(), std::io::Error> {
        use super::cdc_apply::{
            append_record_batch_to_cdc_apply, collapse_apply_batch, ddl_add_order_token_column,
            ddl_create_tombstone_table, guarded_warehouse_cdc_row_sql, tombstone_table_name,
            warehouse_bulk_cdc_sql, CdcApplyBatch, CdcApplyColumn, CdcWarehouseDialect,
        };
        use skippr_runtime_sdk::metrics::counters;

        tracing::debug!(
            target: "redshift",
            blocker = REDSHIFT_CDC_S3_STAGE_BLOCKER,
            "using bounded Data API staging with guarded overflow"
        );
        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "redshift", "CDC context without contract or business keys; falling back to append");
                return self.sync_append(stream, &filename).await;
            }
        };

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = self.table_name(&namespace);
        let fq_table = self.fq_table(&namespace);
        let col_defs = self.batch_columns(&namespace, &stream.schema()).await?;
        let key_types = col_defs.key_types(&contract.business_key_columns)?;
        counters::inc_uploads_in_flight();

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<RedshiftCdcBackend>(&fq_table);
            if let Err(e) = self.execute_statement(&order_col_ddl).await {
                let msg = e.to_string();
                if !msg.contains("already exists") {
                    CDC_DDL_ENSURED.remove(&fq_table);
                    counters::dec_uploads_in_flight();
                    return Err(e);
                }
            }

            let tombstone_tbl = tombstone_table_name(&fq_table);
            let tombstone_ddl =
                ddl_create_tombstone_table::<RedshiftCdcBackend>(&tombstone_tbl, &key_types);
            if let Err(e) = self.execute_statement(&tombstone_ddl).await {
                let msg = e.to_string();
                if !msg.contains("already exists") {
                    CDC_DDL_ENSURED.remove(&fq_table);
                    counters::dec_uploads_in_flight();
                    return Err(e);
                }
            }

            info!(target: "redshift", "CDC DDL applied for {}", fq_table);
        }

        let tombstone_table = tombstone_table_name(&fq_table);

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
                CdcWarehouseDialect::Redshift,
                &fq_table,
                &tombstone_table,
                &apply_batch,
            ) {
                Ok(sql) => {
                    self.execute_transaction(sql.atomic_statements(), &sql.idempotency_key)
                        .await
                        .map_err(|error| {
                            error!("Redshift bulk CDC apply failed for {}: {}", fq_table, error);
                            counters::dec_uploads_in_flight();
                            error
                        })?;
                }
                Err(error) if error.is_warehouse_stage_limit() => {
                    warn!(
                        target: "redshift",
                        "CDC chunk exceeds Data API 100 KiB/40-statement envelope; using guarded row apply: {}",
                        error
                    );
                    let collapsed = collapse_apply_batch(&apply_batch);
                    for row in &collapsed.rows {
                        let statement = guarded_warehouse_cdc_row_sql::<RedshiftCdcBackend>(
                            CdcWarehouseDialect::Redshift,
                            &fq_table,
                            &tombstone_table,
                            &collapsed,
                            row,
                        )?;
                        self.execute_statement(&statement).await.map_err(|error| {
                            counters::dec_uploads_in_flight();
                            error
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
                target: "redshift",
                "CDC safely applied {} rows to {}",
                total_rows,
                table_name
            );
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Redshift CDC sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }
}

/// Parquet COPY maps columns by position and needs an equal column count, so
/// it loads a stage in batch order and inserts into the target by name.
fn s3_copy_statements(
    table_name: &str,
    columns: &[(String, &'static str)],
    s3_uri: &str,
    iam_role: &str,
    file_id: &str,
) -> Vec<String> {
    let stage = WarehouseDialect::Redshift.quote_ident(&format!("_skippr_copy_{file_id}"));
    let defs = columns
        .iter()
        .map(|(name, ty)| format!("{} {ty}", WarehouseDialect::Redshift.quote_ident(name)))
        .collect::<Vec<_>>()
        .join(", ");
    let names = columns
        .iter()
        .map(|(name, _)| WarehouseDialect::Redshift.quote_ident(name))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        format!("CREATE TEMP TABLE {stage} ({defs})"),
        format!(
            "COPY {stage} FROM '{}' IAM_ROLE '{}' FORMAT AS PARQUET",
            s3_uri.replace('\'', "''"),
            iam_role.replace('\'', "''")
        ),
        format!("INSERT INTO {table_name} ({names}) SELECT {names} FROM {stage}"),
    ]
}

#[async_trait]
impl DataSink for DataSinkRedshiftPlugin {
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
            None => self.sync_append(stream, &filename).await,
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
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::REDSHIFT
    }

    fn installed_dest_schema(&self) -> Option<&tokio::sync::RwLock<InstalledDestSchema>> {
        Some(&self.installed)
    }

    fn alter_support(&self) -> skippr_runtime_sdk::plugins::AlterSupport {
        skippr_runtime_sdk::plugins::AlterSupport::Warehouse(
            skippr_runtime_sdk::plugins::alter_ddl::WarehouseDialect::Redshift,
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
            WarehouseDialect::Redshift,
            &self.fq_table(namespace),
            namespace,
            alter,
        )
        .await
    }
}

#[async_trait]
impl skippr_runtime_sdk::plugins::alter_ddl::WarehouseExec for DataSinkRedshiftPlugin {
    async fn columns(&self, namespace: &str) -> std::io::Result<Vec<String>> {
        let table_name = self.table_name(namespace);
        let sql = match &self.config.schema {
            Some(schema) => format!(
                "SELECT column_name FROM information_schema.columns WHERE table_schema = '{}' AND table_name = '{}'",
                schema.replace('\'', "''"),
                table_name.replace('\'', "''"),
            ),
            None => format!(
                "SELECT column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = '{}'",
                table_name.replace('\'', "''"),
            ),
        };
        let statement_id = self.execute_statement(&sql).await?;
        let mut names = Vec::new();
        let mut next_token = None;
        loop {
            let page = self
                .redshift_client
                .get_statement_result()
                .id(&statement_id)
                .set_next_token(next_token)
                .send()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            for row in page.records() {
                names.push(redshift_row_text(row)?);
            }
            match page.next_token() {
                Some(token) if !token.is_empty() => next_token = Some(token.to_string()),
                _ => return Ok(names),
            }
        }
    }

    async fn execute(&self, sql: &str) -> std::io::Result<()> {
        self.execute_statement(sql).await.map(|_| ())
    }

    async fn query_first(&self, sql: &str) -> std::io::Result<Option<String>> {
        let statement_id = self.execute_statement(sql).await?;
        let page = self
            .redshift_client
            .get_statement_result()
            .id(&statement_id)
            .send()
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        let Some(row) = page.records().first() else {
            return Ok(None);
        };
        redshift_row_text(row).map(Some)
    }
}

fn redshift_row_text(row: &[aws_sdk_redshiftdata::types::Field]) -> std::io::Result<String> {
    let Some(field) = row.first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Redshift result row has no cell",
        ));
    };
    match field {
        aws_sdk_redshiftdata::types::Field::StringValue(value) => Ok(value.clone()),
        aws_sdk_redshiftdata::types::Field::LongValue(value) => Ok(value.to_string()),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Redshift cell is not text or long: {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn redshift_bulk_cdc_sql_is_one_batch_transaction_statement_family() {
        let batch = crate::cdc_apply::warehouse_sql_test_batch();
        let sql = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Redshift,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &batch,
        )
        .unwrap();
        let statements = sql.atomic_statements();
        let all_sql = statements.join("\n");

        assert!(all_sql.contains("CREATE TEMP TABLE"));
        assert!(all_sql.contains("FROM_HEX"));
        assert!(all_sql.contains("UPDATE \"users\" AS target"));
        assert!(all_sql.contains("INSERT INTO \"users\""));
        assert!(all_sql.contains("DELETE FROM \"users\" AS target"));
        assert!(all_sql.contains("UPDATE \"_skippr_tombstones_users\" AS tombstone"));
        assert_eq!(all_sql.matches("DROP TABLE IF EXISTS").count(), 2);
        assert!(sql.idempotency_key.starts_with("skippr-cdc-"));
        assert!(statements.len() <= 40);
    }

    #[test]
    fn redshift_splits_under_data_api_limits_and_falls_back_by_row_cap() {
        assert!(crate::cdc_apply::CdcWarehouseDialect::Redshift
            .validate_stage_row_count(15_000)
            .is_ok());
        assert!(crate::cdc_apply::CdcWarehouseDialect::Redshift
            .validate_stage_row_count(15_001)
            .unwrap_err()
            .is_warehouse_stage_limit());
        assert!(crate::cdc_apply::CdcWarehouseDialect::Redshift
            .validate_stage_row_count(100_000)
            .unwrap_err()
            .is_warehouse_stage_limit());
        let split_batch = crate::cdc_apply::warehouse_sql_test_batch_with_rows(501, 8);
        let split = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Redshift,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
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
        assert!(split.atomic_statements().len() <= 40);
        assert!(split
            .atomic_statements()
            .iter()
            .all(|statement| statement.len() <= 90 * 1024));

        let oversized = crate::cdc_apply::warehouse_sql_test_batch_with_rows(15_001, 0);
        let error = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Redshift,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &oversized,
        )
        .unwrap_err();
        assert!(error.is_warehouse_stage_limit());
        let guarded = crate::cdc_apply::guarded_warehouse_cdc_sql::<super::RedshiftCdcBackend>(
            crate::cdc_apply::CdcWarehouseDialect::Redshift,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &oversized,
        )
        .unwrap();
        assert_eq!(guarded.len(), 15_001);
        assert!(super::REDSHIFT_CDC_S3_STAGE_BLOCKER.contains("target-only"));
    }

    #[test]
    fn redshift_row_text_fails_closed_on_empty_or_unexpected_cells() {
        use aws_sdk_redshiftdata::types::Field;
        assert_eq!(
            super::redshift_row_text(&[Field::StringValue("region".into())]).unwrap(),
            "region"
        );
        assert_eq!(
            super::redshift_row_text(&[Field::LongValue(7)]).unwrap(),
            "7"
        );
        assert!(super::redshift_row_text(&[]).is_err());
        assert!(super::redshift_row_text(&[Field::IsNull(true)]).is_err());
    }

    #[test]
    fn redshift_insert_row_quotes_published_date_timestamp_and_binary() {
        use datafusion::arrow::array::{
            ArrayRef, BinaryArray, Date32Array, TimestampMicrosecondArray,
        };
        use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
        use datafusion::arrow::record_batch::RecordBatch;
        use std::sync::Arc;

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("day", DataType::Date32, true),
                Field::new("at", DataType::Timestamp(TimeUnit::Microsecond, None), true),
                Field::new("raw", DataType::Binary, true),
            ])),
            vec![
                Arc::new(Date32Array::from(vec![Some(19_723)])) as ArrayRef,
                Arc::new(TimestampMicrosecondArray::from(vec![Some(0)])),
                Arc::new(BinaryArray::from_vec(vec![b"ab".as_slice()])),
            ],
        )
        .unwrap();
        assert_eq!(
            crate::cdc_apply::sql_values_row::<super::RedshiftCdcBackend>(&batch, 0).unwrap(),
            "('2024-01-01', '1970-01-01T00:00:00', FROM_HEX('6162'))"
        );
    }

    #[test]
    fn s3_copy_loads_a_batch_ordered_stage_then_inserts_by_name() {
        let statements = super::s3_copy_statements(
            "\"public\".\"trips\"",
            &[
                ("amount".to_string(), "DOUBLE PRECISION"),
                ("id".to_string(), "BIGINT"),
            ],
            "s3://bucket/stage/abc.parquet",
            "arn:aws:iam::1:role/r",
            "abc",
        );
        assert_eq!(
            statements,
            vec![
                "CREATE TEMP TABLE \"_skippr_copy_abc\" (\"amount\" DOUBLE PRECISION, \"id\" BIGINT)",
                "COPY \"_skippr_copy_abc\" FROM 's3://bucket/stage/abc.parquet' IAM_ROLE 'arn:aws:iam::1:role/r' FORMAT AS PARQUET",
                "INSERT INTO \"public\".\"trips\" (\"amount\", \"id\") SELECT \"amount\", \"id\" FROM \"_skippr_copy_abc\"",
            ]
        );
    }
}
