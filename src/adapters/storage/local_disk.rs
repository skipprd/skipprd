use async_trait::async_trait;
use serde_json::Value;

use super::StorageAdapter;

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
                        .replace('\\', "/");
                    if path.is_dir() {
                        walk(&path, root, prefix, out);
                    } else if rel.starts_with(prefix) {
                        out.push(rel);
                    }
                }
            }
        }
        walk(
            &std::path::PathBuf::from(&self.root),
            &self.root,
            prefix,
            &mut out,
        );
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
