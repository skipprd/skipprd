use async_trait::async_trait;
use dashmap::DashSet;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use serde_derive::Deserialize;
use tracing::{error, info, warn};

use skippr_runtime_sdk::sink_compat::BufferChunker;
use skippr_runtime_sdk::plugins::{DataSink, SchemaSink};

static ENSURED_DATASETS: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
static ENSURED_TABLES: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkBigqueryPluginConfig {
    pub project: String,
    pub dataset: String,
    pub location: Option<String>,
    pub credentials_path: Option<String>,
    pub format: Option<String>,
}

pub struct BigqueryCdcBackend;

impl super::cdc_apply::CdcApplyBackend for BigqueryCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "BYTES";

    fn binary_literal(hex: &str) -> String {
        format!("FROM_HEX('{hex}')")
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
}

const JOB_POLL_MAX: u32 = 120;
const JOB_POLL_INTERVAL_MS: u64 = 500;

pub struct DataSinkBigqueryPlugin {
    config: DataSinkBigqueryPluginConfig,
    #[allow(dead_code)]
    buffer_name: String,
    client: reqwest::Client,
    token: tokio::sync::RwLock<Option<(String, std::time::Instant)>>,
}

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
        }
    }

    fn namespace_to_table_name(namespace: &str) -> String {
        namespace.replace('.', "_").to_lowercase()
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
        let token = self.authenticate().await?;
        let url = format!(
            "https://bigquery.googleapis.com/bigquery/v2/projects/{}/queries",
            self.config.project
        );

        let mut payload = serde_json::json!({
            "query": sql,
            "useLegacySql": false,
            "timeoutMs": 120_000,
            "maxResults": 0,
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

    fn arrow_type_to_bigquery(dt: &ArrowDataType) -> &'static str {
        match dt {
            ArrowDataType::Boolean => "BOOL",
            ArrowDataType::Int8
            | ArrowDataType::Int16
            | ArrowDataType::Int32
            | ArrowDataType::Int64
            | ArrowDataType::UInt8
            | ArrowDataType::UInt16
            | ArrowDataType::UInt32
            | ArrowDataType::UInt64 => "INT64",
            ArrowDataType::Float16 | ArrowDataType::Float32 | ArrowDataType::Float64 => "FLOAT64",
            ArrowDataType::Date32 | ArrowDataType::Date64 => "DATE",
            ArrowDataType::Timestamp(_, _) => "TIMESTAMP",
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => "STRING",
            _ => "STRING",
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
            ArrowDataType::UInt8 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<UInt8Array>()
                    .unwrap()
                    .value(row)
            ),
            ArrowDataType::UInt16 => format!(
                "{}",
                array
                    .as_any()
                    .downcast_ref::<UInt16Array>()
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
            ArrowDataType::Date32 => {
                let days = array
                    .as_any()
                    .downcast_ref::<Date32Array>()
                    .unwrap()
                    .value(row);
                let date = chrono::NaiveDate::from_num_days_from_ce_opt(days + 719_163)
                    .unwrap_or_default();
                format!("DATE '{}'", date.format("%Y-%m-%d"))
            }
            ArrowDataType::Date64 => {
                let ms = array
                    .as_any()
                    .downcast_ref::<Date64Array>()
                    .unwrap()
                    .value(row);
                let secs = ms / 1000;
                let dt = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default();
                format!("DATE '{}'", dt.format("%Y-%m-%d"))
            }
            ArrowDataType::Timestamp(unit, _) => {
                let ts = match unit {
                    datafusion::arrow::datatypes::TimeUnit::Second => {
                        let a = array
                            .as_any()
                            .downcast_ref::<TimestampSecondArray>()
                            .unwrap();
                        chrono::DateTime::from_timestamp(a.value(row), 0)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Millisecond => {
                        let a = array
                            .as_any()
                            .downcast_ref::<TimestampMillisecondArray>()
                            .unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(v / 1000, ((v % 1000) * 1_000_000) as u32)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Microsecond => {
                        let a = array
                            .as_any()
                            .downcast_ref::<TimestampMicrosecondArray>()
                            .unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(
                            v / 1_000_000,
                            ((v % 1_000_000) * 1000) as u32,
                        )
                    }
                    datafusion::arrow::datatypes::TimeUnit::Nanosecond => {
                        let a = array
                            .as_any()
                            .downcast_ref::<TimestampNanosecondArray>()
                            .unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(
                            v / 1_000_000_000,
                            (v % 1_000_000_000) as u32,
                        )
                    }
                };
                let dt = ts.unwrap_or_default();
                format!("TIMESTAMP '{}'", dt.format("%Y-%m-%d %H:%M:%S%.6f"))
            }
            ArrowDataType::Utf8 => {
                let a = array.as_any().downcast_ref::<StringArray>().unwrap();
                format!(
                    "'{}'",
                    a.value(row).replace('\'', "\\'").replace('\\', "\\\\")
                )
            }
            ArrowDataType::LargeUtf8 => {
                let a = array.as_any().downcast_ref::<LargeStringArray>().unwrap();
                format!(
                    "'{}'",
                    a.value(row).replace('\'', "\\'").replace('\\', "\\\\")
                )
            }
            _ => {
                let a = array.as_any().downcast_ref::<StringArray>();
                match a {
                    Some(s) => format!(
                        "'{}'",
                        s.value(row).replace('\'', "\\'").replace('\\', "\\\\")
                    ),
                    None => "NULL".to_string(),
                }
            }
        }
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

    async fn ensure_table(
        &self,
        fq_table: &str,
        col_defs: &[(String, &str)],
    ) -> Result<(), std::io::Error> {
        if col_defs.is_empty() {
            return Ok(());
        }

        if !ENSURED_TABLES.insert(fq_table.to_string()) {
            return Ok(());
        }

        let cols_sql: Vec<String> = col_defs
            .iter()
            .map(|(name, bq_type)| format!("`{}` {}", name, bq_type))
            .collect();

        let create_ddl = format!(
            "CREATE TABLE IF NOT EXISTS {} ({})",
            fq_table,
            cols_sql.join(", ")
        );
        info!("BigQuery DDL: {}", create_ddl);
        if let Err(e) = self.execute_sql(&create_ddl).await {
            ENSURED_TABLES.remove(&fq_table.to_string());
            error!("BigQuery CREATE TABLE failed: {}", e);
            return Err(std::io::Error::other(format!(
                "BigQuery CREATE TABLE: {}",
                e
            )));
        }

        for (col_name, bq_type) in col_defs {
            let alter_ddl = format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS `{}` {}",
                fq_table, col_name, bq_type
            );
            if let Err(e) = self.execute_sql(&alter_ddl).await {
                let msg = e.to_string();
                if !msg.contains("already exists") && !msg.contains("duplicate column") {
                    ENSURED_TABLES.remove(&fq_table.to_string());
                    error!("BigQuery ALTER TABLE ADD COLUMN failed: {}", e);
                    return Err(std::io::Error::other(format!(
                        "BigQuery ALTER TABLE: {}",
                        e
                    )));
                }
            }
        }

        info!("Ensured table {}", fq_table);
        Ok(())
    }

    async fn inner_sync(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::metrics::counters;
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();

        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_bigquery(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!(
            "`{}.{}.{}`",
            self.config.project, self.config.dataset, table_name
        );

        self.ensure_dataset().await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        self.ensure_table(&fq_table, &col_defs).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

        let col_names: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("`{}`", f.name().to_lowercase()))
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
            ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
            upsert_if_newer_sql,
        };
        use skippr_runtime_sdk::metrics::counters;
        use skippr_runtime_sdk::plugins::cdc::MutationKind;

        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "bigquery", "CDC context without contract or business keys; falling back to append");
                return self.inner_sync(stream, filename).await;
            }
        };

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();

        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_bigquery(f.data_type()),
                )
            })
            .collect();

        let fq_table_ddl = format!(
            "`{}.{}.{}`",
            self.config.project, self.config.dataset, table_name
        );
        let fq_table = format!(
            "{}.{}.{}",
            self.config.project, self.config.dataset, table_name
        );

        self.ensure_dataset().await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        self.ensure_table(&fq_table_ddl, &col_defs)
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                e
            })?;

        let tombstone_table = format!(
            "{}.{}._skippr_tombstones_{}",
            self.config.project, self.config.dataset, table_name
        );

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<BigqueryCdcBackend>(&fq_table);
            if let Err(e) = self.execute_sql(&order_col_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(std::io::Error::other(format!("BigQuery CDC DDL: {}", e)));
            }

            let bk_type_pairs: Vec<(String, String)> = contract
                .business_key_columns
                .iter()
                .map(|bk| {
                    let bq_type = col_defs
                        .iter()
                        .find(|(name, _)| name == bk)
                        .map(|(_, t)| (*t).to_string())
                        .unwrap_or_else(|| "STRING".to_string());
                    (bk.clone(), bq_type)
                })
                .collect();
            let tombstone_ddl =
                ddl_create_tombstone_table::<BigqueryCdcBackend>(&tombstone_table, &bk_type_pairs);
            if let Err(e) = self.execute_sql(&tombstone_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(std::io::Error::other(format!("BigQuery CDC DDL: {}", e)));
            }

            info!(target: "bigquery", "CDC DDL applied for {}", fq_table);
        }

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
                    .unwrap_or_else(|| "STRING".to_string())
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

                        let sql = upsert_if_newer_sql::<BigqueryCdcBackend>(
                            &fq_table,
                            &tombstone_table,
                            &all_names,
                            &all_values,
                            &bk_names_quoted,
                            &order_token_hex,
                        );

                        if let Err(e) = self.execute_sql(&sql).await {
                            error!("CDC upsert failed for {}: {}", fq_table, e);
                            counters::dec_uploads_in_flight();
                            return Err(std::io::Error::other(format!(
                                "BigQuery CDC upsert: {}",
                                e
                            )));
                        }
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

                        let sql = delete_if_newer_sql::<BigqueryCdcBackend>(
                            &fq_table,
                            &tombstone_table,
                            &bk_names_quoted,
                            &bk_values,
                            &bk_types,
                            &order_token_hex,
                        );

                        if let Err(e) = self.execute_sql(&sql).await {
                            error!("CDC delete failed for {}: {}", fq_table, e);
                            counters::dec_uploads_in_flight();
                            return Err(std::io::Error::other(format!(
                                "BigQuery CDC delete: {}",
                                e
                            )));
                        }
                    }
                }
            }

            row_offset += num_rows;
            total_rows += num_rows;
            counters::add_parquet_rows(num_rows as u64);
            info!(
                "CDC applied {} rows to {} (total: {})",
                num_rows, table_name, total_rows
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
        match cdc_ctx {
            Some(ctx) => self.sync_cdc(stream, filename, ctx).await,
            None => self.inner_sync(stream, filename).await,
        }
    }

    fn capability(&self) -> Option<&'static skippr_runtime_sdk::plugins::cdc::SinkCapability> {
        Some(&skippr_runtime_sdk::plugins::cdc::sink_capabilities::BIGQUERY)
    }
}

#[async_trait]
impl SchemaSink for DataSinkBigqueryPlugin {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &skippr_runtime_sdk::discover::OutputMetadata,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::converters::skippr_arrow::convert_skippr_to_arrow;

        self.ensure_dataset().await?;

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

        let table_name = Self::namespace_to_table_name(namespace);
        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_lowercase(),
                    Self::arrow_type_to_bigquery(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!(
            "`{}.{}.{}`",
            self.config.project, self.config.dataset, table_name
        );

        self.ensure_table(&fq_table, &col_defs).await
    }
}
