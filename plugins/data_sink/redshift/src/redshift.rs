use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_redshiftdata::Client as RedshiftClient;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use serde_derive::Deserialize;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use dashmap::DashSet;
use once_cell::sync::Lazy;

use super::parquet_util::serialize_to_parquet;
use skippr_runtime_sdk::sink_compat::BufferChunker;
use crate::helpers::configuration::DataSinkPluginConfig;
use skippr_runtime_sdk::plugins::{DataSink, SchemaSink};

static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

pub struct RedshiftCdcBackend;

impl super::cdc_apply::CdcApplyBackend for RedshiftCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "VARBYTE";

    fn binary_literal(hex: &str) -> String {
        format!("FROM_HEX('{hex}')")
    }
}

#[derive(Debug, Deserialize, Clone)]
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
    pub format: Option<String>,
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
}

impl DataSinkRedshiftPlugin {
    pub async fn new_with_config(
        buffer_name: String,
        config: DataSinkRedshiftPluginConfig,
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
        }
    }

    fn namespace_to_table_name(namespace: &str) -> String {
        namespace.replace('.', "_").to_lowercase()
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

    fn arrow_value_to_sql(array: &dyn Array, row: usize) -> String {
        if array.is_null(row) {
            return "NULL".to_string();
        }
        match array.data_type() {
            ArrowDataType::Boolean => {
                let a = array.as_any().downcast_ref::<BooleanArray>().unwrap();
                if a.value(row) { "TRUE" } else { "FALSE" }.to_string()
            }
            ArrowDataType::Int8 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Int8Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Int16 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Int16Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Int32 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Int32Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Int64 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::UInt32 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<UInt32Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::UInt64 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Float32 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Float32Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Float64 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::Utf8 => {
                let a = array.as_any().downcast_ref::<StringArray>().unwrap();
                format!("'{}'", a.value(row).replace('\'', "''"))
            }
            ArrowDataType::LargeUtf8 => {
                let a = array.as_any().downcast_ref::<LargeStringArray>().unwrap();
                format!("'{}'", a.value(row).replace('\'', "''"))
            }
            _ => {
                if let Some(s) = array.as_any().downcast_ref::<StringArray>() {
                    format!("'{}'", s.value(row).replace('\'', "''"))
                } else {
                    "NULL".to_string()
                }
            }
        }
    }

    fn arrow_type_to_redshift(dt: &ArrowDataType) -> &'static str {
        match dt {
            ArrowDataType::Boolean => "BOOLEAN",
            ArrowDataType::Int8 | ArrowDataType::Int16 => "SMALLINT",
            ArrowDataType::Int32 | ArrowDataType::UInt8 | ArrowDataType::UInt16 => "INTEGER",
            ArrowDataType::Int64 | ArrowDataType::UInt32 | ArrowDataType::UInt64 => "BIGINT",
            ArrowDataType::Float16 | ArrowDataType::Float32 => "REAL",
            ArrowDataType::Float64 => "DOUBLE PRECISION",
            ArrowDataType::Date32 | ArrowDataType::Date64 => "DATE",
            ArrowDataType::Timestamp(_, _) => "TIMESTAMP",
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => "VARCHAR(65535)",
            _ => "VARCHAR(65535)",
        }
    }

    async fn sync_via_s3(
        &self,
        stream: SendableRecordBatchStream,
        filename: &str,
        table_name: &str,
    ) -> Result<(), std::io::Error> {
        let s3_client = self.s3_client.as_ref().unwrap();
        let bucket = self.config.staging_s3_bucket.as_deref().unwrap();
        let prefix = self
            .config
            .staging_s3_prefix
            .as_deref()
            .unwrap_or("skippr-staging");

        let parquet_bytes = serialize_to_parquet(stream).await?;
        let row_count = parquet_bytes.meta_data.num_rows as u64;

        let md5_digest = md5::compute(filename.as_bytes());
        let s3_key = format!(
            "{}/{}.parquet",
            prefix.trim_matches('/'),
            hex::encode(&md5_digest.0)
        );

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

        let copy_sql = format!(
            "COPY {} FROM 's3://{}/{}' IAM_ROLE '{}' FORMAT AS PARQUET",
            table_name, bucket, s3_key, iam_role
        );

        self.execute_statement(&copy_sql).await?;
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
    ) -> Result<(), std::io::Error> {
        let arrow_schema = stream.schema();
        let col_names: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name().to_lowercase()))
            .collect();
        let col_list = col_names.join(", ");

        let mut total_rows = 0usize;
        while let Some(batch_result) = stream.next().await {
            let batch =
                batch_result.map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }

            let mut value_rows = Vec::with_capacity(num_rows);
            for row in 0..num_rows {
                let vals: Vec<String> = (0..batch.num_columns())
                    .map(|col| Self::arrow_value_to_sql(batch.column(col).as_ref(), row))
                    .collect();
                value_rows.push(format!("({})", vals.join(", ")));
            }

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
    async fn ensure_table(
        &self,
        table_name: &str,
        col_defs: &[(String, &str)],
    ) -> Result<(), std::io::Error> {
        if col_defs.is_empty() {
            return Ok(());
        }
        let cols_sql: Vec<String> = col_defs
            .iter()
            .map(|(name, rs_type)| format!("\"{}\" {}", name, rs_type))
            .collect();
        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} ({})",
            table_name,
            cols_sql.join(", ")
        );
        info!("Redshift DDL: {}", create_sql);
        if let Err(e) = self.execute_statement(&create_sql).await {
            let msg = e.to_string();
            if !msg.contains("already exists") {
                return Err(e);
            }
        }
        Ok(())
    }

    async fn sync_cdc(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
        ctx: &skippr_runtime_sdk::plugins::cdc::SyncContext,
    ) -> Result<(), std::io::Error> {
        use super::cdc_apply::{
            ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
            tombstone_table_name, upsert_if_newer_sql,
        };
        use skippr_runtime_sdk::metrics::counters;
        use skippr_runtime_sdk::plugins::cdc::MutationKind;

        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "redshift", "CDC context without contract or business keys; falling back to append");
                counters::inc_uploads_in_flight();
                let namespace = BufferChunker::decode_file_namespace(&filename);
                let table_name = self
                    .config
                    .table
                    .clone()
                    .unwrap_or_else(|| Self::namespace_to_table_name(&namespace));
                let arrow_schema = stream.schema();
                let col_defs: Vec<(String, &str)> = arrow_schema
                    .fields()
                    .iter()
                    .map(|f| {
                        (
                            f.name().to_lowercase(),
                            Self::arrow_type_to_redshift(f.data_type()),
                        )
                    })
                    .collect();
                self.ensure_table(&table_name, &col_defs)
                    .await
                    .map_err(|e| {
                        counters::dec_uploads_in_flight();
                        e
                    })?;
                let result = if self.config.staging_s3_bucket.is_some() {
                    self.sync_via_s3(stream, &filename, &table_name).await
                } else {
                    self.sync_via_insert(stream, &table_name).await
                };
                match &result {
                    Ok(_) => counters::add_upload(1),
                    Err(e) => error!("Redshift sync failed for {}: {}", table_name, e),
                }
                counters::dec_uploads_in_flight();
                return result;
            }
        };

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = self
            .config
            .table
            .clone()
            .unwrap_or_else(|| Self::namespace_to_table_name(&namespace));
        let arrow_schema = stream.schema();

        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_redshift(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!("\"{}\"", table_name);

        self.ensure_table(&table_name, &col_defs)
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                e
            })?;

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
            let bk_type_pairs: Vec<(String, String)> = contract
                .business_key_columns
                .iter()
                .map(|bk| {
                    let rs_type = col_defs
                        .iter()
                        .find(|(name, _)| name == bk)
                        .map(|(_, t)| (*t).to_string())
                        .unwrap_or_else(|| "VARCHAR(65535)".to_string());
                    (bk.clone(), rs_type)
                })
                .collect();
            let tombstone_ddl =
                ddl_create_tombstone_table::<RedshiftCdcBackend>(&tombstone_tbl, &bk_type_pairs);
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

        let bk_names_quoted: Vec<String> = contract
            .business_key_columns
            .iter()
            .map(|bk| format!("\"{}\"", bk))
            .collect();

        let bk_types: Vec<String> = contract
            .business_key_columns
            .iter()
            .map(|bk| {
                col_defs
                    .iter()
                    .find(|(name, _)| name == bk)
                    .map(|(_, t)| (*t).to_string())
                    .unwrap_or_else(|| "VARCHAR(65535)".to_string())
            })
            .collect();

        let col_names_quoted: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name().to_lowercase()))
            .collect();

        let mut row_offset = 0usize;
        let mut total_rows = 0usize;

        while let Some(batch_result) = stream.next().await {
            let batch =
                batch_result.map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }

            for row in 0..num_rows {
                let meta_idx = row_offset + row;
                let row_meta = ctx.part_meta.rows.get(meta_idx).ok_or_else(|| {
                    std::io::Error::other(format!(
                        "CDC row metadata missing at index {} (have {})",
                        meta_idx,
                        ctx.part_meta.rows.len()
                    ))
                })?;

                let order_token_hex: String = row_meta
                    .order_token
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect();

                match row_meta.mutation {
                    MutationKind::Snapshot | MutationKind::Insert | MutationKind::Update => {
                        let mut all_names = col_names_quoted.clone();
                        all_names.push("\"_skippr_order_token\"".to_string());

                        let mut all_values: Vec<String> = (0..batch.num_columns())
                            .map(|col| Self::arrow_value_to_sql(batch.column(col).as_ref(), row))
                            .collect();
                        all_values.push(format!("FROM_HEX('{}')", order_token_hex));

                        let sql = upsert_if_newer_sql::<RedshiftCdcBackend>(
                            &fq_table,
                            &tombstone_table,
                            &all_names,
                            &all_values,
                            &bk_names_quoted,
                            &order_token_hex,
                        );

                        self.execute_statement(&sql).await.map_err(|e| {
                            error!("CDC upsert failed for {}: {}", fq_table, e);
                            counters::dec_uploads_in_flight();
                            e
                        })?;
                    }
                    MutationKind::Delete => {
                        let bk_values: Vec<String> = contract
                            .business_key_columns
                            .iter()
                            .map(|bk| {
                                let col_idx = arrow_schema
                                    .fields()
                                    .iter()
                                    .position(|f| f.name().to_lowercase() == *bk)
                                    .unwrap_or(0);
                                Self::arrow_value_to_sql(batch.column(col_idx).as_ref(), row)
                            })
                            .collect();

                        let sql = delete_if_newer_sql::<RedshiftCdcBackend>(
                            &fq_table,
                            &tombstone_table,
                            &bk_names_quoted,
                            &bk_values,
                            &bk_types,
                            &order_token_hex,
                        );

                        self.execute_statement(&sql).await.map_err(|e| {
                            error!("CDC delete failed for {}: {}", fq_table, e);
                            counters::dec_uploads_in_flight();
                            e
                        })?;
                    }
                }
            }

            row_offset += num_rows;
            total_rows += num_rows;
            skippr_runtime_sdk::metrics::counters::add_parquet_rows(num_rows as u64);
            info!(
                "CDC applied {} rows to {} (total: {})",
                num_rows, table_name, total_rows
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

#[async_trait]
impl DataSink for DataSinkRedshiftPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        if let Some(ctx) = cdc_ctx {
            return self.sync_cdc(stream, filename, ctx).await;
        }
        use skippr_runtime_sdk::metrics::counters;
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = self
            .config
            .table
            .clone()
            .unwrap_or_else(|| Self::namespace_to_table_name(&namespace));

        let arrow_schema = stream.schema();
        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_redshift(f.data_type()),
                )
            })
            .collect();

        self.ensure_table(&table_name, &col_defs)
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                e
            })?;

        let result = if self.config.staging_s3_bucket.is_some() {
            self.sync_via_s3(stream, &filename, &table_name).await
        } else {
            self.sync_via_insert(stream, &table_name).await
        };

        match &result {
            Ok(_) => {
                counters::add_upload(1);
                info!("Redshift sync complete for {}", table_name);
            }
            Err(e) => {
                error!("Redshift sync failed for {}: {}", table_name, e);
            }
        }

        counters::dec_uploads_in_flight();
        result
    }

    fn capability(&self) -> Option<&'static skippr_runtime_sdk::plugins::cdc::SinkCapability> {
        Some(&skippr_runtime_sdk::plugins::cdc::sink_capabilities::REDSHIFT)
    }
}

#[async_trait]
impl SchemaSink for DataSinkRedshiftPlugin {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &skippr_runtime_sdk::discover::OutputMetadata,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::converters::skippr_arrow::convert_skippr_to_arrow;

        let fields: std::collections::HashMap<String, skippr_runtime_sdk::discover::OutputMetadata> = metadata
            .child_fields()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let arrow_schema = convert_skippr_to_arrow(Box::new(fields)).map_err(|e| {
            std::io::Error::other(format!(
                "Arrow schema conversion for '{}': {}",
                namespace, e
            ))
        })?;

        let table_name = self
            .config
            .table
            .clone()
            .unwrap_or_else(|| Self::namespace_to_table_name(namespace));
        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_redshift(f.data_type()),
                )
            })
            .collect();

        self.ensure_table(&table_name, &col_defs).await
    }
}
