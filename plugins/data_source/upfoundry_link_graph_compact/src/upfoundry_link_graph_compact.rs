use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arrow::array::{
    Array, BooleanArray, Float64Array, Int32Array, Int64Array, RecordBatch, StringArray,
    UInt32Array, UInt64Array, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow_array::RecordBatchIterator;
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use lance::dataset::WriteMode;
use lance::dataset::WriteParams;
use lance::Dataset;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use skippr_plugin_shared_link_graph::{RawEdgeObservation, RawPageFact};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::config::UpfoundryLinkGraphCompactConfig;
use crate::streams::{all_namespace_contracts, NAMESPACE_COMPACT_RUN};

const HOT_DOMAIN_EDGE_SPLIT_THRESHOLD: usize = 100_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeByTargetRow {
    pub snapshot_id: String,
    pub edge_id: String,
    pub url_from: String,
    pub url_to: String,
    pub domain_from: String,
    pub domain_to: String,
    pub target_domain_hash_bucket: u8,
    pub source_domain_hash_bucket: u8,
    pub target_domain_id: u64,
    pub source_domain_id: u64,
    pub url_from_id: u64,
    pub url_to_id: u64,
    pub anchor_text: String,
    pub anchor_id: u64,
    pub anchor_hash: u64,
    pub link_context: String,
    pub rel_semantics: String,
    pub first_seen: String,
    pub last_seen: String,
    pub lost_seen_date: Option<String>,
    pub state: String,
    pub rel_flags: u32,
    pub is_image_link: bool,
    pub latest_edge_observation_id: String,
    pub cc_crawl_id: String,
    pub warc_record_id: String,
    pub page_from_rank: Option<f64>,
    pub http_status_from: Option<u32>,
    pub is_broken: bool,
    pub discovered_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollisionQuarantineRow {
    pub snapshot_id: String,
    pub edge_id: String,
    pub existing_url_from_id: u64,
    pub observed_url_from_id: u64,
    pub existing_url_to_id: u64,
    pub observed_url_to_id: u64,
    pub existing_source_domain_id: u64,
    pub observed_source_domain_id: u64,
    pub existing_target_domain_id: u64,
    pub observed_target_domain_id: u64,
    pub existing_link_context: String,
    pub observed_link_context: String,
    pub existing_rel_flags: u32,
    pub observed_rel_flags: u32,
    pub observed_edge_observation_id: String,
    pub quarantine_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictionaryCollisionQuarantineRow {
    pub snapshot_id: String,
    pub id_kind: String,
    pub id_value: u64,
    pub existing_value: String,
    pub observed_value: String,
    pub observed_edge_observation_id: String,
    pub quarantine_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageByDomainRow {
    pub snapshot_id: String,
    pub domain_hash_bucket: u8,
    pub domain_id: u64,
    pub url_id: u64,
    pub cc_crawl_id: String,
    pub warc_record_id: String,
    pub parse_status: String,
    pub fetch_status: Option<u32>,
    pub content_mime_type: String,
    pub outbound_edge_count: u32,
    pub raw_link_count: u32,
    pub links_stored_count: u32,
    pub links_truncated: bool,
    pub page_quality_flags: serde_json::Value,
    pub parser_version: String,
    pub first_seen: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageRankRow {
    pub corpus_snapshot_id: String,
    pub node_id: u64,
    pub node_kind: String,
    pub pagerank: f64,
    pub rank_percentile: f64,
    pub iteration_count: u32,
    pub converged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpamScoreRow {
    pub corpus_snapshot_id: String,
    pub domain_id: u64,
    pub model_version: String,
    pub spam_score: Option<i32>,
    pub spam_score_status: String,
    pub feature_schema_version: String,
    pub inference_timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainAuthorityPriorRow {
    pub domain_id: u64,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub rank_percentile: Option<f64>,
    #[serde(default)]
    pub pagerank: Option<f64>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpamModelMetadata {
    pub model_version: String,
    #[serde(default = "default_spam_feature_schema_version")]
    pub feature_schema_version: String,
    pub intercept: f64,
    pub coefficients: SpamModelCoefficients,
    #[serde(default)]
    pub metrics: serde_json::Value,
    #[serde(default)]
    pub label_counts: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpamModelCoefficients {
    #[serde(default)]
    pub in_degree: f64,
    #[serde(default)]
    pub out_degree: f64,
    #[serde(default)]
    pub outbound_inbound_ratio: f64,
    #[serde(default)]
    pub pagerank_percentile: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AffectedBuckets {
    pub target_domain_ids: Vec<u64>,
    pub target_domain_hash_buckets: Vec<u8>,
    pub source_domain_hash_buckets: Vec<u8>,
    pub source_pages: Vec<(u64, u64)>,
    pub pages_by_domain_ids: Vec<u64>,
    pub pages_by_domain_hash_buckets: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeTransitionRow {
    pub snapshot_id: String,
    pub edge_id: String,
    pub source_domain_id: u64,
    pub url_from_id: u64,
    pub target_domain_id: u64,
    pub previous_state: String,
    pub next_state: String,
    pub transition_date: String,
    pub evidence: String,
}

fn default_spam_feature_schema_version() -> String {
    "v1".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRow {
    pub corpus_snapshot_id: String,
    pub node_id: u64,
    pub node_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainFeatureRow {
    pub corpus_snapshot_id: String,
    pub domain_id: u64,
    pub in_degree: u32,
    pub out_degree: u32,
    pub pagerank_percentile: f64,
    pub feature_schema_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorFeatureRow {
    pub corpus_snapshot_id: String,
    pub anchor_id: u64,
    pub target_domain_id: u64,
    pub edge_count: u32,
    pub feature_schema_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageFeatureRow {
    pub corpus_snapshot_id: String,
    pub url_id: u64,
    pub domain_id: u64,
    pub outbound_edge_count: u32,
    pub feature_schema_version: String,
}

pub struct UpfoundryLinkGraphCompactPlugin {
    config: UpfoundryLinkGraphCompactConfig,
}

impl UpfoundryLinkGraphCompactPlugin {
    pub fn new(config: UpfoundryLinkGraphCompactConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self { config })
    }

    async fn s3_client() -> Result<Client, std::io::Error> {
        let cfg = aws_config::load_defaults(BehaviorVersion::latest()).await;
        Ok(Client::new(&cfg))
    }

    async fn list_staging_keys(client: &Client, bucket: &str, prefix: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = client.list_objects_v2().bucket(bucket).prefix(prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(_) => break,
            };
            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    if key.ends_with(".parquet") || key.ends_with(".jsonl") {
                        keys.push(key.to_string());
                    }
                }
            }
            token = resp.next_continuation_token().map(str::to_string);
            if token.is_none() {
                break;
            }
        }
        keys
    }

    async fn put_json_value(
        client: &Client,
        bucket: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), std::io::Error> {
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("application/json")
            .body(aws_sdk_s3::primitives::ByteStream::from(
                serde_json::to_vec(value).map_err(std::io::Error::other)?,
            ))
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }

    fn parquet_bytes(batch: RecordBatch) -> Result<Vec<u8>, std::io::Error> {
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .build();
        let schema = batch.schema();
        let mut out = Vec::new();
        {
            let mut writer = ArrowWriter::try_new(&mut out, schema, Some(props))
                .map_err(std::io::Error::other)?;
            writer.write(&batch).map_err(std::io::Error::other)?;
            writer.close().map_err(std::io::Error::other)?;
        }
        Ok(out)
    }

    async fn put_parquet_object(
        client: &Client,
        bucket: &str,
        key: &str,
        batch: RecordBatch,
    ) -> Result<u64, std::io::Error> {
        let bytes = Self::parquet_bytes(batch)?;
        let byte_len = bytes.len() as u64;
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("application/vnd.apache.parquet")
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(byte_len)
    }

    async fn read_json_value(
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

    async fn processed_input_partitions(
        client: &Client,
        bucket: &str,
        root: &str,
    ) -> HashSet<String> {
        let prefix = format!("{root}manifests/");
        let resp = match client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(&prefix)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(_) => return HashSet::new(),
        };
        let mut processed = HashSet::new();
        for obj in resp.contents() {
            let Some(key) = obj.key() else { continue };
            if !key.ends_with(".json") || key.contains("/pending/") {
                continue;
            }
            let Ok(manifest) = Self::read_json_value(client, bucket, key).await else {
                continue;
            };
            if manifest.get("status").and_then(Value::as_str) != Some("complete") {
                continue;
            }
            if let Some(parts) = manifest.get("input_partitions").and_then(Value::as_array) {
                processed.extend(parts.iter().filter_map(Value::as_str).map(str::to_string));
            }
        }
        processed
    }

    fn date_from_staging_key(key: &str) -> Option<String> {
        key.split('/')
            .find_map(|part| part.strip_prefix("date=").map(str::to_string))
    }

    async fn cleanup_expired_staging(
        client: &Client,
        bucket: &str,
        root: &str,
        keep_days: u32,
        protected_keys: &HashSet<String>,
    ) -> Result<usize, std::io::Error> {
        if keep_days == 0 {
            return Ok(0);
        }
        let cutoff = (Utc::now() - ChronoDuration::days(i64::from(keep_days)))
            .format("%Y-%m-%d")
            .to_string();
        let prefix = format!("{root}raw/staging/");
        let mut token: Option<String> = None;
        let mut deleted = 0usize;
        loop {
            let mut req = client.list_objects_v2().bucket(bucket).prefix(&prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req
                .send()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            for obj in resp.contents() {
                let Some(key) = obj.key() else { continue };
                if protected_keys.contains(key) {
                    continue;
                }
                let Some(date) = Self::date_from_staging_key(key) else {
                    continue;
                };
                if date >= cutoff {
                    continue;
                }
                client
                    .delete_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|err| std::io::Error::other(err.to_string()))?;
                deleted += 1;
            }
            token = resp.next_continuation_token().map(str::to_string);
            if token.is_none() {
                break;
            }
        }
        Ok(deleted)
    }

    async fn latest_complete_snapshot_id(
        client: &Client,
        bucket: &str,
        root: &str,
    ) -> Option<String> {
        let prefix = format!("{root}manifests/");
        let resp = client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(&prefix)
            .send()
            .await
            .ok()?;
        let mut ids = resp
            .contents()
            .iter()
            .filter_map(|obj| obj.key())
            .filter(|key| !key.contains("/pending/"))
            .filter_map(|key| {
                key.strip_prefix(&prefix)
                    .and_then(|name| name.strip_suffix(".json"))
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        ids.sort();
        ids.reverse();
        for id in ids {
            let Ok(manifest) =
                Self::read_json_value(client, bucket, &format!("{root}manifests/{id}.json")).await
            else {
                continue;
            };
            if manifest.get("status").and_then(Value::as_str) == Some("complete")
                && manifest
                    .get("output_indexes")
                    .and_then(Value::as_array)
                    .map(|indexes| {
                        indexes
                            .iter()
                            .any(|value| value.as_str() == Some("edges_by_target"))
                    })
                    .unwrap_or(false)
            {
                return Some(id);
            }
        }
        None
    }

    async fn load_index_manifest(
        client: &Client,
        bucket: &str,
        root: &str,
        index: &str,
        snapshot_id: &str,
    ) -> Option<Value> {
        Self::read_json_value(
            client,
            bucket,
            &format!("{root}indexes/{index}/snapshot_id={snapshot_id}/manifest.json"),
        )
        .await
        .ok()
    }

    fn edge_file_keys_from_manifest(manifest: &Value) -> Vec<String> {
        let mut keys = Vec::new();
        let Some(partitions) = manifest.get("partitions").and_then(Value::as_array) else {
            return keys;
        };
        for partition in partitions {
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

    async fn load_edges_index_keys(
        client: &Client,
        bucket: &str,
        keys: &[String],
        snapshot_id: &str,
    ) -> Vec<EdgeByTargetRow> {
        let mut rows = Vec::new();
        for key in keys {
            if key.ends_with(".parquet") {
                rows.extend(Self::load_edges_index_parquet(client, bucket, key, snapshot_id).await);
            } else if key.ends_with(".jsonl") {
                let resp = match client.get_object().bucket(bucket).key(key).send().await {
                    Ok(resp) => resp,
                    Err(_) => continue,
                };
                let bytes = match resp.body.collect().await {
                    Ok(bytes) => bytes.into_bytes(),
                    Err(_) => continue,
                };
                for line in String::from_utf8_lossy(&bytes).lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(mut row) = serde_json::from_str::<EdgeByTargetRow>(line) {
                        row.snapshot_id = snapshot_id.to_string();
                        rows.push(row);
                    }
                }
            }
        }
        rows
    }

    fn page_file_keys_for_domains(manifest: &Value, domain_ids: &HashSet<u64>) -> Vec<String> {
        let mut keys = Vec::new();
        let Some(partitions) = manifest.get("partitions").and_then(Value::as_array) else {
            return keys;
        };
        for partition in partitions {
            let Some(domain_id) = partition.get("domain_id").and_then(Value::as_u64) else {
                continue;
            };
            if !domain_ids.contains(&domain_id) {
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

    fn page_index_from_batch(
        batch: &RecordBatch,
        row: usize,
        snapshot_id: &str,
    ) -> Option<PageByDomainRow> {
        let page_quality_flags = Self::batch_string(batch, "page_quality_flags", row)
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_else(|| json!({}));
        Some(PageByDomainRow {
            snapshot_id: snapshot_id.to_string(),
            domain_hash_bucket: Self::batch_u8(batch, "domain_hash_bucket", row)?,
            domain_id: Self::batch_u64(batch, "domain_id", row)?,
            url_id: Self::batch_u64(batch, "url_id", row)?,
            cc_crawl_id: Self::batch_string(batch, "cc_crawl_id", row).unwrap_or_default(),
            warc_record_id: Self::batch_string(batch, "warc_record_id", row).unwrap_or_default(),
            parse_status: Self::batch_string(batch, "parse_status", row).unwrap_or_default(),
            fetch_status: Self::batch_u32(batch, "fetch_status", row),
            content_mime_type: Self::batch_string(batch, "content_mime_type", row)
                .unwrap_or_else(|| "text/html".into()),
            outbound_edge_count: Self::batch_u32(batch, "outbound_edge_count", row).unwrap_or(0),
            raw_link_count: Self::batch_u32(batch, "raw_link_count", row).unwrap_or(0),
            links_stored_count: Self::batch_u32(batch, "links_stored_count", row).unwrap_or(0),
            links_truncated: Self::batch_bool(batch, "links_truncated", row).unwrap_or(false),
            page_quality_flags,
            parser_version: Self::batch_string(batch, "parser_version", row)
                .unwrap_or_else(|| "link_graph_html_v1".into()),
            first_seen: Self::batch_string(batch, "first_seen", row)?,
            last_seen: Self::batch_string(batch, "last_seen", row)?,
        })
    }

    async fn load_pages_index_parquet(
        client: &Client,
        bucket: &str,
        key: &str,
        snapshot_id: &str,
    ) -> Vec<PageByDomainRow> {
        let resp = match client.get_object().bucket(bucket).key(key).send().await {
            Ok(resp) => resp,
            Err(_) => return Vec::new(),
        };
        let bytes = match resp.body.collect().await {
            Ok(bytes) => bytes.into_bytes(),
            Err(_) => return Vec::new(),
        };
        let builder = match ParquetRecordBatchReaderBuilder::try_new(bytes) {
            Ok(builder) => builder,
            Err(_) => return Vec::new(),
        };
        let reader = match builder.build() {
            Ok(reader) => reader,
            Err(_) => return Vec::new(),
        };
        let mut rows = Vec::new();
        for batch in reader.flatten() {
            for row in 0..batch.num_rows() {
                if let Some(page) = Self::page_index_from_batch(&batch, row, snapshot_id) {
                    rows.push(page);
                }
            }
        }
        rows
    }

    async fn load_pages_index_keys(
        client: &Client,
        bucket: &str,
        keys: &[String],
        snapshot_id: &str,
    ) -> Vec<PageByDomainRow> {
        let mut rows = Vec::new();
        for key in keys {
            if key.ends_with(".parquet") {
                rows.extend(Self::load_pages_index_parquet(client, bucket, key, snapshot_id).await);
            } else if key.ends_with(".jsonl") {
                let resp = match client.get_object().bucket(bucket).key(key).send().await {
                    Ok(resp) => resp,
                    Err(_) => continue,
                };
                let bytes = match resp.body.collect().await {
                    Ok(bytes) => bytes.into_bytes(),
                    Err(_) => continue,
                };
                for line in String::from_utf8_lossy(&bytes).lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(mut row) = serde_json::from_str::<PageByDomainRow>(line) {
                        row.snapshot_id = snapshot_id.to_string();
                        rows.push(row);
                    }
                }
            }
        }
        rows
    }

    async fn load_pages_by_domain_manifest_domains(
        client: &Client,
        bucket: &str,
        manifest: &Value,
        domain_ids: &HashSet<u64>,
        snapshot_id: &str,
    ) -> Vec<PageByDomainRow> {
        let keys = Self::page_file_keys_for_domains(manifest, domain_ids);
        Self::load_pages_index_keys(client, bucket, &keys, snapshot_id).await
    }

    async fn load_edges_by_target_snapshot(
        client: &Client,
        bucket: &str,
        root: &str,
        snapshot_id: &str,
    ) -> Vec<EdgeByTargetRow> {
        if let Some(manifest) =
            Self::load_index_manifest(client, bucket, root, "edges_by_target", snapshot_id).await
        {
            let keys = Self::edge_file_keys_from_manifest(&manifest);
            let rows = Self::load_edges_index_keys(client, bucket, &keys, snapshot_id).await;
            if !rows.is_empty() {
                return rows;
            }
        }
        let prefix = format!("{root}indexes/edges_by_target/snapshot_id={snapshot_id}/");
        let resp = match client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(_) => return Vec::new(),
        };
        let mut rows = Vec::new();
        let mut parquet_keys = Vec::new();
        let mut jsonl_keys = Vec::new();
        for obj in resp.contents() {
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
            rows.extend(Self::load_edges_index_parquet(client, bucket, &key, snapshot_id).await);
        }
        if !rows.is_empty() {
            return rows;
        }
        for key in jsonl_keys {
            let resp = match client.get_object().bucket(bucket).key(&key).send().await {
                Ok(resp) => resp,
                Err(_) => continue,
            };
            let bytes = match resp.body.collect().await {
                Ok(bytes) => bytes.into_bytes(),
                Err(_) => continue,
            };
            for line in String::from_utf8_lossy(&bytes).lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(mut row) = serde_json::from_str::<EdgeByTargetRow>(line) {
                    row.snapshot_id = snapshot_id.to_string();
                    rows.push(row);
                }
            }
        }
        rows
    }

    async fn load_edges_index_parquet(
        client: &Client,
        bucket: &str,
        key: &str,
        snapshot_id: &str,
    ) -> Vec<EdgeByTargetRow> {
        let resp = match client.get_object().bucket(bucket).key(key).send().await {
            Ok(resp) => resp,
            Err(_) => return Vec::new(),
        };
        let bytes = match resp.body.collect().await {
            Ok(bytes) => bytes.into_bytes(),
            Err(_) => return Vec::new(),
        };
        let builder = match ParquetRecordBatchReaderBuilder::try_new(bytes) {
            Ok(builder) => builder,
            Err(_) => return Vec::new(),
        };
        let reader = match builder.build() {
            Ok(reader) => reader,
            Err(_) => return Vec::new(),
        };
        let mut rows = Vec::new();
        for batch in reader.flatten() {
            for row in 0..batch.num_rows() {
                if let Some(mut edge) = Self::edge_index_from_batch(&batch, row) {
                    edge.snapshot_id = snapshot_id.to_string();
                    rows.push(edge);
                }
            }
        }
        rows
    }

    fn merge_edge_snapshots(
        prior_edges: Vec<EdgeByTargetRow>,
        new_edges: Vec<EdgeByTargetRow>,
        snapshot_id: &str,
    ) -> Vec<EdgeByTargetRow> {
        let mut by_edge: HashMap<String, EdgeByTargetRow> = HashMap::new();
        for mut edge in prior_edges {
            edge.snapshot_id = snapshot_id.to_string();
            by_edge.insert(edge.edge_id.clone(), edge);
        }
        for mut edge in new_edges {
            edge.snapshot_id = snapshot_id.to_string();
            match by_edge.get_mut(&edge.edge_id) {
                Some(existing) => {
                    if edge.first_seen < existing.first_seen {
                        existing.first_seen = edge.first_seen.clone();
                    }
                    if edge.last_seen > existing.last_seen {
                        edge.first_seen = existing.first_seen.clone();
                        if existing.state == "lost" && edge.state == "active" {
                            edge.lost_seen_date = None;
                        }
                        *existing = edge;
                    }
                }
                None => {
                    by_edge.insert(edge.edge_id.clone(), edge);
                }
            }
        }
        by_edge.into_values().collect()
    }

    fn batch_string(batch: &RecordBatch, name: &str, row: usize) -> Option<String> {
        let idx = batch.schema().index_of(name).ok()?;
        let arr = batch.column(idx).as_any().downcast_ref::<StringArray>()?;
        if arr.is_null(row) {
            return None;
        }
        Some(arr.value(row).to_string())
    }

    fn batch_u64(batch: &RecordBatch, name: &str, row: usize) -> Option<u64> {
        let idx = batch.schema().index_of(name).ok()?;
        let arr = batch.column(idx).as_any().downcast_ref::<UInt64Array>()?;
        if arr.is_null(row) {
            return None;
        }
        Some(arr.value(row))
    }

    fn batch_u32(batch: &RecordBatch, name: &str, row: usize) -> Option<u32> {
        let idx = batch.schema().index_of(name).ok()?;
        let arr = batch.column(idx).as_any().downcast_ref::<UInt32Array>()?;
        if arr.is_null(row) {
            return None;
        }
        Some(arr.value(row))
    }

    fn batch_u8(batch: &RecordBatch, name: &str, row: usize) -> Option<u8> {
        let idx = batch.schema().index_of(name).ok()?;
        let arr = batch.column(idx).as_any().downcast_ref::<UInt8Array>()?;
        if arr.is_null(row) {
            return None;
        }
        Some(arr.value(row))
    }

    fn batch_i64(batch: &RecordBatch, name: &str, row: usize) -> Option<i64> {
        let idx = batch.schema().index_of(name).ok()?;
        let arr = batch.column(idx).as_any().downcast_ref::<Int64Array>()?;
        if arr.is_null(row) {
            return None;
        }
        Some(arr.value(row))
    }

    fn batch_bool(batch: &RecordBatch, name: &str, row: usize) -> Option<bool> {
        let idx = batch.schema().index_of(name).ok()?;
        let arr = batch.column(idx).as_any().downcast_ref::<BooleanArray>()?;
        if arr.is_null(row) {
            return None;
        }
        Some(arr.value(row))
    }

    fn batch_f64(batch: &RecordBatch, name: &str, row: usize) -> Option<f64> {
        let idx = batch.schema().index_of(name).ok()?;
        let arr = batch.column(idx).as_any().downcast_ref::<Float64Array>()?;
        if arr.is_null(row) {
            return None;
        }
        Some(arr.value(row))
    }

    fn edge_index_from_batch(batch: &RecordBatch, row: usize) -> Option<EdgeByTargetRow> {
        Some(EdgeByTargetRow {
            snapshot_id: Self::batch_string(batch, "snapshot_id", row)?,
            edge_id: Self::batch_string(batch, "edge_id", row)?,
            url_from: Self::batch_string(batch, "url_from", row).unwrap_or_default(),
            url_to: Self::batch_string(batch, "url_to", row).unwrap_or_default(),
            domain_from: Self::batch_string(batch, "domain_from", row).unwrap_or_default(),
            domain_to: Self::batch_string(batch, "domain_to", row).unwrap_or_default(),
            target_domain_hash_bucket: Self::batch_u8(batch, "target_domain_hash_bucket", row)
                .unwrap_or(0),
            source_domain_hash_bucket: Self::batch_u8(batch, "source_domain_hash_bucket", row)
                .unwrap_or(0),
            target_domain_id: Self::batch_u64(batch, "target_domain_id", row)?,
            source_domain_id: Self::batch_u64(batch, "source_domain_id", row)?,
            url_from_id: Self::batch_u64(batch, "url_from_id", row)?,
            url_to_id: Self::batch_u64(batch, "url_to_id", row)?,
            anchor_text: Self::batch_string(batch, "anchor_text", row).unwrap_or_default(),
            anchor_id: Self::batch_u64(batch, "anchor_id", row)?,
            anchor_hash: Self::batch_u64(batch, "anchor_hash", row).unwrap_or(0),
            link_context: Self::batch_string(batch, "link_context", row)
                .unwrap_or_else(|| "unknown".into()),
            rel_semantics: Self::batch_string(batch, "rel_semantics", row).unwrap_or_default(),
            first_seen: Self::batch_string(batch, "first_seen", row)?,
            last_seen: Self::batch_string(batch, "last_seen", row)?,
            lost_seen_date: Self::batch_string(batch, "lost_seen_date", row),
            state: Self::batch_string(batch, "state", row).unwrap_or_else(|| "active".into()),
            rel_flags: Self::batch_u32(batch, "rel_flags", row).unwrap_or(0),
            is_image_link: Self::batch_bool(batch, "is_image_link", row).unwrap_or(false),
            latest_edge_observation_id: Self::batch_string(
                batch,
                "latest_edge_observation_id",
                row,
            )
            .unwrap_or_default(),
            cc_crawl_id: Self::batch_string(batch, "cc_crawl_id", row).unwrap_or_default(),
            warc_record_id: Self::batch_string(batch, "warc_record_id", row).unwrap_or_default(),
            page_from_rank: Self::batch_f64(batch, "page_from_rank", row),
            http_status_from: Self::batch_u32(batch, "http_status_from", row),
            is_broken: Self::batch_bool(batch, "is_broken", row).unwrap_or(false),
            discovered_by: Self::batch_string(batch, "discovered_by", row).unwrap_or_default(),
        })
    }

    fn edge_from_batch(batch: &RecordBatch, row: usize) -> Option<RawEdgeObservation> {
        Some(RawEdgeObservation {
            edge_observation_id: Self::batch_string(batch, "edge_observation_id", row)?,
            edge_id: Self::batch_string(batch, "edge_id", row)?,
            url_from: Self::batch_string(batch, "url_from", row).unwrap_or_default(),
            url_to: Self::batch_string(batch, "url_to", row).unwrap_or_default(),
            domain_from: Self::batch_string(batch, "domain_from", row).unwrap_or_default(),
            domain_to: Self::batch_string(batch, "domain_to", row).unwrap_or_default(),
            url_from_id: Self::batch_u64(batch, "url_from_id", row)?,
            url_to_id: Self::batch_u64(batch, "url_to_id", row)?,
            domain_from_id: Self::batch_u64(batch, "domain_from_id", row)?,
            domain_to_id: Self::batch_u64(batch, "domain_to_id", row)?,
            anchor_text: Self::batch_string(batch, "anchor_text", row).unwrap_or_default(),
            anchor_id: Self::batch_u64(batch, "anchor_id", row)?,
            link_context: Self::batch_string(batch, "link_context", row)?,
            rel_flags: Self::batch_u32(batch, "rel_flags", row)?,
            is_image_link: Self::batch_bool(batch, "is_image_link", row).unwrap_or(false),
            link_ordinal: Self::batch_u32(batch, "link_ordinal", row)?,
            cc_crawl_id: Self::batch_string(batch, "cc_crawl_id", row)?,
            warc_file_id: Self::batch_u64(batch, "warc_file_id", row)?,
            warc_record_offset: Self::batch_i64(batch, "warc_record_offset", row)?,
            warc_record_length: Self::batch_i64(batch, "warc_record_length", row)?,
            http_status_from: Self::batch_u32(batch, "http_status_from", row),
            is_broken: Self::batch_bool(batch, "is_broken", row).unwrap_or_else(|| {
                Self::batch_u32(batch, "http_status_from", row)
                    .map(|status| !(200..400).contains(&status))
                    .unwrap_or(false)
            }),
            fetch_time: Self::batch_string(batch, "fetch_time", row)?,
            canonicalization_version: Self::batch_string(batch, "canonicalization_version", row)?,
            discovered_by: Self::batch_string(batch, "discovered_by", row)?,
        })
    }

    fn page_from_batch(batch: &RecordBatch, row: usize) -> Option<RawPageFact> {
        let page_quality_flags = Self::batch_string(batch, "page_quality_flags", row)
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_else(|| json!({}));
        Some(RawPageFact {
            url_id: Self::batch_u64(batch, "url_id", row)?,
            domain_id: Self::batch_u64(batch, "domain_id", row)?,
            cc_crawl_id: Self::batch_string(batch, "cc_crawl_id", row)?,
            warc_file_id: Self::batch_u64(batch, "warc_file_id", row)?,
            warc_record_offset: Self::batch_i64(batch, "warc_record_offset", row)?,
            warc_record_length: Self::batch_i64(batch, "warc_record_length", row)?,
            fetch_status: Self::batch_u32(batch, "fetch_status", row),
            content_mime_type: Self::batch_string(batch, "content_mime_type", row)
                .unwrap_or_else(|| "text/html".into()),
            fetch_time: Self::batch_string(batch, "fetch_time", row)?,
            outbound_link_count: Self::batch_u32(batch, "outbound_link_count", row)?,
            stored_link_count: Self::batch_u32(batch, "stored_link_count", row)?,
            links_truncated: Self::batch_bool(batch, "links_truncated", row).unwrap_or(false),
            raw_link_count: Self::batch_u32(batch, "raw_link_count", row)?,
            page_quality_flags,
            canonicalization_version: Self::batch_string(batch, "canonicalization_version", row)?,
            parser_version: Self::batch_string(batch, "parser_version", row)
                .unwrap_or_else(|| "link_graph_html_v1".into()),
            parse_status: Self::batch_string(batch, "parse_status", row)?,
        })
    }

    async fn read_staging_edges(
        client: &Client,
        bucket: &str,
        key: &str,
    ) -> Vec<RawEdgeObservation> {
        let resp = match client.get_object().bucket(bucket).key(key).send().await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let bytes = match resp.body.collect().await {
            Ok(b) => b.into_bytes(),
            Err(_) => return Vec::new(),
        };
        if key.ends_with(".parquet") {
            let builder = match ParquetRecordBatchReaderBuilder::try_new(bytes) {
                Ok(b) => b,
                Err(_) => return Vec::new(),
            };
            let reader = match builder.build() {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let mut rows = Vec::new();
            for batch in reader.flatten() {
                for row in 0..batch.num_rows() {
                    if let Some(edge) = Self::edge_from_batch(&batch, row) {
                        rows.push(edge);
                    }
                }
            }
            return rows;
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut rows = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(row) = serde_json::from_str::<RawEdgeObservation>(line) {
                rows.push(row);
            }
        }
        rows
    }

    fn page_staging_key_for_edge_key(edge_key: &str) -> String {
        edge_key.replacen("/edges/", "/pages/", 1)
    }

    async fn read_staging_pages(client: &Client, bucket: &str, key: &str) -> Vec<RawPageFact> {
        let resp = match client.get_object().bucket(bucket).key(key).send().await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let bytes = match resp.body.collect().await {
            Ok(b) => b.into_bytes(),
            Err(_) => return Vec::new(),
        };
        if key.ends_with(".parquet") {
            let builder = match ParquetRecordBatchReaderBuilder::try_new(bytes) {
                Ok(b) => b,
                Err(_) => return Vec::new(),
            };
            let reader = match builder.build() {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let mut rows = Vec::new();
            for batch in reader.flatten() {
                for row in 0..batch.num_rows() {
                    if let Some(page) = Self::page_from_batch(&batch, row) {
                        rows.push(page);
                    }
                }
            }
            return rows;
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut rows = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(row) = serde_json::from_str::<RawPageFact>(line) {
                rows.push(row);
            }
        }
        rows
    }

    fn edge_identity_mismatch(edge: &EdgeByTargetRow, obs: &RawEdgeObservation) -> bool {
        edge.url_from_id != obs.url_from_id
            || edge.url_to_id != obs.url_to_id
            || edge.source_domain_id != obs.domain_from_id
            || edge.target_domain_id != obs.domain_to_id
            || edge.link_context != obs.link_context
            || edge.rel_flags != obs.rel_flags
    }

    fn rel_semantics(rel_flags: u32) -> String {
        let mut rel = Vec::new();
        if rel_flags & 1 != 0 {
            rel.push("nofollow");
        }
        if rel_flags & 2 != 0 {
            rel.push("sponsored");
        }
        if rel_flags & 4 != 0 {
            rel.push("ugc");
        }
        if rel.is_empty() {
            "none".into()
        } else {
            rel.join(",")
        }
    }

    fn warc_record_id(obs: &RawEdgeObservation) -> String {
        format!(
            "{}:{}:{}",
            obs.warc_file_id, obs.warc_record_offset, obs.warc_record_length
        )
    }

    fn page_warc_record_id(page: &RawPageFact) -> String {
        format!(
            "{}:{}:{}",
            page.warc_file_id, page.warc_record_offset, page.warc_record_length
        )
    }

    fn collision_row(
        snapshot_id: &str,
        edge: &EdgeByTargetRow,
        obs: &RawEdgeObservation,
    ) -> CollisionQuarantineRow {
        CollisionQuarantineRow {
            snapshot_id: snapshot_id.to_string(),
            edge_id: edge.edge_id.clone(),
            existing_url_from_id: edge.url_from_id,
            observed_url_from_id: obs.url_from_id,
            existing_url_to_id: edge.url_to_id,
            observed_url_to_id: obs.url_to_id,
            existing_source_domain_id: edge.source_domain_id,
            observed_source_domain_id: obs.domain_from_id,
            existing_target_domain_id: edge.target_domain_id,
            observed_target_domain_id: obs.domain_to_id,
            existing_link_context: edge.link_context.clone(),
            observed_link_context: obs.link_context.clone(),
            existing_rel_flags: edge.rel_flags,
            observed_rel_flags: obs.rel_flags,
            observed_edge_observation_id: obs.edge_observation_id.clone(),
            quarantine_reason: "edge_id_collision".into(),
        }
    }

    fn rollup_edges(
        observations: Vec<RawEdgeObservation>,
        snapshot_id: &str,
    ) -> (
        Vec<EdgeByTargetRow>,
        Vec<CollisionQuarantineRow>,
        Vec<DictionaryCollisionQuarantineRow>,
    ) {
        let mut by_edge: HashMap<String, EdgeByTargetRow> = HashMap::new();
        let mut quarantine = Vec::new();
        let mut dictionary_quarantine = Vec::new();
        let mut url_values: HashMap<u64, String> = HashMap::new();
        let mut domain_values: HashMap<u64, String> = HashMap::new();
        let mut anchor_values: HashMap<u64, String> = HashMap::new();
        for obs in observations {
            if Self::record_dictionary_collision(
                snapshot_id,
                &mut url_values,
                "url_id",
                obs.url_from_id,
                &obs.url_from,
                &obs.edge_observation_id,
                &mut dictionary_quarantine,
            ) || Self::record_dictionary_collision(
                snapshot_id,
                &mut url_values,
                "url_id",
                obs.url_to_id,
                &obs.url_to,
                &obs.edge_observation_id,
                &mut dictionary_quarantine,
            ) || Self::record_dictionary_collision(
                snapshot_id,
                &mut domain_values,
                "domain_id",
                obs.domain_from_id,
                &obs.domain_from,
                &obs.edge_observation_id,
                &mut dictionary_quarantine,
            ) || Self::record_dictionary_collision(
                snapshot_id,
                &mut domain_values,
                "domain_id",
                obs.domain_to_id,
                &obs.domain_to,
                &obs.edge_observation_id,
                &mut dictionary_quarantine,
            ) || Self::record_dictionary_collision(
                snapshot_id,
                &mut anchor_values,
                "anchor_id",
                obs.anchor_id,
                &obs.anchor_text,
                &obs.edge_observation_id,
                &mut dictionary_quarantine,
            ) {
                continue;
            }
            let entry = by_edge
                .entry(obs.edge_id.clone())
                .or_insert_with(|| EdgeByTargetRow {
                    snapshot_id: snapshot_id.to_string(),
                    edge_id: obs.edge_id.clone(),
                    url_from: obs.url_from.clone(),
                    url_to: obs.url_to.clone(),
                    domain_from: obs.domain_from.clone(),
                    domain_to: obs.domain_to.clone(),
                    target_domain_hash_bucket: Self::target_domain_hash_bucket(obs.domain_to_id),
                    source_domain_hash_bucket: Self::target_domain_hash_bucket(obs.domain_from_id),
                    target_domain_id: obs.domain_to_id,
                    source_domain_id: obs.domain_from_id,
                    url_from_id: obs.url_from_id,
                    url_to_id: obs.url_to_id,
                    anchor_text: obs.anchor_text.clone(),
                    anchor_id: obs.anchor_id,
                    anchor_hash: obs.anchor_id,
                    link_context: obs.link_context.clone(),
                    rel_semantics: Self::rel_semantics(obs.rel_flags),
                    first_seen: obs.fetch_time.clone(),
                    last_seen: obs.fetch_time.clone(),
                    lost_seen_date: None,
                    state: "active".into(),
                    rel_flags: obs.rel_flags,
                    is_image_link: obs.is_image_link,
                    latest_edge_observation_id: obs.edge_observation_id.clone(),
                    cc_crawl_id: obs.cc_crawl_id.clone(),
                    warc_record_id: Self::warc_record_id(&obs),
                    page_from_rank: None,
                    http_status_from: obs.http_status_from,
                    is_broken: obs.is_broken,
                    discovered_by: obs.discovered_by.clone(),
                });
            if Self::edge_identity_mismatch(entry, &obs) {
                quarantine.push(Self::collision_row(snapshot_id, entry, &obs));
                continue;
            }
            if obs.fetch_time < entry.first_seen {
                entry.first_seen = obs.fetch_time.clone();
            }
            if obs.fetch_time > entry.last_seen {
                entry.last_seen = obs.fetch_time.clone();
                entry.latest_edge_observation_id = obs.edge_observation_id.clone();
                entry.cc_crawl_id = obs.cc_crawl_id.clone();
                entry.warc_record_id = Self::warc_record_id(&obs);
                entry.http_status_from = obs.http_status_from;
                entry.is_broken = obs.is_broken;
                entry.discovered_by = obs.discovered_by.clone();
            }
            entry.is_image_link |= obs.is_image_link;
        }
        (
            by_edge.into_values().collect(),
            quarantine,
            dictionary_quarantine,
        )
    }

    fn record_dictionary_collision(
        snapshot_id: &str,
        values: &mut HashMap<u64, String>,
        id_kind: &str,
        id_value: u64,
        observed_value: &str,
        observed_edge_observation_id: &str,
        quarantine: &mut Vec<DictionaryCollisionQuarantineRow>,
    ) -> bool {
        match values.get(&id_value) {
            Some(existing) if existing != observed_value => {
                quarantine.push(DictionaryCollisionQuarantineRow {
                    snapshot_id: snapshot_id.to_string(),
                    id_kind: id_kind.to_string(),
                    id_value,
                    existing_value: existing.clone(),
                    observed_value: observed_value.to_string(),
                    observed_edge_observation_id: observed_edge_observation_id.to_string(),
                    quarantine_reason: "deterministic_id_collision".into(),
                });
                true
            }
            Some(_) => false,
            None => {
                values.insert(id_value, observed_value.to_string());
                false
            }
        }
    }

    fn compute_domain_pagerank(
        edges: &[EdgeByTargetRow],
        snapshot_id: &str,
        damping: f64,
        max_iter: u32,
        authority_priors: &HashMap<u64, f64>,
    ) -> (Vec<PageRankRow>, u32, bool) {
        let mut nodes: HashSet<u64> = HashSet::new();
        let mut out_degree: HashMap<u64, u32> = HashMap::new();
        let mut adj: HashMap<u64, Vec<u64>> = HashMap::new();
        for e in edges {
            nodes.insert(e.source_domain_id);
            nodes.insert(e.target_domain_id);
            *out_degree.entry(e.source_domain_id).or_insert(0) += 1;
            adj.entry(e.source_domain_id)
                .or_default()
                .push(e.target_domain_id);
        }
        let n = nodes.len().max(1) as f64;
        let prior_sum: f64 = nodes.iter().filter_map(|id| authority_priors.get(id)).sum();
        let mut ranks: HashMap<u64, f64> = if prior_sum > 0.0 {
            nodes
                .iter()
                .map(|id| {
                    (
                        *id,
                        authority_priors.get(id).copied().unwrap_or(0.0) / prior_sum,
                    )
                })
                .collect()
        } else {
            nodes.iter().map(|id| (*id, 1.0 / n)).collect()
        };
        let mut iterations = 0u32;
        let mut converged = false;
        for iter in 0..max_iter {
            iterations = iter + 1;
            let mut next: HashMap<u64, f64> = HashMap::new();
            let mut dangling_mass = 0.0;
            for id in &nodes {
                let out = *out_degree.get(id).unwrap_or(&0);
                if out == 0 {
                    dangling_mass += ranks.get(id).copied().unwrap_or(0.0);
                }
            }
            let teleport = (1.0 - damping) / n;
            let dangling_share = damping * dangling_mass / n;
            for target in &nodes {
                let mut sum = teleport + dangling_share;
                for source in &nodes {
                    let out = *out_degree.get(source).unwrap_or(&0);
                    if out == 0 {
                        continue;
                    }
                    if adj.get(source).map(|t| t.contains(target)).unwrap_or(false) {
                        sum += damping * ranks.get(source).copied().unwrap_or(0.0) / out as f64;
                    }
                }
                next.insert(*target, sum);
            }
            let delta: f64 = nodes
                .iter()
                .map(|id| {
                    (next.get(id).copied().unwrap_or(0.0) - ranks.get(id).copied().unwrap_or(0.0))
                        .abs()
                })
                .sum();
            ranks = next;
            if delta < 1e-6 {
                converged = true;
                break;
            }
        }
        let mut sorted: Vec<(u64, f64)> = ranks.into_iter().collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let corpus_snapshot_id = snapshot_id.to_string();
        let rows = sorted
            .iter()
            .enumerate()
            .map(|(i, (node_id, pr))| {
                let percentile = if sorted.is_empty() {
                    0.0
                } else {
                    (i as f64 / sorted.len() as f64) * 100.0
                };
                PageRankRow {
                    corpus_snapshot_id: corpus_snapshot_id.clone(),
                    node_id: *node_id,
                    node_kind: "domain".into(),
                    pagerank: *pr,
                    rank_percentile: percentile,
                    iteration_count: iterations,
                    converged,
                }
            })
            .collect();
        (rows, iterations, converged)
    }

    async fn load_spam_model(
        client: &Client,
        bucket: &str,
        root: &str,
        model_version: &str,
    ) -> Result<SpamModelMetadata, std::io::Error> {
        let resp = client
            .get_object()
            .bucket(bucket)
            .key(format!(
                "{root}model_registry/spam/{model_version}/metadata.json"
            ))
            .send()
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?
            .into_bytes();
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }

    async fn load_domain_authority_rows(
        client: &Client,
        bucket: &str,
        key: &str,
    ) -> Vec<DomainAuthorityPriorRow> {
        let resp = match client.get_object().bucket(bucket).key(key).send().await {
            Ok(resp) => resp,
            Err(_) => return Vec::new(),
        };
        let bytes = match resp.body.collect().await {
            Ok(bytes) => bytes.into_bytes(),
            Err(_) => return Vec::new(),
        };
        let mut rows = Vec::new();
        for line in String::from_utf8_lossy(&bytes).lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(row) = serde_json::from_str::<DomainAuthorityPriorRow>(line) else {
                continue;
            };
            rows.push(row);
        }
        rows
    }

    async fn load_authority_priors(client: &Client, bucket: &str, root: &str) -> HashMap<u64, f64> {
        let authority_key = format!("{root}authority/domain_ranks/current.jsonl");
        let mut rows = Self::load_domain_authority_rows(client, bucket, &authority_key).await;
        if rows.is_empty() {
            let cc_key = format!("{root}cc/webgraph/domain_ranks/current.jsonl");
            rows = Self::load_domain_authority_rows(client, bucket, &cc_key).await;
            if !rows.is_empty() {
                let _ = Self::put_jsonl(client, bucket, &authority_key, &rows).await;
            }
        }
        let mut priors = HashMap::new();
        for row in rows {
            let weight = row
                .pagerank
                .filter(|v| *v > 0.0)
                .or_else(|| {
                    row.rank_percentile
                        .map(|pct| (101.0 - pct.clamp(0.0, 100.0)).max(1.0))
                })
                .unwrap_or(1.0);
            priors.insert(row.domain_id, weight);
        }
        priors
    }

    fn spam_scores(
        edges: &[EdgeByTargetRow],
        pagerank: &[PageRankRow],
        snapshot_id: &str,
        model: Option<&SpamModelMetadata>,
    ) -> Vec<SpamScoreRow> {
        let inference_timestamp = Utc::now().to_rfc3339();
        let Some(model) = model else {
            let domains = Self::domains(edges);
            return domains
                .into_iter()
                .map(|domain_id| SpamScoreRow {
                    corpus_snapshot_id: snapshot_id.to_string(),
                    domain_id,
                    model_version: "unavailable".into(),
                    spam_score: None,
                    spam_score_status: "missing".into(),
                    feature_schema_version: "v1".into(),
                    inference_timestamp: inference_timestamp.clone(),
                })
                .collect();
        };
        let pr_map: HashMap<u64, f64> = pagerank
            .iter()
            .map(|r| (r.node_id, r.rank_percentile))
            .collect();
        let mut in_degree: HashMap<u64, u32> = HashMap::new();
        let mut out_degree: HashMap<u64, u32> = HashMap::new();
        for e in edges {
            *in_degree.entry(e.target_domain_id).or_insert(0) += 1;
            *out_degree.entry(e.source_domain_id).or_insert(0) += 1;
        }
        let mut domains: HashSet<u64> = HashSet::new();
        domains.extend(in_degree.keys().copied());
        domains.extend(out_degree.keys().copied());
        domains
            .into_iter()
            .map(|domain_id| {
                let out = *out_degree.get(&domain_id).unwrap_or(&0) as f64;
                let inn = *in_degree.get(&domain_id).unwrap_or(&0) as f64;
                let ratio = if inn > 0.0 { out / inn } else { out };
                let pr_pct = pr_map.get(&domain_id).copied().unwrap_or(50.0);
                let logit = model.intercept
                    + model.coefficients.in_degree * inn
                    + model.coefficients.out_degree * out
                    + model.coefficients.outbound_inbound_ratio * ratio
                    + model.coefficients.pagerank_percentile * pr_pct;
                let probability = 1.0 / (1.0 + (-logit).exp());
                let spam_score = (probability * 100.0).round().clamp(0.0, 100.0) as i32;
                SpamScoreRow {
                    corpus_snapshot_id: snapshot_id.to_string(),
                    domain_id,
                    model_version: model.model_version.clone(),
                    spam_score: Some(spam_score),
                    spam_score_status: "scored".into(),
                    feature_schema_version: model.feature_schema_version.clone(),
                    inference_timestamp: inference_timestamp.clone(),
                }
            })
            .collect()
    }

    fn domains(edges: &[EdgeByTargetRow]) -> HashSet<u64> {
        let mut domains = HashSet::new();
        for edge in edges {
            domains.insert(edge.source_domain_id);
            domains.insert(edge.target_domain_id);
        }
        domains
    }

    fn node_rows(edges: &[EdgeByTargetRow], snapshot_id: &str) -> Vec<NodeRow> {
        Self::domains(edges)
            .into_iter()
            .map(|node_id| NodeRow {
                corpus_snapshot_id: snapshot_id.to_string(),
                node_id,
                node_kind: "domain".into(),
            })
            .collect()
    }

    fn domain_feature_rows(
        edges: &[EdgeByTargetRow],
        pagerank: &[PageRankRow],
        snapshot_id: &str,
    ) -> Vec<DomainFeatureRow> {
        let mut in_degree: HashMap<u64, u32> = HashMap::new();
        let mut out_degree: HashMap<u64, u32> = HashMap::new();
        for edge in edges {
            *in_degree.entry(edge.target_domain_id).or_insert(0) += 1;
            *out_degree.entry(edge.source_domain_id).or_insert(0) += 1;
        }
        let pr_map: HashMap<u64, f64> = pagerank
            .iter()
            .map(|row| (row.node_id, row.rank_percentile))
            .collect();
        Self::domains(edges)
            .into_iter()
            .map(|domain_id| DomainFeatureRow {
                corpus_snapshot_id: snapshot_id.to_string(),
                domain_id,
                in_degree: *in_degree.get(&domain_id).unwrap_or(&0),
                out_degree: *out_degree.get(&domain_id).unwrap_or(&0),
                pagerank_percentile: pr_map.get(&domain_id).copied().unwrap_or(0.0),
                feature_schema_version: "v1".into(),
            })
            .collect()
    }

    fn apply_page_from_rank(edges: &mut [EdgeByTargetRow], pagerank: &[PageRankRow]) {
        let rank_by_domain: HashMap<u64, f64> = pagerank
            .iter()
            .map(|row| (row.node_id, row.rank_percentile))
            .collect();
        for edge in edges {
            edge.page_from_rank = rank_by_domain.get(&edge.source_domain_id).copied();
        }
    }

    fn anchor_feature_rows(edges: &[EdgeByTargetRow], snapshot_id: &str) -> Vec<AnchorFeatureRow> {
        let mut counts: HashMap<(u64, u64), u32> = HashMap::new();
        for edge in edges {
            *counts
                .entry((edge.anchor_id, edge.target_domain_id))
                .or_insert(0) += 1;
        }
        counts
            .into_iter()
            .map(
                |((anchor_id, target_domain_id), edge_count)| AnchorFeatureRow {
                    corpus_snapshot_id: snapshot_id.to_string(),
                    anchor_id,
                    target_domain_id,
                    edge_count,
                    feature_schema_version: "v1".into(),
                },
            )
            .collect()
    }

    fn page_feature_rows(edges: &[EdgeByTargetRow], snapshot_id: &str) -> Vec<PageFeatureRow> {
        let mut counts: HashMap<(u64, u64), u32> = HashMap::new();
        for edge in edges {
            *counts
                .entry((edge.url_from_id, edge.source_domain_id))
                .or_insert(0) += 1;
        }
        counts
            .into_iter()
            .map(
                |((url_id, domain_id), outbound_edge_count)| PageFeatureRow {
                    corpus_snapshot_id: snapshot_id.to_string(),
                    url_id,
                    domain_id,
                    outbound_edge_count,
                    feature_schema_version: "v1".into(),
                },
            )
            .collect()
    }

    fn target_domain_hash_bucket(target_domain_id: u64) -> u8 {
        (target_domain_id % 256) as u8
    }

    fn affected_buckets(edges: &[EdgeByTargetRow], pages: &[RawPageFact]) -> AffectedBuckets {
        let mut target_domain_ids = HashSet::new();
        let mut target_domain_hash_buckets = HashSet::new();
        let mut source_domain_hash_buckets = HashSet::new();
        let mut source_pages = HashSet::new();
        let mut page_domain_ids = HashSet::new();
        let mut page_domain_hash_buckets = HashSet::new();
        for edge in edges {
            target_domain_ids.insert(edge.target_domain_id);
            target_domain_hash_buckets.insert(edge.target_domain_hash_bucket);
            source_domain_hash_buckets.insert(edge.source_domain_hash_bucket);
            source_pages.insert((edge.source_domain_id, edge.url_from_id));
            page_domain_ids.insert(edge.source_domain_id);
            page_domain_hash_buckets.insert(Self::target_domain_hash_bucket(edge.source_domain_id));
        }
        for page in pages {
            source_pages.insert((page.domain_id, page.url_id));
            page_domain_ids.insert(page.domain_id);
            page_domain_hash_buckets.insert(Self::target_domain_hash_bucket(page.domain_id));
        }
        let mut out = AffectedBuckets {
            target_domain_ids: target_domain_ids.into_iter().collect(),
            target_domain_hash_buckets: target_domain_hash_buckets.into_iter().collect(),
            source_domain_hash_buckets: source_domain_hash_buckets.into_iter().collect(),
            source_pages: source_pages.into_iter().collect(),
            pages_by_domain_ids: page_domain_ids.into_iter().collect(),
            pages_by_domain_hash_buckets: page_domain_hash_buckets.into_iter().collect(),
        };
        out.target_domain_ids.sort_unstable();
        out.target_domain_hash_buckets.sort_unstable();
        out.source_domain_hash_buckets.sort_unstable();
        out.source_pages.sort_unstable();
        out.pages_by_domain_ids.sort_unstable();
        out.pages_by_domain_hash_buckets.sort_unstable();
        out
    }

    fn successful_page_parse(page: &RawPageFact) -> bool {
        page.parse_status.starts_with("parsed")
    }

    fn timestamp_millis(value: &str) -> Option<i64> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Ok(ts) = DateTime::parse_from_rfc3339(trimmed) {
            return Some(ts.timestamp_millis());
        }
        NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .map(|ts| ts.and_utc().timestamp_millis())
    }

    fn parse_is_newer_than_edge(parse_time: &str, prior_edge: &EdgeByTargetRow) -> bool {
        let Some(parse_ts) = Self::timestamp_millis(parse_time) else {
            return false;
        };
        let Some(last_seen_ts) = Self::timestamp_millis(&prior_edge.last_seen) else {
            return false;
        };
        parse_ts > last_seen_ts
    }

    fn latest_successful_page_parses(pages: &[RawPageFact]) -> HashMap<(u64, u64), String> {
        let mut parsed_pages = HashMap::new();
        for page in pages
            .iter()
            .filter(|page| Self::successful_page_parse(page))
        {
            let Some(fetch_ts) = Self::timestamp_millis(&page.fetch_time) else {
                continue;
            };
            let key = (page.domain_id, page.url_id);
            let replace = parsed_pages
                .get(&key)
                .and_then(|existing: &String| Self::timestamp_millis(existing))
                .map(|existing_ts| fetch_ts > existing_ts)
                .unwrap_or(true);
            if replace {
                parsed_pages.insert(key, page.fetch_time.clone());
            }
        }
        parsed_pages
    }

    fn apply_lost_edges(
        prior_edges: &[EdgeByTargetRow],
        new_edges: &mut Vec<EdgeByTargetRow>,
        pages: &[RawPageFact],
        snapshot_id: &str,
    ) -> Vec<EdgeTransitionRow> {
        let mut transitions = Vec::new();
        let mut current_by_page: HashMap<(u64, u64), HashSet<String>> = HashMap::new();
        for edge in new_edges.iter() {
            current_by_page
                .entry((edge.source_domain_id, edge.url_from_id))
                .or_default()
                .insert(edge.edge_id.clone());
        }
        let parsed_pages = Self::latest_successful_page_parses(pages);
        let prior_by_edge = prior_edges
            .iter()
            .map(|edge| (edge.edge_id.as_str(), edge))
            .collect::<HashMap<_, _>>();
        for edge in new_edges.iter_mut() {
            if let Some(prior) = prior_by_edge.get(edge.edge_id.as_str()) {
                if prior.state != "lost"
                    || edge.state != "active"
                    || !Self::parse_is_newer_than_edge(&edge.last_seen, prior)
                {
                    continue;
                }
                transitions.push(EdgeTransitionRow {
                    snapshot_id: snapshot_id.to_string(),
                    edge_id: edge.edge_id.clone(),
                    source_domain_id: edge.source_domain_id,
                    url_from_id: edge.url_from_id,
                    target_domain_id: edge.target_domain_id,
                    previous_state: "lost".into(),
                    next_state: "active".into(),
                    transition_date: edge.last_seen.clone(),
                    evidence: "edge_reobserved".into(),
                });
                edge.lost_seen_date = None;
            }
        }
        for prior in prior_edges.iter().filter(|edge| edge.state == "active") {
            let page_key = (prior.source_domain_id, prior.url_from_id);
            let Some(parse_date) = parsed_pages.get(&page_key) else {
                continue;
            };
            if !Self::parse_is_newer_than_edge(parse_date, prior) {
                continue;
            }
            let current = current_by_page.get(&page_key);
            if current
                .map(|edges| edges.contains(&prior.edge_id))
                .unwrap_or(false)
            {
                continue;
            }
            let mut lost = prior.clone();
            lost.snapshot_id = snapshot_id.to_string();
            lost.state = "lost".into();
            lost.lost_seen_date = Some(parse_date.clone());
            lost.last_seen = parse_date.clone();
            transitions.push(EdgeTransitionRow {
                snapshot_id: snapshot_id.to_string(),
                edge_id: lost.edge_id.clone(),
                source_domain_id: lost.source_domain_id,
                url_from_id: lost.url_from_id,
                target_domain_id: lost.target_domain_id,
                previous_state: "active".into(),
                next_state: "lost".into(),
                transition_date: parse_date.clone(),
                evidence: "newer_source_page_parse_missing_edge".into(),
            });
            new_edges.push(lost);
        }
        transitions
    }

    fn pages_by_domain_rows(
        edges: &[EdgeByTargetRow],
        pages: &[RawPageFact],
        snapshot_id: &str,
    ) -> Vec<PageByDomainRow> {
        let mut compacted_edge_counts: HashMap<(u64, u64), u32> = HashMap::new();
        for edge in edges {
            *compacted_edge_counts
                .entry((edge.source_domain_id, edge.url_from_id))
                .or_insert(0) += 1;
        }

        let mut by_page: HashMap<(u64, u64), PageByDomainRow> = HashMap::new();
        for page in pages {
            let key = (page.domain_id, page.url_id);
            let outbound_edge_count = compacted_edge_counts.get(&key).copied().unwrap_or(0);
            let candidate = PageByDomainRow {
                snapshot_id: snapshot_id.to_string(),
                domain_hash_bucket: Self::target_domain_hash_bucket(page.domain_id),
                domain_id: page.domain_id,
                url_id: page.url_id,
                cc_crawl_id: page.cc_crawl_id.clone(),
                warc_record_id: Self::page_warc_record_id(page),
                parse_status: page.parse_status.clone(),
                fetch_status: page.fetch_status,
                content_mime_type: if page.content_mime_type.is_empty() {
                    "text/html".into()
                } else {
                    page.content_mime_type.clone()
                },
                outbound_edge_count,
                raw_link_count: page.raw_link_count,
                links_stored_count: page.stored_link_count,
                links_truncated: page.links_truncated,
                page_quality_flags: page.page_quality_flags.clone(),
                parser_version: page.parser_version.clone(),
                first_seen: page.fetch_time.clone(),
                last_seen: page.fetch_time.clone(),
            };
            by_page
                .entry(key)
                .and_modify(|existing| {
                    let first_seen = if candidate.first_seen < existing.first_seen {
                        candidate.first_seen.clone()
                    } else {
                        existing.first_seen.clone()
                    };
                    if candidate.last_seen >= existing.last_seen {
                        *existing = candidate.clone();
                        existing.first_seen = first_seen;
                    } else {
                        existing.first_seen = first_seen;
                    }
                })
                .or_insert(candidate);
        }

        for edge in edges {
            let entry = by_page
                .entry((edge.source_domain_id, edge.url_from_id))
                .or_insert_with(|| PageByDomainRow {
                    snapshot_id: snapshot_id.to_string(),
                    domain_hash_bucket: Self::target_domain_hash_bucket(edge.source_domain_id),
                    domain_id: edge.source_domain_id,
                    url_id: edge.url_from_id,
                    cc_crawl_id: edge.cc_crawl_id.clone(),
                    warc_record_id: edge.warc_record_id.clone(),
                    parse_status: "parsed".into(),
                    fetch_status: edge.http_status_from,
                    content_mime_type: "text/html".into(),
                    outbound_edge_count: 0,
                    raw_link_count: 0,
                    links_stored_count: 0,
                    links_truncated: false,
                    page_quality_flags: json!({"source": "edge_fallback"}),
                    parser_version: "link_graph_html_v1".into(),
                    first_seen: edge.first_seen.clone(),
                    last_seen: edge.last_seen.clone(),
                });
            if entry
                .page_quality_flags
                .get("source")
                .and_then(Value::as_str)
                == Some("edge_fallback")
            {
                entry.outbound_edge_count += 1;
                entry.links_stored_count += 1;
                entry.raw_link_count += 1;
            }
            if edge.first_seen < entry.first_seen {
                entry.first_seen = edge.first_seen.clone();
            }
            if edge.last_seen > entry.last_seen {
                entry.last_seen = edge.last_seen.clone();
                entry.cc_crawl_id = edge.cc_crawl_id.clone();
                entry.warc_record_id = edge.warc_record_id.clone();
            }
        }
        by_page.into_values().collect()
    }

    fn merge_page_snapshots(
        prior_rows: Vec<PageByDomainRow>,
        current_rows: Vec<PageByDomainRow>,
        snapshot_id: &str,
    ) -> Vec<PageByDomainRow> {
        let mut by_page: HashMap<(u64, u64), PageByDomainRow> = HashMap::new();
        for mut row in prior_rows {
            row.snapshot_id = snapshot_id.to_string();
            by_page.insert((row.domain_id, row.url_id), row);
        }
        for mut row in current_rows {
            row.snapshot_id = snapshot_id.to_string();
            match by_page.get_mut(&(row.domain_id, row.url_id)) {
                Some(existing) => {
                    if row.first_seen < existing.first_seen {
                        existing.first_seen = row.first_seen.clone();
                    } else {
                        row.first_seen = existing.first_seen.clone();
                    }
                    if row.last_seen >= existing.last_seen {
                        *existing = row;
                    }
                }
                None => {
                    by_page.insert((row.domain_id, row.url_id), row);
                }
            }
        }
        by_page.into_values().collect()
    }

    fn edge_partition_stats(rows: &[EdgeByTargetRow]) -> Value {
        json!({
            "min_source_domain_id": rows.iter().map(|row| row.source_domain_id).min(),
            "max_source_domain_id": rows.iter().map(|row| row.source_domain_id).max(),
            "min_url_from_id": rows.iter().map(|row| row.url_from_id).min(),
            "max_url_from_id": rows.iter().map(|row| row.url_from_id).max(),
            "min_first_seen": rows.iter().map(|row| row.first_seen.as_str()).min(),
            "max_first_seen": rows.iter().map(|row| row.first_seen.as_str()).max(),
            "min_edge_id": rows.iter().map(|row| row.edge_id.as_str()).min(),
            "max_edge_id": rows.iter().map(|row| row.edge_id.as_str()).max(),
        })
    }

    fn page_partition_stats(rows: &[PageByDomainRow]) -> Value {
        json!({
            "min_url_id": rows.iter().map(|row| row.url_id).min(),
            "max_url_id": rows.iter().map(|row| row.url_id).max(),
            "min_first_seen": rows.iter().map(|row| row.first_seen.as_str()).min(),
            "max_first_seen": rows.iter().map(|row| row.first_seen.as_str()).max(),
        })
    }

    fn edge_manifest_file_source_bucket(file: &Value) -> Option<u8> {
        file.get("source_domain_hash_bucket")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
    }

    fn hot_edge_partition_requires_full_rewrite(prior_partition: Option<&Value>) -> bool {
        let Some(files) = prior_partition
            .and_then(|partition| partition.get("files"))
            .and_then(Value::as_array)
        else {
            return true;
        };
        files.is_empty()
            || files
                .iter()
                .any(|file| Self::edge_manifest_file_source_bucket(file).is_none())
    }

    async fn put_edges_by_target_index(
        client: &Client,
        bucket: &str,
        root: &str,
        snapshot_id: &str,
        edges: &[EdgeByTargetRow],
        prior_manifest: Option<&Value>,
        affected: &AffectedBuckets,
    ) -> Result<Value, std::io::Error> {
        let mut grouped: HashMap<u64, Vec<EdgeByTargetRow>> = HashMap::new();
        for edge in edges {
            grouped
                .entry(edge.target_domain_id)
                .or_default()
                .push(edge.clone());
        }
        let affected_targets: HashSet<u64> = affected.target_domain_ids.iter().copied().collect();
        let affected_source_buckets: HashSet<u8> = affected
            .source_domain_hash_buckets
            .iter()
            .copied()
            .collect();
        let prior_partitions = prior_manifest
            .and_then(|manifest| manifest.get("partitions").and_then(Value::as_array))
            .cloned()
            .unwrap_or_default();
        let mut partitions = Vec::new();
        let mut hot_domain_manifests = Vec::new();
        for partition in &prior_partitions {
            let Some(target_domain_id) = partition.get("target_domain_id").and_then(Value::as_u64)
            else {
                continue;
            };
            if !affected_targets.contains(&target_domain_id) {
                partitions.push(partition.clone());
            }
        }
        for (target_domain_id, mut rows) in grouped {
            if prior_manifest.is_some() && !affected_targets.contains(&target_domain_id) {
                continue;
            }
            let hash_bucket = Self::target_domain_hash_bucket(target_domain_id);
            rows.sort_by(|a, b| {
                (
                    a.source_domain_id,
                    a.url_from_id,
                    a.first_seen.as_str(),
                    a.edge_id.as_str(),
                )
                    .cmp(&(
                        b.source_domain_id,
                        b.url_from_id,
                        b.first_seen.as_str(),
                        b.edge_id.as_str(),
                    ))
            });
            let base_prefix = format!("{root}indexes/edges_by_target/snapshot_id={snapshot_id}/target_domain_hash_bucket={hash_bucket:03}/target_domain_id={target_domain_id}/");
            let hot_domain = rows.len() >= HOT_DOMAIN_EDGE_SPLIT_THRESHOLD;
            let prior_partition = prior_partitions.iter().find(|partition| {
                partition.get("target_domain_id").and_then(Value::as_u64) == Some(target_domain_id)
            });
            let full_rewrite = !hot_domain
                || prior_manifest.is_none()
                || Self::hot_edge_partition_requires_full_rewrite(prior_partition);
            let mut domain_files = Vec::new();
            if hot_domain {
                if !full_rewrite {
                    if let Some(prior_partition) = prior_partition {
                        if let Some(files) = prior_partition.get("files").and_then(Value::as_array)
                        {
                            domain_files.extend(files.iter().filter_map(|file| {
                                let source_bucket = Self::edge_manifest_file_source_bucket(file)?;
                                if affected_source_buckets.contains(&source_bucket) {
                                    None
                                } else {
                                    Some(file.clone())
                                }
                            }));
                        }
                    }
                }
                let mut by_source_bucket: HashMap<u8, Vec<EdgeByTargetRow>> = HashMap::new();
                for row in rows {
                    if !full_rewrite
                        && prior_manifest.is_some()
                        && !affected_source_buckets.contains(&row.source_domain_hash_bucket)
                    {
                        continue;
                    }
                    by_source_bucket
                        .entry(row.source_domain_hash_bucket)
                        .or_default()
                        .push(row);
                }
                let mut buckets = by_source_bucket.into_iter().collect::<Vec<_>>();
                buckets.sort_by_key(|(bucket, _)| *bucket);
                for (source_bucket, bucket_rows) in buckets {
                    let key = format!("{base_prefix}source_domain_hash_bucket={source_bucket:03}/part-00000.parquet");
                    let stats = Self::edge_partition_stats(&bucket_rows);
                    let bytes = Self::put_parquet_object(
                        client,
                        bucket,
                        &key,
                        Self::edges_batch(&bucket_rows)?,
                    )
                    .await?;
                    domain_files.push(json!({
                        "key": key,
                        "source_domain_hash_bucket": source_bucket,
                        "row_count": bucket_rows.len(),
                        "byte_count": bytes,
                        "stats": stats,
                    }));
                }
            } else {
                let key = format!("{base_prefix}part-00000.parquet");
                let stats = Self::edge_partition_stats(&rows);
                let bytes =
                    Self::put_parquet_object(client, bucket, &key, Self::edges_batch(&rows)?)
                        .await?;
                domain_files.push(json!({
                    "key": key,
                    "row_count": rows.len(),
                    "byte_count": bytes,
                    "stats": stats,
                }));
            }
            let domain_manifest = json!({
                "index": "edges_by_target",
                "snapshot_id": snapshot_id,
                "format": "parquet",
                "compression": "zstd",
                "target_domain_hash_bucket": hash_bucket,
                "target_domain_id": target_domain_id,
                "hot_domain": hot_domain,
                "sort_order": ["source_domain_id", "url_from_id", "first_seen", "edge_id"],
                "carried_forward": false,
                "full_rewrite": full_rewrite,
                "files": domain_files,
            });
            let manifest_key = format!("{base_prefix}manifest.json");
            Self::put_json_value(client, bucket, &manifest_key, &domain_manifest).await?;
            if hot_domain {
                hot_domain_manifests.push(json!({
                    "target_domain_id": target_domain_id,
                    "target_domain_hash_bucket": hash_bucket,
                    "manifest_key": manifest_key,
                    "source_bucket_count": domain_manifest
                        .get("files")
                        .and_then(Value::as_array)
                        .map(|files| files.len())
                        .unwrap_or(0),
                }));
            }
            partitions.push(domain_manifest);
        }
        let index_manifest = json!({
            "index": "edges_by_target",
            "snapshot_id": snapshot_id,
            "format": "parquet",
            "compression": "zstd",
            "partitioning": ["snapshot_id", "target_domain_hash_bucket", "target_domain_id", "source_domain_hash_bucket"],
            "sort_order": ["source_domain_id", "url_from_id", "first_seen", "edge_id"],
            "row_count": edges.len(),
            "partition_count": partitions.len(),
            "affected_partition_count": affected_targets.len(),
            "hot_domain_manifests": hot_domain_manifests,
            "partitions": partitions,
        });
        Self::put_json_value(
            client,
            bucket,
            &format!("{root}indexes/edges_by_target/snapshot_id={snapshot_id}/manifest.json"),
            &index_manifest,
        )
        .await?;
        Ok(index_manifest)
    }

    async fn put_pages_by_domain_index(
        client: &Client,
        bucket: &str,
        root: &str,
        snapshot_id: &str,
        rows: &[PageByDomainRow],
        prior_manifest: Option<&Value>,
        affected: &AffectedBuckets,
    ) -> Result<Value, std::io::Error> {
        let mut grouped: HashMap<u64, Vec<PageByDomainRow>> = HashMap::new();
        for row in rows {
            grouped.entry(row.domain_id).or_default().push(row.clone());
        }
        let affected_domains: HashSet<u64> = affected.pages_by_domain_ids.iter().copied().collect();
        let prior_partitions = prior_manifest
            .and_then(|manifest| manifest.get("partitions").and_then(Value::as_array))
            .cloned()
            .unwrap_or_default();
        let mut partitions = Vec::new();
        for partition in &prior_partitions {
            let Some(domain_id) = partition.get("domain_id").and_then(Value::as_u64) else {
                continue;
            };
            if !affected_domains.contains(&domain_id) {
                partitions.push(partition.clone());
            }
        }
        for (domain_id, mut rows) in grouped {
            if prior_manifest.is_some() && !affected_domains.contains(&domain_id) {
                continue;
            }
            let hash_bucket = Self::target_domain_hash_bucket(domain_id);
            rows.sort_by_key(|row| row.url_id);
            let key = format!("{root}indexes/pages_by_domain/snapshot_id={snapshot_id}/domain_hash_bucket={hash_bucket:03}/domain_id={domain_id}/part-00000.parquet");
            let stats = Self::page_partition_stats(&rows);
            let bytes =
                Self::put_parquet_object(client, bucket, &key, Self::pages_batch(&rows)?).await?;
            partitions.push(json!({
                "index": "pages_by_domain",
                "snapshot_id": snapshot_id,
                "format": "parquet",
                "compression": "zstd",
                "domain_hash_bucket": hash_bucket,
                "domain_id": domain_id,
                "sort_order": ["url_id"],
                "carried_forward": false,
                "files": [{
                    "key": key,
                    "row_count": rows.len(),
                    "byte_count": bytes,
                    "stats": stats,
                }],
            }));
        }
        let index_manifest = json!({
            "index": "pages_by_domain",
            "snapshot_id": snapshot_id,
            "format": "parquet",
            "compression": "zstd",
            "partitioning": ["snapshot_id", "domain_hash_bucket", "domain_id"],
            "sort_order": ["url_id"],
            "row_count": rows.len(),
            "partition_count": partitions.len(),
            "affected_partition_count": affected_domains.len(),
            "partitions": partitions,
        });
        Self::put_json_value(
            client,
            bucket,
            &format!("{root}indexes/pages_by_domain/snapshot_id={snapshot_id}/manifest.json"),
            &index_manifest,
        )
        .await?;
        Ok(index_manifest)
    }

    async fn write_lance_batch(uri: String, batch: RecordBatch) -> Result<(), std::io::Error> {
        let schema = batch.schema();
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        let mut params = WriteParams::default();
        params.mode = WriteMode::Overwrite;
        Dataset::write(reader, uri.as_str(), Some(params))
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        Ok(())
    }

    fn lance_uri(bucket: &str, root: &str, artifact: &str, snapshot_id: &str) -> String {
        format!("s3://{bucket}/{root}lance/{artifact}.lance/snapshot_id={snapshot_id}")
    }

    fn edges_batch(rows: &[EdgeByTargetRow]) -> Result<RecordBatch, std::io::Error> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("snapshot_id", DataType::Utf8, false),
            Field::new("edge_id", DataType::Utf8, false),
            Field::new("url_from", DataType::Utf8, false),
            Field::new("url_to", DataType::Utf8, false),
            Field::new("domain_from", DataType::Utf8, false),
            Field::new("domain_to", DataType::Utf8, false),
            Field::new("target_domain_hash_bucket", DataType::UInt8, false),
            Field::new("source_domain_hash_bucket", DataType::UInt8, false),
            Field::new("target_domain_id", DataType::UInt64, false),
            Field::new("source_domain_id", DataType::UInt64, false),
            Field::new("url_from_id", DataType::UInt64, false),
            Field::new("url_to_id", DataType::UInt64, false),
            Field::new("anchor_text", DataType::Utf8, false),
            Field::new("anchor_id", DataType::UInt64, false),
            Field::new("anchor_hash", DataType::UInt64, false),
            Field::new("link_context", DataType::Utf8, false),
            Field::new("rel_semantics", DataType::Utf8, false),
            Field::new("first_seen", DataType::Utf8, false),
            Field::new("last_seen", DataType::Utf8, false),
            Field::new("lost_seen_date", DataType::Utf8, true),
            Field::new("state", DataType::Utf8, false),
            Field::new("rel_flags", DataType::UInt32, false),
            Field::new("is_image_link", DataType::Boolean, false),
            Field::new("latest_edge_observation_id", DataType::Utf8, false),
            Field::new("cc_crawl_id", DataType::Utf8, false),
            Field::new("warc_record_id", DataType::Utf8, false),
            Field::new("page_from_rank", DataType::Float64, true),
            Field::new("http_status_from", DataType::UInt32, true),
            Field::new("is_broken", DataType::Boolean, false),
            Field::new("discovered_by", DataType::Utf8, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.snapshot_id.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.edge_id.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.url_from.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.url_to.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.domain_from.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.domain_to.as_str()),
                )),
                Arc::new(UInt8Array::from_iter_values(
                    rows.iter().map(|r| r.target_domain_hash_bucket),
                )),
                Arc::new(UInt8Array::from_iter_values(
                    rows.iter().map(|r| r.source_domain_hash_bucket),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.target_domain_id),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.source_domain_id),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.url_from_id),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.url_to_id),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.anchor_text.as_str()),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.anchor_id),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.anchor_hash),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.link_context.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.rel_semantics.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.first_seen.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.last_seen.as_str()),
                )),
                Arc::new(StringArray::from_iter(
                    rows.iter().map(|r| r.lost_seen_date.as_deref()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.state.as_str()),
                )),
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|r| r.rel_flags),
                )),
                Arc::new(BooleanArray::from_iter(
                    rows.iter().map(|r| Some(r.is_image_link)),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.latest_edge_observation_id.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.cc_crawl_id.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.warc_record_id.as_str()),
                )),
                Arc::new(Float64Array::from_iter(
                    rows.iter().map(|r| r.page_from_rank),
                )),
                Arc::new(UInt32Array::from_iter(
                    rows.iter().map(|r| r.http_status_from),
                )),
                Arc::new(BooleanArray::from_iter(
                    rows.iter().map(|r| Some(r.is_broken)),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.discovered_by.as_str()),
                )),
            ],
        )
        .map_err(std::io::Error::other)
    }

    fn pages_batch(rows: &[PageByDomainRow]) -> Result<RecordBatch, std::io::Error> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("snapshot_id", DataType::Utf8, false),
            Field::new("domain_hash_bucket", DataType::UInt8, false),
            Field::new("domain_id", DataType::UInt64, false),
            Field::new("url_id", DataType::UInt64, false),
            Field::new("cc_crawl_id", DataType::Utf8, false),
            Field::new("warc_record_id", DataType::Utf8, false),
            Field::new("parse_status", DataType::Utf8, false),
            Field::new("fetch_status", DataType::UInt32, true),
            Field::new("content_mime_type", DataType::Utf8, false),
            Field::new("outbound_edge_count", DataType::UInt32, false),
            Field::new("raw_link_count", DataType::UInt32, false),
            Field::new("links_stored_count", DataType::UInt32, false),
            Field::new("links_truncated", DataType::Boolean, false),
            Field::new("page_quality_flags", DataType::Utf8, false),
            Field::new("parser_version", DataType::Utf8, false),
            Field::new("first_seen", DataType::Utf8, false),
            Field::new("last_seen", DataType::Utf8, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.snapshot_id.as_str()),
                )),
                Arc::new(UInt8Array::from_iter_values(
                    rows.iter().map(|r| r.domain_hash_bucket),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.domain_id),
                )),
                Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.url_id))),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.cc_crawl_id.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.warc_record_id.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.parse_status.as_str()),
                )),
                Arc::new(UInt32Array::from_iter(rows.iter().map(|r| r.fetch_status))),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.content_mime_type.as_str()),
                )),
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|r| r.outbound_edge_count),
                )),
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|r| r.raw_link_count),
                )),
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|r| r.links_stored_count),
                )),
                Arc::new(BooleanArray::from_iter(
                    rows.iter().map(|r| Some(r.links_truncated)),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.page_quality_flags.to_string()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.parser_version.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.first_seen.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.last_seen.as_str()),
                )),
            ],
        )
        .map_err(std::io::Error::other)
    }

    fn nodes_batch(rows: &[NodeRow]) -> Result<RecordBatch, std::io::Error> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("corpus_snapshot_id", DataType::Utf8, false),
            Field::new("node_id", DataType::UInt64, false),
            Field::new("node_kind", DataType::Utf8, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.corpus_snapshot_id.as_str()),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.node_id),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.node_kind.as_str()),
                )),
            ],
        )
        .map_err(std::io::Error::other)
    }

    fn pagerank_batch(rows: &[PageRankRow]) -> Result<RecordBatch, std::io::Error> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("corpus_snapshot_id", DataType::Utf8, false),
            Field::new("node_id", DataType::UInt64, false),
            Field::new("node_kind", DataType::Utf8, false),
            Field::new("pagerank", DataType::Float64, false),
            Field::new("rank_percentile", DataType::Float64, false),
            Field::new("iteration_count", DataType::UInt32, false),
            Field::new("converged", DataType::Boolean, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.corpus_snapshot_id.as_str()),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.node_id),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.node_kind.as_str()),
                )),
                Arc::new(Float64Array::from_iter_values(
                    rows.iter().map(|r| r.pagerank),
                )),
                Arc::new(Float64Array::from_iter_values(
                    rows.iter().map(|r| r.rank_percentile),
                )),
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|r| r.iteration_count),
                )),
                Arc::new(BooleanArray::from_iter(
                    rows.iter().map(|r| Some(r.converged)),
                )),
            ],
        )
        .map_err(std::io::Error::other)
    }

    fn domain_features_batch(rows: &[DomainFeatureRow]) -> Result<RecordBatch, std::io::Error> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("corpus_snapshot_id", DataType::Utf8, false),
            Field::new("domain_id", DataType::UInt64, false),
            Field::new("in_degree", DataType::UInt32, false),
            Field::new("out_degree", DataType::UInt32, false),
            Field::new("pagerank_percentile", DataType::Float64, false),
            Field::new("feature_schema_version", DataType::Utf8, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.corpus_snapshot_id.as_str()),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.domain_id),
                )),
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|r| r.in_degree),
                )),
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|r| r.out_degree),
                )),
                Arc::new(Float64Array::from_iter_values(
                    rows.iter().map(|r| r.pagerank_percentile),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.feature_schema_version.as_str()),
                )),
            ],
        )
        .map_err(std::io::Error::other)
    }

    fn anchor_features_batch(rows: &[AnchorFeatureRow]) -> Result<RecordBatch, std::io::Error> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("corpus_snapshot_id", DataType::Utf8, false),
            Field::new("anchor_id", DataType::UInt64, false),
            Field::new("target_domain_id", DataType::UInt64, false),
            Field::new("edge_count", DataType::UInt32, false),
            Field::new("feature_schema_version", DataType::Utf8, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.corpus_snapshot_id.as_str()),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.anchor_id),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.target_domain_id),
                )),
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|r| r.edge_count),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.feature_schema_version.as_str()),
                )),
            ],
        )
        .map_err(std::io::Error::other)
    }

    fn page_features_batch(rows: &[PageFeatureRow]) -> Result<RecordBatch, std::io::Error> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("corpus_snapshot_id", DataType::Utf8, false),
            Field::new("url_id", DataType::UInt64, false),
            Field::new("domain_id", DataType::UInt64, false),
            Field::new("outbound_edge_count", DataType::UInt32, false),
            Field::new("feature_schema_version", DataType::Utf8, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.corpus_snapshot_id.as_str()),
                )),
                Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.url_id))),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.domain_id),
                )),
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|r| r.outbound_edge_count),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.feature_schema_version.as_str()),
                )),
            ],
        )
        .map_err(std::io::Error::other)
    }

    fn spam_batch(rows: &[SpamScoreRow]) -> Result<RecordBatch, std::io::Error> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("corpus_snapshot_id", DataType::Utf8, false),
            Field::new("domain_id", DataType::UInt64, false),
            Field::new("model_version", DataType::Utf8, false),
            Field::new("spam_score", DataType::Int32, true),
            Field::new("spam_score_status", DataType::Utf8, false),
            Field::new("feature_schema_version", DataType::Utf8, false),
            Field::new("inference_timestamp", DataType::Utf8, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.corpus_snapshot_id.as_str()),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.domain_id),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.model_version.as_str()),
                )),
                Arc::new(Int32Array::from_iter(rows.iter().map(|r| r.spam_score))),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.spam_score_status.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.feature_schema_version.as_str()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|r| r.inference_timestamp.as_str()),
                )),
            ],
        )
        .map_err(std::io::Error::other)
    }

    async fn put_jsonl<T: Serialize>(
        client: &Client,
        bucket: &str,
        key: &str,
        rows: &[T],
    ) -> Result<(), std::io::Error> {
        let mut body = String::new();
        for row in rows {
            body.push_str(&serde_json::to_string(row).map_err(std::io::Error::other)?);
            body.push('\n');
        }
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("application/x-ndjson")
            .body(aws_sdk_s3::primitives::ByteStream::from(body.into_bytes()))
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(last_seen: &str) -> EdgeByTargetRow {
        EdgeByTargetRow {
            snapshot_id: "snap-prior".into(),
            edge_id: "edge-1".into(),
            url_from: "https://source.example/page".into(),
            url_to: "https://target.example/".into(),
            domain_from: "source.example".into(),
            domain_to: "target.example".into(),
            target_domain_hash_bucket: 2,
            source_domain_hash_bucket: 1,
            target_domain_id: 20,
            source_domain_id: 10,
            url_from_id: 100,
            url_to_id: 200,
            anchor_text: "Target".into(),
            anchor_id: 300,
            anchor_hash: 301,
            link_context: "body".into(),
            rel_semantics: "follow".into(),
            first_seen: "2025-01-01T00:00:00Z".into(),
            last_seen: last_seen.into(),
            lost_seen_date: None,
            state: "active".into(),
            rel_flags: 0,
            is_image_link: false,
            latest_edge_observation_id: "obs-1".into(),
            cc_crawl_id: "CC-MAIN-2025-08".into(),
            warc_record_id: "warc:1:2".into(),
            page_from_rank: None,
            http_status_from: Some(200),
            is_broken: false,
            discovered_by: "cc_warc".into(),
        }
    }

    fn parsed_page(fetch_time: &str) -> RawPageFact {
        RawPageFact {
            url_id: 100,
            domain_id: 10,
            cc_crawl_id: "CC-MAIN-2025-08".into(),
            warc_file_id: 1,
            warc_record_offset: 1,
            warc_record_length: 2,
            fetch_status: Some(200),
            content_mime_type: "text/html".into(),
            fetch_time: fetch_time.into(),
            outbound_link_count: 0,
            stored_link_count: 0,
            links_truncated: false,
            raw_link_count: 0,
            page_quality_flags: json!({}),
            canonicalization_version: "urlcanon_v1".into(),
            parser_version: "link_graph_html_v1".into(),
            parse_status: "parsed_warc".into(),
        }
    }

    fn page_row(domain_id: u64, url_id: u64, first_seen: &str, last_seen: &str) -> PageByDomainRow {
        PageByDomainRow {
            snapshot_id: "snap-prior".into(),
            domain_hash_bucket: UpfoundryLinkGraphCompactPlugin::target_domain_hash_bucket(
                domain_id,
            ),
            domain_id,
            url_id,
            cc_crawl_id: "CC-MAIN-2025-08".into(),
            warc_record_id: format!("warc:{domain_id}:{url_id}"),
            parse_status: "parsed".into(),
            fetch_status: Some(200),
            content_mime_type: "text/html".into(),
            outbound_edge_count: 1,
            raw_link_count: 1,
            links_stored_count: 1,
            links_truncated: false,
            page_quality_flags: json!({}),
            parser_version: "link_graph_html_v1".into(),
            first_seen: first_seen.into(),
            last_seen: last_seen.into(),
        }
    }

    #[test]
    fn merge_page_snapshots_preserves_prior_unseen_page() {
        let prior = vec![page_row(
            10,
            100,
            "2025-01-01T00:00:00Z",
            "2025-01-01T00:00:00Z",
        )];
        let current = vec![page_row(
            10,
            200,
            "2025-03-01T00:00:00Z",
            "2025-03-01T00:00:00Z",
        )];

        let merged =
            UpfoundryLinkGraphCompactPlugin::merge_page_snapshots(prior, current, "snap-current");

        assert_eq!(merged.len(), 2);
        assert!(merged
            .iter()
            .any(|row| row.domain_id == 10 && row.url_id == 100));
        assert!(merged.iter().all(|row| row.snapshot_id == "snap-current"));
    }

    #[test]
    fn merge_page_snapshots_updates_existing_page_but_preserves_first_seen() {
        let prior = vec![page_row(
            10,
            100,
            "2025-01-01T00:00:00Z",
            "2025-01-01T00:00:00Z",
        )];
        let mut current_row = page_row(10, 100, "2025-03-01T00:00:00Z", "2025-03-01T00:00:00Z");
        current_row.outbound_edge_count = 7;

        let merged = UpfoundryLinkGraphCompactPlugin::merge_page_snapshots(
            prior,
            vec![current_row],
            "snap-current",
        );
        let row = merged.first().expect("merged page row");

        assert_eq!(merged.len(), 1);
        assert_eq!(row.first_seen, "2025-01-01T00:00:00Z");
        assert_eq!(row.last_seen, "2025-03-01T00:00:00Z");
        assert_eq!(row.outbound_edge_count, 7);
        assert_eq!(row.snapshot_id, "snap-current");
    }

    #[test]
    fn hot_partition_with_unbucketed_prior_file_requires_full_rewrite() {
        let partition = json!({
            "target_domain_id": 20,
            "files": [{
                "key": "indexes/edges_by_target/snapshot_id=snap-old/target_domain_id=20/part-00000.parquet",
                "row_count": 250_000
            }]
        });

        assert!(
            UpfoundryLinkGraphCompactPlugin::hot_edge_partition_requires_full_rewrite(Some(
                &partition
            ))
        );
    }

    #[test]
    fn hot_partition_with_bucketed_prior_files_can_carry_forward() {
        let partition = json!({
            "target_domain_id": 20,
            "files": [
                {
                    "key": "indexes/edges_by_target/snapshot_id=snap-old/target_domain_id=20/source_domain_hash_bucket=001/part-00000.parquet",
                    "source_domain_hash_bucket": 1,
                    "row_count": 125_000
                },
                {
                    "key": "indexes/edges_by_target/snapshot_id=snap-old/target_domain_id=20/source_domain_hash_bucket=002/part-00000.parquet",
                    "source_domain_hash_bucket": 2,
                    "row_count": 125_000
                }
            ]
        });

        assert!(
            !UpfoundryLinkGraphCompactPlugin::hot_edge_partition_requires_full_rewrite(Some(
                &partition
            ))
        );
    }

    #[test]
    fn older_successful_parse_does_not_mark_prior_edge_lost() {
        let prior = vec![edge("2025-02-01T00:00:00Z")];
        let pages = vec![parsed_page("2025-01-01T00:00:00Z")];
        let mut new_edges = Vec::new();

        let transitions = UpfoundryLinkGraphCompactPlugin::apply_lost_edges(
            &prior,
            &mut new_edges,
            &pages,
            "snap-current",
        );

        assert!(transitions.is_empty());
        assert!(new_edges.is_empty());
    }

    #[test]
    fn newer_successful_parse_marks_missing_prior_edge_lost() {
        let prior = vec![edge("2025-02-01T00:00:00Z")];
        let pages = vec![parsed_page("2025-03-01T00:00:00Z")];
        let mut new_edges = Vec::new();

        let transitions = UpfoundryLinkGraphCompactPlugin::apply_lost_edges(
            &prior,
            &mut new_edges,
            &pages,
            "snap-current",
        );

        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].previous_state, "active");
        assert_eq!(transitions[0].next_state, "lost");
        assert_eq!(new_edges.len(), 1);
        assert_eq!(new_edges[0].state, "lost");
        assert_eq!(
            new_edges[0].lost_seen_date.as_deref(),
            Some("2025-03-01T00:00:00Z")
        );
    }

    #[test]
    fn newer_reobserved_lost_edge_emits_regained_transition() {
        let mut lost = edge("2025-02-01T00:00:00Z");
        lost.state = "lost".into();
        lost.lost_seen_date = Some("2025-02-01T00:00:00Z".into());
        let mut reobserved = edge("2025-03-01T00:00:00Z");
        reobserved.latest_edge_observation_id = "obs-2".into();
        let prior = vec![lost];
        let mut new_edges = vec![reobserved];

        let transitions = UpfoundryLinkGraphCompactPlugin::apply_lost_edges(
            &prior,
            &mut new_edges,
            &[],
            "snap-current",
        );
        let merged =
            UpfoundryLinkGraphCompactPlugin::merge_edge_snapshots(prior, new_edges, "snap-current");
        let merged_edge = merged
            .iter()
            .find(|edge| edge.edge_id == "edge-1")
            .expect("merged edge");

        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].previous_state, "lost");
        assert_eq!(transitions[0].next_state, "active");
        assert_eq!(transitions[0].evidence, "edge_reobserved");
        assert_eq!(transitions[0].transition_date, "2025-03-01T00:00:00Z");
        assert_eq!(merged_edge.state, "active");
        assert_eq!(merged_edge.first_seen, "2025-01-01T00:00:00Z");
        assert_eq!(merged_edge.last_seen, "2025-03-01T00:00:00Z");
        assert_eq!(merged_edge.lost_seen_date, None);
    }

    #[test]
    fn stale_reobserved_lost_edge_does_not_emit_regained_transition() {
        let mut lost = edge("2025-03-01T00:00:00Z");
        lost.state = "lost".into();
        lost.lost_seen_date = Some("2025-03-01T00:00:00Z".into());
        let prior = vec![lost];
        let mut new_edges = vec![edge("2025-02-01T00:00:00Z")];

        let transitions = UpfoundryLinkGraphCompactPlugin::apply_lost_edges(
            &prior,
            &mut new_edges,
            &[],
            "snap-current",
        );
        let merged =
            UpfoundryLinkGraphCompactPlugin::merge_edge_snapshots(prior, new_edges, "snap-current");
        let merged_edge = merged
            .iter()
            .find(|edge| edge.edge_id == "edge-1")
            .expect("merged edge");

        assert!(transitions.is_empty());
        assert_eq!(merged_edge.state, "lost");
        assert_eq!(
            merged_edge.lost_seen_date.as_deref(),
            Some("2025-03-01T00:00:00Z")
        );
    }
}

#[async_trait]
impl DataSource for UpfoundryLinkGraphCompactPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(
        &self,
    ) -> Vec<skippr_runtime_sdk::plugins::source_contract::SourceNamespaceContract> {
        all_namespace_contracts()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let run_date = Utc::now().format("%Y-%m-%d").to_string();
        let snapshot_id = format!("snap-{}", Utc::now().timestamp());
        let client = Self::s3_client().await?;
        let root = self.config.corpus_root();
        let staging_prefix = format!("{root}raw/staging/edges/");
        let processed =
            Self::processed_input_partitions(&client, &self.config.ops_bucket, &root).await;
        let keys = Self::list_staging_keys(&client, &self.config.ops_bucket, &staging_prefix).await;
        let keys = keys
            .into_iter()
            .filter(|key| !processed.contains(key))
            .take(self.config.max_staging_partitions.max(1) as usize)
            .collect::<Vec<_>>();
        if keys.is_empty() {
            let payload = serde_json::to_string(&json!({
                "corpus_snapshot_id": snapshot_id,
                "run_date": run_date,
                "edge_row_count": 0,
                "status": "no_input",
            }))
            .map_err(std::io::Error::other)?;
            submit_payload_batches(
                ctx.as_ref(),
                vec![IngestBatch {
                    offset_key: OffsetKey::new(NAMESPACE_COMPACT_RUN, run_date.clone()),
                    data: payload.clone(),
                    bytes: payload.len(),
                    namespace: Some(NAMESPACE_COMPACT_RUN.to_string()),
                    source_uri: format!("upfoundry-link-graph-compact://{snapshot_id}"),
                    offset_pos: None,
                    cdc_rows: None,
                }],
            )?;
            return Ok(());
        }

        let pending_manifest_key = format!("{root}manifests/pending/{snapshot_id}.json");
        Self::put_json_value(
            &client,
            &self.config.ops_bucket,
            &pending_manifest_key,
            &json!({
                "corpus_snapshot_id": snapshot_id,
                "input_partitions": keys,
                "input_high_watermarks": {
                    "raw_staging_prefix": staging_prefix,
                    "selected_partition_count": keys.len(),
                },
                "status": "pending",
                "compaction_started_at": Utc::now().to_rfc3339(),
            }),
        )
        .await?;

        let prior_snapshot_id =
            Self::latest_complete_snapshot_id(&client, &self.config.ops_bucket, &root).await;
        let prior_edges_manifest = match prior_snapshot_id.as_deref() {
            Some(prior_id) => {
                Self::load_index_manifest(
                    &client,
                    &self.config.ops_bucket,
                    &root,
                    "edges_by_target",
                    prior_id,
                )
                .await
            }
            None => None,
        };
        let prior_pages_manifest = match prior_snapshot_id.as_deref() {
            Some(prior_id) => {
                Self::load_index_manifest(
                    &client,
                    &self.config.ops_bucket,
                    &root,
                    "pages_by_domain",
                    prior_id,
                )
                .await
            }
            None => None,
        };
        let prior_edges = match prior_snapshot_id.as_deref() {
            Some(prior_id) => {
                Self::load_edges_by_target_snapshot(
                    &client,
                    &self.config.ops_bucket,
                    &root,
                    prior_id,
                )
                .await
            }
            None => Vec::new(),
        };

        let mut observations = Vec::new();
        let mut page_facts = Vec::new();
        for key in &keys {
            observations
                .extend(Self::read_staging_edges(&client, &self.config.ops_bucket, key).await);
            let page_key = Self::page_staging_key_for_edge_key(key);
            page_facts.extend(
                Self::read_staging_pages(&client, &self.config.ops_bucket, &page_key).await,
            );
        }
        let mut seen_observations = HashSet::new();
        observations.retain(|obs| seen_observations.insert(obs.edge_observation_id.clone()));
        let mut seen_pages = HashSet::new();
        page_facts.retain(|page| {
            seen_pages.insert((
                page.domain_id,
                page.url_id,
                page.cc_crawl_id.clone(),
                page.warc_file_id,
                page.warc_record_offset,
                page.warc_record_length,
            ))
        });

        let (new_edges, collision_rows, dictionary_collision_rows) =
            Self::rollup_edges(observations, &snapshot_id);
        if !collision_rows.is_empty() || !dictionary_collision_rows.is_empty() {
            Self::put_jsonl(
                &client,
                &self.config.ops_bucket,
                &format!(
                    "{root}quarantine/edge_id_collision/snapshot_id={snapshot_id}/part-00000.jsonl"
                ),
                &collision_rows,
            )
            .await?;
            Self::put_jsonl(
                &client,
                &self.config.ops_bucket,
                &format!(
                    "{root}quarantine/deterministic_id_collision/snapshot_id={snapshot_id}/part-00000.jsonl"
                ),
                &dictionary_collision_rows,
            )
            .await?;
            Self::put_json_value(
                &client,
                &self.config.ops_bucket,
                &pending_manifest_key,
                &json!({
                    "corpus_snapshot_id": snapshot_id,
                    "input_partitions": keys,
                    "status": "failed",
                    "failure_reason": "deterministic_id_collision_quarantine",
                    "collision_quarantine_count": collision_rows.len(),
                    "dictionary_collision_quarantine_count": dictionary_collision_rows.len(),
                    "compaction_failed_at": Utc::now().to_rfc3339(),
                }),
            )
            .await?;
            return Err(std::io::Error::other(format!(
                "deterministic ID quarantine contains {} edge row(s) and {} dictionary row(s)",
                collision_rows.len(),
                dictionary_collision_rows.len()
            )));
        }
        let mut new_edges = new_edges;
        let transitions =
            Self::apply_lost_edges(&prior_edges, &mut new_edges, &page_facts, &snapshot_id);
        let affected = Self::affected_buckets(&new_edges, &page_facts);
        Self::put_json_value(
            &client,
            &self.config.ops_bucket,
            &pending_manifest_key,
            &json!({
                "corpus_snapshot_id": snapshot_id,
                "input_partitions": keys,
                "input_high_watermarks": {
                    "raw_staging_prefix": staging_prefix,
                    "selected_partition_count": keys.len(),
                },
                "affected_buckets": affected.clone(),
                "edge_transition_count": transitions.len(),
                "status": "planning_complete",
                "compaction_started_at": Utc::now().to_rfc3339(),
            }),
        )
        .await?;
        let mut edges = Self::merge_edge_snapshots(prior_edges, new_edges, &snapshot_id);
        let authority_priors =
            Self::load_authority_priors(&client, &self.config.ops_bucket, &root).await;
        let (pagerank, iterations, converged) = Self::compute_domain_pagerank(
            &edges,
            &snapshot_id,
            self.config.pagerank_damping,
            self.config.pagerank_max_iterations,
            &authority_priors,
        );
        Self::apply_page_from_rank(&mut edges, &pagerank);
        let model_version = self.config.spam_model_version.clone();
        let loaded_spam_model = match model_version.as_deref() {
            Some(version) => Self::load_spam_model(
                &client,
                &self.config.ops_bucket,
                &self.config.corpus_root(),
                version,
            )
            .await
            .ok(),
            None => None,
        };
        let spam = Self::spam_scores(&edges, &pagerank, &snapshot_id, loaded_spam_model.as_ref());
        let nodes = Self::node_rows(&edges, &snapshot_id);
        let domain_features = Self::domain_feature_rows(&edges, &pagerank, &snapshot_id);
        let anchor_features = Self::anchor_feature_rows(&edges, &snapshot_id);
        let page_features = Self::page_feature_rows(&edges, &snapshot_id);
        let current_pages_by_domain = Self::pages_by_domain_rows(&edges, &page_facts, &snapshot_id);
        let affected_page_domains = affected
            .pages_by_domain_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let prior_pages_by_domain = match prior_pages_manifest.as_ref() {
            Some(manifest) if !affected_page_domains.is_empty() => {
                Self::load_pages_by_domain_manifest_domains(
                    &client,
                    &self.config.ops_bucket,
                    manifest,
                    &affected_page_domains,
                    &snapshot_id,
                )
                .await
            }
            _ => Vec::new(),
        };
        let pages_by_domain = Self::merge_page_snapshots(
            prior_pages_by_domain,
            current_pages_by_domain,
            &snapshot_id,
        );
        let spam_score_status = if loaded_spam_model.is_some() {
            "scored"
        } else {
            "missing"
        };

        let edges_index_manifest = Self::put_edges_by_target_index(
            &client,
            &self.config.ops_bucket,
            &root,
            &snapshot_id,
            &edges,
            prior_edges_manifest.as_ref(),
            &affected,
        )
        .await?;
        let pages_index_manifest = Self::put_pages_by_domain_index(
            &client,
            &self.config.ops_bucket,
            &root,
            &snapshot_id,
            &pages_by_domain,
            prior_pages_manifest.as_ref(),
            &affected,
        )
        .await?;
        if !transitions.is_empty() {
            Self::put_jsonl(
                &client,
                &self.config.ops_bucket,
                &format!("{root}transitions/edges/snapshot_id={snapshot_id}/part-00000.jsonl"),
                &transitions,
            )
            .await?;
        }
        Self::write_lance_batch(
            Self::lance_uri(
                &self.config.ops_bucket,
                &root,
                "edges_current",
                &snapshot_id,
            ),
            Self::edges_batch(&edges)?,
        )
        .await?;
        Self::write_lance_batch(
            Self::lance_uri(
                &self.config.ops_bucket,
                &root,
                "nodes_current",
                &snapshot_id,
            ),
            Self::nodes_batch(&nodes)?,
        )
        .await?;
        Self::write_lance_batch(
            Self::lance_uri(
                &self.config.ops_bucket,
                &root,
                "domain_feature_matrix",
                &snapshot_id,
            ),
            Self::domain_features_batch(&domain_features)?,
        )
        .await?;
        Self::write_lance_batch(
            Self::lance_uri(
                &self.config.ops_bucket,
                &root,
                "page_feature_matrix",
                &snapshot_id,
            ),
            Self::page_features_batch(&page_features)?,
        )
        .await?;
        Self::write_lance_batch(
            Self::lance_uri(
                &self.config.ops_bucket,
                &root,
                "anchor_feature_matrix",
                &snapshot_id,
            ),
            Self::anchor_features_batch(&anchor_features)?,
        )
        .await?;
        Self::write_lance_batch(
            Self::lance_uri(
                &self.config.ops_bucket,
                &root,
                "pagerank_scores",
                &snapshot_id,
            ),
            Self::pagerank_batch(&pagerank)?,
        )
        .await?;
        Self::write_lance_batch(
            Self::lance_uri(
                &self.config.ops_bucket,
                &root,
                "spam_feature_matrix",
                &snapshot_id,
            ),
            Self::domain_features_batch(&domain_features)?,
        )
        .await?;
        Self::write_lance_batch(
            Self::lance_uri(&self.config.ops_bucket, &root, "spam_scores", &snapshot_id),
            Self::spam_batch(&spam)?,
        )
        .await?;

        let model_metadata = json!({
            "model_version": model_version.clone().unwrap_or_else(|| "unavailable".into()),
            "feature_schema_version": "v1",
            "spam_score_status": spam_score_status,
            "configured_model_version": model_version.clone(),
            "loaded_model_version": loaded_spam_model.as_ref().map(|m| m.model_version.clone()),
            "trained_model_available": loaded_spam_model.is_some(),
            "metrics": loaded_spam_model.as_ref().map(|m| m.metrics.clone()),
            "label_counts": loaded_spam_model.as_ref().map(|m| m.label_counts.clone()),
        });
        client
            .put_object()
            .bucket(&self.config.ops_bucket)
            .key(format!(
                "{root}model_registry/spam/{}/last_inference.json",
                model_version
                    .clone()
                    .unwrap_or_else(|| "unavailable".into())
            ))
            .content_type("application/json")
            .body(aws_sdk_s3::primitives::ByteStream::from(
                serde_json::to_vec(&model_metadata).map_err(std::io::Error::other)?,
            ))
            .send()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let protected_staging_keys = keys.iter().cloned().collect::<HashSet<_>>();
        let expired_staging_deleted = Self::cleanup_expired_staging(
            &client,
            &self.config.ops_bucket,
            &root,
            self.config.keep_staging_days,
            &protected_staging_keys,
        )
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "failed to clean up expired link graph staging");
            0
        });

        let manifest = json!({
            "corpus_snapshot_id": snapshot_id,
            "input_partitions": keys,
            "pending_manifest_key": pending_manifest_key,
            "prior_snapshot_id": prior_snapshot_id.clone(),
            "affected_buckets": affected.clone(),
            "affected_partition_count": affected.target_domain_ids.len() + affected.pages_by_domain_ids.len(),
            "graph_artifacts_recomputed": true,
            "serving_indexes_incremental": true,
            "edge_transition_count": transitions.len(),
            "retention_policy": {
                "raw_promotion": "disabled",
                "warc_replay_source_of_truth": true,
                "keep_staging_days": self.config.keep_staging_days,
                "keep_complete_snapshots": self.config.keep_complete_snapshots,
                "keep_failed_manifest_days": self.config.keep_failed_manifest_days,
                "serving_snapshot_cleanup": "deferred_until_active_tenant_projection_references_are_available",
                "lance_snapshot_cleanup": "deferred_until_active_tenant_projection_references_are_available",
            },
            "expired_staging_deleted": expired_staging_deleted,
            "output_indexes": ["edges_by_target", "pages_by_domain"],
            "serving_indexes": {
                "edges_by_target": {
                    "format": "parquet",
                    "compression": "zstd",
                    "manifest_key": format!("{root}indexes/edges_by_target/snapshot_id={snapshot_id}/manifest.json"),
                    "row_count": edges_index_manifest.get("row_count").cloned().unwrap_or(Value::Null),
                    "partition_count": edges_index_manifest.get("partition_count").cloned().unwrap_or(Value::Null),
                    "hot_domain_manifest_count": edges_index_manifest
                        .get("hot_domain_manifests")
                        .and_then(Value::as_array)
                        .map(|items| items.len())
                        .unwrap_or(0),
                    "sort_order": ["source_domain_id", "url_from_id", "first_seen", "edge_id"],
                },
                "pages_by_domain": {
                    "format": "parquet",
                    "compression": "zstd",
                    "manifest_key": format!("{root}indexes/pages_by_domain/snapshot_id={snapshot_id}/manifest.json"),
                    "row_count": pages_index_manifest.get("row_count").cloned().unwrap_or(Value::Null),
                    "partition_count": pages_index_manifest.get("partition_count").cloned().unwrap_or(Value::Null),
                    "sort_order": ["url_id"],
                },
            },
            "lance_artifacts": [
                "edges_current.lance",
                "nodes_current.lance",
                "domain_feature_matrix.lance",
                "page_feature_matrix.lance",
                "anchor_feature_matrix.lance",
                "pagerank_scores.lance",
                "spam_feature_matrix.lance",
                "spam_scores.lance"
            ],
            "edge_row_count": edges.len(),
            "node_row_count": nodes.len(),
            "page_index_row_count": pages_by_domain.len(),
            "collision_quarantine_count": collision_rows.len(),
            "dictionary_collision_quarantine_count": dictionary_collision_rows.len(),
            "pagerank_iterations": iterations,
            "pagerank_converged": converged,
            "authority_prior_count": authority_priors.len(),
            "authority_prior_source": "authority/domain_ranks/current.jsonl",
            "custom_pagerank_status": if converged { "complete" } else { "max_iterations" },
            "spam_model_version": loaded_spam_model.as_ref().map(|m| m.model_version.clone()).unwrap_or_else(|| "unavailable".into()),
            "spam_score_status": spam_score_status,
            "compaction_completed_at": Utc::now().to_rfc3339(),
            "status": "complete",
        });
        Self::put_json_value(
            &client,
            &self.config.ops_bucket,
            &format!("{root}manifests/{snapshot_id}.json"),
            &manifest,
        )
        .await?;

        info!(
            snapshot_id,
            edges = edges.len(),
            converged,
            "link graph compaction complete"
        );

        let payload = serde_json::to_string(&json!({
            "corpus_snapshot_id": snapshot_id,
            "run_date": run_date,
            "edge_row_count": edges.len(),
            "status": "complete",
        }))
        .map_err(std::io::Error::other)?;
        submit_payload_batches(
            ctx.as_ref(),
            vec![IngestBatch {
                offset_key: OffsetKey::new(NAMESPACE_COMPACT_RUN, run_date.clone()),
                data: payload.clone(),
                bytes: payload.len(),
                namespace: Some(NAMESPACE_COMPACT_RUN.to_string()),
                source_uri: format!("upfoundry-link-graph-compact://{snapshot_id}"),
                offset_pos: None,
                cdc_rows: None,
            }],
        )?;

        Ok(())
    }
}
