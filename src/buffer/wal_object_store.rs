//! Object-store seam for the S3 WAL. Production talks to S3; tests inject a
//! scripted in-memory store. Timeouts are terminal: applied or not applied.

use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::CompletedPart;
use rand::{thread_rng, Rng};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;
use url::Url;

use crate::metrics::counters as metrics_counters;

const MPU_PART_SIZE: usize = 8 * 1024 * 1024;
const PUT_ATTEMPTS: u32 = 5;

/// Outcome of a PUT. `Unknown` is a timed-out or response-lost call: the
/// object may or may not exist. Persist MUST GET-admit this snapshot's pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PutOutcome {
    Applied,
    Unknown,
    DefiniteFailure(String),
}

/// Outcome of a GET used for timeout reconciliation and recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GetOutcome {
    Found(Vec<u8>),
    Missing,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListOutcome {
    Keys(Vec<String>),
    Failed(String),
}

#[async_trait]
pub trait WalObjectStore: Send + Sync {
    async fn put(&self, key: &str, body: Vec<u8>) -> PutOutcome;
    async fn get(&self, key: &str) -> GetOutcome;
    async fn delete(&self, key: &str) -> Result<(), String>;
    async fn list(&self, prefix: &str) -> ListOutcome;
}

/// In-memory store that always applies PUTs. Used as the reference backend.
#[derive(Default)]
pub struct MemoryObjectStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.objects
            .lock()
            .expect("object store poisoned")
            .contains_key(key)
    }

    pub fn get_bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.objects
            .lock()
            .expect("object store poisoned")
            .get(key)
            .cloned()
    }
}

#[async_trait]
impl WalObjectStore for MemoryObjectStore {
    async fn put(&self, key: &str, body: Vec<u8>) -> PutOutcome {
        self.objects
            .lock()
            .expect("object store poisoned")
            .insert(key.to_string(), body);
        PutOutcome::Applied
    }

    async fn get(&self, key: &str) -> GetOutcome {
        match self.get_bytes(key) {
            Some(bytes) => GetOutcome::Found(bytes),
            None => GetOutcome::Missing,
        }
    }

    async fn delete(&self, key: &str) -> Result<(), String> {
        self.objects
            .lock()
            .expect("object store poisoned")
            .remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> ListOutcome {
        let keys = self
            .objects
            .lock()
            .expect("object store poisoned")
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect();
        ListOutcome::Keys(keys)
    }
}

/// Scripted PUT/GET behavior for timeout and fail-closed tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptedPut {
    Apply,
    /// Persist the object but report Unknown (applied-with-lost-response).
    ApplyLostResponse,
    /// Do not persist; report Unknown (never-landed timeout).
    NeverLand,
    DefiniteFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptedGet {
    Live,
    FailOnceThenLive,
    FoundThenMissing,
    AlwaysFail,
    Missing,
}

#[derive(Default)]
pub struct ScriptedObjectStore {
    inner: MemoryObjectStore,
    puts: Mutex<BTreeMap<String, Vec<ScriptedPut>>>,
    gets: Mutex<BTreeMap<String, ScriptedGet>>,
    get_attempts: Mutex<BTreeMap<String, u32>>,
    list_fail: Mutex<Option<String>>,
}

impl ScriptedObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn script_put(&self, key: &str, outcomes: Vec<ScriptedPut>) {
        self.puts
            .lock()
            .expect("script poisoned")
            .insert(key.to_string(), outcomes);
    }

    pub fn script_get(&self, key: &str, script: ScriptedGet) {
        self.gets
            .lock()
            .expect("script poisoned")
            .insert(key.to_string(), script);
    }

    pub fn fail_list(&self, message: impl Into<String>) {
        *self.list_fail.lock().expect("script poisoned") = Some(message.into());
    }

    pub fn inner(&self) -> &MemoryObjectStore {
        &self.inner
    }

    fn next_put(&self, key: &str) -> ScriptedPut {
        let mut puts = self.puts.lock().expect("script poisoned");
        match puts.get_mut(key) {
            Some(queue) if !queue.is_empty() => queue.remove(0),
            _ => ScriptedPut::Apply,
        }
    }
}

#[async_trait]
impl WalObjectStore for ScriptedObjectStore {
    async fn put(&self, key: &str, body: Vec<u8>) -> PutOutcome {
        match self.next_put(key) {
            ScriptedPut::Apply => self.inner.put(key, body).await,
            ScriptedPut::ApplyLostResponse => {
                let _ = self.inner.put(key, body).await;
                PutOutcome::Unknown
            }
            ScriptedPut::NeverLand => PutOutcome::Unknown,
            ScriptedPut::DefiniteFailure => {
                PutOutcome::DefiniteFailure(format!("scripted definite failure for {key}"))
            }
        }
    }

    async fn get(&self, key: &str) -> GetOutcome {
        let script = self
            .gets
            .lock()
            .expect("script poisoned")
            .get(key)
            .copied()
            .unwrap_or(ScriptedGet::Live);
        match script {
            ScriptedGet::Live => self.inner.get(key).await,
            ScriptedGet::Missing => GetOutcome::Missing,
            ScriptedGet::AlwaysFail => GetOutcome::Unknown,
            ScriptedGet::FailOnceThenLive => {
                let first = {
                    let mut attempts = self.get_attempts.lock().expect("script poisoned");
                    let count = attempts.entry(key.to_string()).or_insert(0);
                    *count = count.saturating_add(1);
                    *count == 1
                };
                if first {
                    GetOutcome::Unknown
                } else {
                    self.inner.get(key).await
                }
            }
            ScriptedGet::FoundThenMissing => {
                let first = {
                    let mut attempts = self.get_attempts.lock().expect("script poisoned");
                    let count = attempts.entry(key.to_string()).or_insert(0);
                    *count = count.saturating_add(1);
                    *count == 1
                };
                if first {
                    self.inner.get(key).await
                } else {
                    GetOutcome::Missing
                }
            }
        }
    }

    async fn delete(&self, key: &str) -> Result<(), String> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> ListOutcome {
        if let Some(message) = self.list_fail.lock().expect("script poisoned").clone() {
            return ListOutcome::Failed(message);
        }
        self.inner.list(prefix).await
    }
}

pub struct S3WalObjectStore {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3WalObjectStore {
    pub fn new(client: aws_sdk_s3::Client, bucket: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
        }
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}

fn backoff_ms(attempts: u32) -> u64 {
    let base = 200u64.saturating_mul(1u64 << attempts.min(10));
    let jitter: u64 = thread_rng().gen_range(0..100);
    (base + jitter).min(5_000)
}

fn is_no_such_key(
    err: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
) -> bool {
    err.as_service_error().is_some_and(|e| e.is_no_such_key())
}

fn continuation_after_page(
    is_truncated: bool,
    token: Option<String>,
) -> Result<Option<String>, String> {
    if !is_truncated {
        return Ok(None);
    }
    match token {
        Some(t) if !t.is_empty() => Ok(Some(t)),
        _ => Err("truncated S3 list without continuation token".into()),
    }
}

#[async_trait]
impl WalObjectStore for S3WalObjectStore {
    async fn put(&self, key: &str, body: Vec<u8>) -> PutOutcome {
        if body.len() <= MPU_PART_SIZE {
            return put_object(&self.client, &self.bucket, key, body).await;
        }
        put_multipart(&self.client, &self.bucket, key, body).await
    }

    async fn get(&self, key: &str) -> GetOutcome {
        let mut attempts: u32 = 0;
        loop {
            match self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(resp) => match resp.body.collect().await {
                    Ok(agg) => return GetOutcome::Found(agg.into_bytes().to_vec()),
                    Err(_) => {
                        attempts = attempts.saturating_add(1);
                        metrics_counters::add_s3_wal_retry(1);
                        if attempts >= PUT_ATTEMPTS {
                            metrics_counters::add_s3_wal_error(1);
                            return GetOutcome::Unknown;
                        }
                        tokio::time::sleep(Duration::from_millis(backoff_ms(attempts))).await;
                    }
                },
                Err(err) if is_no_such_key(&err) => return GetOutcome::Missing,
                Err(_) => {
                    attempts = attempts.saturating_add(1);
                    metrics_counters::add_s3_wal_retry(1);
                    if attempts >= PUT_ATTEMPTS {
                        metrics_counters::add_s3_wal_error(1);
                        return GetOutcome::Unknown;
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempts))).await;
                }
            }
        }
    }

    async fn delete(&self, key: &str) -> Result<(), String> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    async fn list(&self, prefix: &str) -> ListOutcome {
        let mut token: Option<String> = None;
        let mut keys = Vec::new();
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            match req.send().await {
                Ok(resp) => {
                    if let Some(contents) = resp.contents {
                        for obj in contents {
                            if let Some(k) = obj.key() {
                                keys.push(k.to_string());
                            }
                        }
                    }
                    match continuation_after_page(
                        resp.is_truncated.unwrap_or(false),
                        resp.next_continuation_token,
                    ) {
                        Ok(None) => return ListOutcome::Keys(keys),
                        Ok(Some(next)) => token = Some(next),
                        Err(message) => return ListOutcome::Failed(message),
                    }
                }
                Err(err) => return ListOutcome::Failed(err.to_string()),
            }
        }
    }
}

async fn put_object(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    body: Vec<u8>,
) -> PutOutcome {
    let mut attempts: u32 = 0;
    loop {
        match client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(body.clone()))
            .content_type("application/octet-stream")
            .send()
            .await
        {
            Ok(_) => return PutOutcome::Applied,
            Err(err) => {
                attempts = attempts.saturating_add(1);
                metrics_counters::add_s3_wal_retry(1);
                if attempts >= PUT_ATTEMPTS {
                    metrics_counters::add_s3_wal_error(1);
                    let message = err.to_string();
                    if message.contains("Invalid") || message.contains("403") {
                        return PutOutcome::DefiniteFailure(message);
                    }
                    return PutOutcome::Unknown;
                }
                tokio::time::sleep(Duration::from_millis(backoff_ms(attempts))).await;
            }
        }
    }
}

async fn abort_multipart(client: &aws_sdk_s3::Client, bucket: &str, key: &str, upload_id: &str) {
    let _ = client
        .abort_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .send()
        .await;
}

async fn put_multipart(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    body: Vec<u8>,
) -> PutOutcome {
    let upload_id = {
        let mut attempts: u32 = 0;
        loop {
            match client
                .create_multipart_upload()
                .bucket(bucket)
                .key(key)
                .content_type("application/octet-stream")
                .send()
                .await
            {
                Ok(resp) => break resp.upload_id().unwrap_or_default().to_string(),
                Err(err) => {
                    attempts = attempts.saturating_add(1);
                    metrics_counters::add_s3_wal_retry(1);
                    if attempts >= PUT_ATTEMPTS {
                        metrics_counters::add_s3_wal_error(1);
                        return PutOutcome::Unknown;
                    }
                    let _ = err;
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempts))).await;
                }
            }
        }
    };
    let mut parts = Vec::new();
    let mut part_number = 1i32;
    for chunk in body.chunks(MPU_PART_SIZE) {
        let mut attempts: u32 = 0;
        let etag = loop {
            match client
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(part_number)
                .body(ByteStream::from(chunk.to_vec()))
                .send()
                .await
            {
                Ok(out) => break out.e_tag().unwrap_or_default().to_string(),
                Err(_) => {
                    attempts = attempts.saturating_add(1);
                    metrics_counters::add_s3_wal_retry(1);
                    if attempts >= PUT_ATTEMPTS {
                        metrics_counters::add_s3_wal_error(1);
                        abort_multipart(client, bucket, key, &upload_id).await;
                        return PutOutcome::Unknown;
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempts))).await;
                }
            }
        };
        parts.push(
            CompletedPart::builder()
                .set_e_tag(Some(etag))
                .part_number(part_number)
                .build(),
        );
        part_number += 1;
    }
    let mut attempts: u32 = 0;
    loop {
        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(parts.clone()))
            .build();
        match client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
        {
            Ok(_) => return PutOutcome::Applied,
            Err(_) => {
                attempts = attempts.saturating_add(1);
                metrics_counters::add_s3_wal_retry(1);
                if attempts >= PUT_ATTEMPTS {
                    metrics_counters::add_s3_wal_error(1);
                    // Complete timed out: the object may already exist. Do not
                    // abort; persist GET-admits this same id.
                    return PutOutcome::Unknown;
                }
                tokio::time::sleep(Duration::from_millis(backoff_ms(attempts))).await;
            }
        }
    }
}

pub fn parse_s3_prefix(prefix_url: &str) -> std::io::Result<(String, String)> {
    let u = Url::parse(prefix_url)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    if u.scheme() != "s3" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "prefix must be s3://",
        ));
    }
    let bucket = u
        .host_str()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing bucket"))?
        .to_string();
    let base = u
        .path()
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string();
    Ok((bucket, base))
}

pub fn snapshot_pair_keys(
    prefix_url: &str,
    snapshot_id: &str,
) -> std::io::Result<(String, String, String)> {
    let (bucket, base) = parse_s3_prefix(prefix_url)?;
    let now = chrono::Utc::now();
    let key_prefix = format!(
        "{}/p_year={}/p_month={}/p_day={}",
        base,
        now.format("%Y"),
        now.format("%m"),
        now.format("%d")
    );
    let seg_key = format!("{}/{}.seg", key_prefix, snapshot_id);
    let commit_key = format!("{}.commit", seg_key);
    Ok((bucket, seg_key, commit_key))
}

/// Un-own then drop the pair. Commit is deleted first; a missing key is fine.
pub fn commit_key_for_body(seg_key: &str) -> String {
    format!("{seg_key}.commit")
}

pub async fn reclaim_owned_pair(
    store: &dyn WalObjectStore,
    seg_key: &str,
    lease: &skippr_lease::LeaseGuard,
) -> Result<(), String> {
    lease
        .require_active_epoch()
        .map_err(|err| format!("s3 wal reclaim fenced: {err}"))?;
    store.delete(&commit_key_for_body(seg_key)).await?;
    lease
        .require_active_epoch()
        .map_err(|err| format!("s3 wal reclaim fenced: {err}"))?;
    store.delete(seg_key).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use skippr_lease::{LeaseGuard, PipelineKey, SystemClock};
    use std::sync::Arc;

    fn test_guard() -> Arc<LeaseGuard> {
        let key = PipelineKey::new("t", "w", "p").expect("pipeline key");
        LeaseGuard::single_node(key, Arc::new(SystemClock::new()))
    }

    #[test]
    fn reclaim_deletes_commit_before_body() {
        assert_eq!(commit_key_for_body("p/id.seg"), "p/id.seg.commit");
    }

    #[tokio::test]
    async fn reclaim_owned_pair_uses_typed_delete() {
        let store = MemoryObjectStore::new();
        store.put("p/id.seg", b"body".to_vec()).await;
        store.put("p/id.seg.commit", b"commit".to_vec()).await;
        let guard = test_guard();
        reclaim_owned_pair(&store, "p/id.seg", guard.as_ref())
            .await
            .unwrap();
        assert!(!store.contains("p/id.seg"));
        assert!(!store.contains("p/id.seg.commit"));
    }

    #[test]
    fn truncated_list_without_token_fails_closed() {
        match continuation_after_page(true, None) {
            Err(message) => assert!(message.contains("continuation token")),
            Ok(_) => panic!("truncated list without a token must fail closed"),
        }
        match continuation_after_page(true, Some(String::new())) {
            Err(_) => {}
            Ok(_) => panic!("empty continuation token must fail closed"),
        }
        assert_eq!(continuation_after_page(false, None).unwrap(), None);
        assert_eq!(
            continuation_after_page(true, Some("page-2".into())).unwrap(),
            Some("page-2".into())
        );
    }
}
