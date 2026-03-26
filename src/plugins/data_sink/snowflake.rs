use async_trait::async_trait;
use aws_credential_types::provider::ProvideCredentials;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use dashmap::DashMap;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::buffer::BufferChunker;
use crate::discover::SkipprDataType;
use crate::helpers::configuration::{Config, DataOutputSnowflakePluginConfig, OutputPluginConfig};
use crate::plugins::DataSink;

static ENSURED_SCHEMAS: Lazy<DashMap<String, Arc<tokio::sync::OnceCell<()>>>> =
    Lazy::new(DashMap::new);
static TABLE_DDL_GUARDS: Lazy<DashMap<String, Arc<tokio::sync::OnceCell<()>>>> =
    Lazy::new(DashMap::new);
static ENSURED_TABLES: Lazy<DashMap<String, Vec<(String, String)>>> = Lazy::new(DashMap::new);

const ASYNC_POLL_MAX: u32 = 600;
const ASYNC_POLL_INTERVAL_MS: u64 = 500;

const INSERT_CHUNK_STRUCTURED: usize = 100;
const INSERT_CHUNK_FLAT: usize = 1000;

struct StageUploadInfo {
    location_type: String,
    /// S3 location in format "bucket/prefix/"
    location: String,
    region: String,
    aws_key_id: String,
    aws_secret_key: String,
    aws_token: String,
    encryption_material: Option<EncryptionMaterial>,
    #[allow(dead_code)]
    end_point: Option<String>,
}

struct EncryptionMaterial {
    query_stage_master_key: String,
    query_id: String,
    smk_id: i64,
}

#[allow(dead_code)]
pub struct DataOutputSnowflakePlugin {
    pub(crate) config: DataOutputSnowflakePluginConfig,
    pub(crate) buffer_name: String,
    client: reqwest::Client,
    /// Cached v2 API token (JWT for key-pair, session token for password)
    token: tokio::sync::RwLock<Option<(String, std::time::Instant)>>,
    /// Cached v1 session token (always a session token, works for PUT)
    session_token: tokio::sync::RwLock<Option<(String, std::time::Instant)>>,
}

const TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(50 * 60);
const SESSION_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(3 * 3600);

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
            token: Default::default(),
            session_token: Default::default(),
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
            token: Default::default(),
            session_token: Default::default(),
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
            staging_s3_bucket: {
                let v = Config::getenv("SNOWFLAKE_STAGING_S3_BUCKET", "");
                if v.is_empty() { None } else { Some(v) }
            },
            staging_s3_prefix: {
                let v = Config::getenv("SNOWFLAKE_STAGING_S3_PREFIX", "skippr-staging");
                Some(v)
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
            info!(target: "snowflake", "authenticating via password (no SNOWFLAKE_PRIVATE_KEY_PATH set)");
            self.authenticate_password().await?
        };

        {
            let mut guard = self.token.write().await;
            *guard = Some((token.clone(), std::time::Instant::now()));
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
            let resp = self
                .client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .json(&payload)
                .send()
                .await?;
            let body: serde_json::Value = resp.json().await?;
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

    // ── Snowflake file transfer protocol (PUT) ──────────────────────────

    /// Send a PUT command via the v1 session API to obtain stage upload
    /// credentials and encryption material.
    async fn initiate_put(
        &self,
        session_token: &str,
        stage_path: &str,
        filename: &str,
    ) -> Result<StageUploadInfo, Box<dyn std::error::Error + Send + Sync>> {
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

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", "skippr/1.0")
            .header(
                "Authorization",
                format!("Snowflake Token=\"{}\"", session_token),
            )
            .json(&payload)
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        let success = body.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
        if !success {
            let msg = body
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(format!("PUT initiation failed: {}", msg).into());
        }

        let data = body
            .get("data")
            .ok_or("Missing 'data' in PUT response")?;
        let stage_info = data
            .get("stageInfo")
            .ok_or("Missing 'stageInfo' in PUT response")?;

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

        let creds = stage_info
            .get("creds")
            .ok_or("Missing 'creds' in stage info")?;
        let aws_key_id = creds["AWS_KEY_ID"].as_str().unwrap_or("").to_string();
        let aws_secret_key = creds["AWS_SECRET_KEY"].as_str().unwrap_or("").to_string();
        let aws_token = creds
            .get("AWS_TOKEN")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        let end_point = stage_info
            .get("endPoint")
            .and_then(|e| e.as_str())
            .map(String::from);

        // encryptionMaterial can be an object, an array, or null
        let enc_mat_raw = data.get("encryptionMaterial");
        let encryption_material = Self::parse_encryption_material(enc_mat_raw);

        Ok(StageUploadInfo {
            location_type,
            location,
            region,
            aws_key_id,
            aws_secret_key,
            aws_token,
            encryption_material,
            end_point,
        })
    }

    fn parse_encryption_material(
        raw: Option<&serde_json::Value>,
    ) -> Option<EncryptionMaterial> {
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

    /// Upload a file to the stage's backing S3 storage using the temporary
    /// credentials returned by the PUT initiation. Encrypts if required.
    async fn upload_to_stage(
        &self,
        info: &StageUploadInfo,
        filename: &str,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if info.location_type != "S3" {
            return Err(format!(
                "Unsupported stage storage: '{}'. Only S3-backed stages are currently supported.",
                info.location_type
            )
            .into());
        }

        // location format: "bucket/prefix/" — split into bucket + key prefix
        let slash_pos = info.location.find('/').unwrap_or(info.location.len());
        let bucket = &info.location[..slash_pos];
        let prefix = info
            .location
            .get(slash_pos + 1..)
            .unwrap_or("")
            .trim_end_matches('/');

        let s3_key = if prefix.is_empty() {
            filename.to_string()
        } else {
            format!("{}/{}", prefix, filename)
        };

        // Encrypt if stage requires client-side encryption.
        // S3 metadata uses x-amz-key / x-amz-iv / x-amz-matdesc (not Azure's
        // encryptiondata blob).
        let (upload_bytes, extra_metadata) = if let Some(ref enc) = info.encryption_material {
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

            let mut meta = std::collections::HashMap::<String, String>::new();
            meta.insert("sfc-digest".to_string(), Self::sha256_digest(data));
            meta.insert("x-amz-key".to_string(), enc_key_b64);
            meta.insert("x-amz-iv".to_string(), iv_b64);
            meta.insert("x-amz-matdesc".to_string(), matdesc);
            (encrypted, Some(meta))
        } else {
            let mut meta = std::collections::HashMap::<String, String>::new();
            meta.insert("sfc-digest".to_string(), Self::sha256_digest(data));
            (data.to_vec(), Some(meta))
        };

        // Build a one-shot S3 client with the stage's temporary credentials
        let creds = aws_credential_types::Credentials::new(
            &info.aws_key_id,
            &info.aws_secret_key,
            if info.aws_token.is_empty() {
                None
            } else {
                Some(info.aws_token.clone())
            },
            None,
            "snowflake-stage",
        );
        let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .credentials_provider(creds)
            .region(aws_types::region::Region::new(info.region.clone()))
            .load()
            .await;
        let s3_client = S3Client::new(&aws_cfg);

        let mut req = s3_client
            .put_object()
            .bucket(bucket)
            .key(&s3_key)
            .content_type("application/octet-stream")
            .body(ByteStream::from(upload_bytes));

        if let Some(meta) = extra_metadata {
            for (k, v) in meta {
                req = req.metadata(k, v);
            }
        }

        req.send().await.map_err(|e| {
            format!("S3 upload to Snowflake stage failed: {}", e)
        })?;

        Ok(())
    }

    // ── Client-side encryption (AES-CBC + AES-ECB key wrapping) ─────────

    #[allow(deprecated)]
    fn encrypt_for_stage(
        data: &[u8],
        query_stage_master_key: &str,
    ) -> Result<(Vec<u8>, String, String), std::io::Error> {
        use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
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
                    for i in 0..16 { block[i] = chunk[i] ^ prev[i]; }
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
                cbc_encrypt!(aes::Aes128::new(GenericArray::from_slice(&file_key)), padded_data, iv),
                ecb_encrypt!(aes::Aes128::new(GenericArray::from_slice(&master_key)), padded_key),
            ),
            24 => (
                cbc_encrypt!(aes::Aes192::new(GenericArray::from_slice(&file_key)), padded_data, iv),
                ecb_encrypt!(aes::Aes192::new(GenericArray::from_slice(&master_key)), padded_key),
            ),
            32 => (
                cbc_encrypt!(aes::Aes256::new(GenericArray::from_slice(&file_key)), padded_data, iv),
                ecb_encrypt!(aes::Aes256::new(GenericArray::from_slice(&master_key)), padded_key),
            ),
            n => {
                return Err(std::io::Error::other(format!(
                    "Unsupported master key length: {} bytes", n
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
            ArrowDataType::Int8 | ArrowDataType::Int16 | ArrowDataType::Int32
            | ArrowDataType::Int64 | ArrowDataType::UInt8 | ArrowDataType::UInt16
            | ArrowDataType::UInt32 | ArrowDataType::UInt64 => "NUMBER(38,0)".into(),
            ArrowDataType::Float16 | ArrowDataType::Float32 | ArrowDataType::Float64 => "DOUBLE".into(),
            ArrowDataType::Date32 | ArrowDataType::Date64 => "DATE".into(),
            ArrowDataType::Timestamp(_, _) => "TIMESTAMP_NTZ".into(),
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => "VARCHAR".into(),
            ArrowDataType::Struct(_) | ArrowDataType::List(_) | ArrowDataType::LargeList(_)
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
            self.execute_sql(&create_ddl).await.map(|_| ()).map_err(|e| {
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
        // Prefer explicit S3 staging when configured
        if self.config.staging_s3_bucket.is_some() {
            return self.inner_sync_s3_staging(stream, filename).await;
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
        use crate::metrics::counters;
        use crate::plugins::parquet_util::serialize_to_parquet;

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let fq_table = format!(
            "\"{}\".\"{}\".\"{}\"",
            self.config.database,
            self.config.schema,
            table_name.to_uppercase()
        );

        let parquet = match serialize_to_parquet(stream).await {
            Ok(p) => p,
            Err(e) => {
                counters::dec_uploads_in_flight();
                return Err(e);
            }
        };
        let row_count = parquet.meta_data.num_rows;
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

        let copy_sql = format!(
            "COPY INTO {} FROM {}/{}/{} FILE_FORMAT = (TYPE = PARQUET) MATCH_BY_COLUMN_NAME = CASE_INSENSITIVE",
            fq_table, stage, table_name, parquet_filename
        );

        info!(
            "Snowflake COPY INTO {} ({} rows)",
            fq_table, row_count
        );

        let copy_result = self.execute_sql(&copy_sql).await;

        // Best-effort cleanup of the staged file
        let remove_sql = format!(
            "REMOVE {}/{}/{}",
            stage, table_name, parquet_filename
        );
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

    /// Optional fallback: upload Parquet to a user-supplied S3 bucket and
    /// COPY INTO with inline AWS credentials.
    async fn inner_sync_s3_staging(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        use crate::plugins::parquet_util::serialize_to_parquet;

        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let fq_table = format!(
            "\"{}\".\"{}\".\"{}\"",
            self.config.database,
            self.config.schema,
            table_name.to_uppercase()
        );

        let parquet = match serialize_to_parquet(stream).await {
            Ok(p) => p,
            Err(e) => {
                counters::dec_uploads_in_flight();
                return Err(e);
            }
        };
        let row_count = parquet.meta_data.num_rows;
        let byte_count = parquet.size_bytes;

        let staging_bucket = self.config.staging_s3_bucket.as_deref().unwrap();
        let staging_prefix = self
            .config
            .staging_s3_prefix
            .as_deref()
            .unwrap_or("skippr-staging");
        let file_id = uuid::Uuid::new_v4();
        let staging_key = format!(
            "{}/{}/{}.parquet",
            staging_prefix.trim_matches('/'),
            table_name,
            file_id
        );

        let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let s3_client = S3Client::new(&aws_cfg);

        s3_client
            .put_object()
            .bucket(staging_bucket)
            .key(&staging_key)
            .body(ByteStream::from(parquet.bytes))
            .send()
            .await
            .map_err(|e| {
                counters::dec_uploads_in_flight();
                std::io::Error::other(format!("S3 staging upload failed: {}", e))
            })?;

        info!(
            "Uploaded staging Parquet s3://{}/{} ({} rows, {})",
            staging_bucket,
            staging_key,
            row_count,
            crate::helpers::Helpers::human_readable_size(byte_count)
        );

        let credentials = aws_cfg
            .credentials_provider()
            .ok_or_else(|| std::io::Error::other("No AWS credentials provider for COPY INTO"))?
            .provide_credentials()
            .await
            .map_err(|e| {
                std::io::Error::other(format!("Failed to resolve AWS credentials: {}", e))
            })?;

        let access_key = credentials.access_key_id();
        let secret_key = credentials.secret_access_key();
        let token_clause = credentials
            .session_token()
            .map(|t| format!(" AWS_TOKEN='{}'", t))
            .unwrap_or_default();

        let copy_sql = format!(
            "COPY INTO {} FROM 's3://{}/{}' \
             CREDENTIALS = (AWS_KEY_ID='{}' AWS_SECRET_KEY='{}'{}) \
             FILE_FORMAT = (TYPE = PARQUET) \
             MATCH_BY_COLUMN_NAME = CASE_INSENSITIVE",
            fq_table, staging_bucket, staging_key, access_key, secret_key, token_clause
        );

        info!(
            "Snowflake COPY INTO {} from s3 staging ({} rows)",
            fq_table, row_count
        );

        let copy_result = self.execute_sql(&copy_sql).await;

        let _ = s3_client
            .delete_object()
            .bucket(staging_bucket)
            .key(&staging_key)
            .send()
            .await;

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
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let namespace = BufferChunker::decode_file_namespace(&filename);
        let table_name = Self::namespace_to_table_name(&namespace);
        let arrow_schema = stream.schema();

        let col_defs: Vec<(String, String)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_uppercase(),
                    Self::arrow_type_to_snowflake_ddl(f.data_type()),
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
            let batch = batch_result
                .map_err(|e| std::io::Error::other(format!("stream error: {}", e)))?;
            let num_rows = batch.num_rows();
            if num_rows == 0 {
                continue;
            }

            let insert_sql = if has_structured_cols {
                let select_rows: Vec<String> = (0..num_rows)
                    .map(|row| {
                        let vals: Vec<String> = (0..batch.num_columns())
                            .map(|col_idx| {
                                let raw = Self::arrow_value_to_sql(
                                    batch.column(col_idx).as_ref(),
                                    row,
                                );
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
                            .map(|col| {
                                Self::arrow_value_to_sql(batch.column(col).as_ref(), row)
                            })
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
}

#[async_trait]
impl DataSink for DataOutputSnowflakePlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
    ) -> Result<(), std::io::Error> {
        self.inner_sync(stream, filename).await
    }
}

impl DataOutputSnowflakePlugin {
    pub async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &crate::discover::OutputMetadata,
    ) -> Result<(), std::io::Error> {
        use crate::converters::skippr_arrow::convert_skippr_to_arrow;

        self.ensure_schema().await?;

        let fields: std::collections::HashMap<String, crate::discover::OutputMetadata> =
            metadata
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

        let arrow_schema = convert_skippr_to_arrow(Box::new(fields)).map_err(|e| {
            std::io::Error::other(format!(
                "Arrow schema conversion for '{}': {}",
                namespace, e
            ))
        })?;

        let table_name = Self::namespace_to_table_name(namespace);
        let col_defs: Vec<(String, String)> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                (
                    f.name().to_uppercase(),
                    Self::arrow_type_to_snowflake_ddl(f.data_type()),
                )
            })
            .collect();

        let fq_table = format!(
            "\"{}\".\"{}\".\"{}\"",
            self.config.database,
            self.config.schema,
            table_name.to_uppercase()
        );

        self.ensure_table(&fq_table, &col_defs).await
    }
}
