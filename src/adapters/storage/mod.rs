#[cfg(test)]
mod in_memory;
mod local_disk;
mod s3;

#[cfg(test)]
pub use in_memory::InMemoryStorageAdapter;
pub use local_disk::LocalDiskStorageAdapter;
pub use s3::S3StorageAdapter;

use async_trait::async_trait;
use once_cell::sync::OnceCell;
use serde_json::Value;
use std::sync::Arc;

fn is_not_found_error(e: &str) -> bool {
    e.contains("not found")
        || e.contains("NotFound")
        || e.contains("NoSuchKey")
        || e.contains("No such file")
        || e.contains("cannot find")
        || e.contains("os error 2")
        || e.contains("os error 3")
}

/// Storage adapter interface.
///
/// Implementations route to either S3 or local disk based on
/// `SKIPPRD_EL_STORAGE_MODE`.
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
            Err(e) if is_not_found_error(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_bytes_opt(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        match self.get_bytes(key).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if is_not_found_error(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

static STORAGE: OnceCell<Arc<dyn StorageAdapter>> = OnceCell::new();

/// Clustered WAL is per-node; extract/load objects (`metadata.json`) are cluster
/// SoT, same as S3. Local clustered processes on one host share a sibling dir.
pub(crate) fn clustered_local_storage_root(data_dir: &str) -> String {
    std::path::Path::new(data_dir)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.join("el-storage").to_string_lossy().into_owned())
        .unwrap_or_else(|| data_dir.to_string())
}

pub fn get_storage() -> Arc<dyn StorageAdapter> {
    STORAGE
        .get_or_init(|| {
            let mode = crate::helpers::configuration::Config::get_storage_mode();
            if mode == "local" {
                let data_dir = crate::helpers::configuration::Config::get_pipeline_data_dir();
                let root = if crate::helpers::configuration::Config::wal_storage_raw()
                    .eq_ignore_ascii_case("clustered")
                {
                    clustered_local_storage_root(&data_dir)
                } else {
                    crate::helpers::configuration::Config::get_data_dir()
                };
                Arc::new(LocalDiskStorageAdapter::new(&root))
            } else {
                Arc::new(S3StorageAdapter)
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::clustered_local_storage_root;

    #[test]
    fn clustered_local_storage_is_shared_sibling_of_node_data_dir() {
        assert_eq!(
            clustered_local_storage_root("/tmp/skippr-hla-e2e/node1"),
            "/tmp/skippr-hla-e2e/el-storage"
        );
        assert_eq!(
            clustered_local_storage_root("/tmp/skippr-hla-e2e/node2"),
            clustered_local_storage_root("/tmp/skippr-hla-e2e/query")
        );
    }
}
