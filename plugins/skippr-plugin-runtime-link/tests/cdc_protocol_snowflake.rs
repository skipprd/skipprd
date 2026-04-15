/// CDC Protocol Tests: Snowflake (secondary coverage)
///
/// Low-level tests that validate Snowflake CDC SQL apply generation.
/// These are NOT the CDC acceptance gate — see `cdc_e2e_snowflake` for
/// the full-pipeline end-to-end suite.
///
/// Requires a live Snowflake account with key-pair auth.
///
/// Run: `cargo test -p skippr-plugin-runtime-link --features runtime-sink-link --test cdc_protocol_snowflake -- --ignored`
use skippr_plugin_runtime_link::runtime_sink_link::cdc_apply::{
    ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
    tombstone_table_name, upsert_if_newer_sql,
};
use skippr_plugin_runtime_link::runtime_sink_link::snowflake::SnowflakeCdcBackend;

const SNOWFLAKE_ACCOUNT: &str = "RSSKNWT-KC53195";
const SNOWFLAKE_USER: &str = "paulhudson";
const SNOWFLAKE_PRIVATE_KEY_PATH: &str =
    "/Users/huders2000/Documents/sites/skippr/react/snowflake_key.p8";
const SNOWFLAKE_DATABASE: &str = "ANALYTICS";
const SNOWFLAKE_SCHEMA: &str = "RAW";
const SNOWFLAKE_WAREHOUSE: &str = "COMPUTE_WH";
const SNOWFLAKE_ROLE: &str = "ACCOUNTADMIN";

struct SnowflakeTestClient {
    http: reqwest::Client,
    token: String,
}

impl SnowflakeTestClient {
    async fn new() -> Self {
        let token = Self::authenticate_keypair().expect("Snowflake key-pair auth failed");
        Self {
            http: reqwest::Client::new(),
            token,
        }
    }

    fn authenticate_keypair() -> Result<String, String> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use rsa::pkcs8::DecodePrivateKey;
        use sha2::{Digest, Sha256};

        let pem = std::fs::read_to_string(SNOWFLAKE_PRIVATE_KEY_PATH)
            .map_err(|e| format!("read key: {}", e))?;
        let private_key =
            rsa::RsaPrivateKey::from_pkcs8_pem(&pem).map_err(|e| format!("parse key: {}", e))?;
        let public_key = private_key.to_public_key();
        let public_key_der = rsa::pkcs8::EncodePublicKey::to_public_key_der(&public_key)
            .map_err(|e| format!("encode pubkey: {}", e))?;

        let fingerprint = {
            let mut hasher = Sha256::new();
            hasher.update(public_key_der.as_bytes());
            STANDARD.encode(hasher.finalize())
        };

        let qualified_user = format!(
            "{}.{}",
            SNOWFLAKE_ACCOUNT.to_uppercase(),
            SNOWFLAKE_USER.to_uppercase()
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = serde_json::json!({
            "iss": format!("{}.SHA256:{}", qualified_user, fingerprint),
            "sub": qualified_user,
            "iat": now,
            "exp": now + 3600,
        });
        let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
            .map_err(|e| format!("encoding key: {}", e))?;
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        jsonwebtoken::encode(&header, &claims, &encoding_key)
            .map_err(|e| format!("sign JWT: {}", e))
    }

    async fn execute_sql(&self, sql: &str) -> Result<serde_json::Value, String> {
        let url = format!(
            "https://{}.snowflakecomputing.com/api/v2/statements",
            SNOWFLAKE_ACCOUNT
        );
        let payload = serde_json::json!({
            "statement": sql,
            "timeout": 120,
            "database": SNOWFLAKE_DATABASE,
            "schema": SNOWFLAKE_SCHEMA,
            "warehouse": SNOWFLAKE_WAREHOUSE,
            "role": SNOWFLAKE_ROLE,
        });

        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", "skippr-test/1.0")
            .header("Authorization", format!("Bearer {}", self.token))
            .header("X-Snowflake-Authorization-Token-Type", "KEYPAIR_JWT")
            .json(&payload)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if !status.is_success() && status.as_u16() != 202 {
            let msg = body["message"].as_str().unwrap_or("unknown");
            return Err(format!("HTTP {}: {}", status, msg));
        }

        let code = body.get("code").and_then(|c| c.as_str()).unwrap_or("");
        if code == "333334" {
            return self.poll_statement(&body).await;
        }
        if code == "090001" || code == "000000" || code.is_empty() {
            return Ok(body);
        }

        let msg = body["message"].as_str().unwrap_or("unknown");
        Err(format!("SQL error ({}): {}", code, msg))
    }

    async fn poll_statement(
        &self,
        initial: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let handle = initial
            .get("statementHandle")
            .and_then(|h| h.as_str())
            .ok_or("missing statementHandle")?;
        let poll_url = format!(
            "https://{}.snowflakecomputing.com/api/v2/statements/{}",
            SNOWFLAKE_ACCOUNT, handle
        );

        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let resp = self
                .http
                .get(&poll_url)
                .header("Authorization", format!("Bearer {}", self.token))
                .header("X-Snowflake-Authorization-Token-Type", "KEYPAIR_JWT")
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            if body.get("code").and_then(|c| c.as_str()).unwrap_or("") != "333334" {
                return Ok(body);
            }
        }

        Err("poll timed out".into())
    }

    async fn query_scalar(&self, sql: &str) -> Result<String, String> {
        let body = self.execute_sql(sql).await?;
        Ok(body
            .pointer("/data/0/0")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    async fn execute_multi_sql(&self, sql: &str) -> Result<(), String> {
        for stmt in sql.split(';') {
            let trimmed = stmt.trim();
            if trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("BEGIN")
                || trimmed.eq_ignore_ascii_case("COMMIT")
                || trimmed.eq_ignore_ascii_case("START TRANSACTION")
            {
                continue;
            }
            self.execute_sql(trimmed).await?;
        }
        Ok(())
    }
}

fn fq_table(name: &str) -> String {
    format!(r#""{SNOWFLAKE_DATABASE}"."{SNOWFLAKE_SCHEMA}"."{name}""#)
}

#[tokio::test]
#[ignore]
async fn cdc_snowflake_ddl_order_token_and_tombstone() {
    let sf = SnowflakeTestClient::new().await;
    let table = fq_table("CDC_SF_DDL_TEST");
    let tombstone = tombstone_table_name(&table);

    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", table))
        .await
        .unwrap();
    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", tombstone))
        .await
        .unwrap();
    sf.execute_sql(&format!(
        r#"CREATE TABLE {} ("ID" NUMBER, "NAME" VARCHAR, "_skippr_order_token" BINARY)"#,
        table
    ))
    .await
    .unwrap();

    let ddl = ddl_add_order_token_column::<SnowflakeCdcBackend>(&table);
    sf.execute_sql(&ddl).await.unwrap();
    sf.execute_sql(&ddl).await.unwrap();

    let tombstone_ddl = ddl_create_tombstone_table::<SnowflakeCdcBackend>(
        &tombstone,
        &[("ID".to_string(), "NUMBER".to_string())],
    );
    sf.execute_sql(&tombstone_ddl).await.unwrap();
    sf.execute_sql(&tombstone_ddl).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn cdc_snowflake_upsert_if_newer() {
    let sf = SnowflakeTestClient::new().await;
    let table = fq_table("CDC_SF_UPSERT_TEST");
    let tombstone = tombstone_table_name(&table);

    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", table))
        .await
        .unwrap();
    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", tombstone))
        .await
        .unwrap();
    sf.execute_sql(&format!(
        r#"CREATE TABLE {} ("ID" NUMBER, "NAME" VARCHAR, "_skippr_order_token" BINARY)"#,
        table
    ))
    .await
    .unwrap();
    let tombstone_ddl = ddl_create_tombstone_table::<SnowflakeCdcBackend>(
        &tombstone,
        &[("ID".to_string(), "NUMBER".to_string())],
    );
    sf.execute_sql(&tombstone_ddl).await.unwrap();

    let sql = upsert_if_newer_sql::<SnowflakeCdcBackend>(
        &table,
        &tombstone,
        &[
            r#""ID""#.into(),
            r#""NAME""#.into(),
            r#""_skippr_order_token""#.into(),
        ],
        &[
            "1".into(),
            "'alice'".into(),
            "HEX_DECODE_BINARY('0000000000000002')".into(),
        ],
        &[r#""ID""#.into()],
        "0000000000000002",
    );
    sf.execute_multi_sql(&sql).await.unwrap();

    let sql2 = upsert_if_newer_sql::<SnowflakeCdcBackend>(
        &table,
        &tombstone,
        &[
            r#""ID""#.into(),
            r#""NAME""#.into(),
            r#""_skippr_order_token""#.into(),
        ],
        &[
            "1".into(),
            "'bob'".into(),
            "HEX_DECODE_BINARY('0000000000000005')".into(),
        ],
        &[r#""ID""#.into()],
        "0000000000000005",
    );
    sf.execute_multi_sql(&sql2).await.unwrap();

    let sql3 = upsert_if_newer_sql::<SnowflakeCdcBackend>(
        &table,
        &tombstone,
        &[
            r#""ID""#.into(),
            r#""NAME""#.into(),
            r#""_skippr_order_token""#.into(),
        ],
        &[
            "1".into(),
            "'stale_charlie'".into(),
            "HEX_DECODE_BINARY('0000000000000002')".into(),
        ],
        &[r#""ID""#.into()],
        "0000000000000002",
    );
    sf.execute_multi_sql(&sql3).await.unwrap();

    let name = sf
        .query_scalar(&format!(r#"SELECT "NAME" FROM {} WHERE "ID" = 1"#, table))
        .await
        .unwrap();
    assert_eq!(name, "bob");
}

#[tokio::test]
#[ignore]
async fn cdc_snowflake_delete_if_newer() {
    let sf = SnowflakeTestClient::new().await;
    let table = fq_table("CDC_SF_DELETE_TEST");
    let tombstone = tombstone_table_name(&table);

    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", table))
        .await
        .unwrap();
    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", tombstone))
        .await
        .unwrap();
    sf.execute_sql(&format!(
        r#"CREATE TABLE {} ("ID" NUMBER, "NAME" VARCHAR, "_skippr_order_token" BINARY)"#,
        table
    ))
    .await
    .unwrap();
    let tombstone_ddl = ddl_create_tombstone_table::<SnowflakeCdcBackend>(
        &tombstone,
        &[("ID".to_string(), "NUMBER".to_string())],
    );
    sf.execute_sql(&tombstone_ddl).await.unwrap();

    let sql = upsert_if_newer_sql::<SnowflakeCdcBackend>(
        &table,
        &tombstone,
        &[
            r#""ID""#.into(),
            r#""NAME""#.into(),
            r#""_skippr_order_token""#.into(),
        ],
        &[
            "1".into(),
            "'alice'".into(),
            "HEX_DECODE_BINARY('0000000000000010')".into(),
        ],
        &[r#""ID""#.into()],
        "0000000000000010",
    );
    sf.execute_multi_sql(&sql).await.unwrap();

    let del = delete_if_newer_sql::<SnowflakeCdcBackend>(
        &table,
        &tombstone,
        &[r#""ID""#.into()],
        &["1".into()],
        &["NUMBER".into()],
        "0000000000000020",
    );
    sf.execute_multi_sql(&del).await.unwrap();

    let stale = upsert_if_newer_sql::<SnowflakeCdcBackend>(
        &table,
        &tombstone,
        &[
            r#""ID""#.into(),
            r#""NAME""#.into(),
            r#""_skippr_order_token""#.into(),
        ],
        &[
            "1".into(),
            "'zombie'".into(),
            "HEX_DECODE_BINARY('0000000000000015')".into(),
        ],
        &[r#""ID""#.into()],
        "0000000000000015",
    );
    sf.execute_multi_sql(&stale).await.unwrap();

    let count = sf
        .query_scalar(&format!(r#"SELECT COUNT(*) FROM {} WHERE "ID" = 1"#, table))
        .await
        .unwrap();
    assert_eq!(count, "0");
}

#[tokio::test]
#[ignore]
async fn cdc_snowflake_replay_idempotency() {
    let sf = SnowflakeTestClient::new().await;
    let table = fq_table("CDC_SF_REPLAY_TEST");
    let tombstone = tombstone_table_name(&table);

    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", table))
        .await
        .unwrap();
    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", tombstone))
        .await
        .unwrap();
    sf.execute_sql(&format!(
        r#"CREATE TABLE {} ("ID" NUMBER, "NAME" VARCHAR, "_skippr_order_token" BINARY)"#,
        table
    ))
    .await
    .unwrap();
    let tombstone_ddl = ddl_create_tombstone_table::<SnowflakeCdcBackend>(
        &tombstone,
        &[("ID".to_string(), "NUMBER".to_string())],
    );
    sf.execute_sql(&tombstone_ddl).await.unwrap();

    let sql = upsert_if_newer_sql::<SnowflakeCdcBackend>(
        &table,
        &tombstone,
        &[
            r#""ID""#.into(),
            r#""NAME""#.into(),
            r#""_skippr_order_token""#.into(),
        ],
        &[
            "42".into(),
            "'replay_test'".into(),
            "HEX_DECODE_BINARY('00000000000000ff')".into(),
        ],
        &[r#""ID""#.into()],
        "00000000000000ff",
    );
    sf.execute_multi_sql(&sql).await.unwrap();
    sf.execute_multi_sql(&sql).await.unwrap();

    let count = sf
        .query_scalar(&format!(
            r#"SELECT COUNT(*) FROM {} WHERE "ID" = 42"#,
            table
        ))
        .await
        .unwrap();
    assert_eq!(count, "1");
}

#[tokio::test]
#[ignore]
async fn cdc_snowflake_composite_key() {
    let sf = SnowflakeTestClient::new().await;
    let table = fq_table("CDC_SF_COMPOSITE_TEST");
    let tombstone = tombstone_table_name(&table);
    let bk = vec![r#""TENANT_ID""#.to_string(), r#""USER_ID""#.to_string()];

    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", table))
        .await
        .unwrap();
    sf.execute_sql(&format!("DROP TABLE IF EXISTS {}", tombstone))
        .await
        .unwrap();
    sf.execute_sql(&format!(
        r#"CREATE TABLE {} ("TENANT_ID" NUMBER, "USER_ID" NUMBER, "EMAIL" VARCHAR, "_skippr_order_token" BINARY)"#,
        table
    ))
    .await
    .unwrap();
    let tombstone_ddl = ddl_create_tombstone_table::<SnowflakeCdcBackend>(
        &tombstone,
        &[
            ("TENANT_ID".to_string(), "NUMBER".to_string()),
            ("USER_ID".to_string(), "NUMBER".to_string()),
        ],
    );
    sf.execute_sql(&tombstone_ddl).await.unwrap();

    let sql = upsert_if_newer_sql::<SnowflakeCdcBackend>(
        &table,
        &tombstone,
        &[
            r#""TENANT_ID""#.into(),
            r#""USER_ID""#.into(),
            r#""EMAIL""#.into(),
            r#""_skippr_order_token""#.into(),
        ],
        &[
            "1".into(),
            "100".into(),
            "'a@b.com'".into(),
            "HEX_DECODE_BINARY('0000000000000001')".into(),
        ],
        &bk,
        "0000000000000001",
    );
    sf.execute_multi_sql(&sql).await.unwrap();

    let sql2 = upsert_if_newer_sql::<SnowflakeCdcBackend>(
        &table,
        &tombstone,
        &[
            r#""TENANT_ID""#.into(),
            r#""USER_ID""#.into(),
            r#""EMAIL""#.into(),
            r#""_skippr_order_token""#.into(),
        ],
        &[
            "1".into(),
            "200".into(),
            "'c@d.com'".into(),
            "HEX_DECODE_BINARY('0000000000000001')".into(),
        ],
        &bk,
        "0000000000000001",
    );
    sf.execute_multi_sql(&sql2).await.unwrap();

    let count = sf
        .query_scalar(&format!("SELECT COUNT(*) FROM {}", table))
        .await
        .unwrap();
    assert_eq!(count, "2");
}
