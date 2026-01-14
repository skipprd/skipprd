use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[async_trait]
pub trait StorageAdapter: Send + Sync {
    async fn get_json(&self, key: &str) -> Result<Value, String>;
    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String>;

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String>;
    async fn put_bytes(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), String>;

    async fn delete_object(&self, key: &str) -> Result<(), String>;
    async fn head_etag(&self, key: &str) -> Result<Option<String>, String>;

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, String>;
}

/// Simple in-memory storage adapter for tests and local runs.
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
        let mut out: Vec<String> = g.keys().filter(|k| k.starts_with(prefix)).cloned().collect();
        out.sort();
        Ok(out)
    }
}

/// S3 storage adapter (used by ReAct WS deployments).
#[derive(Clone)]
pub struct S3StorageAdapter {
    pub bucket: String,
    client: aws_sdk_s3::Client,
}

impl S3StorageAdapter {
    pub async fn from_env(bucket: String) -> Self {
        let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let client = aws_sdk_s3::Client::new(&aws_cfg);
        Self { bucket, client }
    }
}

#[async_trait]
impl StorageAdapter for S3StorageAdapter {
    async fn get_json(&self, key: &str) -> Result<Value, String> {
        let bytes = self.get_bytes(key).await?;
        serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())
    }

    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        self.put_bytes(key, &bytes, "application/json").await
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| format!("{:?}", e))?;
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| format!("{:?}", e))?
            .into_bytes();
        Ok(bytes.to_vec())
    }

    async fn put_bytes(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), String> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .body(aws_sdk_s3::primitives::ByteStream::from(bytes.to_vec()))
            .send()
            .await
            .map_err(|e| format!("{:?}", e))?;
        Ok(())
    }

    async fn delete_object(&self, key: &str) -> Result<(), String> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| format!("{:?}", e))?;
        Ok(())
    }

    async fn head_etag(&self, key: &str) -> Result<Option<String>, String> {
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| format!("{:?}", e))?;
        Ok(resp.e_tag().map(|s| s.to_string()))
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, String> {
        let mut token: Option<String> = None;
        let mut out: Vec<String> = Vec::new();
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .max_keys(1000);
            if let Some(t) = token.as_ref() {
                req = req.continuation_token(t);
            }
            let resp = req.send().await.map_err(|e| format!("{:?}", e))?;
            for obj in resp.contents() {
                if let Some(k) = obj.key() {
                    out.push(k.to_string());
                }
            }
            token = resp.next_continuation_token().map(|s| s.to_string());
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

