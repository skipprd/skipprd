//! Async CloudTables JSON HTTP client.
//!
//! Guests use mesh loopback/CNI + workload JWT (D37). Public `*.cloud.skippr.io`
//! hairpins fail closed. This crate does not depend on Cloud `tables-client`.

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
pub struct CloudCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl CloudCredentials {
    pub fn from_env_for_endpoint(endpoint: &str) -> Result<Self, TablesClientError> {
        if !is_mesh_endpoint(endpoint) {
            return Err(TablesClientError::msg(
                "clustered Cloud tables requires a mesh/loopback CLOUD_TABLES_ENDPOINT",
            ));
        }
        let token = std::env::var("CLOUD_TABLES_ACCESS_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                std::env::var("CLOUD_ACCESS_TOKEN")
                    .ok()
                    .filter(|t| !t.trim().is_empty())
            });
        if token.is_none() {
            return Err(TablesClientError::msg(
                "clustered Cloud tables requires CLOUD_TABLES_ENDPOINT plus a mesh JWT (CLOUD_TABLES_ACCESS_TOKEN or CLOUD_ACCESS_TOKEN)",
            ));
        }
        Ok(Self {
            access_key_id: String::new(),
            secret_access_key: String::new(),
            session_token: None,
        })
    }
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
        let token = resolve_access_token(&self.credentials, &self.tenant_id, &self.http).await?;
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

async fn resolve_access_token(
    _credentials: &CloudCredentials,
    tenant_id: &str,
    _http: &reqwest::Client,
) -> Result<String, TablesClientError> {
    if let Ok(token) = std::env::var("CLOUD_TABLES_ACCESS_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(token);
        }
    }
    let tenant_key = format!(
        "CLOUD_ACCESS_TOKEN_{}",
        tenant_id.trim().replace('-', "_").to_ascii_uppercase()
    );
    if let Ok(token) = std::env::var(&tenant_key) {
        if !token.trim().is_empty() {
            return Ok(token);
        }
    }
    if let Ok(token) = std::env::var("CLOUD_ACCESS_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(token);
        }
    }
    Err(TablesClientError::msg(
        "mesh Cloud tables requires CLOUD_TABLES_ACCESS_TOKEN or CLOUD_ACCESS_TOKEN",
    ))
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn rejects_public_cloud_hostname() {
        assert!(parse_tables_endpoint("https://tables.cloud.skippr.io").is_err());
        assert!(parse_tables_endpoint("https://api.cloud.skippr.io").is_err());
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
    fn mesh_token_constructs_and_missing_token_fails() {
        std::env::remove_var("CLOUD_TABLES_ACCESS_TOKEN");
        std::env::remove_var("CLOUD_ACCESS_TOKEN");
        assert!(CloudCredentials::from_env_for_endpoint("http://127.0.0.1:8003").is_err());
        std::env::set_var("CLOUD_TABLES_ACCESS_TOKEN", "mesh-jwt");
        assert!(CloudCredentials::from_env_for_endpoint("http://127.0.0.1:8003").is_ok());
        assert!(CloudCredentials::from_env_for_endpoint("https://tables.cloud.skippr.io").is_err());
        std::env::remove_var("CLOUD_TABLES_ACCESS_TOKEN");
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
        std::env::set_var("CLOUD_TABLES_ACCESS_TOKEN", "test-jwt");
        let client = TablesClient::new(
            format!("http://{addr}"),
            CloudCredentials {
                access_key_id: "AKIATEST".into(),
                secret_access_key: String::new(),
                session_token: None,
            },
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
