use async_trait::async_trait;
use serde_json::Value;

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
}

