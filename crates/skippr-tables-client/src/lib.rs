//! Async CloudTables JSON HTTP client.
//!
//! Guests use mesh loopback/CNI + a host-broker-issued workload JWT (D37).
//! Public `*.cloud.skippr.io` hairpins fail closed.

use guest_broker::SystemBrokerClient;
use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};
use thiserror::Error;

pub const CLOUD_TARGET_HEADER: &str = "x-cloud-target";
pub const GATEWAY_HOP_HEADER: &str = "x-cloud-gateway-hop";
pub const TENANT_HEADER: &str = "x-cloud-tenant-id";
pub const ACCESS_TOKEN_HEADER: &str = "x-cloud-access-token";

#[derive(Debug, Error)]
pub enum TablesClientError {
    #[error("{0}")]
    Message(String),
    #[error("{target}: {message}")]
    Cloud {
        target: String,
        status: u16,
        code: Option<String>,
        message: String,
    },
}

impl TablesClientError {
    pub fn msg(s: impl Into<String>) -> Self {
        Self::Message(s.into())
    }

    pub fn is_conditional_check_failed(&self) -> bool {
        matches!(
            self,
            Self::Cloud {
                code: Some(code),
                message,
                ..
            } if code == "ConditionalCheckFailedException"
                || (code == "TransactionCanceledException"
                    && message.contains("\"code\":\"ConditionalCheckFailed\""))
        )
    }
}

#[derive(Clone, Debug)]
enum MeshAuthority {
    Broker(SystemBrokerClient),
    #[cfg(test)]
    TestToken(String),
}

#[derive(Clone, Debug)]
pub struct CloudCredentials {
    authority: MeshAuthority,
}

impl CloudCredentials {
    pub fn from_env_for_endpoint(endpoint: &str) -> Result<Self, TablesClientError> {
        if !is_mesh_endpoint(endpoint) {
            return Err(TablesClientError::msg(
                "clustered Cloud tables requires a mesh/loopback CLOUD_TABLES_ENDPOINT",
            ));
        }
        if !mesh_auth_configured() {
            return Err(TablesClientError::msg(
                "clustered Cloud tables requires GuestCredentialBroker (CLOUD_SYSTEM_BROKER_CONFIG)",
            ));
        }
        let broker = SystemBrokerClient::from_env().map_err(|error| {
            TablesClientError::msg(format!("shared guest broker required: {error}"))
        })?;
        Ok(Self {
            authority: MeshAuthority::Broker(broker),
        })
    }

    #[cfg(test)]
    fn for_test_token(token: impl Into<String>) -> Self {
        Self {
            authority: MeshAuthority::TestToken(token.into()),
        }
    }
}

/// Guest mesh auth is a readable host broker config drive. Env JWTs are not authority.
pub fn mesh_auth_configured() -> bool {
    SystemBrokerClient::from_env().is_ok()
}

/// Fail closed on public Cloud hostnames (D37: no `*.cloud.skippr.io` hairpin).
pub fn parse_tables_endpoint(raw: &str) -> Result<String, TablesClientError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(TablesClientError::msg("CLOUD_TABLES_ENDPOINT is required"));
    }
    let url = url::Url::parse(raw)
        .map_err(|err| TablesClientError::msg(format!("invalid CLOUD_TABLES_ENDPOINT: {err}")))?;
    let host = url.host_str().unwrap_or("");
    if is_public_cloud_hostname(host) {
        return Err(TablesClientError::msg(
            "CLOUD_TABLES_ENDPOINT must be mesh/loopback, not *.cloud.skippr.io",
        ));
    }
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(TablesClientError::msg(
            "CLOUD_TABLES_ENDPOINT must be http or https",
        ));
    }
    if !is_mesh_endpoint(raw) {
        return Err(TablesClientError::msg(
            "CLOUD_TABLES_ENDPOINT must be mesh/loopback, not an arbitrary host",
        ));
    }
    Ok(raw.trim_end_matches('/').to_string())
}

pub fn is_public_cloud_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "cloud.skippr.io" || host.ends_with(".cloud.skippr.io")
}

pub fn is_mesh_endpoint(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return false;
    }
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .map(is_mesh_ip)
            .unwrap_or(false),
        None => false,
    }
}

fn is_mesh_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback() || (o[0] == 10 && (1..=254).contains(&o[1]))
        }
        std::net::IpAddr::V6(v6) => v6.is_loopback(),
    }
}

#[derive(Clone)]
pub struct TablesClient {
    endpoint: String,
    credentials: CloudCredentials,
    tenant_id: String,
    http: reqwest::Client,
}

impl TablesClient {
    pub fn from_env() -> Result<Self, TablesClientError> {
        let endpoint =
            parse_tables_endpoint(&std::env::var("CLOUD_TABLES_ENDPOINT").unwrap_or_default())?;
        let credentials = CloudCredentials::from_env_for_endpoint(&endpoint)?;
        let tenant_id = std::env::var("CLOUD_TENANT_ID")
            .or_else(|_| std::env::var("SKIPPR_TENANT"))
            .unwrap_or_else(|_| "system".into());
        Self::new(endpoint, credentials, tenant_id)
    }

    pub fn new(
        endpoint: String,
        credentials: CloudCredentials,
        tenant_id: String,
    ) -> Result<Self, TablesClientError> {
        let endpoint = parse_tables_endpoint(&endpoint)?;
        Ok(Self {
            endpoint,
            credentials,
            tenant_id,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .map_err(|err| TablesClientError::msg(err.to_string()))?,
        })
    }

    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    pub async fn invoke(&self, target: &str, body: Value) -> Result<Value, TablesClientError> {
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|err| TablesClientError::msg(format!("encode body: {err}")))?;
        let request = self
            .http
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(CLOUD_TARGET_HEADER, target);
        if !is_mesh_endpoint(&self.endpoint) {
            return Err(TablesClientError::msg(
                "Cloud tables client is mesh JWT only",
            ));
        }
        let token = resolve_access_token(&self.credentials).await?;
        let request = request
            .header(GATEWAY_HOP_HEADER, "1")
            .header(TENANT_HEADER, &self.tenant_id)
            .header(ACCESS_TOKEN_HEADER, &token)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        let response = request
            .body(body_bytes)
            .send()
            .await
            .map_err(|err| TablesClientError::msg(format!("tables request: {err}")))?;
        parse_response(response, target).await
    }

    pub async fn put_item(
        &self,
        table_name: &str,
        item: Value,
        condition: Option<&str>,
        expression_attribute_values: Option<Value>,
    ) -> Result<(), TablesClientError> {
        let mut body = json!({
            "tableName": table_name,
            "item": item
        });
        if let Some(expr) = condition {
            body["conditionExpression"] = json!(expr);
        }
        if let Some(values) = expression_attribute_values {
            body["expressionAttributeValues"] = values;
        }
        self.invoke("CloudTables.PutItem", body).await?;
        Ok(())
    }

    pub async fn get_item(
        &self,
        table_name: &str,
        pk: &str,
        sk: &str,
        consistent: bool,
    ) -> Result<Option<Value>, TablesClientError> {
        let body = json!({
            "tableName": table_name,
            "key": {
                "PK": { "S": pk },
                "SK": { "S": sk }
            },
            "consistentRead": consistent
        });
        let payload = self.invoke("CloudTables.GetItem", body).await?;
        Ok(item_from_response(&payload))
    }

    pub async fn update_item(
        &self,
        table_name: &str,
        pk: &str,
        sk: &str,
        update_expression: &str,
        expression_values: Value,
        condition: Option<&str>,
        return_all_new: bool,
    ) -> Result<Option<Value>, TablesClientError> {
        let mut body = json!({
            "tableName": table_name,
            "key": {
                "PK": { "S": pk },
                "SK": { "S": sk }
            },
            "updateExpression": update_expression,
            "expressionAttributeValues": expression_values,
        });
        if let Some(expr) = condition {
            body["conditionExpression"] = json!(expr);
        }
        if return_all_new {
            body["returnValues"] = json!("ALL_NEW");
        }
        let payload = self.invoke("CloudTables.UpdateItem", body).await?;
        Ok(item_from_response(&payload))
    }

    pub async fn delete_item(
        &self,
        table_name: &str,
        pk: &str,
        sk: &str,
        condition: Option<&str>,
        expression_attribute_values: Option<Value>,
    ) -> Result<(), TablesClientError> {
        let mut body = json!({
            "tableName": table_name,
            "key": {
                "PK": { "S": pk },
                "SK": { "S": sk }
            }
        });
        if let Some(expr) = condition {
            body["conditionExpression"] = json!(expr);
        }
        if let Some(values) = expression_attribute_values {
            body["expressionAttributeValues"] = values;
        }
        self.invoke("CloudTables.DeleteItem", body).await?;
        Ok(())
    }

    pub async fn query(
        &self,
        table_name: &str,
        key_condition: &str,
        expression_values: Value,
    ) -> Result<Vec<Value>, TablesClientError> {
        let body = json!({
            "tableName": table_name,
            "keyConditionExpression": key_condition,
            "expressionAttributeValues": expression_values,
            "scanIndexForward": true,
        });
        let payload = self.invoke("CloudTables.Query", body).await?;
        Ok(items_from_response(&payload))
    }

    pub async fn scan(&self, table_name: &str) -> Result<Vec<Value>, TablesClientError> {
        let body = json!({ "tableName": table_name });
        let payload = self.invoke("CloudTables.Scan", body).await?;
        Ok(items_from_response(&payload))
    }

    pub async fn transact_write(&self, transact_items: Value) -> Result<(), TablesClientError> {
        let body = json!({ "transactItems": transact_items });
        self.invoke("CloudTables.TransactWriteItems", body).await?;
        Ok(())
    }
}

fn item_from_response(payload: &Value) -> Option<Value> {
    payload.get("item").cloned()
}

fn items_from_response(payload: &Value) -> Vec<Value> {
    payload
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

async fn parse_response(
    response: reqwest::Response,
    target: &str,
) -> Result<Value, TablesClientError> {
    let status = response.status().as_u16();
    let payload: Value = response
        .json()
        .await
        .unwrap_or_else(|_| json!({ "message": "non-json tables response" }));
    if (200..300).contains(&status) {
        return Ok(payload);
    }
    let code = payload
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| payload.get("__type").and_then(Value::as_str))
        .map(str::to_string);
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("tables request failed")
        .to_string();
    Err(TablesClientError::Cloud {
        target: target.to_string(),
        status,
        code,
        message,
    })
}

async fn resolve_access_token(credentials: &CloudCredentials) -> Result<String, TablesClientError> {
    match &credentials.authority {
        MeshAuthority::Broker(broker) => {
            let broker = broker.clone();
            tokio::task::spawn_blocking(move || broker.platform_workload_jwt())
                .await
                .map_err(|error| TablesClientError::msg(format!("broker task: {error}")))?
                .map_err(|error| TablesClientError::msg(error.to_string()))
        }
        #[cfg(test)]
        MeshAuthority::TestToken(token) => Ok(token.clone()),
    }
}

pub fn attr_s(item: &Value, name: &str) -> Option<String> {
    item.get(name)
        .and_then(|v| v.get("S"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub fn attr_n(item: &Value, name: &str) -> Option<u64> {
    item.get(name)
        .and_then(|v| v.get("N"))
        .and_then(Value::as_str)
        .and_then(|n| n.parse().ok())
}

pub fn attr_bool(item: &Value, name: &str) -> Option<bool> {
    item.get(name)
        .and_then(|v| v.get("BOOL"))
        .and_then(Value::as_bool)
}

pub fn s(value: impl Into<String>) -> Value {
    json!({ "S": value.into() })
}

pub fn n(value: impl ToString) -> Value {
    json!({ "N": value.to_string() })
}

pub fn bflag(value: bool) -> Value {
    json!({ "BOOL": value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const SKIPPR_BROKER_DRIVE: &str = r#"{
  "profile": "skippr",
  "guest_id": "skippr-0",
  "cluster_generation": 1,
  "guest_incarnation": "inc-1",
  "broker_endpoint": "http://127.0.0.1:8097",
  "capability": "opaque"
}
"#;

    #[test]
    fn rejects_public_cloud_hostname() {
        assert!(parse_tables_endpoint("https://tables.cloud.skippr.io").is_err());
        assert!(parse_tables_endpoint("https://api.cloud.skippr.io").is_err());
        assert!(parse_tables_endpoint("http://example.com").is_err());
        assert!(parse_tables_endpoint("http://127.0.0.1:8003").is_ok());
        assert!(parse_tables_endpoint("http://10.1.0.12:8003").is_ok());
    }

    #[test]
    fn mesh_loopback_and_cni() {
        assert!(is_mesh_endpoint("http://127.0.0.1:8003"));
        assert!(is_mesh_endpoint("http://10.90.0.1:8003"));
        assert!(!is_mesh_endpoint("https://tables.cloud.skippr.io"));
    }

    #[test]
    fn mesh_authority_requires_the_shared_broker() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
        std::env::remove_var("CLOUD_SYSTEM_BROKER_ENDPOINT");
        std::env::set_var("CLOUD_TABLES_ACCESS_TOKEN", "guest-held-jwt");
        std::env::set_var("CLOUD_ACCESS_TOKEN", "guest-held-jwt");
        assert!(
            CloudCredentials::from_env_for_endpoint("http://127.0.0.1:8003").is_err(),
            "env JWTs must not mint clustered Cloud tables authority"
        );
        assert!(CloudCredentials::from_env_for_endpoint("https://tables.cloud.skippr.io").is_err());
        assert!(
            !mesh_auth_configured(),
            "ACCESS_TOKEN env must not count as broker config"
        );
        std::env::remove_var("CLOUD_TABLES_ACCESS_TOKEN");
        std::env::remove_var("CLOUD_ACCESS_TOKEN");
    }

    #[test]
    fn mesh_auth_rejects_endpoint_env_alone() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
        std::env::set_var("CLOUD_SYSTEM_BROKER_ENDPOINT", "http://127.0.0.1:8097");
        assert!(
            !mesh_auth_configured(),
            "CLOUD_SYSTEM_BROKER_ENDPOINT must not authorize clustered tables"
        );
        std::env::remove_var("CLOUD_SYSTEM_BROKER_ENDPOINT");
    }

    #[test]
    fn mesh_auth_rejects_empty_broker_json() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLOUD_SYSTEM_BROKER_ENDPOINT");
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("broker.json"), "{}\n").unwrap();
        std::env::set_var("CLOUD_SYSTEM_BROKER_CONFIG", tmp.path());
        assert!(
            !mesh_auth_configured(),
            "empty broker.json must not count as a Skippr config drive"
        );
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
    }

    #[test]
    fn mesh_auth_follows_the_broker_config_drive() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLOUD_SYSTEM_BROKER_ENDPOINT");
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("broker.json"), SKIPPR_BROKER_DRIVE).unwrap();
        std::env::set_var("CLOUD_SYSTEM_BROKER_CONFIG", tmp.path());
        assert!(mesh_auth_configured());
        std::env::remove_var("CLOUD_SYSTEM_BROKER_CONFIG");
        assert!(!mesh_auth_configured());
    }

    #[test]
    fn conditional_error_codes() {
        let err = TablesClientError::Cloud {
            target: "CloudTables.PutItem".into(),
            status: 400,
            code: Some("ConditionalCheckFailedException".into()),
            message: "failed".into(),
        };
        assert!(err.is_conditional_check_failed());
    }

    #[tokio::test]
    async fn put_item_posts_camel_case_and_target_header() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = "{}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            req
        });
        let client = TablesClient::new(
            format!("http://{addr}"),
            CloudCredentials::for_test_token("test-jwt"),
            "tenant-a".into(),
        )
        .unwrap();
        client
            .put_item(
                "offsets",
                json!({
                    "PK": s("t#w#p"),
                    "SK": s("lease"),
                }),
                Some("attribute_not_exists(PK)"),
                None,
            )
            .await
            .unwrap();
        let captured = server.await.unwrap();
        assert!(captured.contains("x-cloud-target: CloudTables.PutItem"));
        assert!(captured.contains("\"tableName\":\"offsets\""));
        assert!(captured.contains("\"conditionExpression\":\"attribute_not_exists(PK)\""));
        assert!(captured.contains("Bearer test-jwt"));
    }
}
