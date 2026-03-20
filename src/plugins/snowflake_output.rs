use async_trait::async_trait;
use dashmap::DashSet;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use tracing::{error, info, warn};

use crate::buffer::BufferChunker;
use crate::discover::SkipprDataType;
use crate::helpers::configuration::{Config, DataOutputSnowflakePluginConfig, OutputPluginConfig};
use crate::plugins::DataOutputPlugin;

static ENSURED_SCHEMAS: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
static ENSURED_TABLES: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

const ASYNC_POLL_MAX: u32 = 120;
const ASYNC_POLL_INTERVAL_MS: u64 = 500;

#[allow(dead_code)]
pub struct DataOutputSnowflakePlugin {
    pub(crate) config: DataOutputSnowflakePluginConfig,
    pub(crate) buffer_name: String,
    client: reqwest::Client,
    token: tokio::sync::RwLock<Option<String>>,
}

impl From<OutputPluginConfig> for DataOutputSnowflakePluginConfig {
    fn from(plugin_config: OutputPluginConfig) -> Self {
        match plugin_config {
            OutputPluginConfig::Snowflake(config) => config,
            _ => panic!("Invalid plugin type for Snowflake"),
        }
    }
}

impl DataOutputSnowflakePlugin {
    pub async fn new(buffer_name: String) -> Self {
        let config = Self::load_config();
        Self {
            config,
            buffer_name,
            client: reqwest::Client::new(),
            token: tokio::sync::RwLock::new(None),
        }
    }

    pub async fn new_with_config(
        buffer_name: String,
        mut config: DataOutputSnowflakePluginConfig,
    ) -> Self {
        if config.private_key_path.is_none() {
            let p = Config::getenv("SNOWFLAKE_PRIVATE_KEY_PATH", "");
            if !p.is_empty() {
                config.private_key_path = Some(p);
            }
        }
        Self {
            config,
            buffer_name,
            client: reqwest::Client::new(),
            token: tokio::sync::RwLock::new(None),
        }
    }

    fn load_config() -> DataOutputSnowflakePluginConfig {
        DataOutputSnowflakePluginConfig {
            account: Config::getenv("SNOWFLAKE_ACCOUNT", ""),
            user: Config::getenv("SNOWFLAKE_USER", ""),
            password: {
                let v = Config::getenv("SNOWFLAKE_PASSWORD", "");
                if v.is_empty() { None } else { Some(v) }
            },
            warehouse: Config::getenv("SNOWFLAKE_WAREHOUSE", ""),
            database: Config::getenv("SNOWFLAKE_DATABASE", ""),
            schema: Config::getenv("SNOWFLAKE_SCHEMA", ""),
            role: {
                let r = Config::getenv("SNOWFLAKE_ROLE", "");
                if r.is_empty() { None } else { Some(r) }
            },
            stage: {
                let s = Config::getenv("SNOWFLAKE_STAGE", "@~");
                Some(s)
            },
            format: None,
            private_key_path: {
                let p = Config::getenv("SNOWFLAKE_PRIVATE_KEY_PATH", "");
                if p.is_empty() { None } else { Some(p) }
            },
        }
    }

    pub fn namespace_to_table_name(namespace: &str) -> String {
        namespace.replace('.', "_").to_lowercase()
    }

    #[allow(dead_code)]
    fn skippr_type_to_snowflake(data_type: &SkipprDataType) -> &'static str {
        match data_type {
            SkipprDataType::String => "VARCHAR",
            SkipprDataType::Integer => "NUMBER(38,0)",
            SkipprDataType::Long => "NUMBER(38,0)",
            SkipprDataType::Double => "DOUBLE",
            SkipprDataType::Boolean => "BOOLEAN",
            SkipprDataType::Date => "DATE",
            SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => "TIMESTAMP_NTZ",
            SkipprDataType::Array | SkipprDataType::Record | SkipprDataType::Map => "VARIANT",
            _ => "VARCHAR",
        }
    }

    async fn authenticate(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        {
            let guard = self.token.read().await;
            if let Some(ref t) = *guard {
                return Ok(t.clone());
            }
        }

        let token = if let Some(ref key_path) = self.config.private_key_path {
            info!(target: "snowflake", "authenticating via key-pair: {}", key_path);
            self.authenticate_keypair(key_path).await?
        } else {
            info!(target: "snowflake", "authenticating via password (no SNOWFLAKE_PRIVATE_KEY_PATH set)");
            self.authenticate_password().await?
        };

        {
            let mut guard = self.token.write().await;
            *guard = Some(token.clone());
        }

        Ok(token)
    }

    async fn authenticate_password(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let password = self.config.password.as_deref().unwrap_or_default();
        let url = format!(
            "https://{}.snowflakecomputing.com/session/v1/login-request",
            self.config.account
        );

        let payload = serde_json::json!({
            "data": {
                "CLIENT_APP_ID": "skippr",
                "CLIENT_APP_VERSION": "1.0",
                "ACCOUNT_NAME": self.config.account,
                "LOGIN_NAME": self.config.user,
                "PASSWORD": password,
            }
        });

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        let token = body
            .pointer("/data/token")
            .and_then(|t| t.as_str())
            .ok_or("No token in auth response")?
            .to_string();

        Ok(token)
    }

    async fn authenticate_keypair(&self, key_path: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use rsa::pkcs8::DecodePrivateKey;
        use sha2::{Sha256, Digest};

        let pem_content = std::fs::read_to_string(key_path)
            .map_err(|e| format!("failed to read private key at {}: {}", key_path, e))?;

        let private_key = rsa::RsaPrivateKey::from_pkcs8_pem(&pem_content)
            .map_err(|e| format!("failed to parse PKCS8 private key: {}", e))?;

        let public_key = private_key.to_public_key();
        let public_key_der = rsa::pkcs8::EncodePublicKey::to_public_key_der(&public_key)
            .map_err(|e| format!("failed to encode public key: {}", e))?;
        let fingerprint = {
            let mut hasher = Sha256::new();
            hasher.update(public_key_der.as_bytes());
            STANDARD.encode(hasher.finalize())
        };

        let account_upper = self.config.account.to_uppercase();
        let user_upper = self.config.user.to_uppercase();
        let qualified_user = format!("{}.{}", account_upper, user_upper);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        let claims = serde_json::json!({
            "iss": format!("{}.SHA256:{}", qualified_user, fingerprint),
            "sub": qualified_user,
            "iat": now,
            "exp": now + 3600,
        });

        let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(pem_content.as_bytes())
            .map_err(|e| format!("failed to create JWT encoding key: {}", e))?;

        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let jwt = jsonwebtoken::encode(&header, &claims, &encoding_key)
            .map_err(|e| format!("failed to sign JWT: {}", e))?;

        tracing::info!(target: "snowflake", "authenticated via key-pair JWT");
        Ok(jwt)
    }

    fn auth_headers(
        &self,
        token: &str,
    ) -> (String, &'static str) {
        if self.config.private_key_path.is_some() {
            (format!("Bearer {}", token), "KEYPAIR_JWT")
        } else {
            (format!("Snowflake Token=\"{}\"", token), "SNOWFLAKE_TOKEN")
        }
    }

    async fn execute_sql(
        &self,
        sql: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let token = self.authenticate().await?;
        let url = format!(
            "https://{}.snowflakecomputing.com/api/v2/statements",
            self.config.account
        );

        let mut payload = serde_json::json!({
            "statement": sql,
            "timeout": 60,
            "database": self.config.database,
            "schema": self.config.schema,
            "warehouse": self.config.warehouse,
        });

        if let Some(ref role) = self.config.role {
            payload["role"] = serde_json::json!(role);
        }

        let (auth_header, token_type) = self.auth_headers(&token);

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", "skippr/1.0")
            .header("Authorization", &auth_header)
            .header("X-Snowflake-Authorization-Token-Type", token_type)
            .json(&payload)
            .send()
            .await?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;

        if !status.is_success() && status.as_u16() != 202 {
            let msg = body["message"].as_str().unwrap_or("unknown error");
            return Err(format!("Snowflake API HTTP {}: {}", status, msg).into());
        }

        let code = body
            .get("code")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        if code == "090001" || code == "000000" || code.is_empty() {
            return Ok(body);
        }

        if code == "333334" {
            return self
                .poll_statement(&body, &token, &auth_header, token_type)
                .await;
        }

        let msg = body["message"].as_str().unwrap_or("unknown error");
        error!("Snowflake SQL error code={} message={}", code, msg);
        Err(format!("Snowflake SQL error ({}): {}", code, msg).into())
    }

    async fn poll_statement(
        &self,
        initial_body: &serde_json::Value,
        _token: &str,
        auth_header: &str,
        token_type: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let handle = initial_body
            .get("statementHandle")
            .and_then(|h| h.as_str())
            .ok_or("Async response missing statementHandle")?;

        let poll_url = format!(
            "https://{}.snowflakecomputing.com/api/v2/statements/{}",
            self.config.account, handle
        );

        for attempt in 1..=ASYNC_POLL_MAX {
            tokio::time::sleep(std::time::Duration::from_millis(ASYNC_POLL_INTERVAL_MS)).await;

            let resp = self
                .client
                .get(&poll_url)
                .header("Accept", "application/json")
                .header("User-Agent", "skippr/1.0")
                .header("Authorization", auth_header)
                .header("X-Snowflake-Authorization-Token-Type", token_type)
                .send()
                .await?;

            let body: serde_json::Value = resp.json().await?;
            let code = body
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("");

            match code {
                "090001" | "000000" | "" => return Ok(body),
                "333334" => {
                    if attempt % 20 == 0 {
                        warn!(
                            "Snowflake statement {} still running after {}s",
                            handle,
                            (attempt as u64 * ASYNC_POLL_INTERVAL_MS) / 1000
                        );
                    }
                    continue;
                }
                _ => {
                    let msg = body["message"].as_str().unwrap_or("unknown error");
                    error!("Snowflake SQL error code={} message={}", code, msg);
                    return Err(
                        format!("Snowflake SQL error ({}): {}", code, msg).into(),
                    );
                }
            }
        }

        Err(format!(
            "Snowflake statement {} timed out after {}s",
            handle,
            (ASYNC_POLL_MAX as u64 * ASYNC_POLL_INTERVAL_MS) / 1000
        )
        .into())
    }

    fn arrow_type_to_snowflake(dt: &ArrowDataType) -> &'static str {
        match dt {
            ArrowDataType::Boolean => "BOOLEAN",
            ArrowDataType::Int8 | ArrowDataType::Int16 | ArrowDataType::Int32
            | ArrowDataType::Int64 | ArrowDataType::UInt8 | ArrowDataType::UInt16
            | ArrowDataType::UInt32 | ArrowDataType::UInt64 => "NUMBER(38,0)",
            ArrowDataType::Float16 | ArrowDataType::Float32 | ArrowDataType::Float64 => "DOUBLE",
            ArrowDataType::Date32 | ArrowDataType::Date64 => "DATE",
            ArrowDataType::Timestamp(_, _) => "TIMESTAMP_NTZ",
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => "VARCHAR",
            _ => "VARCHAR",
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
            ArrowDataType::Int8 => format!("{}", array.as_any().downcast_ref::<Int8Array>().unwrap().value(row)),
            ArrowDataType::Int16 => format!("{}", array.as_any().downcast_ref::<Int16Array>().unwrap().value(row)),
            ArrowDataType::Int32 => format!("{}", array.as_any().downcast_ref::<Int32Array>().unwrap().value(row)),
            ArrowDataType::Int64 => format!("{}", array.as_any().downcast_ref::<Int64Array>().unwrap().value(row)),
            ArrowDataType::UInt8 => format!("{}", array.as_any().downcast_ref::<UInt8Array>().unwrap().value(row)),
            ArrowDataType::UInt16 => format!("{}", array.as_any().downcast_ref::<UInt16Array>().unwrap().value(row)),
            ArrowDataType::UInt32 => format!("{}", array.as_any().downcast_ref::<UInt32Array>().unwrap().value(row)),
            ArrowDataType::UInt64 => format!("{}", array.as_any().downcast_ref::<UInt64Array>().unwrap().value(row)),
            ArrowDataType::Float32 => format!("{}", array.as_any().downcast_ref::<Float32Array>().unwrap().value(row)),
            ArrowDataType::Float64 => format!("{}", array.as_any().downcast_ref::<Float64Array>().unwrap().value(row)),
            ArrowDataType::Date32 => {
                let days = array.as_any().downcast_ref::<Date32Array>().unwrap().value(row);
                let date = chrono::NaiveDate::from_num_days_from_ce_opt(days + 719_163).unwrap_or_default();
                format!("'{}'", date.format("%Y-%m-%d"))
            }
            ArrowDataType::Date64 => {
                let ms = array.as_any().downcast_ref::<Date64Array>().unwrap().value(row);
                let secs = ms / 1000;
                let dt = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default();
                format!("'{}'", dt.format("%Y-%m-%d"))
            }
            ArrowDataType::Timestamp(unit, _) => {
                let ts = match unit {
                    datafusion::arrow::datatypes::TimeUnit::Second => {
                        let a = array.as_any().downcast_ref::<TimestampSecondArray>().unwrap();
                        chrono::DateTime::from_timestamp(a.value(row), 0)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Millisecond => {
                        let a = array.as_any().downcast_ref::<TimestampMillisecondArray>().unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(v / 1000, ((v % 1000) * 1_000_000) as u32)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Microsecond => {
                        let a = array.as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(v / 1_000_000, ((v % 1_000_000) * 1000) as u32)
                    }
                    datafusion::arrow::datatypes::TimeUnit::Nanosecond => {
                        let a = array.as_any().downcast_ref::<TimestampNanosecondArray>().unwrap();
                        let v = a.value(row);
                        chrono::DateTime::from_timestamp(v / 1_000_000_000, (v % 1_000_000_000) as u32)
                    }
                };
                let dt = ts.unwrap_or_default();
                format!("'{}'", dt.format("%Y-%m-%d %H:%M:%S%.f"))
            }
            ArrowDataType::Utf8 => {
                let a = array.as_any().downcast_ref::<StringArray>().unwrap();
                format!("'{}'", a.value(row).replace('\'', "''"))
            }
            ArrowDataType::LargeUtf8 => {
                let a = array.as_any().downcast_ref::<LargeStringArray>().unwrap();
                format!("'{}'", a.value(row).replace('\'', "''"))
            }
            _ => {
                let a = array.as_any().downcast_ref::<StringArray>();
                match a {
                    Some(s) => format!("'{}'", s.value(row).replace('\'', "''")),
                    None => "NULL".to_string(),
                }
            }
        }
    }

    async fn ensure_schema(&self) -> Result<(), std::io::Error> {
        let key = format!("{}.{}", self.config.database, self.config.schema);
        if !ENSURED_SCHEMAS.insert(key.clone()) {
            return Ok(());
        }

        let ddl = format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\".\"{}\"",
            self.config.database, self.config.schema
        );
        info!("Snowflake DDL: {}", ddl);
        if let Err(e) = self.execute_sql(&ddl).await {
            ENSURED_SCHEMAS.remove(&key);
            error!("Snowflake CREATE SCHEMA failed: {}", e);
            return Err(std::io::Error::other(format!("Snowflake CREATE SCHEMA: {}", e)));
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
            .map(|(name, sf_type)| format!("\"{}\" {}", name, sf_type))
            .collect();

        let create_ddl = format!(
            "CREATE TABLE IF NOT EXISTS {} ({})",
            fq_table,
            cols_sql.join(", ")
        );
        info!("Snowflake DDL: {}", create_ddl);
        if let Err(e) = self.execute_sql(&create_ddl).await {
            ENSURED_TABLES.remove(&fq_table.to_string());
            error!("Snowflake CREATE TABLE failed: {}", e);
            return Err(std::io::Error::other(format!("Snowflake CREATE TABLE: {}", e)));
        }

        for (col_name, sf_type) in col_defs {
            let alter_ddl = format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS \"{}\" {}",
                fq_table, col_name, sf_type
            );
            if let Err(e) = self.execute_sql(&alter_ddl).await {
                let msg = e.to_string();
                if !msg.contains("already exists") {
                    ENSURED_TABLES.remove(&fq_table.to_string());
                    error!("Snowflake ALTER TABLE ADD COLUMN failed: {}", e);
                    return Err(std::io::Error::other(format!(
                        "Snowflake ALTER TABLE: {}",
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
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();

        let col_defs: Vec<(String, &str)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_uppercase(),
                    Self::arrow_type_to_snowflake(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!(
            "\"{}\".\"{}\".\"{}\"",
            self.config.database,
            self.config.schema,
            table_name.to_uppercase()
        );

        self.ensure_schema().await?;
        self.ensure_table(&fq_table, &col_defs).await?;

        let col_names: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name().to_uppercase()))
            .collect();
        let col_list = col_names.join(", ");

        let mut total_rows = 0usize;
        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
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
                    info!("Inserted {} rows into {} (total: {})", num_rows, table_name, total_rows);
                }
                Err(e) => {
                    error!("Snowflake INSERT failed: {}", e);
                    counters::dec_uploads_in_flight();
                    return Err(std::io::Error::other(format!("Snowflake INSERT: {}", e)));
                }
            }
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!("Snowflake sync complete: {} total rows into {}", total_rows, table_name);
        Ok(())
    }
}

#[async_trait]
impl DataOutputPlugin for DataOutputSnowflakePlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        self.inner_sync(stream, filename).await
    }
}
