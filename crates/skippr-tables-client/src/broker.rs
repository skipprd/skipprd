//! Cloud tables plugin auth: mint a mesh JWT from the host-injected broker drive.
//!
//! skipprd owns this HTTP client. It MUST NOT cargo-depend on Cloud crates
//! (`guest-broker`, `fleet-spec`, …). Cloud deploys skipprd and injects
//! `CLOUD_SYSTEM_BROKER_CONFIG` / `broker.json`; skipprd POSTs CatalogJwt.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::TablesClientError;

const DEFAULT_BROKER_DIR: &str = "/run/cloud/broker";
const BROKER_MINT_TIMEOUT: Duration = Duration::from_secs(5);
const SKIPPR_AUTHORITY: &str = "skippr_system";

/// Wire-compatible with Cloud `guest-broker::SystemBrokerRequest`.
/// This crate MUST NOT cargo-depend on Cloud crates.
#[derive(Serialize)]
struct CatalogJwtMintRequest<'a> {
    capability: &'a str,
    op: SystemBrokerOp,
    body: Option<()>,
}

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum SystemBrokerOp {
    Platform(PlatformBrokerOp),
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PlatformBrokerOp {
    CatalogJwt { authority: &'static str },
}

#[derive(Clone, Debug)]
pub(crate) struct BrokerMint {
    endpoint: String,
    capability: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SkipprBrokerProfile {
    Skippr,
}

#[derive(Debug, Deserialize)]
struct SkipprBrokerDrive {
    profile: SkipprBrokerProfile,
    broker_endpoint: String,
    capability: String,
}

#[derive(Debug, Deserialize)]
struct BrokerMintResponse {
    status: u16,
    body: BrokerMintBody,
}

#[derive(Debug, Deserialize)]
struct BrokerMintBody {
    token: String,
}

impl BrokerMint {
    pub(crate) fn from_env() -> Result<Self, TablesClientError> {
        let dir = std::env::var("CLOUD_SYSTEM_BROKER_CONFIG")
            .unwrap_or_else(|_| DEFAULT_BROKER_DIR.into());
        Self::from_dir(Path::new(&dir))
    }

    pub(crate) fn from_dir(dir: &Path) -> Result<Self, TablesClientError> {
        let path = dir.join("broker.json");
        let raw = std::fs::read_to_string(&path).map_err(|err| {
            TablesClientError::msg(format!(
                "clustered Cloud tables requires GuestCredentialBroker (CLOUD_SYSTEM_BROKER_CONFIG): {err}"
            ))
        })?;
        parse_skippr_broker_drive(&raw)
    }

    pub(crate) async fn mint_platform_jwt(&self) -> Result<String, TablesClientError> {
        let url = format!("{}/v1/system-broker", self.endpoint.trim_end_matches('/'));
        let request = CatalogJwtMintRequest {
            capability: &self.capability,
            op: SystemBrokerOp::Platform(PlatformBrokerOp::CatalogJwt {
                authority: SKIPPR_AUTHORITY,
            }),
            body: None,
        };
        let http = reqwest::Client::builder()
            .timeout(BROKER_MINT_TIMEOUT)
            .build()
            .map_err(|err| TablesClientError::msg(format!("broker mint client: {err}")))?;
        let response = http
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|err| TablesClientError::msg(format!("broker mint: {err}")))?;
        let http_status = response.status().as_u16();
        let payload = response
            .bytes()
            .await
            .map_err(|err| TablesClientError::msg(format!("broker mint body: {err}")))?;
        let parsed: BrokerMintResponse = serde_json::from_slice(&payload)
            .map_err(|err| TablesClientError::msg(format!("broker mint protocol: {err}")))?;
        if http_status >= 400 || parsed.status >= 400 {
            return Err(TablesClientError::msg(format!(
                "broker status {}",
                http_status.max(parsed.status)
            )));
        }
        let token = parsed.body.token.trim();
        if token.is_empty() {
            return Err(TablesClientError::msg("broker mint missing token"));
        }
        Ok(token.to_string())
    }
}

pub(crate) fn parse_skippr_broker_drive(raw: &str) -> Result<BrokerMint, TablesClientError> {
    let drive: SkipprBrokerDrive = serde_json::from_str(raw)
        .map_err(|err| TablesClientError::msg(format!("invalid broker.json: {err}")))?;
    let SkipprBrokerDrive {
        profile: SkipprBrokerProfile::Skippr,
        broker_endpoint,
        capability,
    } = drive;
    let endpoint = broker_endpoint.trim();
    let capability = capability.trim();
    if endpoint.is_empty() {
        return Err(TablesClientError::msg(
            "broker.json missing broker_endpoint",
        ));
    }
    if capability.is_empty() {
        return Err(TablesClientError::msg("broker.json missing capability"));
    }
    Ok(BrokerMint {
        endpoint: endpoint.trim_end_matches('/').to_string(),
        capability: capability.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn parse_requires_skippr_profile_endpoint_and_capability() {
        assert!(parse_skippr_broker_drive("{}").is_err());
        assert!(parse_skippr_broker_drive(
            r#"{"profile":"secrets","broker_endpoint":"http://127.0.0.1:8097","capability":"opaque"}"#
        )
        .is_err());
        assert!(parse_skippr_broker_drive(
            r#"{"profile":"skippr","broker_endpoint":"http://127.0.0.1:8097"}"#
        )
        .is_err());
        assert!(parse_skippr_broker_drive(
            r#"{"profile":"skippr","broker_endpoint":"","capability":"opaque"}"#
        )
        .is_err());
        let mint = parse_skippr_broker_drive(
            r#"{
  "profile": "skippr",
  "guest_id": "skippr-0",
  "cluster_generation": 1,
  "guest_incarnation": "inc-1",
  "broker_endpoint": "http://127.0.0.1:8097/",
  "capability": "opaque"
}"#,
        )
        .unwrap();
        assert_eq!(mint.endpoint, "http://127.0.0.1:8097");
        assert_eq!(mint.capability, "opaque");
    }

    #[test]
    fn catalog_jwt_mint_request_matches_guest_broker_wire() {
        let request = CatalogJwtMintRequest {
            capability: "opaque",
            op: SystemBrokerOp::Platform(PlatformBrokerOp::CatalogJwt {
                authority: SKIPPR_AUTHORITY,
            }),
            body: None,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "capability": "opaque",
                "op": {
                    "op": "platform",
                    "kind": "catalog_jwt",
                    "authority": "skippr_system"
                },
                "body": null
            })
        );
    }

    async fn serve_once(body: &str, status_line: &str) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status_line.to_string();
        let body = body.to_string();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!(
                "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            req
        });
        (addr.port(), server)
    }

    #[tokio::test]
    async fn mint_fails_closed_on_non_json() {
        let (port, server) = serve_once("not-json", "HTTP/1.1 200 OK").await;
        let mint = BrokerMint {
            endpoint: format!("http://127.0.0.1:{port}"),
            capability: "opaque".into(),
        };
        let err = mint.mint_platform_jwt().await.unwrap_err();
        let _ = server.await;
        assert!(
            err.to_string().contains("broker mint protocol"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn mint_fails_closed_on_missing_token() {
        let (port, server) = serve_once(r#"{"status":200,"body":{}}"#, "HTTP/1.1 200 OK").await;
        let mint = BrokerMint {
            endpoint: format!("http://127.0.0.1:{port}"),
            capability: "opaque".into(),
        };
        let err = mint.mint_platform_jwt().await.unwrap_err();
        let _ = server.await;
        assert!(
            err.to_string().contains("broker mint protocol"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn mint_fails_closed_on_broker_status() {
        let (port, server) = serve_once(
            r#"{"status":403,"body":{"token":"nope"}}"#,
            "HTTP/1.1 200 OK",
        )
        .await;
        let mint = BrokerMint {
            endpoint: format!("http://127.0.0.1:{port}"),
            capability: "opaque".into(),
        };
        let err = mint.mint_platform_jwt().await.unwrap_err();
        let _ = server.await;
        assert!(err.to_string().contains("broker status 403"), "got {err}");
    }
}
