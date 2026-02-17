use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Storage adapter interface.
///
/// Initial implementation is S3-backed by delegating to `crate::helpers::s3`.
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    async fn get_json(&self, key: &str) -> Result<Value, String>;
    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String>;

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String>;
    async fn put_bytes(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), String>;

    async fn delete_object(&self, key: &str) -> Result<(), String>;
    async fn head_etag(&self, key: &str) -> Result<Option<String>, String>;

    /// List object keys under a prefix (best-effort).
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, String>;
}

#[derive(Clone, Default)]
pub struct S3StorageAdapter;

#[async_trait]
impl StorageAdapter for S3StorageAdapter {
    async fn get_json(&self, key: &str) -> Result<Value, String> {
        crate::helpers::s3::get_json(key)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String> {
        crate::helpers::s3::put_json(key, value)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String> {
        crate::helpers::s3::get_bytes(key)
            .await
            .map_err(|e| format!("{:?}", e))
            .map(|b| b.to_vec())
    }

    async fn put_bytes(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), String> {
        crate::helpers::s3::put_bytes(key, bytes, content_type)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn delete_object(&self, key: &str) -> Result<(), String> {
        crate::helpers::s3::delete_object(key)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn head_etag(&self, key: &str) -> Result<Option<String>, String> {
        crate::helpers::s3::head_etag(key)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, String> {
        let bucket = crate::helpers::configuration::Config::get_skippr_s3_bucket();
        let client = crate::helpers::s3::get_s3_client().await;
        let mut token: Option<String> = None;
        let mut out: Vec<String> = Vec::new();
        loop {
            let mut req = client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(prefix)
                .max_keys(1000);
            if let Some(t) = token.as_ref() {
                req = req.continuation_token(t);
            }
            match req.send().await {
                Ok(resp) => {
                    for obj in resp.contents() {
                        if let Some(k) = obj.key() {
                            out.push(k.to_string());
                        }
                    }
                    if resp.next_continuation_token().is_none() {
                        break;
                    }
                    token = resp.next_continuation_token().map(|s| s.to_string());
                }
                Err(e) => return Err(format!("{:?}", e)),
            }
        }
        Ok(out)
    }
}

/// Simple in-memory storage adapter.
///
/// Useful for tests, local runs, and ephemeral deployments.
#[derive(Clone, Default)]
pub struct InMemoryStorageAdapter {
    inner: Arc<RwLock<HashMap<String, StoredObject>>>,
}

#[derive(Clone, Debug)]
struct StoredObject {
    bytes: Vec<u8>,
    content_type: String,
    etag: String,
}

impl InMemoryStorageAdapter {
    fn next_etag(bytes: &[u8]) -> String {
        // Deterministic-enough for tests; avoids hashing deps.
        format!("mem-etag-{}", bytes.len())
    }
}

#[async_trait]
impl StorageAdapter for InMemoryStorageAdapter {
    async fn get_json(&self, key: &str) -> Result<Value, String> {
        let bytes = self.get_bytes(key).await?;
        serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())
    }

    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        self.put_bytes(key, &bytes, "application/json").await
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String> {
        let g = self
            .inner
            .read()
            .map_err(|_| "storage lock poisoned".to_string())?;
        g.get(key)
            .map(|o| o.bytes.clone())
            .ok_or_else(|| "not found".to_string())
    }

    async fn put_bytes(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), String> {
        let mut g = self
            .inner
            .write()
            .map_err(|_| "storage lock poisoned".to_string())?;
        g.insert(
            key.to_string(),
            StoredObject {
                bytes: bytes.to_vec(),
                content_type: content_type.to_string(),
                etag: Self::next_etag(bytes),
            },
        );
        Ok(())
    }

    async fn delete_object(&self, key: &str) -> Result<(), String> {
        let mut g = self
            .inner
            .write()
            .map_err(|_| "storage lock poisoned".to_string())?;
        g.remove(key);
        Ok(())
    }

    async fn head_etag(&self, key: &str) -> Result<Option<String>, String> {
        let g = self
            .inner
            .read()
            .map_err(|_| "storage lock poisoned".to_string())?;
        Ok(g.get(key).map(|o| o.etag.clone()))
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, String> {
        let g = self
            .inner
            .read()
            .map_err(|_| "storage lock poisoned".to_string())?;
        let mut out: Vec<String> = g
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }
}
