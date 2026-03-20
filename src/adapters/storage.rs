use async_trait::async_trait;
use once_cell::sync::OnceCell;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Storage adapter interface.
///
/// Implementations route to either S3 or local disk based on
/// `SKIPPR_STORAGE_MODE`.
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    async fn get_json(&self, key: &str) -> Result<Value, String>;
    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String>;

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String>;
    async fn put_bytes(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), String>;

    async fn delete_object(&self, key: &str) -> Result<(), String>;
    async fn head_etag(&self, key: &str) -> Result<Option<String>, String>;

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, String>;
    async fn delete_prefix(&self, prefix: &str) -> Result<usize, String>;

    /// Returns `Ok(None)` when the key does not exist, `Ok(Some(v))` when it
    /// does, and `Err` only for real failures.
    async fn get_json_opt(&self, key: &str) -> Result<Option<Value>, String> {
        match self.get_json(key).await {
            Ok(v) => Ok(Some(v)),
            Err(e)
                if e.contains("not found")
                    || e.contains("NotFound")
                    || e.contains("NoSuchKey")
                    || e.contains("No such file") =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    async fn get_bytes_opt(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        match self.get_bytes(key).await {
            Ok(v) => Ok(Some(v)),
            Err(e)
                if e.contains("not found")
                    || e.contains("NotFound")
                    || e.contains("NoSuchKey")
                    || e.contains("No such file") =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Global accessor — returns the storage adapter matching SKIPPR_STORAGE_MODE
// ---------------------------------------------------------------------------

static STORAGE: OnceCell<Arc<dyn StorageAdapter>> = OnceCell::new();

pub fn get_storage() -> Arc<dyn StorageAdapter> {
    STORAGE
        .get_or_init(|| {
            let mode = crate::helpers::configuration::Config::get_storage_mode();
            if mode == "local" {
                let data_dir = crate::helpers::configuration::Config::get_data_dir();
                Arc::new(LocalDiskStorageAdapter::new(&data_dir))
            } else {
                Arc::new(S3StorageAdapter)
            }
        })
        .clone()
}

// ---------------------------------------------------------------------------
// S3 backend
// ---------------------------------------------------------------------------

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

    async fn delete_prefix(&self, prefix: &str) -> Result<usize, String> {
        crate::helpers::s3::delete_prefix(prefix).await
    }
}

// ---------------------------------------------------------------------------
// Local-disk backend
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LocalDiskStorageAdapter {
    root: String,
}

impl LocalDiskStorageAdapter {
    pub fn new(root: &str) -> Self {
        Self {
            root: root.to_string(),
        }
    }

    fn resolve(&self, key: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.root).join(key)
    }

    fn ensure_parent(path: &std::path::Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create_dir_all {:?}: {}", parent, e))?;
        }
        Ok(())
    }
}

#[async_trait]
impl StorageAdapter for LocalDiskStorageAdapter {
    async fn get_json(&self, key: &str) -> Result<Value, String> {
        let path = self.resolve(key);
        let contents =
            std::fs::read_to_string(&path).map_err(|e| format!("read {:?}: {}", path, e))?;
        serde_json::from_str(&contents).map_err(|e| format!("parse {:?}: {}", path, e))
    }

    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String> {
        let path = self.resolve(key);
        Self::ensure_parent(&path)?;
        let tmp = path.with_extension("json.tmp");
        let json_str =
            serde_json::to_string_pretty(value).map_err(|e| format!("serialize: {}", e))?;
        std::fs::write(&tmp, &json_str).map_err(|e| format!("write {:?}: {}", tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("rename {:?} -> {:?}: {}", tmp, path, e))
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String> {
        let path = self.resolve(key);
        std::fs::read(&path).map_err(|e| format!("read {:?}: {}", path, e))
    }

    async fn put_bytes(&self, key: &str, bytes: &[u8], _content_type: &str) -> Result<(), String> {
        let path = self.resolve(key);
        Self::ensure_parent(&path)?;
        std::fs::write(&path, bytes).map_err(|e| format!("write {:?}: {}", path, e))
    }

    async fn delete_object(&self, key: &str) -> Result<(), String> {
        let path = self.resolve(key);
        match std::fs::remove_file(&path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {:?}: {}", path, e)),
        }
    }

    async fn head_etag(&self, key: &str) -> Result<Option<String>, String> {
        let path = self.resolve(key);
        match std::fs::metadata(&path) {
            Ok(m) => Ok(Some(format!("local-{}", m.len()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("stat {:?}: {}", path, e)),
        }
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, String> {
        let base = self.resolve(prefix);
        let dir = if base.is_dir() {
            base.clone()
        } else {
            base.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from(&self.root))
        };
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        fn walk(dir: &std::path::Path, root: &str, prefix: &str, out: &mut Vec<String>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    if path.is_dir() {
                        walk(&path, root, prefix, out);
                    } else if rel.starts_with(prefix) {
                        out.push(rel);
                    }
                }
            }
        }
        walk(&std::path::PathBuf::from(&self.root), &self.root, prefix, &mut out);
        out.sort();
        Ok(out)
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<usize, String> {
        let base = self.resolve(prefix);
        if !base.exists() {
            return Ok(0);
        }
        if base.is_dir() {
            let mut count = 0usize;
            fn rm_recursive(dir: &std::path::Path, count: &mut usize) -> Result<(), String> {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            rm_recursive(&path, count)?;
                            let _ = std::fs::remove_dir(&path);
                        } else {
                            std::fs::remove_file(&path)
                                .map_err(|e| format!("rm {:?}: {}", path, e))?;
                            *count += 1;
                        }
                    }
                }
                Ok(())
            }
            rm_recursive(&base, &mut count)?;
            let _ = std::fs::remove_dir(&base);
            Ok(count)
        } else {
            let keys = self.list_prefix(prefix).await?;
            for k in &keys {
                let _ = self.delete_object(k).await;
            }
            Ok(keys.len())
        }
    }
}

// ---------------------------------------------------------------------------
// In-memory backend (for tests)
// ---------------------------------------------------------------------------

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
