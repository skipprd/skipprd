use async_trait::async_trait;
use serde_json::Value;

use super::StorageAdapter;

pub struct S3StorageAdapter {
    bucket: String,
}

impl S3StorageAdapter {
    pub fn new(bucket: String) -> Self {
        Self { bucket }
    }
}

#[async_trait]
impl StorageAdapter for S3StorageAdapter {
    async fn get_json(&self, key: &str) -> Result<Value, String> {
        crate::helpers::s3::get_json(&self.bucket, key)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String> {
        crate::helpers::s3::put_json(&self.bucket, key, value)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String> {
        crate::helpers::s3::get_bytes(&self.bucket, key)
            .await
            .map_err(|e| format!("{:?}", e))
            .map(|b| b.to_vec())
    }

    async fn put_bytes(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), String> {
        crate::helpers::s3::put_bytes(&self.bucket, key, bytes, content_type)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn delete_object(&self, key: &str) -> Result<(), String> {
        crate::helpers::s3::delete_object(&self.bucket, key)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn head_etag(&self, key: &str) -> Result<Option<String>, String> {
        crate::helpers::s3::head_etag(&self.bucket, key)
            .await
            .map_err(|e| format!("{:?}", e))
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, String> {
        let client = crate::helpers::s3::get_s3_client().await;
        let mut token: Option<String> = None;
        let mut out: Vec<String> = Vec::new();
        loop {
            let mut req = client
                .list_objects_v2()
                .bucket(&self.bucket)
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
        crate::helpers::s3::delete_prefix(&self.bucket, prefix).await
    }
}
