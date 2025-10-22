use aws_sdk_s3::{Client as S3Client, Error as S3Error};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::delete_object::DeleteObjectError;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::OnceCell;
use crate::helpers::configuration::Config;
use rand::{thread_rng, Rng};
use std::time::Duration;

static S3_CLIENT: OnceCell<Arc<S3Client>> = OnceCell::const_new();

pub async fn get_s3_client() -> Arc<S3Client> {
    S3_CLIENT
        .get_or_init(|| async {
            let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
            Arc::new(S3Client::new(&aws_config))
        })
        .await
        .clone()
}

fn get_skippr_bucket() -> String { Config::get_skippr_s3_bucket() }

pub async fn put_json(key: &str, value: &Value) -> Result<(), S3Error> {
    let client = get_s3_client().await;
    let bucket = get_skippr_bucket();
    let body = serde_json::to_vec(value).unwrap();

    // Simple retry with exponential backoff and jitter for transient throttling
    let mut attempt: u32 = 0;
    let max_attempts: u32 = 6;
    loop {
        match client
            .put_object()
            .bucket(&bucket)
            .key(key)
            .body(ByteStream::from(body.clone()))
            .send()
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                attempt += 1;
                if attempt >= max_attempts {
                    println!("Failed to upload metrics to S3 after {} attempts: {:?}", attempt, e);
                    return Err(e.into());
                }
                // backoff 200ms * 2^attempt with jitter up to 100ms
                let base = 200u64.saturating_mul(1u64 << attempt.min(10));
                let jitter: u64 = thread_rng().gen_range(0..100);
                let sleep_ms = (base + jitter).min(5_000);
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            }
        }
    }
}

pub async fn get_json(key: &str) -> Result<Value, SdkError<GetObjectError>> {
    let client = get_s3_client().await;
    let bucket = get_skippr_bucket();

    // Retry non-404 errors with backoff; return 404 immediately
    let mut attempt: u32 = 0;
    let max_attempts: u32 = 6;
    loop {
        let res = client.get_object().bucket(&bucket).key(key).send().await;
        match res {
            Ok(resp) => {
                let bytes = resp.body.collect().await.unwrap().into_bytes();
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                return Ok(value);
            }
            Err(e) => {
                // If the error is a 404, return immediately
                if let SdkError::ServiceError(se) = &e {
                    if se.err().is_no_such_key() { return Err(e); }
                }
                attempt += 1;
                if attempt >= max_attempts { return Err(e); }
                let base = 200u64.saturating_mul(1u64 << attempt.min(10));
                let jitter: u64 = thread_rng().gen_range(0..100);
                let sleep_ms = (base + jitter).min(5_000);
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            }
        }
    }
}

pub async fn delete_object(key: &str) -> Result<(), SdkError<DeleteObjectError>> {
    let client = get_s3_client().await;
    let bucket = get_skippr_bucket();
    client.delete_object().bucket(bucket).key(key).send().await?;
    Ok(())
}

pub async fn delete_prefix(prefix: &str) -> Result<usize, String> {
    use aws_sdk_s3::types::{Delete, ObjectIdentifier};
    let client = get_s3_client().await;
    let bucket = get_skippr_bucket();
    // Safety checks
    if prefix.is_empty() || prefix == "/" || prefix == "." || prefix == ".." {
        return Err("Refusing to delete unsafe or empty prefix".to_string());
    }
    // Disallow top-level bucket wipes
    if !prefix.contains('/') { return Err("Refusing to delete top-level prefix without '/'".to_string()); }
    // Cap total deletes per invocation to avoid runaway operations
    let max_objects: usize = 50_000;
    let mut token: Option<String> = None;
    let mut total: usize = 0;
    loop {
        let mut req = client.list_objects_v2().bucket(&bucket).prefix(prefix).max_keys(1000);
        if let Some(t) = token.as_ref() { req = req.continuation_token(t); }
        let resp = req.send().await.map_err(|e| format!("list_objects_v2 error: {:?}", e))?;
        let keys: Vec<String> = resp
            .contents()
            .iter()
            .filter_map(|o| o.key().map(|s| s.to_string()))
            .collect();
        if keys.is_empty() {
            if resp.next_continuation_token().is_none() { break; }
        } else {
            let mut batch: Vec<ObjectIdentifier> = Vec::new();
            for k in keys.iter() {
                if let Ok(obj) = ObjectIdentifier::builder().key(k).build() { batch.push(obj); }
                if batch.len() == 1000 { // S3 DeleteObjects limit
                    let del = Delete::builder().set_objects(Some(batch)).build().map_err(|e| format!("delete build error: {:?}", e))?;
                    client.delete_objects().bucket(&bucket).delete(del).send().await.map_err(|e| format!("delete_objects error: {:?}", e))?;
                    total += 1000;
                    batch = Vec::new();
                    if total >= max_objects { break; }
                }
            }
            if !batch.is_empty() {
                let del = Delete::builder().set_objects(Some(batch)).build().map_err(|e| format!("delete build error: {:?}", e))?;
                client.delete_objects().bucket(&bucket).delete(del).send().await.map_err(|e| format!("delete_objects error: {:?}", e))?;
                total += keys.len() % 1000;
            }
        }
        if total >= max_objects { break; }
        if resp.next_continuation_token().is_none() { break; }
        token = resp.next_continuation_token().map(|s| s.to_string());
    }
    Ok(total)
}

pub async fn delete_prefix_in_bucket(bucket: &str, prefix: &str) -> Result<usize, String> {
    use aws_sdk_s3::types::{Delete, ObjectIdentifier};
    let client = get_s3_client().await;
    // Safety checks
    if bucket.is_empty() { return Err("Bucket required".to_string()); }
    if prefix.is_empty() || prefix == "/" || prefix == "." || prefix == ".." {
        return Err("Refusing to delete unsafe or empty prefix".to_string());
    }
    if !prefix.contains('/') { return Err("Refusing to delete top-level prefix without '/'".to_string()); }
    let max_objects: usize = 50_000;
    let mut token: Option<String> = None;
    let mut total: usize = 0;
    loop {
        let mut req = client.list_objects_v2().bucket(bucket).prefix(prefix).max_keys(1000);
        if let Some(t) = token.as_ref() { req = req.continuation_token(t); }
        let resp = req.send().await.map_err(|e| format!("list_objects_v2 error: {:?}", e))?;
        let keys: Vec<String> = resp
            .contents()
            .iter()
            .filter_map(|o| o.key().map(|s| s.to_string()))
            .collect();
        if keys.is_empty() {
            if resp.next_continuation_token().is_none() { break; }
        } else {
            let mut batch: Vec<ObjectIdentifier> = Vec::new();
            for k in keys.iter() {
                if let Ok(obj) = ObjectIdentifier::builder().key(k).build() { batch.push(obj); }
                if batch.len() == 1000 {
                    let del = Delete::builder().set_objects(Some(batch)).build().map_err(|e| format!("delete build error: {:?}", e))?;
                    client.delete_objects().bucket(bucket).delete(del).send().await.map_err(|e| format!("delete_objects error: {:?}", e))?;
                    total += 1000;
                    batch = Vec::new();
                    if total >= max_objects { break; }
                }
            }
            if !batch.is_empty() {
                let del = Delete::builder().set_objects(Some(batch)).build().map_err(|e| format!("delete build error: {:?}", e))?;
                client.delete_objects().bucket(bucket).delete(del).send().await.map_err(|e| format!("delete_objects error: {:?}", e))?;
                total += keys.len() % 1000;
            }
        }
        if total >= max_objects { break; }
        if resp.next_continuation_token().is_none() { break; }
        token = resp.next_continuation_token().map(|s| s.to_string());
    }
    Ok(total)
}

/// List up to `max` Parquet object keys under the given bucket+prefix, ordered by LastModified ascending
pub async fn list_parquet_keys(bucket: &str, prefix: &str, max: usize) -> Vec<String> {
    let client = get_s3_client().await;
    let mut out: Vec<(String, i64)> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut req = client.list_objects_v2().bucket(bucket).prefix(prefix).max_keys(1000);
        if let Some(t) = token.as_ref() { req = req.continuation_token(t); }
        match req.send().await {
            Ok(resp) => {
                let contents = resp.contents();
                for obj in contents {
                    if let Some(k) = obj.key() {
                        if k.ends_with(".parquet") { out.push((k.to_string(), obj.last_modified().map(|t| t.secs()).unwrap_or_default())); }
                    }
                }
                if resp.next_continuation_token().is_none() { break; }
                token = resp.next_continuation_token().map(|s| s.to_string());
            }
            Err(_) => { break; }
        }
        if out.len() >= max { break; }
    }
    out.sort_by_key(|(_, ts)| *ts);
    out.into_iter().map(|(k, _)| k).take(max).collect()
}