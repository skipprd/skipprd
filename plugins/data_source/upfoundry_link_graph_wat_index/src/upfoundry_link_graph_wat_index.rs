use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client as S3Client;
use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value};
use skippr_plugin_shared_link_graph::{
    canonicalize_url, domain_id, id64_string, parse_wat_metadata_record, WatRecordLocation,
};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::config::UpfoundryLinkGraphWatIndexConfig;
use crate::streams::{
    all_namespace_contracts, NAMESPACE_AUDIT_SKIP, NAMESPACE_RUN_DAILY, NAMESPACE_TARGET_INDEX,
};
use crate::wat_stream::{open_wat_stream, WatStreamOpenError};

const MAX_TARGET_INDEX_BATCH_BYTES: usize = 32 * 1024 * 1024;
const MAX_RUNTIME_PAYLOAD_FRAME_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
struct TargetIndexRow {
    crawl_id: String,
    target_domain_hash_bucket: u32,
    target_domain_id: String,
    target_domain: String,
    source_url_id: String,
    source_url: String,
    source_domain_id: String,
    source_domain: String,
    source_host: String,
    warc_filename: String,
    warc_record_offset: i64,
    warc_record_length: i64,
    wat_filename: String,
    wat_record_offset: i64,
    wat_record_length: i64,
    fetch_status: Option<u32>,
    content_mime_type: String,
    fetch_time: String,
    link_count_to_target: u32,
    wat_path: String,
}

#[derive(Default)]
struct PendingBatch {
    rows: Vec<String>,
    bytes: usize,
}

pub struct UpfoundryLinkGraphWatIndexPlugin {
    config: UpfoundryLinkGraphWatIndexConfig,
}

impl UpfoundryLinkGraphWatIndexPlugin {
    pub fn new(config: UpfoundryLinkGraphWatIndexConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self { config })
    }

    fn fixture_dir() -> Option<String> {
        std::env::var("SKIPPR_WAT_INDEX_FIXTURE_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    async fn s3_client() -> S3Client {
        let cfg = aws_config::load_defaults(BehaviorVersion::latest()).await;
        S3Client::new(&cfg)
    }

    async fn read_uri(uri: &str) -> Result<Vec<u8>, std::io::Error> {
        if let Some(path) = uri.strip_prefix("s3://") {
            let (bucket, key) = path.split_once('/').ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid s3 uri")
            })?;
            let client = Self::s3_client().await;
            let resp = client
                .get_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            let bytes = resp
                .body
                .collect()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?
                .into_bytes();
            return Ok(bytes.to_vec());
        }
        if uri.starts_with("http://") || uri.starts_with("https://") {
            let bytes = reqwest::get(uri)
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?
                .bytes()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            return Ok(bytes.to_vec());
        }
        std::fs::read(uri)
    }

    async fn load_wat_paths(&self) -> Result<Vec<String>, std::io::Error> {
        if let Some(dir) = Self::fixture_dir() {
            let manifest = Path::new(&dir).join("wat.paths");
            if manifest.exists() {
                let content = std::fs::read_to_string(manifest)?;
                return Ok(content
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect());
            }
            return Ok(vec![Path::new(&dir)
                .join("sample.warc.wat.gz")
                .to_string_lossy()
                .to_string()]);
        }
        let uri = self
            .config
            .wat_paths_manifest_uri
            .as_deref()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "wat_paths_manifest_uri is required outside fixture mode",
                )
            })?;
        let mut bytes = Self::read_uri(uri).await?;
        if uri.ends_with(".gz") {
            let mut decoder = flate2::read::MultiGzDecoder::new(bytes.as_slice());
            let mut out = Vec::new();
            decoder.read_to_end(&mut out)?;
            bytes = out;
        }
        let content = String::from_utf8_lossy(&bytes);
        Ok(content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn parse_wat_member_payload(payload: &[u8]) -> Option<Value> {
        let text = String::from_utf8_lossy(payload);
        let json_start = text.find('{')?;
        let candidate = text[json_start..].trim();
        let json_end = candidate.rfind('}')?;
        serde_json::from_str::<Value>(&candidate[..=json_end]).ok()
    }

    fn target_bucket(&self, target_domain_id: u64) -> u32 {
        (target_domain_id % u64::from(self.config.target_domain_bucket_count)) as u32
    }

    fn effective_batch_size_bytes(&self) -> usize {
        self.config
            .batch_size_bytes
            .min(MAX_TARGET_INDEX_BATCH_BYTES)
    }

    fn ingest_batches_bytes(batches: &[IngestBatch]) -> usize {
        batches
            .iter()
            .map(|batch| batch.bytes)
            .fold(0usize, usize::saturating_add)
    }

    fn submit_if_frame_full(
        ctx: &dyn SourceSyncContext,
        ingest_batches: &mut Vec<IngestBatch>,
    ) -> Result<(), std::io::Error> {
        if Self::ingest_batches_bytes(ingest_batches) >= MAX_RUNTIME_PAYLOAD_FRAME_BYTES {
            submit_payload_batches(ctx, std::mem::take(ingest_batches))?;
        }
        Ok(())
    }

    fn rows_for_extraction(
        &self,
        wat_path: &str,
        location: WatRecordLocation,
        json: &Value,
    ) -> Vec<TargetIndexRow> {
        let Some(extraction) = parse_wat_metadata_record(
            &self.config.crawl_id,
            location,
            json,
            self.config.max_links_per_page,
        ) else {
            return Vec::new();
        };
        let mut by_target: HashMap<u64, (String, u32)> = HashMap::new();
        for link in &extraction.links {
            let Some(target) = canonicalize_url(&link.target_url) else {
                continue;
            };
            let tid = domain_id(&target.host);
            let entry = by_target.entry(tid).or_insert((target.host, 0));
            entry.1 = entry.1.saturating_add(1);
        }
        let mut seen = HashSet::new();
        by_target
            .into_iter()
            .filter_map(
                |(target_domain_id, (target_domain, link_count_to_target))| {
                    if !seen.insert(target_domain_id) {
                        return None;
                    }
                    Some(TargetIndexRow {
                        crawl_id: self.config.crawl_id.clone(),
                        target_domain_hash_bucket: self.target_bucket(target_domain_id),
                        target_domain_id: id64_string(target_domain_id),
                        target_domain,
                        source_url_id: id64_string(extraction.page_ref.source_url_id),
                        source_url: extraction.page_ref.source_url.clone(),
                        source_domain_id: id64_string(extraction.page_ref.source_domain_id),
                        source_domain: extraction.page_ref.source_host.clone(),
                        source_host: extraction.page_ref.source_host.clone(),
                        warc_filename: extraction.page_ref.warc.filename.clone(),
                        warc_record_offset: extraction.page_ref.warc.record_offset,
                        warc_record_length: extraction.page_ref.warc.record_length,
                        wat_filename: extraction.page_ref.wat.filename.clone(),
                        wat_record_offset: extraction.page_ref.wat.record_offset,
                        wat_record_length: extraction.page_ref.wat.record_length,
                        fetch_status: extraction.page_ref.fetch_status,
                        content_mime_type: extraction.page_ref.content_mime_type.clone(),
                        fetch_time: extraction.page_ref.fetch_time.clone(),
                        link_count_to_target,
                        wat_path: wat_path.to_string(),
                    })
                },
            )
            .collect()
    }

    fn push_row(
        &self,
        batches: &mut HashMap<u32, PendingBatch>,
        row: TargetIndexRow,
        output: &mut Vec<IngestBatch>,
    ) -> Result<(), std::io::Error> {
        let bucket = row.target_domain_hash_bucket;
        let line = serde_json::to_string(&row)?;
        let batch = batches.entry(bucket).or_default();
        batch.bytes = batch.bytes.saturating_add(line.len() + 1);
        batch.rows.push(line);
        if batch.bytes >= self.effective_batch_size_bytes()
            || batch.rows.len() >= self.config.max_records_per_batch
        {
            self.flush_bucket(bucket, batches, output)?;
        }
        Ok(())
    }

    fn flush_bucket(
        &self,
        bucket: u32,
        batches: &mut HashMap<u32, PendingBatch>,
        output: &mut Vec<IngestBatch>,
    ) -> Result<(), std::io::Error> {
        let Some(batch) = batches.remove(&bucket) else {
            return Ok(());
        };
        if batch.rows.is_empty() {
            return Ok(());
        }
        let data = batch.rows.join("\n");
        output.push(IngestBatch {
            offset_key: OffsetKey::new(
                NAMESPACE_TARGET_INDEX,
                format!("{}#{bucket:05}", self.config.crawl_id),
            ),
            bytes: data.len(),
            data,
            namespace: Some(NAMESPACE_TARGET_INDEX.to_string()),
            source_uri: format!("commoncrawl-wat://{}", self.config.crawl_id),
            offset_pos: None,
            cdc_rows: None,
        });
        Ok(())
    }

    fn submit_rows(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        partition: String,
        rows: Vec<Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let data = rows
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(namespace, partition),
                bytes: data.len(),
                data,
                namespace: Some(namespace.to_string()),
                source_uri: format!("commoncrawl-wat://{}", self.config.crawl_id),
                offset_pos: None,
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}

#[async_trait]
impl DataSource for UpfoundryLinkGraphWatIndexPlugin {
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
        let mut paths = self.load_wat_paths().await?;
        let start = self.config.wat_path_start.unwrap_or(0).min(paths.len());
        let end = self
            .config
            .wat_path_end
            .unwrap_or(paths.len())
            .min(paths.len());
        paths = paths[start..end].to_vec();
        paths.truncate(self.config.max_wat_objects_per_sync);

        let mut pending: HashMap<u32, PendingBatch> = HashMap::new();
        let mut ingest_batches = Vec::new();
        let mut files_processed = 0u32;
        let mut records_seen = 0u64;
        let mut rows_emitted = 0u64;
        let mut skip_rows = Vec::new();

        for path in &paths {
            let mut records_seen_in_object = 0usize;
            let mut stream = match open_wat_stream(path, self.config.max_wat_object_bytes).await {
                Ok(stream) => stream,
                Err(WatStreamOpenError::TooLarge {
                    compressed_bytes,
                    max_wat_object_bytes,
                }) => {
                    skip_rows.push(json!({
                        "crawl_id": self.config.crawl_id,
                        "wat_path": path,
                        "run_date": run_date,
                        "reason": "wat_object_too_large",
                        "compressed_bytes": compressed_bytes,
                        "max_wat_object_bytes": max_wat_object_bytes,
                    }));
                    continue;
                }
                Err(err) => {
                    skip_rows.push(json!({
                        "crawl_id": self.config.crawl_id,
                        "wat_path": path,
                        "run_date": run_date,
                        "reason": "read_failed",
                        "error": err.to_string(),
                    }));
                    continue;
                }
            };

            loop {
                if self
                    .config
                    .max_wat_records_per_object
                    .is_some_and(|limit| records_seen_in_object >= limit)
                {
                    break;
                }

                let member = match stream.next_member().await {
                    Ok(Some(member)) => member,
                    Ok(None) => break,
                    Err(err) => {
                        skip_rows.push(json!({
                            "crawl_id": self.config.crawl_id,
                            "wat_path": path,
                            "run_date": run_date,
                            "reason": "parse_records_failed",
                            "error": err.to_string(),
                            "compressed_bytes_read": stream.bytes_read(),
                        }));
                        break;
                    }
                };

                let (record_offset, record_length, payload) = member;
                let Some(value) = Self::parse_wat_member_payload(&payload) else {
                    continue;
                };
                records_seen = records_seen.saturating_add(1);
                records_seen_in_object = records_seen_in_object.saturating_add(1);
                let location = WatRecordLocation {
                    filename: path.to_string(),
                    record_offset: i64::try_from(record_offset).unwrap_or(0),
                    record_length: i64::try_from(record_length).unwrap_or(0),
                };
                for row in self.rows_for_extraction(path, location, &value) {
                    rows_emitted = rows_emitted.saturating_add(1);
                    self.push_row(&mut pending, row, &mut ingest_batches)?;
                    Self::submit_if_frame_full(ctx.as_ref(), &mut ingest_batches)?;
                }
            }
            files_processed = files_processed.saturating_add(1);
        }
        let buckets = pending.keys().copied().collect::<Vec<_>>();
        for bucket in buckets {
            self.flush_bucket(bucket, &mut pending, &mut ingest_batches)?;
            Self::submit_if_frame_full(ctx.as_ref(), &mut ingest_batches)?;
        }
        if !ingest_batches.is_empty() {
            submit_payload_batches(ctx.as_ref(), ingest_batches)?;
        }

        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_RUN_DAILY,
            run_date.clone(),
            vec![json!({
                "crawl_id": self.config.crawl_id,
                "run_date": run_date,
                "wat_files_selected": paths.len(),
                "wat_files_processed": files_processed,
                "records_seen": records_seen,
                "rows_emitted": rows_emitted,
                "target_domain_bucket_count": self.config.target_domain_bucket_count,
            })],
        )?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_AUDIT_SKIP, run_date, skip_rows)?;

        info!(
            files_processed,
            records_seen, rows_emitted, "wat index build complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wat_stream::WatGzipStream;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use serde_json::json;
    use std::io::Write;

    fn test_plugin() -> UpfoundryLinkGraphWatIndexPlugin {
        UpfoundryLinkGraphWatIndexPlugin::new(UpfoundryLinkGraphWatIndexConfig {
            crawl_id: "CC-MAIN-X".into(),
            wat_paths_manifest_uri: None,
            wat_path_start: None,
            wat_path_end: None,
            target_domain_bucket_count: 32_768,
            batch_size_bytes: 1024,
            max_records_per_batch: 10,
            max_links_per_page: 10,
            max_wat_objects_per_sync: 1,
            max_wat_object_bytes: 1024 * 1024,
            max_wat_records_per_object: None,
            include_subdomains: false,
        })
        .unwrap()
    }

    #[test]
    fn bucket_is_stable() {
        let plugin = test_plugin();
        let id = domain_id("example.com");
        assert_eq!(plugin.target_bucket(id), (id % 32_768) as u32);
    }

    #[test]
    fn effective_batch_size_clamps_oversized_config() {
        let mut plugin = test_plugin();
        plugin.config.batch_size_bytes = usize::MAX;

        assert_eq!(
            plugin.effective_batch_size_bytes(),
            MAX_TARGET_INDEX_BATCH_BYTES
        );
    }

    #[test]
    fn ingest_batches_bytes_saturates_total_payload_size() {
        let batches = vec![
            IngestBatch {
                offset_key: OffsetKey::new(NAMESPACE_TARGET_INDEX, "a"),
                bytes: usize::MAX,
                data: String::new(),
                namespace: Some(NAMESPACE_TARGET_INDEX.to_string()),
                source_uri: "test://wat".to_string(),
                offset_pos: None,
                cdc_rows: None,
            },
            IngestBatch {
                offset_key: OffsetKey::new(NAMESPACE_TARGET_INDEX, "b"),
                bytes: 1,
                data: String::new(),
                namespace: Some(NAMESPACE_TARGET_INDEX.to_string()),
                source_uri: "test://wat".to_string(),
                offset_pos: None,
                cdc_rows: None,
            },
        ];

        assert_eq!(
            UpfoundryLinkGraphWatIndexPlugin::ingest_batches_bytes(&batches),
            usize::MAX
        );
    }

    #[test]
    fn dedupes_target_domains_per_source_page() {
        let plugin = test_plugin();
        let record = json!({
            "Container": {
                "Filename": "crawl-data/CC-MAIN-X/segments/1/warc/source.warc.gz",
                "Offset": 100,
                "Gzip-Metadata": { "Deflate-Length": 200 }
            },
            "Envelope": {
                "WARC-Header-Metadata": {
                    "WARC-Target-URI": "https://source.example/page",
                    "WARC-Date": "2026-01-01T00:00:00Z"
                },
                "Payload-Metadata": {
                    "HTTP-Response-Metadata": {
                        "Response-Message": { "Status": 200 },
                        "Headers": { "Content-Type": "text/html" },
                        "HTML-Metadata": {
                            "Links": [
                                { "path": "A@/href", "url": "https://target.example/a" },
                                { "path": "A@/href", "url": "https://target.example/b" },
                                { "path": "A@/href", "url": "https://other.example/" }
                            ]
                        }
                    }
                }
            }
        });
        let rows = plugin.rows_for_extraction(
            "crawl-data/CC-MAIN-X/segments/1/wat/source.warc.wat.gz",
            WatRecordLocation {
                filename: "crawl-data/CC-MAIN-X/segments/1/wat/source.warc.wat.gz".into(),
                record_offset: 10,
                record_length: 20,
            },
            &record,
        );
        assert_eq!(rows.len(), 2);
        let target = rows
            .iter()
            .find(|row| row.target_domain == "target.example")
            .unwrap();
        assert_eq!(target.link_count_to_target, 2);
        assert_eq!(target.wat_record_offset, 10);
        assert_eq!(
            target.target_domain_id,
            id64_string(domain_id("target.example"))
        );
    }

    #[test]
    fn target_index_row_serializes_hash_ids_as_json_strings() {
        let plugin = test_plugin();
        let record = json!({
            "Container": {
                "Filename": "crawl-data/CC-MAIN-X/segments/1/warc/source.warc.gz",
                "Offset": 100,
                "Gzip-Metadata": { "Deflate-Length": 200 }
            },
            "Envelope": {
                "WARC-Header-Metadata": {
                    "WARC-Target-URI": "https://source.example/page",
                    "WARC-Date": "2026-01-01T00:00:00Z"
                },
                "Payload-Metadata": {
                    "HTTP-Response-Metadata": {
                        "Response-Message": { "Status": 200 },
                        "Headers": { "Content-Type": "text/html" },
                        "HTML-Metadata": {
                            "Links": [
                                { "path": "A@/href", "url": "https://target.example/a" }
                            ]
                        }
                    }
                }
            }
        });
        let rows = plugin.rows_for_extraction(
            "crawl-data/CC-MAIN-X/segments/1/wat/source.warc.wat.gz",
            WatRecordLocation {
                filename: "crawl-data/CC-MAIN-X/segments/1/wat/source.warc.wat.gz".into(),
                record_offset: 10,
                record_length: 20,
            },
            &record,
        );
        let value: Value = serde_json::to_value(&rows[0]).unwrap();
        assert!(value["target_domain_id"].is_string());
        assert!(value["source_url_id"].is_string());
        assert!(value["source_domain_id"].is_string());
    }

    #[tokio::test]
    async fn records_compressed_wat_member_ranges() {
        fn member(payload: &str) -> Vec<u8> {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(payload.as_bytes()).unwrap();
            encoder.finish().unwrap()
        }
        let first = member("WARC/1.0\r\n\r\n{\"ignored\":true}");
        let second_json = r#"{
            "Container": {
                "Filename": "crawl-data/CC-MAIN-X/segments/1/warc/source.warc.gz",
                "Offset": 100,
                "Gzip-Metadata": { "Deflate-Length": 200 }
            },
            "Envelope": {
                "WARC-Header-Metadata": {
                    "WARC-Target-URI": "https://source.example/page",
                    "WARC-Date": "2026-01-01T00:00:00Z"
                },
                "Payload-Metadata": {
                    "HTTP-Response-Metadata": {
                        "Response-Message": { "Status": 200 },
                        "Headers": { "Content-Type": "text/html" },
                        "HTML-Metadata": {
                            "Links": [
                                { "path": "A@/href", "url": "https://target.example/a" }
                            ]
                        }
                    }
                }
            }
        }"#;
        let second_payload = format!("WARC/1.0\r\n\r\n{second_json}");
        let second = member(&second_payload);
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second);

        let mut stream = WatGzipStream::new(bytes.as_slice(), usize::MAX);
        let mut payloads = Vec::new();
        while let Some((record_offset, record_length, payload)) =
            stream.next_member().await.unwrap()
        {
            if let Some(value) =
                UpfoundryLinkGraphWatIndexPlugin::parse_wat_member_payload(&payload)
            {
                payloads.push((
                    WatRecordLocation {
                        filename: "crawl-data/CC-MAIN-X/segments/1/wat/source.warc.wat.gz".into(),
                        record_offset: i64::try_from(record_offset).unwrap_or(0),
                        record_length: i64::try_from(record_length).unwrap_or(0),
                    },
                    value,
                ));
            }
        }
        assert_eq!(payloads.len(), 2);
        let (location, value) = &payloads[1];
        assert_eq!(location.record_offset, first.len() as i64);
        assert_eq!(location.record_length, second.len() as i64);
        assert_eq!(
            value
                .pointer("/Envelope/WARC-Header-Metadata/WARC-Target-URI")
                .and_then(Value::as_str),
            Some("https://source.example/page")
        );

        let range = &bytes[location.record_offset as usize
            ..(location.record_offset + location.record_length) as usize];
        let mut decoder = flate2::read::GzDecoder::new(range);
        let mut roundtrip = Vec::new();
        decoder.read_to_end(&mut roundtrip).unwrap();
        assert!(String::from_utf8_lossy(&roundtrip).contains("https://target.example/a"));
    }
}
