use arrow::array::{
    Array, BooleanArray, Float64Array, Int32Array, StringArray, UInt32Array, UInt64Array,
    UInt8Array,
};
use arrow::record_batch::RecordBatch;
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use futures::StreamExt;
use lance::Dataset;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct EdgeByTargetRow {
    pub snapshot_id: String,
    pub edge_id: String,
    #[serde(default)]
    pub url_from: String,
    #[serde(default)]
    pub url_to: String,
    #[serde(default)]
    pub domain_from: String,
    #[serde(default)]
    pub domain_to: String,
    #[serde(default)]
    pub target_domain_hash_bucket: u8,
    #[serde(default)]
    pub source_domain_hash_bucket: u8,
    pub target_domain_id: u64,
    pub source_domain_id: u64,
    pub url_from_id: u64,
    pub url_to_id: u64,
    #[serde(default)]
    pub anchor_text: String,
    pub anchor_id: u64,
    #[serde(default)]
    pub anchor_hash: u64,
    pub link_context: String,
    #[serde(default)]
    pub rel_semantics: String,
    pub first_seen: String,
    pub last_seen: String,
    #[serde(default)]
    pub lost_seen_date: Option<String>,
    pub state: String,
    pub rel_flags: u32,
    #[serde(default)]
    pub is_image_link: bool,
    #[serde(default)]
    pub latest_edge_observation_id: String,
    #[serde(default)]
    pub cc_crawl_id: String,
    #[serde(default)]
    pub warc_record_id: String,
    #[serde(default)]
    pub page_from_rank: Option<f64>,
    #[serde(default)]
    pub http_status_from: Option<u32>,
    #[serde(default)]
    pub is_broken: bool,
    #[serde(default)]
    pub discovered_by: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageRankRow {
    pub node_id: u64,
    pub pagerank: f64,
    pub rank_percentile: f64,
    pub converged: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpamScoreRow {
    pub domain_id: u64,
    pub spam_score: Option<i32>,
    pub spam_score_status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CorpusManifest {
    #[serde(default)]
    pub corpus_snapshot_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub output_indexes: Vec<String>,
    #[serde(default)]
    pub serving_indexes: Value,
    #[serde(default)]
    pub compaction_completed_at: Option<String>,
}

impl CorpusManifest {
    pub fn is_complete(&self) -> bool {
        self.status == "complete"
    }

    pub fn has_edges_by_target(&self) -> bool {
        self.output_indexes
            .iter()
            .any(|name| name == "edges_by_target")
    }

    pub fn edges_by_target_is_parquet(&self) -> bool {
        self.serving_indexes
            .get("edges_by_target")
            .and_then(|index| index.get("format"))
            .and_then(Value::as_str)
            .map(|format| format == "parquet")
            .unwrap_or(true)
    }
}

pub async fn s3_client() -> Result<Client, std::io::Error> {
    let cfg = aws_config::load_defaults(BehaviorVersion::latest()).await;
    Ok(Client::new(&cfg))
}

async fn load_manifest_key(
    client: &Client,
    bucket: &str,
    key: &str,
    fallback_snapshot_id: &str,
) -> Result<CorpusManifest, std::io::Error> {
    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let bytes = resp
        .body
        .collect()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .into_bytes();
    let mut manifest: CorpusManifest =
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    if manifest.corpus_snapshot_id.is_empty() {
        manifest.corpus_snapshot_id = fallback_snapshot_id.to_string();
    }
    Ok(manifest)
}

pub async fn load_manifest_for_snapshot(
    client: &Client,
    bucket: &str,
    corpus_root: &str,
    snapshot_id: &str,
) -> Result<CorpusManifest, std::io::Error> {
    load_manifest_key(
        client,
        bucket,
        &format!("{corpus_root}manifests/{snapshot_id}.json"),
        snapshot_id,
    )
    .await
}

pub async fn latest_complete_manifest(
    client: &Client,
    bucket: &str,
    corpus_root: &str,
) -> Result<CorpusManifest, std::io::Error> {
    let prefix = format!("{corpus_root}manifests/");
    let resp = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(&prefix)
        .send()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut ids: Vec<String> = resp
        .contents()
        .iter()
        .filter_map(|o| o.key())
        .filter_map(|k| {
            k.strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix(".json"))
        })
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.reverse();
    for id in ids {
        let manifest = load_manifest_for_snapshot(client, bucket, corpus_root, &id).await?;
        if manifest.is_complete() && manifest.has_edges_by_target() {
            return Ok(manifest);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no complete corpus snapshot manifest with edges_by_target found",
    ))
}

pub async fn latest_snapshot_id(
    client: &Client,
    bucket: &str,
    corpus_root: &str,
) -> Result<String, std::io::Error> {
    latest_complete_manifest(client, bucket, corpus_root)
        .await
        .map(|manifest| manifest.corpus_snapshot_id)
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no corpus snapshot manifest found",
            )
        })
}

pub fn manifest_stale_reason(manifest: &CorpusManifest) -> Option<String> {
    if !manifest.is_complete() {
        return Some(format!("manifest_status_{}", manifest.status));
    }
    if !manifest.has_edges_by_target() {
        return Some("missing_edges_by_target_index".into());
    }
    if !manifest.edges_by_target_is_parquet() {
        return Some("edges_by_target_not_parquet".into());
    }
    None
}

pub async fn load_edges_for_snapshot(
    client: &Client,
    bucket: &str,
    corpus_root: &str,
    snapshot_id: &str,
    target_domain_id: u64,
) -> Result<Vec<EdgeByTargetRow>, std::io::Error> {
    let hash_bucket = target_domain_id % 256;
    let manifest_key =
        format!("{corpus_root}indexes/edges_by_target/snapshot_id={snapshot_id}/manifest.json");
    if let Ok(manifest) = load_json_value(client, bucket, &manifest_key).await {
        let keys = edge_file_keys_for_target(&manifest, target_domain_id);
        if !keys.is_empty() {
            return load_edges_keys(client, bucket, &keys).await;
        }
    }
    let narrow_prefix = format!(
        "{corpus_root}indexes/edges_by_target/snapshot_id={snapshot_id}/target_domain_hash_bucket={hash_bucket:03}/target_domain_id={target_domain_id}/"
    );
    let rows = load_edges_prefix(client, bucket, &narrow_prefix).await?;
    Ok(rows)
}

async fn load_json_value(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<Value, std::io::Error> {
    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let bytes = resp
        .body
        .collect()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .into_bytes();
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

fn edge_file_keys_for_target(manifest: &Value, target_domain_id: u64) -> Vec<String> {
    let mut keys = Vec::new();
    let Some(partitions) = manifest.get("partitions").and_then(Value::as_array) else {
        return keys;
    };
    for partition in partitions {
        if partition.get("target_domain_id").and_then(Value::as_u64) != Some(target_domain_id) {
            continue;
        }
        let Some(files) = partition.get("files").and_then(Value::as_array) else {
            continue;
        };
        keys.extend(
            files
                .iter()
                .filter_map(|file| file.get("key").and_then(Value::as_str).map(str::to_string)),
        );
    }
    keys
}

async fn load_edges_keys(
    client: &Client,
    bucket: &str,
    keys: &[String],
) -> Result<Vec<EdgeByTargetRow>, std::io::Error> {
    let mut rows = Vec::new();
    for key in keys {
        if key.ends_with(".parquet") {
            rows.extend(load_edges_parquet_object(client, bucket, key).await?);
            continue;
        }
        if !key.ends_with(".jsonl") {
            continue;
        }
        let resp = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .into_bytes();
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(row) = serde_json::from_str::<EdgeByTargetRow>(line) {
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

async fn load_edges_prefix(
    client: &Client,
    bucket: &str,
    prefix: &str,
) -> Result<Vec<EdgeByTargetRow>, std::io::Error> {
    let list = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .send()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut rows = Vec::new();
    let mut parquet_keys = Vec::new();
    let mut jsonl_keys = Vec::new();
    for obj in list.contents() {
        let Some(key) = obj.key() else { continue };
        if key.ends_with(".parquet") {
            parquet_keys.push(key.to_string());
        } else if key.ends_with(".jsonl") {
            jsonl_keys.push(key.to_string());
        }
    }
    parquet_keys.sort();
    jsonl_keys.sort();
    for key in parquet_keys {
        rows.extend(load_edges_parquet_object(client, bucket, &key).await?);
    }
    if !rows.is_empty() {
        return Ok(rows);
    }
    for key in jsonl_keys {
        let resp = client
            .get_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .into_bytes();
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(row) = serde_json::from_str::<EdgeByTargetRow>(line) {
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

async fn load_edges_parquet_object(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<Vec<EdgeByTargetRow>, std::io::Error> {
    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let bytes = resp
        .body
        .collect()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .into_bytes();
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes).map_err(std::io::Error::other)?;
    let reader = builder.build().map_err(std::io::Error::other)?;
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.map_err(std::io::Error::other)?;
        for row in 0..batch.num_rows() {
            if let Some(edge) = edge_from_batch(&batch, row) {
                rows.push(edge);
            }
        }
    }
    Ok(rows)
}

async fn load_jsonl_prefix<T: for<'de> Deserialize<'de>>(
    client: &Client,
    bucket: &str,
    prefix: &str,
) -> Result<Vec<T>, std::io::Error> {
    let list = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .send()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut rows = Vec::new();
    for obj in list.contents() {
        let Some(key) = obj.key() else { continue };
        if !key.ends_with(".jsonl") {
            continue;
        }
        let resp = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .into_bytes();
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(row) = serde_json::from_str::<T>(line) {
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

fn lance_uri(bucket: &str, corpus_root: &str, artifact: &str, snapshot_id: &str) -> String {
    format!("s3://{bucket}/{corpus_root}lance/{artifact}.lance/snapshot_id={snapshot_id}")
}

async fn load_lance_batches(uri: String) -> Result<Vec<RecordBatch>, std::io::Error> {
    let dataset = Dataset::open(uri.as_str())
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let mut stream = dataset
        .scan()
        .try_into_stream()
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let mut batches = Vec::new();
    while let Some(batch) = stream.next().await {
        batches.push(batch.map_err(|err| std::io::Error::other(err.to_string()))?);
    }
    Ok(batches)
}

fn string_value(batch: &RecordBatch, name: &str, row: usize) -> Option<String> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx).as_any().downcast_ref::<StringArray>()?;
    if arr.is_null(row) {
        return None;
    }
    Some(arr.value(row).to_string())
}

fn u64_value(batch: &RecordBatch, name: &str, row: usize) -> Option<u64> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx).as_any().downcast_ref::<UInt64Array>()?;
    if arr.is_null(row) {
        return None;
    }
    Some(arr.value(row))
}

fn u32_value(batch: &RecordBatch, name: &str, row: usize) -> Option<u32> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx).as_any().downcast_ref::<UInt32Array>()?;
    if arr.is_null(row) {
        return None;
    }
    Some(arr.value(row))
}

fn u8_value(batch: &RecordBatch, name: &str, row: usize) -> Option<u8> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx).as_any().downcast_ref::<UInt8Array>()?;
    if arr.is_null(row) {
        return None;
    }
    Some(arr.value(row))
}

fn f64_value(batch: &RecordBatch, name: &str, row: usize) -> Option<f64> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx).as_any().downcast_ref::<Float64Array>()?;
    if arr.is_null(row) {
        return None;
    }
    Some(arr.value(row))
}

fn i32_value(batch: &RecordBatch, name: &str, row: usize) -> Option<i32> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx).as_any().downcast_ref::<Int32Array>()?;
    if arr.is_null(row) {
        return None;
    }
    Some(arr.value(row))
}

fn bool_value(batch: &RecordBatch, name: &str, row: usize) -> Option<bool> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx).as_any().downcast_ref::<BooleanArray>()?;
    if arr.is_null(row) {
        return None;
    }
    Some(arr.value(row))
}

fn edge_from_batch(batch: &RecordBatch, row: usize) -> Option<EdgeByTargetRow> {
    Some(EdgeByTargetRow {
        snapshot_id: string_value(batch, "snapshot_id", row)?,
        edge_id: string_value(batch, "edge_id", row)?,
        url_from: string_value(batch, "url_from", row).unwrap_or_default(),
        url_to: string_value(batch, "url_to", row).unwrap_or_default(),
        domain_from: string_value(batch, "domain_from", row).unwrap_or_default(),
        domain_to: string_value(batch, "domain_to", row).unwrap_or_default(),
        target_domain_hash_bucket: u8_value(batch, "target_domain_hash_bucket", row).unwrap_or(0),
        source_domain_hash_bucket: u8_value(batch, "source_domain_hash_bucket", row).unwrap_or(0),
        target_domain_id: u64_value(batch, "target_domain_id", row)?,
        source_domain_id: u64_value(batch, "source_domain_id", row)?,
        url_from_id: u64_value(batch, "url_from_id", row)?,
        url_to_id: u64_value(batch, "url_to_id", row)?,
        anchor_text: string_value(batch, "anchor_text", row).unwrap_or_default(),
        anchor_id: u64_value(batch, "anchor_id", row)?,
        anchor_hash: u64_value(batch, "anchor_hash", row).unwrap_or(0),
        link_context: string_value(batch, "link_context", row).unwrap_or_else(|| "unknown".into()),
        rel_semantics: string_value(batch, "rel_semantics", row).unwrap_or_default(),
        first_seen: string_value(batch, "first_seen", row)?,
        last_seen: string_value(batch, "last_seen", row)?,
        lost_seen_date: string_value(batch, "lost_seen_date", row),
        state: string_value(batch, "state", row).unwrap_or_else(|| "active".into()),
        rel_flags: u32_value(batch, "rel_flags", row).unwrap_or(0),
        is_image_link: bool_value(batch, "is_image_link", row).unwrap_or(false),
        latest_edge_observation_id: string_value(batch, "latest_edge_observation_id", row)
            .unwrap_or_default(),
        cc_crawl_id: string_value(batch, "cc_crawl_id", row).unwrap_or_default(),
        warc_record_id: string_value(batch, "warc_record_id", row).unwrap_or_default(),
        page_from_rank: f64_value(batch, "page_from_rank", row),
        http_status_from: u32_value(batch, "http_status_from", row),
        is_broken: bool_value(batch, "is_broken", row).unwrap_or(false),
        discovered_by: string_value(batch, "discovered_by", row).unwrap_or_default(),
    })
}

async fn load_pagerank_lance(
    bucket: &str,
    corpus_root: &str,
    snapshot_id: &str,
) -> Result<Vec<PageRankRow>, std::io::Error> {
    let batches = load_lance_batches(lance_uri(
        bucket,
        corpus_root,
        "pagerank_scores",
        snapshot_id,
    ))
    .await?;
    let mut rows = Vec::new();
    for batch in batches {
        for row in 0..batch.num_rows() {
            let Some(node_id) = u64_value(&batch, "node_id", row) else {
                continue;
            };
            rows.push(PageRankRow {
                node_id,
                pagerank: f64_value(&batch, "pagerank", row).unwrap_or(0.0),
                rank_percentile: f64_value(&batch, "rank_percentile", row).unwrap_or(0.0),
                converged: bool_value(&batch, "converged", row).unwrap_or(true),
            });
        }
    }
    Ok(rows)
}

async fn load_spam_scores_lance(
    bucket: &str,
    corpus_root: &str,
    snapshot_id: &str,
) -> Result<Vec<SpamScoreRow>, std::io::Error> {
    let batches =
        load_lance_batches(lance_uri(bucket, corpus_root, "spam_scores", snapshot_id)).await?;
    let mut rows = Vec::new();
    for batch in batches {
        for row in 0..batch.num_rows() {
            let Some(domain_id) = u64_value(&batch, "domain_id", row) else {
                continue;
            };
            rows.push(SpamScoreRow {
                domain_id,
                spam_score: i32_value(&batch, "spam_score", row),
                spam_score_status: string_value(&batch, "spam_score_status", row)
                    .unwrap_or_else(|| "missing".into()),
            });
        }
    }
    Ok(rows)
}

pub async fn load_pagerank_for_snapshot(
    client: &Client,
    bucket: &str,
    corpus_root: &str,
    snapshot_id: &str,
) -> Result<Vec<PageRankRow>, std::io::Error> {
    match load_pagerank_lance(bucket, corpus_root, snapshot_id).await {
        Ok(rows) => Ok(rows),
        Err(_) => {
            load_jsonl_prefix(
                client,
                bucket,
                &format!("{corpus_root}lance/pagerank_scores.lance/snapshot_id={snapshot_id}/"),
            )
            .await
        }
    }
}

pub async fn load_spam_scores_for_snapshot(
    client: &Client,
    bucket: &str,
    corpus_root: &str,
    snapshot_id: &str,
) -> Result<Vec<SpamScoreRow>, std::io::Error> {
    match load_spam_scores_lance(bucket, corpus_root, snapshot_id).await {
        Ok(rows) => Ok(rows),
        Err(_) => {
            load_jsonl_prefix(
                client,
                bucket,
                &format!("{corpus_root}lance/spam_scores.lance/snapshot_id={snapshot_id}/"),
            )
            .await
        }
    }
}
