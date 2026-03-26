use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::StorageAdapter;

#[derive(Clone, Default)]
pub struct InMemoryStorageAdapter {
    inner: Arc<RwLock<HashMap<String, StoredObject>>>,
}

#[derive(Clone, Debug)]
struct StoredObject {
    bytes: Vec<u8>,
    #[allow(dead_code)]
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
        let mut out: Vec<String> = g
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<usize, String> {
        let keys = self.list_prefix(prefix).await?;
        let count = keys.len();
        let mut g = self
            .inner
            .write()
            .map_err(|_| "storage lock poisoned".to_string())?;
        for k in keys {
            g.remove(&k);
        }
        Ok(count)
    }
}
