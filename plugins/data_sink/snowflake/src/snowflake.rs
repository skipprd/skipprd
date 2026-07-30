use async_trait::async_trait;
use aws_credential_types::provider::ProvideCredentials;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use dashmap::{DashMap, DashSet};
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::{GcpCredential, GcpCredentialProvider, GoogleCloudStorageBuilder};
use object_store::path::Path as ObjectPath;
use object_store::{
    Attribute, Attributes, ObjectStore, ObjectStoreExt, PutOptions, StaticCredentialProvider,
};
use once_cell::sync::Lazy;
use serde_derive::Deserialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use tracing::{error, info, warn};
use url::Url;

use skippr_runtime_sdk::discover::{OutputMetadata, SkipprDataType};
use skippr_runtime_sdk::plugins::{DataSink, SchemaSink};
use skippr_runtime_sdk::protocol::{RuntimeSchemaState, SchemaDelta};
use skippr_runtime_sdk::sink_compat::BufferChunker;

static ENSURED_SCHEMAS: Lazy<DashMap<String, Arc<tokio::sync::OnceCell<()>>>> =
    Lazy::new(DashMap::new);
static TABLE_DDL_GUARDS: Lazy<DashMap<String, Arc<tokio::sync::OnceCell<()>>>> =
    Lazy::new(DashMap::new);
static ENSURED_TABLES: Lazy<DashMap<String, Vec<(String, String)>>> = Lazy::new(DashMap::new);
static CDC_DDL_ENSURED: Lazy<DashSet<String>> = Lazy::new(DashSet::new);
const SNOWFLAKE_CDC_FILE_STAGE_BLOCKER: &str = "the current PUT/COPY path serializes target-only \
    Parquet and does not load CDC metadata into a transaction-local staging table";

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkSnowflakePluginConfig {
    pub account: String,
    pub user: String,
    #[serde(default)]
    pub password: Option<String>,
    pub warehouse: String,
    pub database: String,
    pub schema: String,
    pub role: Option<String>,
    pub stage: Option<String>,
    pub format: Option<String>,
    #[serde(default)]
    pub private_key_path: Option<String>,
    #[serde(default)]
    pub staging_uri: Option<String>,
    #[serde(default)]
    pub staging_storage_integration: Option<String>,
    #[serde(default)]
    pub staging_azure_sas_token: Option<String>,
    #[serde(default)]
    pub staging_azure_account_key: Option<String>,
    #[serde(default)]
    pub staging_gcs_service_account_key_path: Option<String>,
}

pub struct SnowflakeCdcBackend;

impl super::cdc_apply::CdcApplyBackend for SnowflakeCdcBackend {
    const ORDER_TOKEN_TYPE: &'static str = "BINARY";

    fn binary_literal(hex: &str) -> String {
        format!("HEX_DECODE_BINARY('{hex}')")
    }
}

const ASYNC_POLL_MAX: u32 = 600;
const ASYNC_POLL_INTERVAL_MS: u64 = 500;
const SNOWFLAKE_HTTP_MAX_ATTEMPTS: usize = 3;
const SNOWFLAKE_HTTP_RETRY_BASE_MS: u64 = 250;

const INSERT_CHUNK_STRUCTURED: usize = 100;
const INSERT_CHUNK_FLAT: usize = 1000;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalStageProvider {
    S3,
    Azure,
    Gcs,
}

#[derive(Debug, Clone)]
struct ExternalStageLocation {
    provider: ExternalStageProvider,
    root: String,
    prefix: String,
    azure_account: Option<String>,
    azure_host: Option<String>,
}

struct StageUploadInfo {
    location_type: String,
    /// Provider-native location in format "container-or-bucket/prefix/"
    location: String,
    region: String,
    creds: HashMap<String, String>,
    encryption_material: Option<EncryptionMaterial>,
    end_point: Option<String>,
    storage_account: Option<String>,
    #[allow(dead_code)]
    use_virtual_url: bool,
    #[allow(dead_code)]
    use_regional_url: bool,
}

struct EncryptionMaterial {
    query_stage_master_key: String,
    query_id: String,
    smk_id: i64,
}

#[allow(dead_code)]
pub struct DataSinkSnowflakePlugin {
    pub(crate) config: DataSinkSnowflakePluginConfig,
    pub(crate) buffer_name: String,
    client: reqwest::Client,
    /// Cached v2 API token (JWT for key-pair, session token for password)
    token: tokio::sync::RwLock<Option<(String, std::time::Instant)>>,
    /// Cached v1 session token (always a session token, works for PUT)
    session_token: tokio::sync::RwLock<Option<(String, std::time::Instant)>>,
    schema_state: tokio::sync::RwLock<BTreeMap<String, OutputMetadata>>,
    schema_versions: tokio::sync::RwLock<BTreeMap<String, u64>>,
}

skippr_runtime_sdk::declare_sink_spec!(
    SnowflakeSinkSpec,
    DataSinkSnowflakePlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::SNOWFLAKE,
    skippr_runtime_sdk::plugins::FinalStateIdempotentApply
);

skippr_runtime_sdk::declare_schema_sink_spec!(
    SnowflakeSchemaSinkSpec,
    DataSinkSnowflakePlugin,
    "Snowflake"
);

const TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(50 * 60);
const SESSION_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(3 * 3600);

impl DataSinkSnowflakePlugin {
    fn boxed_error(message: impl Into<String>) -> BoxError {
        std::io::Error::other(message.into()).into()
    }

    fn is_transient_http_message(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        [
            "unexpected end of file",
            "connection reset",
            "connection aborted",
            "connection closed",
            "broken pipe",
            "early eof",
            "eof",
            "operation timed out",
            "timed out",
        ]
        .iter()
        .any(|needle| message.contains(needle))
    }

    fn is_transient_snowflake_http_error(err: &reqwest::Error) -> bool {
        if err.is_timeout() || err.is_connect() {
            return true;
        }

        let mut source = Some(err as &dyn std::error::Error);
        while let Some(err) = source {
            if Self::is_transient_http_message(&err.to_string()) {
                return true;
            }
            source = err.source();
        }
        false
    }

    async fn retry_snowflake_http<T, F, Fut>(
        &self,
        phase: &str,
        mut request: F,
    ) -> Result<T, BoxError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, reqwest::Error>>,
    {
        let mut last_err = None;
        for attempt in 1..=SNOWFLAKE_HTTP_MAX_ATTEMPTS {
            match request().await {
                Ok(value) => return Ok(value),
                Err(err)
                    if attempt < SNOWFLAKE_HTTP_MAX_ATTEMPTS
                        && Self::is_transient_snowflake_http_error(&err) =>
                {
                    warn!(
                        "Snowflake {} HTTP attempt {}/{} failed transiently: {}",
                        phase, attempt, SNOWFLAKE_HTTP_MAX_ATTEMPTS, err
                    );
                    last_err = Some(err);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        SNOWFLAKE_HTTP_RETRY_BASE_MS * attempt as u64,
                    ))
                    .await;
                }
                Err(err) => {
                    return Err(Self::boxed_error(format!(
                        "Snowflake {} HTTP: {}",
                        phase, err
                    )));
                }
            }
        }

        let err = last_err
            .map(|err| err.to_string())
            .unwrap_or_else(|| "request failed without an error".to_string());
        Err(Self::boxed_error(format!(
            "Snowflake {} HTTP: {}",
            phase, err
        )))
    }

    async fn retry_snowflake_json<F>(
        &self,
        phase: &str,
        mut request: F,
    ) -> Result<(reqwest::StatusCode, serde_json::Value), BoxError>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        self.retry_snowflake_http(phase, || {
            let request = request();
            async move {
                let resp = request.send().await?;
                let status = resp.status();
                let body = resp.json().await?;
                Ok((status, body))
            }
        })
        .await
    }

    pub async fn new_with_config(
        buffer_name: String,
        config: DataSinkSnowflakePluginConfig,
    ) -> Self {
        Self {
            config,
            buffer_name,
            client: reqwest::Client::new(),
            token: Default::default(),
            session_token: Default::default(),
            schema_state: Default::default(),
            schema_versions: Default::default(),
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

    // ── Authentication ──────────────────────────────────────────────────

    async fn authenticate(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        {
            let guard = self.token.read().await;
            if let Some((ref t, issued)) = *guard {
                if issued.elapsed() < TOKEN_TTL {
                    return Ok(t.clone());
                }
            }
        }

        let token = if let Some(ref key_path) = self.config.private_key_path {
            info!(target: "snowflake", "authenticating via key-pair: {}", key_path);
            self.authenticate_keypair(key_path).await?
        } else {
            info!(target: "snowflake", "authenticating via password (no private_key_path configured)");
            self.authenticate_password().await?
        };

        {
            let mut guard = self.token.write().await;
            *guard = Some((token.clone(), std::time::Instant::now()));
        }

        Ok(token)
    }

    async fn authenticate_password(
        &self,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
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

        let (_status, body) = self
            .retry_snowflake_json("password login", || {
                self.client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json")
                    .json(&payload)
            })
            .await?;
        let token = body
            .pointer("/data/token")
            .and_then(|t| t.as_str())
            .ok_or("No token in auth response")?
            .to_string();

        Ok(token)
    }

    async fn authenticate_keypair(
        &self,
        key_path: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use rsa::pkcs8::DecodePrivateKey;
        use sha2::{Digest, Sha256};

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

    fn auth_headers(&self, token: &str) -> (String, &'static str) {
        if self.config.private_key_path.is_some() {
            (format!("Bearer {}", token), "KEYPAIR_JWT")
        } else {
            (format!("Snowflake Token=\"{}\"", token), "SNOWFLAKE_TOKEN")
        }
    }

    /// Get a v1-compatible session token. For password auth this is the same
    /// token returned by authenticate(). For key-pair auth we exchange the JWT
    /// for a proper session token via the login endpoint.
    async fn get_session_token(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        {
            let guard = self.session_token.read().await;
            if let Some((ref t, issued)) = *guard {
                if issued.elapsed() < SESSION_TOKEN_TTL {
                    return Ok(t.clone());
                }
            }
        }

        let session_tok = if self.config.private_key_path.is_none() {
            self.authenticate().await?
        } else {
            let jwt = self.authenticate().await?;
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
                    "AUTHENTICATOR": "SNOWFLAKE_JWT",
                    "TOKEN": jwt,
                }
            });
            let (_status, body) = self
                .retry_snowflake_json("key-pair session login", || {
                    self.client
                        .post(&url)
                        .header("Content-Type", "application/json")
                        .header("Accept", "application/json")
                        .json(&payload)
                })
                .await?;
            body.pointer("/data/token")
                .and_then(|t| t.as_str())
                .ok_or("No session token in JWT login response")?
                .to_string()
        };

        {
            let mut guard = self.session_token.write().await;
            *guard = Some((session_tok.clone(), std::time::Instant::now()));
        }
        Ok(session_tok)
    }

    // ── SQL execution ───────────────────────────────────────────────────

    async fn execute_sql(
        &self,
        sql: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        self.execute_sql_request(sql, false).await
    }

    async fn execute_sql_script(
        &self,
        sql: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        self.execute_sql_request(sql, true).await
    }

    async fn execute_sql_request(
        &self,
        sql: &str,
        multi_statement: bool,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let token = self.authenticate().await?;
        let url = format!(
            "https://{}.snowflakecomputing.com/api/v2/statements",
            self.config.account
        );

        let mut payload = serde_json::json!({
            "statement": sql,
            "timeout": 300,
            "database": self.config.database,
            "schema": self.config.schema,
            "warehouse": self.config.warehouse,
        });

        if multi_statement {
            payload["parameters"] = serde_json::json!({ "MULTI_STATEMENT_COUNT": "0" });
        }
        if let Some(ref role) = self.config.role {
            payload["role"] = serde_json::json!(role);
        }

        let (auth_header, token_type) = self.auth_headers(&token);

        let (status, body) = self
            .retry_snowflake_json("SQL execution", || {
                self.client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json")
                    .header("User-Agent", "skippr/1.0")
                    .header("Authorization", &auth_header)
                    .header("X-Snowflake-Authorization-Token-Type", token_type)
                    .json(&payload)
            })
            .await?;

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

            let (_status, body) = self
                .retry_snowflake_json("SQL poll", || {
                    self.client
                        .get(&poll_url)
                        .header("Accept", "application/json")
                        .header("User-Agent", "skippr/1.0")
                        .header("Authorization", auth_header)
                        .header("X-Snowflake-Authorization-Token-Type", token_type)
                })
                .await?;
            let code = body.get("code").and_then(|c| c.as_str()).unwrap_or("");

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
                    return Err(format!("Snowflake SQL error ({}): {}", code, msg).into());
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

    // ── Snowflake file transfer protocol (PUT) ──────────────────────────

    /// Send a PUT command via the v1 session API to obtain stage upload
    /// credentials and encryption material.
    async fn initiate_put(
        &self,
        session_token: &str,
        stage_path: &str,
        filename: &str,
    ) -> Result<StageUploadInfo, BoxError> {
        let request_id = uuid::Uuid::new_v4();
        let url = format!(
            "https://{}.snowflakecomputing.com/queries/v1/query-request?requestId={}",
            self.config.account, request_id
        );

        let sql = format!(
            "PUT 'file:///tmp/{}' '{}' AUTO_COMPRESS=FALSE SOURCE_COMPRESSION=NONE OVERWRITE=TRUE",
            filename, stage_path
        );

        let mut payload = serde_json::json!({
            "sqlText": sql,
            "asyncExec": false,
            "sequenceId": 1,
            "isInternal": false,
        });
        if let Some(ref role) = self.config.role {
            payload["parameters"] = serde_json::json!({"SF_HEADER_ROLE": role});
        }

        let (_status, body) = self
            .retry_snowflake_json("PUT initiation", || {
                self.client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json")
                    .header("User-Agent", "skippr/1.0")
                    .header(
                        "Authorization",
                        format!("Snowflake Token=\"{}\"", session_token),
                    )
                    .json(&payload)
            })
            .await?;
        let success = body
            .get("success")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        if !success {
            let msg = body
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(Self::boxed_error(format!("PUT initiation failed: {}", msg)));
        }

        let data = body.get("data").ok_or("Missing 'data' in PUT response")?;
        Self::parse_stage_upload_info(data)
    }

    fn parse_stage_upload_info(data: &serde_json::Value) -> Result<StageUploadInfo, BoxError> {
        let stage_info = data
            .get("stageInfo")
            .ok_or_else(|| Self::boxed_error("Missing 'stageInfo' in PUT response"))?;

        let location_type = stage_info["locationType"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let location = stage_info["location"].as_str().unwrap_or("").to_string();
        let region = stage_info
            .get("region")
            .and_then(|r| r.as_str())
            .unwrap_or("us-east-1")
            .to_string();
        let end_point = stage_info
            .get("endPoint")
            .and_then(|e| e.as_str())
            .map(String::from);
        let storage_account = stage_info
            .get("storageAccount")
            .and_then(|e| e.as_str())
            .map(String::from);
        let use_virtual_url = stage_info
            .get("useVirtualUrl")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let use_regional_url = stage_info
            .get("useRegionalUrl")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let creds = stage_info
            .get("creds")
            .map(Self::extract_stage_creds)
            .unwrap_or_default();

        Ok(StageUploadInfo {
            location_type,
            location,
            region,
            creds,
            encryption_material: Self::parse_encryption_material(data.get("encryptionMaterial")),
            end_point,
            storage_account,
            use_virtual_url,
            use_regional_url,
        })
    }

    fn extract_stage_creds(raw: &serde_json::Value) -> HashMap<String, String> {
        raw.as_object()
            .map(|obj| {
                obj.iter()
                    .map(|(key, value)| {
                        let parsed = value
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| value.to_string());
                        (key.clone(), parsed)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn parse_encryption_material(raw: Option<&serde_json::Value>) -> Option<EncryptionMaterial> {
        let val = raw?;
        // Can be an array of objects — take the first non-null element
        let obj = if val.is_array() {
            val.as_array()
                .and_then(|arr| arr.first())
                .filter(|v| !v.is_null())
        } else if val.is_object() {
            Some(val)
        } else {
            None
        }?;

        let key = obj
            .get("queryStageMasterKey")
            .and_then(|k| k.as_str())?
            .to_string();
        let query_id = obj
            .get("queryId")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .to_string();
        let smk_id = obj.get("smkId").and_then(|s| s.as_i64()).unwrap_or(0);

        Some(EncryptionMaterial {
            query_stage_master_key: key,
            query_id,
            smk_id,
        })
    }

    fn split_stage_location(location: &str) -> Result<(String, String), BoxError> {
        let slash_pos = location.find('/').unwrap_or(location.len());
        let root = location[..slash_pos].to_string();
        let prefix = location
            .get(slash_pos + 1..)
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();

        if root.is_empty() {
            return Err(Self::boxed_error(format!(
                "Snowflake stage location is missing a bucket or container: '{}'",
                location
            )));
        }

        Ok((root, prefix))
    }

    fn stage_object_key(prefix: &str, filename: &str) -> String {
        if prefix.is_empty() {
            filename.to_string()
        } else {
            format!("{}/{}", prefix, filename)
        }
    }

    fn metadata_attribute(key: &str) -> Attribute {
        Attribute::Metadata(Cow::Owned(key.to_string()))
    }

    fn normalize_https_endpoint(endpoint: &str) -> String {
        let endpoint = endpoint.trim().trim_end_matches('/');
        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.to_string()
        } else {
            format!("https://{}", endpoint.trim_start_matches('/'))
        }
    }

    fn normalize_azure_endpoint(account: &str, endpoint: &str) -> String {
        let endpoint = endpoint.trim();
        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.trim_end_matches('/').to_string()
        } else {
            let endpoint = endpoint.trim_start_matches('.').trim_end_matches('/');
            if endpoint.starts_with(account) {
                format!("https://{}", endpoint)
            } else {
                format!("https://{}.{}", account, endpoint)
            }
        }
    }

    fn parse_sas_query_pairs(sas_token: &str) -> Vec<(String, String)> {
        sas_token
            .trim_start_matches('?')
            .split('&')
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                match (parts.next(), parts.next()) {
                    (Some(key), Some(value)) if !key.is_empty() => Some((
                        Self::percent_decode_query_component(key),
                        Self::percent_decode_query_component(value),
                    )),
                    _ => None,
                }
            })
            .collect()
    }

    fn percent_decode_query_component(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut idx = 0;

        while idx < bytes.len() {
            if bytes[idx] == b'%' && idx + 2 < bytes.len() {
                if let (Some(high), Some(low)) = (
                    Self::hex_digit_value(bytes[idx + 1]),
                    Self::hex_digit_value(bytes[idx + 2]),
                ) {
                    decoded.push((high << 4) | low);
                    idx += 3;
                    continue;
                }
            }

            decoded.push(bytes[idx]);
            idx += 1;
        }

        String::from_utf8(decoded)
            .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
    }

    fn hex_digit_value(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    fn gcs_base_url(info: &StageUploadInfo) -> Option<String> {
        if let Some(ref endpoint) = info.end_point {
            Some(Self::normalize_https_endpoint(endpoint))
        } else if info.use_regional_url {
            Some(format!(
                "https://storage.{}.rep.googleapis.com",
                info.region.trim()
            ))
        } else {
            None
        }
    }

    fn sql_string_literal(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn external_stage_provider_name(provider: ExternalStageProvider) -> &'static str {
        match provider {
            ExternalStageProvider::S3 => "S3",
            ExternalStageProvider::Azure => "Azure Blob Storage",
            ExternalStageProvider::Gcs => "Google Cloud Storage",
        }
    }

    fn parse_external_stage_uri(uri: &str) -> Result<ExternalStageLocation, BoxError> {
        let trimmed = uri.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(Self::boxed_error(
                "SNOWFLAKE_STAGING_URI must not be empty when provided.",
            ));
        }

        let normalized = if let Some(rest) = trimmed.strip_prefix("gs://") {
            format!("gcs://{}", rest)
        } else {
            trimmed.to_string()
        };
        let url = Url::parse(&normalized).map_err(|e| {
            Self::boxed_error(format!("Invalid SNOWFLAKE_STAGING_URI '{}': {}", uri, e))
        })?;

        match url.scheme() {
            "s3" => {
                let bucket = url.host_str().filter(|v| !v.is_empty()).ok_or_else(|| {
                    Self::boxed_error(format!(
                        "S3 staging URI '{}' is missing a bucket name.",
                        uri
                    ))
                })?;
                Ok(ExternalStageLocation {
                    provider: ExternalStageProvider::S3,
                    root: bucket.to_string(),
                    prefix: url.path().trim_matches('/').to_string(),
                    azure_account: None,
                    azure_host: None,
                })
            }
            "gcs" => {
                let bucket = url.host_str().filter(|v| !v.is_empty()).ok_or_else(|| {
                    Self::boxed_error(format!(
                        "GCS staging URI '{}' is missing a bucket name.",
                        uri
                    ))
                })?;
                Ok(ExternalStageLocation {
                    provider: ExternalStageProvider::Gcs,
                    root: bucket.to_string(),
                    prefix: url.path().trim_matches('/').to_string(),
                    azure_account: None,
                    azure_host: None,
                })
            }
            "azure" => {
                let raw_host = url.host_str().filter(|v| !v.is_empty()).ok_or_else(|| {
                    Self::boxed_error(format!(
                        "Azure staging URI '{}' is missing an account host.",
                        uri
                    ))
                })?;
                let azure_host = if raw_host.contains('.') {
                    raw_host.to_string()
                } else {
                    format!("{}.blob.core.windows.net", raw_host)
                };
                let azure_account = raw_host
                    .split('.')
                    .next()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        Self::boxed_error(format!(
                            "Azure staging URI '{}' is missing a storage account name.",
                            uri
                        ))
                    })?
                    .to_string();
                let mut segments = url.path_segments().ok_or_else(|| {
                    Self::boxed_error(format!(
                        "Azure staging URI '{}' is missing a container path.",
                        uri
                    ))
                })?;
                let container = segments.next().filter(|v| !v.is_empty()).ok_or_else(|| {
                    Self::boxed_error(format!(
                        "Azure staging URI '{}' is missing a container name.",
                        uri
                    ))
                })?;
                let prefix = segments.collect::<Vec<_>>().join("/");
                Ok(ExternalStageLocation {
                    provider: ExternalStageProvider::Azure,
                    root: container.to_string(),
                    prefix,
                    azure_account: Some(azure_account),
                    azure_host: Some(azure_host),
                })
            }
            other => Err(Self::boxed_error(format!(
                "Unsupported SNOWFLAKE_STAGING_URI scheme '{}'. Use s3://, azure://, or gcs://.",
                other
            ))),
        }
    }

    fn external_stage_object_key(
        location: &ExternalStageLocation,
        table_name: &str,
        file_id: &uuid::Uuid,
    ) -> String {
        let filename = format!("{}.parquet", file_id);
        let table_prefix = if location.prefix.is_empty() {
            table_name.to_string()
        } else {
            format!("{}/{}", location.prefix, table_name)
        };
        format!("{}/{}", table_prefix.trim_matches('/'), filename)
    }

    fn external_stage_object_uri(location: &ExternalStageLocation, object_key: &str) -> String {
        match location.provider {
            ExternalStageProvider::S3 => format!("s3://{}/{}", location.root, object_key),
            ExternalStageProvider::Azure => format!(
                "azure://{}/{}/{}",
                location.azure_host.as_deref().unwrap_or_default(),
                location.root,
                object_key
            ),
            ExternalStageProvider::Gcs => format!("gcs://{}/{}", location.root, object_key),
        }
    }

    async fn put_external_object(
        store: &dyn ObjectStore,
        object_key: &str,
        payload: bytes::Bytes,
    ) -> Result<(), BoxError> {
        let path = ObjectPath::from(object_key.to_string());
        store
            .put(&path, payload.into())
            .await
            .map_err(|e| Self::boxed_error(e.to_string()))?;
        Ok(())
    }

    async fn delete_external_object(store: &dyn ObjectStore, object_key: &str) {
        let path = ObjectPath::from(object_key.to_string());
        let _ = store.delete(&path).await;
    }

    fn build_external_azure_store(
        &self,
        location: &ExternalStageLocation,
    ) -> Result<Box<dyn ObjectStore>, BoxError> {
        let account = location.azure_account.as_deref().ok_or_else(|| {
            Self::boxed_error("Azure staging URI is missing the storage account name.")
        })?;
        let host = location
            .azure_host
            .as_deref()
            .ok_or_else(|| Self::boxed_error("Azure staging URI is missing the account host."))?;

        let mut builder = MicrosoftAzureBuilder::new()
            .with_account(account)
            .with_container_name(location.root.clone())
            .with_endpoint(format!("https://{}", host));

        if let Some(sas) = self
            .config
            .staging_azure_sas_token
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            builder = builder.with_sas_authorization(Self::parse_sas_query_pairs(&sas));
        } else if let Some(key) = self
            .config
            .staging_azure_account_key
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            builder = builder.with_access_key(key.to_string());
        } else {
            return Err(Self::boxed_error(
                "Azure external staging requires `staging_azure_sas_token` or `staging_azure_account_key` in Snowflake plugin config.",
            ));
        }

        let store = builder.build().map_err(|e| {
            Self::boxed_error(format!(
                "Failed to build Azure external staging store: {}",
                e
            ))
        })?;
        Ok(Box::new(store))
    }

    fn build_external_gcs_store(
        &self,
        location: &ExternalStageLocation,
    ) -> Result<Box<dyn ObjectStore>, BoxError> {
        let mut builder = GoogleCloudStorageBuilder::new().with_bucket_name(location.root.clone());
        if let Some(path) = self
            .config
            .staging_gcs_service_account_key_path
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            builder = builder.with_service_account_path(path);
        } else {
            return Err(Self::boxed_error(
                "GCS external staging requires `staging_gcs_service_account_key_path` in Snowflake plugin config.",
            ));
        }
        let store = builder.build().map_err(|e| {
            Self::boxed_error(format!("Failed to build GCS external staging store: {}", e))
        })?;
        Ok(Box::new(store))
    }

    async fn external_stage_copy_auth_clause(
        &self,
        location: &ExternalStageLocation,
        aws_cfg: Option<&aws_config::SdkConfig>,
    ) -> Result<String, std::io::Error> {
        if let Some(integration) = self.config.staging_storage_integration.as_deref() {
            return Ok(format!("STORAGE_INTEGRATION = {}", integration));
        }

        match location.provider {
            ExternalStageProvider::S3 => {
                let aws_cfg = aws_cfg.ok_or_else(|| {
                    std::io::Error::other(
                        "Missing AWS configuration while preparing Snowflake staging COPY INTO",
                    )
                })?;
                let credentials = aws_cfg
                    .credentials_provider()
                    .ok_or_else(|| {
                        std::io::Error::other("No AWS credentials provider for COPY INTO")
                    })?
                    .provide_credentials()
                    .await
                    .map_err(|e| {
                        std::io::Error::other(format!("Failed to resolve AWS credentials: {}", e))
                    })?;

                let mut clause = format!(
                    "CREDENTIALS = (AWS_KEY_ID={} AWS_SECRET_KEY={}",
                    Self::sql_string_literal(credentials.access_key_id()),
                    Self::sql_string_literal(credentials.secret_access_key())
                );
                if let Some(token) = credentials.session_token() {
                    clause.push_str(&format!(" AWS_TOKEN={}", Self::sql_string_literal(token)));
                }
                clause.push(')');
                Ok(clause)
            }
            ExternalStageProvider::Azure => {
                let sas = self
                    .config
                    .staging_azure_sas_token
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                    std::io::Error::other(
                        "Azure external staging without `staging_storage_integration` requires `staging_azure_sas_token` in Snowflake plugin config.",
                    )
                })?;
                Ok(format!(
                    "CREDENTIALS = (AZURE_SAS_TOKEN={})",
                    Self::sql_string_literal(&sas)
                ))
            }
            ExternalStageProvider::Gcs => Err(std::io::Error::other(
                "GCS external staging requires `staging_storage_integration` for COPY INTO and `staging_gcs_service_account_key_path` for uploads.",
            )),
        }
    }

    fn required_stage_cred(info: &StageUploadInfo, key: &str) -> Result<String, BoxError> {
        info.creds.get(key).cloned().ok_or_else(|| {
            Self::boxed_error(format!(
                "Missing '{}' in Snowflake stage credentials for '{}' storage.",
                key, info.location_type
            ))
        })
    }

    fn build_blob_encryptiondata(enc_key_b64: &str, iv_b64: &str) -> String {
        serde_json::json!({
            "EncryptionMode": "FullBlob",
            "WrappedContentKey": {
                "KeyId": "symmKey1",
                "EncryptedKey": enc_key_b64,
                "Algorithm": "AES_CBC_256"
            },
            "EncryptionAgent": {
                "Protocol": "1.0",
                "EncryptionAlgorithm": "AES_CBC_256"
            },
            "ContentEncryptionIV": iv_b64,
            "KeyWrappingMetadata": {
                "EncryptionLibrary": "Java 5.3.0"
            }
        })
        .to_string()
    }

    fn build_stage_upload_payload(
        info: &StageUploadInfo,
        data: &[u8],
    ) -> Result<(Vec<u8>, Attributes), BoxError> {
        let mut attributes = Attributes::new();
        attributes.insert(Attribute::ContentType, "application/octet-stream".into());

        let digest_key = if info.location_type == "AZURE" {
            "sfcdigest"
        } else {
            "sfc-digest"
        };
        attributes.insert(
            Self::metadata_attribute(digest_key),
            Self::sha256_digest(data).into(),
        );

        let Some(ref enc) = info.encryption_material else {
            return Ok((data.to_vec(), attributes));
        };

        let (encrypted, enc_key_b64, iv_b64) =
            Self::encrypt_for_stage(data, &enc.query_stage_master_key)?;
        let key_size_bits = {
            use base64::{engine::general_purpose::STANDARD, Engine};
            STANDARD
                .decode(&enc.query_stage_master_key)
                .map(|b| b.len() * 8)
                .unwrap_or(256)
        };
        let matdesc = serde_json::json!({
            "queryId": enc.query_id,
            "smkId": enc.smk_id.to_string(),
            "keySize": key_size_bits.to_string()
        })
        .to_string();

        match info.location_type.as_str() {
            "S3" => {
                attributes.insert(Self::metadata_attribute("x-amz-key"), enc_key_b64.into());
                attributes.insert(Self::metadata_attribute("x-amz-iv"), iv_b64.into());
                attributes.insert(Self::metadata_attribute("x-amz-matdesc"), matdesc.into());
            }
            "AZURE" | "GCS" => {
                attributes.insert(Self::metadata_attribute("matdesc"), matdesc.into());
                attributes.insert(
                    Self::metadata_attribute("encryptiondata"),
                    Self::build_blob_encryptiondata(&enc_key_b64, &iv_b64).into(),
                );
            }
            other => {
                return Err(Self::boxed_error(format!(
                    "Unsupported stage storage: '{}'.",
                    other
                )));
            }
        }

        Ok((encrypted, attributes))
    }

    async fn put_stage_object(
        store: &dyn ObjectStore,
        object_key: &str,
        payload: Vec<u8>,
        attributes: Attributes,
    ) -> Result<(), BoxError> {
        let path = ObjectPath::from(object_key.to_string());
        store
            .put_opts(&path, payload.into(), PutOptions::from(attributes))
            .await
            .map_err(|e| Self::boxed_error(e.to_string()))?;
        Ok(())
    }

    async fn upload_to_stage_s3(
        &self,
        info: &StageUploadInfo,
        filename: &str,
        data: &[u8],
    ) -> Result<(), BoxError> {
        let (bucket, prefix) = Self::split_stage_location(&info.location)?;
        let object_key = Self::stage_object_key(&prefix, filename);
        let access_key_id = Self::required_stage_cred(info, "AWS_KEY_ID")?;
        let secret_access_key = Self::required_stage_cred(info, "AWS_SECRET_KEY")?;
        let (payload, attributes) = Self::build_stage_upload_payload(info, data)?;

        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_region(info.region.clone())
            .with_access_key_id(access_key_id)
            .with_secret_access_key(secret_access_key);

        if let Some(token) = info.creds.get("AWS_TOKEN") {
            if !token.is_empty() {
                builder = builder.with_token(token.clone());
            }
        }
        if let Some(ref endpoint) = info.end_point {
            builder = builder.with_endpoint(Self::normalize_https_endpoint(endpoint));
        }

        let store = builder
            .build()
            .map_err(|e| Self::boxed_error(format!("Failed to build S3 stage uploader: {}", e)))?;

        Self::put_stage_object(&store, &object_key, payload, attributes).await
    }

    async fn upload_to_stage_azure(
        &self,
        info: &StageUploadInfo,
        filename: &str,
        data: &[u8],
    ) -> Result<(), BoxError> {
        let (container, prefix) = Self::split_stage_location(&info.location)?;
        let object_key = Self::stage_object_key(&prefix, filename);
        let storage_account = info.storage_account.clone().ok_or_else(|| {
            Self::boxed_error("Missing 'storageAccount' in Snowflake Azure stage info.")
        })?;
        let sas_token = Self::required_stage_cred(info, "AZURE_SAS_TOKEN")?;
        let (payload, attributes) = Self::build_stage_upload_payload(info, data)?;
        let sas_query_pairs = Self::parse_sas_query_pairs(&sas_token);

        let mut builder = MicrosoftAzureBuilder::new()
            .with_account(&storage_account)
            .with_container_name(container)
            .with_sas_authorization(sas_query_pairs);

        if let Some(ref endpoint) = info.end_point {
            builder =
                builder.with_endpoint(Self::normalize_azure_endpoint(&storage_account, endpoint));
        }

        let store = builder.build().map_err(|e| {
            Self::boxed_error(format!("Failed to build Azure Blob stage uploader: {}", e))
        })?;

        Self::put_stage_object(&store, &object_key, payload, attributes).await
    }

    async fn upload_to_stage_gcs(
        &self,
        info: &StageUploadInfo,
        filename: &str,
        data: &[u8],
    ) -> Result<(), BoxError> {
        let (bucket, prefix) = Self::split_stage_location(&info.location)?;
        let object_key = Self::stage_object_key(&prefix, filename);
        let access_token = Self::required_stage_cred(info, "GCS_ACCESS_TOKEN")?;
        let (payload, attributes) = Self::build_stage_upload_payload(info, data)?;
        let credentials: GcpCredentialProvider =
            Arc::new(StaticCredentialProvider::new(GcpCredential {
                bearer: access_token,
            }));

        let mut builder = GoogleCloudStorageBuilder::new()
            .with_bucket_name(bucket)
            .with_credentials(credentials);

        if let Some(base_url) = Self::gcs_base_url(info) {
            let service_account_key = serde_json::json!({
                "gcs_base_url": base_url,
                "disable_oauth": true,
                "client_email": "",
                "private_key": ""
            });
            builder = builder.with_service_account_key(service_account_key.to_string());
        }

        let store = builder
            .build()
            .map_err(|e| Self::boxed_error(format!("Failed to build GCS stage uploader: {}", e)))?;

        Self::put_stage_object(&store, &object_key, payload, attributes).await
    }

    /// Upload a file to the stage's backing object storage using the temporary
    /// credentials returned by the PUT initiation. Encrypts if required.
    async fn upload_to_stage(
        &self,
        info: &StageUploadInfo,
        filename: &str,
        data: &[u8],
    ) -> Result<(), BoxError> {
        match info.location_type.as_str() {
            "S3" => self.upload_to_stage_s3(info, filename, data).await,
            "AZURE" => self.upload_to_stage_azure(info, filename, data).await,
            "GCS" => self.upload_to_stage_gcs(info, filename, data).await,
            other => Err(Self::boxed_error(format!(
                "Unsupported stage storage: '{}'.",
                other
            ))),
        }
    }

    // ── Client-side encryption (AES-CBC + AES-ECB key wrapping) ─────────

    #[allow(deprecated)]
    fn encrypt_for_stage(
        data: &[u8],
        query_stage_master_key: &str,
    ) -> Result<(Vec<u8>, String, String), std::io::Error> {
        use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
        use base64::{engine::general_purpose::STANDARD, Engine};

        let master_key = STANDARD
            .decode(query_stage_master_key)
            .map_err(|e| std::io::Error::other(format!("Invalid master key base64: {}", e)))?;

        let key_len = master_key.len();
        let file_key: Vec<u8> = (0..key_len).map(|_| rand::random::<u8>()).collect();
        let iv: [u8; 16] = rand::random();

        // Helper: AES-CBC encrypt `plaintext` (already PKCS7-padded) with given
        // key, returning ciphertext.
        macro_rules! cbc_encrypt {
            ($cipher:expr, $plaintext:expr, $iv:expr) => {{
                let cipher = $cipher;
                let mut prev = $iv;
                let mut out = Vec::with_capacity($plaintext.len());
                for chunk in $plaintext.chunks(16) {
                    let mut block = [0u8; 16];
                    for i in 0..16 {
                        block[i] = chunk[i] ^ prev[i];
                    }
                    let mut ga = GenericArray::from(block);
                    cipher.encrypt_block(&mut ga);
                    prev.copy_from_slice(&ga);
                    out.extend_from_slice(&ga);
                }
                out
            }};
        }

        // Helper: AES-ECB encrypt each block of `plaintext` (already padded).
        macro_rules! ecb_encrypt {
            ($cipher:expr, $plaintext:expr) => {{
                let cipher = $cipher;
                let mut out = Vec::with_capacity($plaintext.len());
                for chunk in $plaintext.chunks(16) {
                    let mut ga = *GenericArray::from_slice(chunk);
                    cipher.encrypt_block(&mut ga);
                    out.extend_from_slice(&ga);
                }
                out
            }};
        }

        fn pkcs7_pad(src: &[u8]) -> Vec<u8> {
            let pad_len = 16 - (src.len() % 16);
            let mut out = src.to_vec();
            out.extend(std::iter::repeat(pad_len as u8).take(pad_len));
            out
        }

        let padded_data = pkcs7_pad(data);
        let padded_key = pkcs7_pad(&file_key);

        let (encrypted_data, encrypted_file_key) = match key_len {
            16 => (
                cbc_encrypt!(
                    aes::Aes128::new(GenericArray::from_slice(&file_key)),
                    padded_data,
                    iv
                ),
                ecb_encrypt!(
                    aes::Aes128::new(GenericArray::from_slice(&master_key)),
                    padded_key
                ),
            ),
            24 => (
                cbc_encrypt!(
                    aes::Aes192::new(GenericArray::from_slice(&file_key)),
                    padded_data,
                    iv
                ),
                ecb_encrypt!(
                    aes::Aes192::new(GenericArray::from_slice(&master_key)),
                    padded_key
                ),
            ),
            32 => (
                cbc_encrypt!(
                    aes::Aes256::new(GenericArray::from_slice(&file_key)),
                    padded_data,
                    iv
                ),
                ecb_encrypt!(
                    aes::Aes256::new(GenericArray::from_slice(&master_key)),
                    padded_key
                ),
            ),
            n => {
                return Err(std::io::Error::other(format!(
                    "Unsupported master key length: {} bytes",
                    n
                )));
            }
        };

        Ok((
            encrypted_data,
            STANDARD.encode(&encrypted_file_key),
            STANDARD.encode(iv),
        ))
    }

    fn sha256_digest(data: &[u8]) -> String {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        STANDARD.encode(hasher.finalize())
    }

    // ── DDL helpers ─────────────────────────────────────────────────────

    fn arrow_type_to_snowflake_ddl(dt: &ArrowDataType) -> String {
        match dt {
            ArrowDataType::Boolean => "BOOLEAN".into(),
            ArrowDataType::Int8
            | ArrowDataType::Int16
            | ArrowDataType::Int32
            | ArrowDataType::Int64
            | ArrowDataType::UInt8
            | ArrowDataType::UInt16
            | ArrowDataType::UInt32
            | ArrowDataType::UInt64 => "NUMBER(38,0)".into(),
            ArrowDataType::Float16 | ArrowDataType::Float32 | ArrowDataType::Float64 => {
                "DOUBLE".into()
            }
            ArrowDataType::Decimal128(precision, scale)
            | ArrowDataType::Decimal256(precision, scale) => {
                format!("NUMBER({},{})", precision, scale)
            }
            ArrowDataType::Date32 | ArrowDataType::Date64 => "DATE".into(),
            ArrowDataType::Timestamp(_, _) => "TIMESTAMP_NTZ".into(),
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => "VARCHAR".into(),
            ArrowDataType::Struct(_)
            | ArrowDataType::List(_)
            | ArrowDataType::LargeList(_)
            | ArrowDataType::Map(_, _) => "VARIANT".into(),
            _ => "VARCHAR".into(),
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
                format!("'{}'", date.format("%Y-%m-%d"))
            }
            ArrowDataType::Date64 => {
                let ms = array
                    .as_any()
                    .downcast_ref::<Date64Array>()
                    .unwrap()
                    .value(row);
                let secs = ms / 1000;
                let dt = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default();
                format!("'{}'", dt.format("%Y-%m-%d"))
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
            ArrowDataType::Struct(fields) => {
                let sa = array.as_any().downcast_ref::<StructArray>().unwrap();
                let args: Vec<String> = fields
                    .iter()
                    .enumerate()
                    .flat_map(|(i, f)| {
                        let val = Self::arrow_value_to_sql(sa.column(i).as_ref(), row);
                        [format!("'{}'", f.name().replace('\'', "''")), val]
                    })
                    .collect();
                format!("OBJECT_CONSTRUCT({})", args.join(", "))
            }
            ArrowDataType::List(_) => {
                let la = array.as_any().downcast_ref::<ListArray>().unwrap();
                let values = la.value(row);
                let elems: Vec<String> = (0..values.len())
                    .map(|i| Self::arrow_value_to_sql(values.as_ref(), i))
                    .collect();
                format!("ARRAY_CONSTRUCT({})", elems.join(", "))
            }
            ArrowDataType::LargeList(_) => {
                let la = array.as_any().downcast_ref::<LargeListArray>().unwrap();
                let values = la.value(row);
                let elems: Vec<String> = (0..values.len())
                    .map(|i| Self::arrow_value_to_sql(values.as_ref(), i))
                    .collect();
                format!("ARRAY_CONSTRUCT({})", elems.join(", "))
            }
            ArrowDataType::Map(_, _) => {
                let ma = array.as_any().downcast_ref::<MapArray>().unwrap();
                let entries = ma.value(row);
                let sa = entries.as_any().downcast_ref::<StructArray>().unwrap();
                let keys = sa.column(0);
                let vals = sa.column(1);
                let args: Vec<String> = (0..entries.len())
                    .flat_map(|i| {
                        let key = if let Some(s) = keys.as_any().downcast_ref::<StringArray>() {
                            format!("'{}'", s.value(i).replace('\'', "''"))
                        } else {
                            format!("'{}'", i)
                        };
                        let val = Self::arrow_value_to_sql(vals.as_ref(), i);
                        [key, val]
                    })
                    .collect();
                format!("OBJECT_CONSTRUCT({})", args.join(", "))
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
        let cell = ENSURED_SCHEMAS
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
            .clone();

        cell.get_or_try_init(|| async {
            let ddl = format!(
                "CREATE SCHEMA IF NOT EXISTS \"{}\".\"{}\"",
                self.config.database, self.config.schema
            );
            info!("Snowflake DDL: {}", ddl);
            self.execute_sql(&ddl).await.map(|_| ()).map_err(|e| {
                error!("Snowflake CREATE SCHEMA failed: {}", e);
                std::io::Error::other(format!("Snowflake CREATE SCHEMA: {}", e))
            })
        })
        .await?;

        Ok(())
    }

    fn col_defs_hash(col_defs: &[(String, String)]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        col_defs.hash(&mut hasher);
        hasher.finish()
    }

    fn col_defs_for_arrow_schema(
        arrow_schema: &datafusion::arrow::datatypes::Schema,
    ) -> Vec<(String, String)> {
        arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_uppercase(),
                    Self::arrow_type_to_snowflake_ddl(f.data_type()),
                )
            })
            .collect()
    }

    async fn col_defs_for_namespace(
        &self,
        namespace: &str,
        arrow_schema: &datafusion::arrow::datatypes::Schema,
    ) -> Vec<(String, String)> {
        let installed = self.schema_state.read().await.get(namespace).cloned();
        if let Some(metadata) = installed {
            let fields: HashMap<String, OutputMetadata> = metadata
                .child_fields()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            match skippr_runtime_sdk::converters::skippr_arrow::convert_skippr_to_arrow(Box::new(
                fields,
            )) {
                Ok(installed_schema) => {
                    return Self::col_defs_for_arrow_schema(&installed_schema);
                }
                Err(err) => {
                    warn!(
                        "Falling back to stream schema for Snowflake DDL: namespace={} err={}",
                        namespace, err
                    );
                }
            }
        }

        Self::col_defs_for_arrow_schema(arrow_schema)
    }

    fn parquet_file_format_clause() -> &'static str {
        "FILE_FORMAT = (TYPE = PARQUET USE_LOGICAL_TYPE = TRUE)"
    }

    fn copy_into_stage_sql(
        fq_table: &str,
        stage: &str,
        table_name: &str,
        parquet_filename: &str,
    ) -> String {
        format!(
            "COPY INTO {} FROM {}/{}/{} {} MATCH_BY_COLUMN_NAME = CASE_INSENSITIVE",
            fq_table,
            stage,
            table_name,
            parquet_filename,
            Self::parquet_file_format_clause(),
        )
    }

    fn copy_into_external_staging_sql(
        fq_table: &str,
        object_uri: &str,
        copy_auth_clause: &str,
    ) -> String {
        format!(
            "COPY INTO {} FROM {} {} {} MATCH_BY_COLUMN_NAME = CASE_INSENSITIVE",
            fq_table,
            Self::sql_string_literal(object_uri),
            copy_auth_clause,
            Self::parquet_file_format_clause(),
        )
    }

    async fn ensure_table(
        &self,
        fq_table: &str,
        col_defs: &[(String, String)],
    ) -> Result<(), std::io::Error> {
        if col_defs.is_empty() {
            return Ok(());
        }

        if let Some(cached) = ENSURED_TABLES.get(fq_table) {
            let cached_set: std::collections::HashSet<(&str, &str)> = cached
                .value()
                .iter()
                .map(|(n, t)| (n.as_str(), t.as_str()))
                .collect();
            if col_defs
                .iter()
                .all(|(n, t)| cached_set.contains(&(n.as_str(), t.as_str())))
            {
                return Ok(());
            }
        }

        let hash = Self::col_defs_hash(col_defs);
        let guard_key = format!("{}:{:x}", fq_table, hash);
        let cell = TABLE_DDL_GUARDS
            .entry(guard_key)
            .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
            .clone();

        cell.get_or_try_init(|| async {
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
            self.execute_sql(&create_ddl)
                .await
                .map(|_| ())
                .map_err(|e| {
                    error!("Snowflake CREATE TABLE failed: {}", e);
                    std::io::Error::other(format!("Snowflake CREATE TABLE: {}", e))
                })?;

            for (col_name, sf_type) in col_defs {
                let alter_ddl = format!(
                    "ALTER TABLE {} ADD COLUMN IF NOT EXISTS \"{}\" {}",
                    fq_table, col_name, sf_type
                );
                if let Err(e) = self.execute_sql(&alter_ddl).await {
                    let msg = e.to_string();
                    if !msg.contains("already exists") {
                        error!("Snowflake ALTER TABLE ADD COLUMN failed: {}", e);
                        return Err(std::io::Error::other(format!(
                            "Snowflake ALTER TABLE: {}",
                            e
                        )));
                    }
                }
            }

            let mut merged: std::collections::HashMap<String, String> = ENSURED_TABLES
                .get(fq_table)
                .map(|e| e.value().iter().cloned().collect())
                .unwrap_or_default();
            for (name, sf_type) in col_defs {
                merged.insert(name.clone(), sf_type.clone());
            }
            ENSURED_TABLES.insert(fq_table.to_string(), merged.into_iter().collect());
            info!("Ensured table {}", fq_table);
            Ok(())
        })
        .await?;

        Ok(())
    }

    // ── Data sync paths ─────────────────────────────────────────────────

    async fn inner_sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        // Prefer explicit external object storage staging when configured.
        if self.config.staging_uri.is_some() {
            return self.inner_sync_external_staging(stream, filename).await;
        }
        // Default: PUT to Snowflake stage → COPY INTO
        self.inner_sync_stage(stream, filename).await
    }

    /// Primary data path: serialize to Parquet, PUT to Snowflake stage,
    /// COPY INTO target table, then REMOVE the staged file.
    async fn inner_sync_stage(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use super::parquet_util::serialize_to_parquet;
        use skippr_runtime_sdk::metrics::counters;

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();
        let col_defs = self.col_defs_for_namespace(&namespace, &arrow_schema).await;
        let fq_table = format!(
            "\"{}\".\"{}\".\"{}\"",
            self.config.database,
            self.config.schema,
            table_name.to_uppercase()
        );

        self.ensure_schema().await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        self.ensure_table(&fq_table, &col_defs).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

        let parquet = match serialize_to_parquet(stream).await {
            Ok(p) => p,
            Err(e) => {
                counters::dec_uploads_in_flight();
                return Err(e);
            }
        };
        let row_count = parquet.num_rows;
        let byte_count = parquet.size_bytes;

        let stage = self.config.stage.as_deref().unwrap_or("@~");
        let file_id = uuid::Uuid::new_v4();
        let parquet_filename = format!("{}.parquet", file_id);
        let stage_path = format!("{}/{}/", stage, table_name);

        let session_token = self.get_session_token().await.map_err(|e| {
            counters::dec_uploads_in_flight();
            std::io::Error::other(format!("Session token: {}", e))
        })?;

        let stage_info = self
            .initiate_put(&session_token, &stage_path, &parquet_filename)
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("PUT initiation: {}", e))
            })?;

        info!(
            "Uploading Parquet to stage {}{} ({} rows, {})",
            stage_path,
            parquet_filename,
            row_count,
            crate::helpers::Helpers::human_readable_size(byte_count)
        );

        self.upload_to_stage(&stage_info, &parquet_filename, &parquet.bytes)
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("Stage upload: {}", e))
            })?;

        let copy_sql = Self::copy_into_stage_sql(&fq_table, stage, &table_name, &parquet_filename);

        info!("Snowflake COPY INTO {} ({} rows)", fq_table, row_count);

        let copy_result = self.execute_sql(&copy_sql).await;

        // Best-effort cleanup of the staged file
        let remove_sql = format!("REMOVE {}/{}/{}", stage, table_name, parquet_filename);
        let _ = self.execute_sql(&remove_sql).await;

        match copy_result {
            Ok(_) => {
                counters::add_parquet_rows(row_count as u64);
                counters::add_parquet_bytes(byte_count);
                counters::add_upload(1);
                counters::dec_uploads_in_flight();
                info!(
                    "Snowflake COPY INTO complete: {} rows into {}",
                    row_count, fq_table
                );
                Ok(())
            }
            Err(e) => {
                counters::dec_uploads_in_flight();
                error!("Snowflake COPY INTO failed for {}: {}", fq_table, e);
                Err(std::io::Error::other(format!("Snowflake COPY INTO: {}", e)))
            }
        }
    }

    /// Optional override: upload Parquet to a user-supplied external object
    /// store location and COPY INTO from that URI.
    async fn inner_sync_external_staging(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use super::parquet_util::serialize_to_parquet;
        use skippr_runtime_sdk::metrics::counters;

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();
        let col_defs = self.col_defs_for_namespace(&namespace, &arrow_schema).await;
        let fq_table = format!(
            "\"{}\".\"{}\".\"{}\"",
            self.config.database,
            self.config.schema,
            table_name.to_uppercase()
        );

        self.ensure_schema().await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        self.ensure_table(&fq_table, &col_defs).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

        let parquet = match serialize_to_parquet(stream).await {
            Ok(p) => p,
            Err(e) => {
                counters::dec_uploads_in_flight();
                return Err(e);
            }
        };
        let row_count = parquet.num_rows;
        let byte_count = parquet.size_bytes;
        let staging_uri = self.config.staging_uri.as_deref().unwrap();
        let location = Self::parse_external_stage_uri(staging_uri)
            .map_err(|e| std::io::Error::other(format!("External staging URI: {}", e)))?;
        let file_id = uuid::Uuid::new_v4();
        let object_key = Self::external_stage_object_key(&location, &table_name, &file_id);
        let object_uri = Self::external_stage_object_uri(&location, &object_key);
        let parquet_bytes = parquet.bytes;

        let aws_cfg = if location.provider == ExternalStageProvider::S3 {
            Some(
                aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .load()
                    .await,
            )
        } else {
            None
        };
        let copy_auth_clause = self
            .external_stage_copy_auth_clause(&location, aws_cfg.as_ref())
            .await?;

        let mut uploaded_store: Option<Box<dyn ObjectStore>> = None;
        match location.provider {
            ExternalStageProvider::S3 => {
                let aws_cfg = aws_cfg.as_ref().ok_or_else(|| {
                    std::io::Error::other("Missing AWS configuration for Snowflake S3 staging")
                })?;
                let s3_client = S3Client::new(aws_cfg);
                s3_client
                    .put_object()
                    .bucket(&location.root)
                    .key(&object_key)
                    .body(ByteStream::from(parquet_bytes))
                    .send()
                    .await
                    .map_err(|e| {
                        counters::dec_uploads_in_flight();
                        std::io::Error::other(format!("S3 staging upload failed: {}", e))
                    })?;
            }
            ExternalStageProvider::Azure => {
                let store = self.build_external_azure_store(&location).map_err(|e| {
                    counters::dec_uploads_in_flight();
                    std::io::Error::other(format!("Azure staging store: {}", e))
                })?;
                Self::put_external_object(store.as_ref(), &object_key, parquet_bytes)
                    .await
                    .map_err(|e| {
                        counters::dec_uploads_in_flight();
                        std::io::Error::other(format!("Azure staging upload failed: {}", e))
                    })?;
                uploaded_store = Some(store);
            }
            ExternalStageProvider::Gcs => {
                let store = self.build_external_gcs_store(&location).map_err(|e| {
                    counters::dec_uploads_in_flight();
                    std::io::Error::other(format!("GCS staging store: {}", e))
                })?;
                Self::put_external_object(store.as_ref(), &object_key, parquet_bytes)
                    .await
                    .map_err(|e| {
                        counters::dec_uploads_in_flight();
                        std::io::Error::other(format!("GCS staging upload failed: {}", e))
                    })?;
                uploaded_store = Some(store);
            }
        }

        info!(
            "Uploaded staging Parquet {} ({} rows, {})",
            object_uri,
            row_count,
            crate::helpers::Helpers::human_readable_size(byte_count)
        );

        let copy_sql =
            Self::copy_into_external_staging_sql(&fq_table, &object_uri, &copy_auth_clause);

        info!(
            "Snowflake COPY INTO {} from {} staging ({} rows)",
            fq_table,
            Self::external_stage_provider_name(location.provider),
            row_count
        );

        let copy_result = self.execute_sql(&copy_sql).await;

        match location.provider {
            ExternalStageProvider::S3 => {
                if let Some(ref aws_cfg) = aws_cfg {
                    let s3_client = S3Client::new(aws_cfg);
                    let _ = s3_client
                        .delete_object()
                        .bucket(&location.root)
                        .key(&object_key)
                        .send()
                        .await;
                }
            }
            ExternalStageProvider::Azure | ExternalStageProvider::Gcs => {
                if let Some(ref store) = uploaded_store {
                    Self::delete_external_object(store.as_ref(), &object_key).await;
                }
            }
        }

        match copy_result {
            Ok(_) => {
                counters::add_parquet_rows(row_count as u64);
                counters::add_parquet_bytes(byte_count);
                counters::add_upload(1);
                counters::dec_uploads_in_flight();
                info!(
                    "Snowflake COPY INTO complete: {} rows into {}",
                    row_count, fq_table
                );
                Ok(())
            }
            Err(e) => {
                counters::dec_uploads_in_flight();
                error!("Snowflake COPY INTO failed for {}: {}", fq_table, e);
                Err(std::io::Error::other(format!("Snowflake COPY INTO: {}", e)))
            }
        }
    }

    /// Legacy INSERT path kept as last-resort fallback.
    #[allow(dead_code)]
    async fn inner_sync_insert(
        &self,
        mut stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::metrics::counters;
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();

        let col_defs = self.col_defs_for_namespace(&namespace, &arrow_schema).await;

        let fq_table = format!(
            "\"{}\".\"{}\".\"{}\"",
            self.config.database,
            self.config.schema,
            table_name.to_uppercase()
        );

        self.ensure_schema().await?;
        self.ensure_table(&fq_table, &col_defs).await?;

        let table_col_types: std::collections::HashMap<String, String> = ENSURED_TABLES
            .get(&fq_table)
            .map(|entry| entry.value().iter().cloned().collect())
            .unwrap_or_default();

        let col_list: String = col_defs
            .iter()
            .map(|(name, _)| format!("\"{}\"", name))
            .collect::<Vec<_>>()
            .join(", ");

        let has_structured_cols = col_defs.iter().any(|(_, t)| {
            t.starts_with("OBJECT(") || t.starts_with("ARRAY(") || t.starts_with("MAP(")
        });

        let _chunk_size = if has_structured_cols {
            INSERT_CHUNK_STRUCTURED
        } else {
            INSERT_CHUNK_FLAT
        };

        let mut total_rows = 0usize;
        while let Some(batch_result) = stream.next().await {
            let batch =
                batch_result.map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }

            let insert_sql = if has_structured_cols {
                let select_rows: Vec<String> = (0..num_rows)
                    .map(|row| {
                        let vals: Vec<String> = (0..batch.num_columns())
                            .map(|col_idx| {
                                let raw =
                                    Self::arrow_value_to_sql(batch.column(col_idx).as_ref(), row);
                                let col_name = &col_defs[col_idx].0;
                                let cast_type = table_col_types
                                    .get(col_name)
                                    .map(String::as_str)
                                    .unwrap_or(col_defs[col_idx].1.as_str());
                                if raw != "NULL"
                                    && (cast_type.starts_with("OBJECT(")
                                        || cast_type.starts_with("ARRAY(")
                                        || cast_type.starts_with("MAP("))
                                {
                                    format!("{}::{}", raw, cast_type)
                                } else {
                                    raw
                                }
                            })
                            .collect();
                        format!("SELECT {}", vals.join(", "))
                    })
                    .collect();
                format!(
                    "INSERT INTO {} ({}) {}",
                    fq_table,
                    col_list,
                    select_rows.join(" UNION ALL ")
                )
            } else {
                let value_rows: Vec<String> = (0..num_rows)
                    .map(|row| {
                        let vals: Vec<String> = (0..batch.num_columns())
                            .map(|col| Self::arrow_value_to_sql(batch.column(col).as_ref(), row))
                            .collect();
                        format!("({})", vals.join(", "))
                    })
                    .collect();
                format!(
                    "INSERT INTO {} ({}) VALUES {}",
                    fq_table,
                    col_list,
                    value_rows.join(", ")
                )
            };

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
                    error!("Snowflake INSERT failed: {}", e);
                    counters::dec_uploads_in_flight();
                    return Err(std::io::Error::other(format!("Snowflake INSERT: {}", e)));
                }
            }
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Snowflake sync complete: {} total rows into {}",
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
            append_record_batch_to_cdc_apply, ddl_add_order_token_column,
            ddl_create_tombstone_table, delete_if_newer_sql, guarded_warehouse_cdc_row_sql,
            tombstone_table_name, upsert_if_newer_sql, warehouse_bulk_cdc_sql, CdcApplyBatch,
            CdcApplyColumn, CdcWarehouseDialect,
        };
        use skippr_runtime_sdk::metrics::counters;
        use skippr_runtime_sdk::plugins::cdc::MutationKind;

        tracing::debug!(
            target: "snowflake",
            blocker = SNOWFLAKE_CDC_FILE_STAGE_BLOCKER,
            "using bounded SQL staging with guarded overflow"
        );
        let contract = match ctx.contract.as_ref() {
            Some(c) if !c.business_key_columns.is_empty() => c,
            _ => {
                info!(target: "snowflake", "CDC context without contract or business keys; falling back to append");
                return self.inner_sync(stream, filename).await;
            }
        };

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();

        let col_defs = self.col_defs_for_namespace(&namespace, &arrow_schema).await;
        let business_key_columns = contract
            .business_key_columns
            .iter()
            .map(|business_key| {
                col_defs
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(business_key))
                    .map(|(name, _)| name.clone())
                    .unwrap_or_else(|| business_key.to_uppercase())
            })
            .collect::<Vec<_>>();

        let fq_table = format!(
            "\"{}\".\"{}\".\"{}\"",
            self.config.database,
            self.config.schema,
            table_name.to_uppercase()
        );

        self.ensure_schema().await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;
        self.ensure_table(&fq_table, &col_defs).await.map_err(|e| {
            counters::dec_uploads_in_flight();
            e
        })?;

        if CDC_DDL_ENSURED.insert(fq_table.clone()) {
            let order_col_ddl = ddl_add_order_token_column::<SnowflakeCdcBackend>(&fq_table);
            if let Err(e) = self.execute_sql(&order_col_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(std::io::Error::other(format!("Snowflake CDC DDL: {}", e)));
            }

            let tombstone_tbl = tombstone_table_name(&fq_table);
            let bk_type_pairs: Vec<(String, String)> = business_key_columns
                .iter()
                .map(|bk| {
                    let sf_type = col_defs
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(bk))
                        .map(|(_, t)| t.clone())
                        .unwrap_or_else(|| "VARCHAR".to_string());
                    (bk.clone(), sf_type)
                })
                .collect();
            let tombstone_ddl =
                ddl_create_tombstone_table::<SnowflakeCdcBackend>(&tombstone_tbl, &bk_type_pairs);
            if let Err(e) = self.execute_sql(&tombstone_ddl).await {
                CDC_DDL_ENSURED.remove(&fq_table);
                counters::dec_uploads_in_flight();
                return Err(std::io::Error::other(format!("Snowflake CDC DDL: {}", e)));
            }

            info!(target: "snowflake", "CDC DDL applied for {}", fq_table);
        }

        let tombstone_table = tombstone_table_name(&fq_table);

        let bk_names_quoted: Vec<String> = contract
            .business_key_columns
            .iter()
            .zip(business_key_columns.iter())
            .map(|(_, resolved)| format!("\"{}\"", resolved))
            .collect();

        let bk_types: Vec<String> = business_key_columns
            .iter()
            .map(|bk| {
                col_defs
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(bk))
                    .map(|(_, t)| t.clone())
                    .unwrap_or_else(|| "VARCHAR".to_string())
            })
            .collect();

        let col_names_quoted: Vec<String> = arrow_schema
            .fields()
            .iter()
            .map(|f| format!("\"{}\"", f.name().to_uppercase()))
            .collect();

        let mut row_offset = 0usize;
        let mut total_rows = 0usize;

        let bulk_scalar_batch = !col_defs
            .iter()
            .any(|(_, target_type)| target_type.eq_ignore_ascii_case("VARIANT"));
        if bulk_scalar_batch {
            let mut apply_batch = CdcApplyBatch {
                columns: arrow_schema
                    .fields()
                    .iter()
                    .map(|field| {
                        let name = field.name().to_uppercase();
                        let target_type = col_defs
                            .iter()
                            .find(|(column, _)| column.eq_ignore_ascii_case(&name))
                            .map(|(_, target_type)| target_type.clone())
                            .unwrap_or_else(|| {
                                Self::arrow_type_to_snowflake_ddl(field.data_type())
                            });
                        CdcApplyColumn { name, target_type }
                    })
                    .collect(),
                business_key_columns: business_key_columns.clone(),
                rows: Vec::new(),
            };

            while let Some(batch_result) = stream.next().await {
                let batch = batch_result
                    .map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
                let num_rows = batch.num_rows();
                if num_rows == 0 {
                    continue;
                }
                append_record_batch_to_cdc_apply(
                    &mut apply_batch,
                    &batch,
                    &ctx.part_meta.rows,
                    row_offset,
                )
                .map_err(|error| std::io::Error::other(error.to_string()))?;
                row_offset += num_rows;
            }

            total_rows = apply_batch.rows.len();
            if total_rows > 0 {
                match warehouse_bulk_cdc_sql(
                    CdcWarehouseDialect::Snowflake,
                    &fq_table,
                    &tombstone_table,
                    &apply_batch,
                ) {
                    Ok(sql) => {
                        self.execute_sql_script(
                            &sql.transactional_script(CdcWarehouseDialect::Snowflake),
                        )
                        .await
                        .map_err(|error| {
                            counters::dec_uploads_in_flight();
                            std::io::Error::other(format!("Snowflake bulk CDC apply: {error}"))
                        })?;
                    }
                    Err(error) if error.is_warehouse_stage_limit() => {
                        warn!(
                            target: "snowflake",
                            "CDC chunk exceeds the retryable 1 MiB SQL envelope; using guarded row apply: {}",
                            error
                        );
                        for row in &apply_batch.rows {
                            let statement = guarded_warehouse_cdc_row_sql::<SnowflakeCdcBackend>(
                                CdcWarehouseDialect::Snowflake,
                                &fq_table,
                                &tombstone_table,
                                &apply_batch,
                                row,
                            )
                            .map_err(|error| std::io::Error::other(error.to_string()))?;
                            self.execute_sql_script(&statement).await.map_err(|error| {
                                counters::dec_uploads_in_flight();
                                std::io::Error::other(format!(
                                    "Snowflake guarded CDC apply: {error}"
                                ))
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
                    target: "snowflake",
                    "CDC safely applied {} rows to {}",
                    total_rows,
                    table_name
                );
            }
        } else {
            while let Some(batch_result) = stream.next().await {
                let batch = batch_result
                    .map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
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
                                .map(|col| {
                                    Self::arrow_value_to_sql(batch.column(col).as_ref(), row)
                                })
                                .collect();
                            all_values.push(format!("HEX_DECODE_BINARY('{}')", order_token_hex));

                            let sql = upsert_if_newer_sql::<SnowflakeCdcBackend>(
                                &fq_table,
                                &tombstone_table,
                                &all_names,
                                &all_values,
                                &bk_names_quoted,
                                &order_token_hex,
                            );

                            if let Err(e) = self.execute_sql_script(&sql).await {
                                error!("CDC upsert failed for {}: {}", fq_table, e);
                                counters::dec_uploads_in_flight();
                                return Err(std::io::Error::other(format!(
                                    "Snowflake CDC upsert: {}",
                                    e
                                )));
                            }
                        }
                        MutationKind::Delete => {
                            let bk_values: Vec<String> = business_key_columns
                                .iter()
                                .map(|bk| {
                                    let col_idx = arrow_schema
                                        .fields()
                                        .iter()
                                        .position(|f| f.name().eq_ignore_ascii_case(bk))
                                        .unwrap_or(0);
                                    Self::arrow_value_to_sql(batch.column(col_idx).as_ref(), row)
                                })
                                .collect();

                            let sql = delete_if_newer_sql::<SnowflakeCdcBackend>(
                                &fq_table,
                                &tombstone_table,
                                &bk_names_quoted,
                                &bk_values,
                                &bk_types,
                                &order_token_hex,
                            );

                            if let Err(e) = self.execute_sql_script(&sql).await {
                                error!("CDC delete failed for {}: {}", fq_table, e);
                                counters::dec_uploads_in_flight();
                                return Err(std::io::Error::other(format!(
                                    "Snowflake CDC delete: {}",
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
        }

        counters::add_upload(1);
        counters::dec_uploads_in_flight();
        info!(
            "Snowflake CDC sync complete: {} total rows into {}",
            total_rows, table_name
        );
        Ok(())
    }
}

#[async_trait]
impl DataSink for DataSinkSnowflakePlugin {
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
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::SNOWFLAKE
    }

    async fn install_schema_state(
        &self,
        _schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), std::io::Error> {
        *self.schema_state.write().await = namespaces.clone();
        *self.schema_versions.write().await = namespaces
            .keys()
            .map(|namespace| (namespace.clone(), _schema_version))
            .collect();
        Ok(())
    }

    async fn install_schema_snapshot(
        &self,
        schema_state: &RuntimeSchemaState,
    ) -> Result<(), std::io::Error> {
        *self.schema_state.write().await = schema_state.namespaces.clone();
        *self.schema_versions.write().await = schema_state.namespace_versions.clone();
        Ok(())
    }

    async fn install_schema_delta(&self, delta: &SchemaDelta) -> Result<(), std::io::Error> {
        let mut versions = self.schema_versions.write().await;
        let mut schemas = self.schema_state.write().await;
        for (namespace, entry) in &delta.namespaces {
            if versions
                .get(namespace)
                .is_some_and(|installed| *installed >= entry.version)
            {
                continue;
            }
            schemas.insert(namespace.clone(), entry.metadata.clone());
            versions.insert(namespace.clone(), entry.version);
        }
        Ok(())
    }
}

#[async_trait]
impl SchemaSink for DataSinkSnowflakePlugin {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &skippr_runtime_sdk::discover::OutputMetadata,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::converters::skippr_arrow::convert_skippr_to_arrow;

        self.ensure_schema().await?;

        let fields: std::collections::HashMap<
            String,
            skippr_runtime_sdk::discover::OutputMetadata,
        > = metadata
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
        let col_defs = Self::col_defs_for_arrow_schema(&arrow_schema);

        let fq_table = format!(
            "\"{}\".\"{}\".\"{}\"",
            self.config.database,
            self.config.schema,
            table_name.to_uppercase()
        );

        self.ensure_table(&fq_table, &col_defs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    fn sample_master_key() -> String {
        STANDARD.encode([7u8; 32])
    }

    fn sample_encryption_material() -> EncryptionMaterial {
        EncryptionMaterial {
            query_stage_master_key: sample_master_key(),
            query_id: "query-123".to_string(),
            smk_id: 17,
        }
    }

    fn sample_stage_info(location_type: &str) -> StageUploadInfo {
        StageUploadInfo {
            location_type: location_type.to_string(),
            location: "bucket-or-container/prefix/".to_string(),
            region: "us-east-1".to_string(),
            creds: HashMap::new(),
            encryption_material: Some(sample_encryption_material()),
            end_point: None,
            storage_account: None,
            use_virtual_url: false,
            use_regional_url: false,
        }
    }

    fn sample_plugin_config() -> DataSinkSnowflakePluginConfig {
        DataSinkSnowflakePluginConfig {
            account: "acct".to_string(),
            user: "user".to_string(),
            password: None,
            warehouse: "wh".to_string(),
            database: "db".to_string(),
            schema: "public".to_string(),
            role: None,
            stage: Some("@~".to_string()),
            format: None,
            private_key_path: None,
            staging_uri: None,
            staging_storage_integration: None,
            staging_azure_sas_token: None,
            staging_azure_account_key: None,
            staging_gcs_service_account_key_path: None,
        }
    }

    fn metadata_value(attrs: &Attributes, key: &str) -> Option<String> {
        attrs
            .get(&DataSinkSnowflakePlugin::metadata_attribute(key))
            .map(|value| value.as_ref().to_string())
    }

    #[test]
    fn snowflake_http_retry_classifier_matches_transient_transport_messages() {
        assert!(DataSinkSnowflakePlugin::is_transient_http_message(
            "io error: unexpected end of file"
        ));
        assert!(DataSinkSnowflakePlugin::is_transient_http_message(
            "connection reset by peer"
        ));
        assert!(DataSinkSnowflakePlugin::is_transient_http_message(
            "operation timed out"
        ));
    }

    #[test]
    fn snowflake_http_retry_classifier_ignores_api_errors() {
        assert!(!DataSinkSnowflakePlugin::is_transient_http_message(
            "Snowflake SQL error (002003): SQL compilation error"
        ));
        assert!(!DataSinkSnowflakePlugin::is_transient_http_message(
            "Incorrect username or password"
        ));
    }

    #[test]
    fn maps_arrow_decimal_types_to_snowflake_number() {
        assert_eq!(
            DataSinkSnowflakePlugin::arrow_type_to_snowflake_ddl(&ArrowDataType::Decimal128(10, 2)),
            "NUMBER(10,2)"
        );
        assert_eq!(
            DataSinkSnowflakePlugin::arrow_type_to_snowflake_ddl(&ArrowDataType::Decimal128(38, 9)),
            "NUMBER(38,9)"
        );
        assert_eq!(
            DataSinkSnowflakePlugin::arrow_type_to_snowflake_ddl(&ArrowDataType::Decimal256(38, 9)),
            "NUMBER(38,9)"
        );
    }

    #[test]
    fn copy_into_sql_enables_parquet_logical_types() {
        let stage_sql = DataSinkSnowflakePlugin::copy_into_stage_sql(
            "\"DB\".\"PUBLIC\".\"ORDERS\"",
            "@~",
            "orders",
            "batch.parquet",
        );
        assert!(stage_sql.contains("FILE_FORMAT = (TYPE = PARQUET USE_LOGICAL_TYPE = TRUE)"));
        assert!(stage_sql.contains("MATCH_BY_COLUMN_NAME = CASE_INSENSITIVE"));

        let external_sql = DataSinkSnowflakePlugin::copy_into_external_staging_sql(
            "\"DB\".\"PUBLIC\".\"ORDERS\"",
            "s3://bucket/orders/batch.parquet",
            "CREDENTIALS = (AWS_KEY_ID = 'key' AWS_SECRET_KEY = 'secret')",
        );
        assert!(external_sql.contains("FILE_FORMAT = (TYPE = PARQUET USE_LOGICAL_TYPE = TRUE)"));
        assert!(external_sql.contains("'s3://bucket/orders/batch.parquet'"));
    }

    #[test]
    fn parses_azure_stage_upload_info() {
        let data = serde_json::json!({
            "stageInfo": {
                "locationType": "AZURE",
                "location": "container/prefix/",
                "region": "westus2",
                "endPoint": "blob.core.windows.net",
                "storageAccount": "skipprstage",
                "useVirtualUrl": true,
                "useRegionalUrl": false,
                "creds": {
                    "AZURE_SAS_TOKEN": "sv=1&sig=abc"
                }
            },
            "encryptionMaterial": {
                "queryStageMasterKey": sample_master_key(),
                "queryId": "query-123",
                "smkId": 17
            }
        });

        let info = DataSinkSnowflakePlugin::parse_stage_upload_info(&data).unwrap();

        assert_eq!(info.location_type, "AZURE");
        assert_eq!(info.location, "container/prefix/");
        assert_eq!(info.region, "westus2");
        assert_eq!(info.end_point.as_deref(), Some("blob.core.windows.net"));
        assert_eq!(info.storage_account.as_deref(), Some("skipprstage"));
        assert!(info.use_virtual_url);
        assert_eq!(
            info.creds.get("AZURE_SAS_TOKEN").map(String::as_str),
            Some("sv=1&sig=abc")
        );
        assert!(info.encryption_material.is_some());
    }

    #[test]
    fn parses_azure_sas_query_pairs_without_double_encoding() {
        let pairs = DataSinkSnowflakePlugin::parse_sas_query_pairs(
            "?sv=2023-11-03&se=2026-05-06T12%3A00%3A00Z&skt=2026-05-06T11%3A00%3A00Z&sig=abc%2Bdef%2Fghi%3D&raw_plus=a+b&bad=keep%2Zliteral",
        );

        assert_eq!(
            pairs,
            vec![
                ("sv".to_string(), "2023-11-03".to_string()),
                ("se".to_string(), "2026-05-06T12:00:00Z".to_string()),
                ("skt".to_string(), "2026-05-06T11:00:00Z".to_string()),
                ("sig".to_string(), "abc+def/ghi=".to_string()),
                ("raw_plus".to_string(), "a+b".to_string()),
                ("bad".to_string(), "keep%2Zliteral".to_string()),
            ]
        );
    }

    #[test]
    fn builds_s3_stage_payload_metadata() {
        let info = sample_stage_info("S3");
        let (_payload, attrs) =
            DataSinkSnowflakePlugin::build_stage_upload_payload(&info, b"hello").unwrap();

        assert!(metadata_value(&attrs, "sfc-digest").is_some());
        assert!(metadata_value(&attrs, "x-amz-key").is_some());
        assert!(metadata_value(&attrs, "x-amz-iv").is_some());
        assert!(metadata_value(&attrs, "x-amz-matdesc").is_some());
        assert_eq!(
            attrs.get(&Attribute::ContentType).map(|v| v.as_ref()),
            Some("application/octet-stream")
        );
    }

    #[test]
    fn builds_azure_stage_payload_metadata() {
        let info = sample_stage_info("AZURE");
        let (_payload, attrs) =
            DataSinkSnowflakePlugin::build_stage_upload_payload(&info, b"hello").unwrap();

        assert!(metadata_value(&attrs, "sfcdigest").is_some());
        assert!(metadata_value(&attrs, "matdesc").is_some());
        assert!(metadata_value(&attrs, "encryptiondata").is_some());
        assert!(metadata_value(&attrs, "x-amz-key").is_none());
    }

    #[test]
    fn builds_gcs_stage_payload_metadata() {
        let info = sample_stage_info("GCS");
        let (_payload, attrs) =
            DataSinkSnowflakePlugin::build_stage_upload_payload(&info, b"hello").unwrap();

        assert!(metadata_value(&attrs, "sfc-digest").is_some());
        assert!(metadata_value(&attrs, "matdesc").is_some());
        assert!(metadata_value(&attrs, "encryptiondata").is_some());
        assert!(metadata_value(&attrs, "x-amz-key").is_none());
    }

    #[test]
    fn computes_regional_gcs_base_url() {
        let mut info = sample_stage_info("GCS");
        info.region = "us-central1".to_string();
        info.use_regional_url = true;

        assert_eq!(
            DataSinkSnowflakePlugin::gcs_base_url(&info).as_deref(),
            Some("https://storage.us-central1.rep.googleapis.com")
        );
    }

    #[test]
    fn parses_cross_cloud_external_staging_uris() {
        let s3 = DataSinkSnowflakePlugin::parse_external_stage_uri("s3://stage-bucket/prefix/path")
            .unwrap();
        assert_eq!(s3.provider, ExternalStageProvider::S3);
        assert_eq!(s3.root, "stage-bucket");
        assert_eq!(s3.prefix, "prefix/path");

        let azure = DataSinkSnowflakePlugin::parse_external_stage_uri(
            "azure://acct.blob.core.windows.net/container/prefix",
        )
        .unwrap();
        assert_eq!(azure.provider, ExternalStageProvider::Azure);
        assert_eq!(azure.root, "container");
        assert_eq!(azure.prefix, "prefix");
        assert_eq!(azure.azure_account.as_deref(), Some("acct"));
        assert_eq!(
            azure.azure_host.as_deref(),
            Some("acct.blob.core.windows.net")
        );

        let gcs =
            DataSinkSnowflakePlugin::parse_external_stage_uri("gs://stage-bucket/prefix").unwrap();
        assert_eq!(gcs.provider, ExternalStageProvider::Gcs);
        assert_eq!(gcs.root, "stage-bucket");
        assert_eq!(gcs.prefix, "prefix");
    }

    #[tokio::test]
    async fn gcs_external_staging_requires_storage_integration() {
        let plugin =
            DataSinkSnowflakePlugin::new_with_config("buffer".to_string(), sample_plugin_config())
                .await;
        let location =
            DataSinkSnowflakePlugin::parse_external_stage_uri("gcs://stage-bucket/prefix").unwrap();

        let err = plugin
            .external_stage_copy_auth_clause(&location, None)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("staging_storage_integration"));
    }

    #[tokio::test]
    async fn storage_integration_clause_overrides_provider_specific_copy_creds() {
        let mut config = sample_plugin_config();
        config.staging_storage_integration = Some("my_int".to_string());
        let plugin = DataSinkSnowflakePlugin::new_with_config("buffer".to_string(), config).await;
        let location =
            DataSinkSnowflakePlugin::parse_external_stage_uri("azure://acct/container/prefix")
                .unwrap();

        let clause = plugin
            .external_stage_copy_auth_clause(&location, None)
            .await
            .unwrap();

        assert_eq!(clause, "STORAGE_INTEGRATION = my_int");
    }

    #[test]
    fn snowflake_bulk_cdc_sql_uses_temp_stage_and_transactional_merges() {
        let batch = crate::cdc_apply::warehouse_sql_test_batch();
        let sql = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Snowflake,
            "\"DB\".\"PUBLIC\".\"USERS\"",
            "\"DB\".\"PUBLIC\".\"_skippr_tombstones_USERS\"",
            &batch,
        )
        .unwrap();
        let script = sql.transactional_script(crate::cdc_apply::CdcWarehouseDialect::Snowflake);

        assert!(script.contains("CREATE TEMPORARY TABLE"));
        assert!(script.contains("HEX_DECODE_BINARY"));
        assert!(script.contains("BEGIN TRANSACTION"));
        assert_eq!(script.matches("MERGE INTO").count(), 4);
        assert!(script.contains("DROP TABLE IF EXISTS"));
        assert!(script.contains("'O''Brien'"));
    }

    #[test]
    fn snowflake_bounds_retryable_sql_and_preserves_guarded_overflow() {
        assert!(crate::cdc_apply::CdcWarehouseDialect::Snowflake
            .validate_stage_row_count(10_000)
            .is_ok());
        assert!(crate::cdc_apply::CdcWarehouseDialect::Snowflake
            .validate_stage_row_count(10_001)
            .unwrap_err()
            .is_warehouse_stage_limit());
        assert!(crate::cdc_apply::CdcWarehouseDialect::Snowflake
            .validate_stage_row_count(100_000)
            .unwrap_err()
            .is_warehouse_stage_limit());
        let split_batch = crate::cdc_apply::warehouse_sql_test_batch_with_rows(501, 8);
        let split = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Snowflake,
            "\"DB\".\"PUBLIC\".\"USERS\"",
            "\"DB\".\"PUBLIC\".\"_skippr_tombstones_USERS\"",
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
                .transactional_script(crate::cdc_apply::CdcWarehouseDialect::Snowflake)
                .len()
                <= 900 * 1024
        );

        let oversized = crate::cdc_apply::warehouse_sql_test_batch_with_rows(2, 200 * 1024);
        let error = crate::cdc_apply::warehouse_bulk_cdc_sql(
            crate::cdc_apply::CdcWarehouseDialect::Snowflake,
            "\"DB\".\"PUBLIC\".\"USERS\"",
            "\"DB\".\"PUBLIC\".\"_skippr_tombstones_USERS\"",
            &oversized,
        )
        .unwrap_err();
        assert!(error.is_warehouse_stage_limit());
        let guarded = crate::cdc_apply::guarded_warehouse_cdc_sql::<super::SnowflakeCdcBackend>(
            crate::cdc_apply::CdcWarehouseDialect::Snowflake,
            "\"DB\".\"PUBLIC\".\"USERS\"",
            "\"DB\".\"PUBLIC\".\"_skippr_tombstones_USERS\"",
            &oversized,
        )
        .unwrap();
        assert_eq!(guarded.len(), 2);
        assert!(super::SNOWFLAKE_CDC_FILE_STAGE_BLOCKER.contains("target-only"));
    }
}
