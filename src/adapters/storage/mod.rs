use crate::helpers::configuration::Config;
#[cfg(test)]
mod in_memory;
mod local_disk;
mod s3;

#[cfg(test)]
pub use in_memory::InMemoryStorageAdapter;
pub use local_disk::LocalDiskStorageAdapter;
pub use s3::S3StorageAdapter;

use async_trait::async_trait;
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

pub fn get_storage(config: &Config) -> Arc<dyn StorageAdapter> {
    let mode = config.get_storage_mode();
    if mode == crate::helpers::wal_storage::ElStorageMode::Local {
        Arc::new(LocalDiskStorageAdapter::new(&config.get_data_dir()))
    } else {
        Arc::new(S3StorageAdapter::new(config.get_skippr_s3_bucket()))
    }
}
