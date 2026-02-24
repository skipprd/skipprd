use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::UNIX_EPOCH;

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
        let mut out: Vec<String> = g
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }
}

/// Local filesystem-backed storage adapter.
///
/// Keys are treated as **relative** POSIX-like paths (e.g. `tenant/workspace/project/threads/id.json`)
/// and are stored under `base_dir/<key>`.
#[derive(Clone, Debug)]
pub struct LocalFileStorageAdapter {
    base_dir: Arc<PathBuf>,
}

impl LocalFileStorageAdapter {
    pub fn new(base_dir: impl Into<PathBuf>) -> Result<Self, String> {
        let p: PathBuf = base_dir.into();
        std::fs::create_dir_all(&p).map_err(|e| format!("failed to create base dir: {e}"))?;
        Ok(Self {
            base_dir: Arc::new(p),
        })
    }

    fn resolve_key(&self, key: &str) -> Result<PathBuf, String> {
        let k = key.trim().trim_start_matches('/');
        if k.is_empty() {
            return Err("empty key".to_string());
        }
        let mut out = (*self.base_dir).clone();
        for seg in k.split('/') {
            if seg.is_empty() {
                return Err("invalid key (empty path segment)".to_string());
            }
            if seg == "." || seg == ".." || seg.contains("..") {
                return Err("invalid key (path traversal)".to_string());
            }
            if seg.contains('\\') {
                return Err("invalid key (backslash)".to_string());
            }
            out.push(seg);
        }
        Ok(out)
    }

    fn rel_key(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(self.base_dir.as_ref()).ok()?;
        let s = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    fn list_files_recursive(&self, dir: &Path, out: &mut Vec<String>) {
        let rd = match std::fs::read_dir(dir) {
            Ok(v) => v,
            Err(_) => return,
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                self.list_files_recursive(&p, out);
            } else if p.is_file() {
                if let Some(k) = self.rel_key(&p) {
                    out.push(k);
                }
            }
        }
    }

    fn etag_for_metadata(meta: &std::fs::Metadata) -> String {
        let len = meta.len();
        let mt = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("fs-etag-{}-{}", len, mt)
    }
}

#[async_trait]
impl StorageAdapter for LocalFileStorageAdapter {
    async fn get_json(&self, key: &str) -> Result<Value, String> {
        let bytes = self.get_bytes(key).await?;
        serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())
    }

    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
        self.put_bytes(key, &bytes, "application/json").await
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String> {
        let path = self.resolve_key(key)?;
        tokio::task::spawn_blocking(move || std::fs::read(&path).map_err(|e| e.to_string()))
            .await
            .map_err(|e| e.to_string())?
    }

    async fn put_bytes(&self, key: &str, bytes: &[u8], _content_type: &str) -> Result<(), String> {
        let path = self.resolve_key(key)?;
        let b = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, &b).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn delete_object(&self, key: &str) -> Result<(), String> {
        let path = self.resolve_key(key)?;
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn head_etag(&self, key: &str) -> Result<Option<String>, String> {
        let path = self.resolve_key(key)?;
        tokio::task::spawn_blocking(move || match std::fs::metadata(&path) {
            Ok(meta) => Ok(Some(Self::etag_for_metadata(&meta))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, String> {
        let pref = prefix.trim().trim_start_matches('/').to_string();
        let adapter = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut keys: Vec<String> = Vec::new();

            // Prefer walking from the directory implied by the prefix when possible.
            let pref_path = PathBuf::from(&pref);
            let start = adapter.base_dir.as_ref().join(&pref_path);
            let (walk_root, filter_prefix) = if start.is_dir() {
                (start, pref.clone())
            } else {
                let parent = pref_path.parent().unwrap_or(Path::new(""));
                (adapter.base_dir.as_ref().join(parent), pref.clone())
            };

            adapter.list_files_recursive(&walk_root, &mut keys);
            keys.retain(|k| k.starts_with(&filter_prefix));
            keys.sort();
            Ok(keys)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("react-core-local-storage-{}", uuid::Uuid::new_v4()));
        p
    }

    #[tokio::test]
    async fn local_file_storage_put_get_delete_and_list_prefix() {
        let root = temp_root();
        let st = LocalFileStorageAdapter::new(root.clone()).expect("new");

        st.put_bytes("a/b/c.txt", b"hello", "text/plain")
            .await
            .expect("put");
        st.put_json("a/b/d.json", &serde_json::json!({"x": 1}))
            .await
            .expect("put_json");

        let b = st.get_bytes("a/b/c.txt").await.expect("get");
        assert_eq!(b, b"hello");

        let v = st.get_json("a/b/d.json").await.expect("get_json");
        assert_eq!(v.get("x").and_then(|x| x.as_i64()), Some(1));

        let et = st.head_etag("a/b/c.txt").await.expect("head");
        assert!(et.is_some());

        let keys = st.list_prefix("a/b/").await.expect("list");
        assert_eq!(
            keys,
            vec!["a/b/c.txt".to_string(), "a/b/d.json".to_string()]
        );

        st.delete_object("a/b/c.txt").await.expect("delete");
        assert!(st.head_etag("a/b/c.txt").await.unwrap().is_none());
        assert!(st.get_bytes("a/b/c.txt").await.is_err());

        // Best-effort cleanup
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn local_file_storage_rejects_traversal_keys() {
        let root = temp_root();
        let st = LocalFileStorageAdapter::new(root.clone()).expect("new");
        assert!(st.put_bytes("../x", b"nope", "text/plain").await.is_err());
        assert!(st.get_bytes("../x").await.is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
