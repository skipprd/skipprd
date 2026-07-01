use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sqs::Client as SqsClient;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::source_compat::{load_checkpoint_payload, store_checkpoint_payload};
use tracing::{info, warn};

use crate::config::UpfoundryLinkGraphWatIndexConfig;
use crate::job::{
    apply_path_cap, checkpoint_key, cleared_checkpoint, default_manifest_uri, effective_checkpoint,
    ActiveSqsJob, WatIndexJobMessage, WatManifestCheckpoint,
};
use crate::prefetch::ParallelProcessPaths;
use crate::streams::all_namespace_contracts;

pub struct UpfoundryLinkGraphWatIndexPlugin {
    config: UpfoundryLinkGraphWatIndexConfig,
}

impl UpfoundryLinkGraphWatIndexPlugin {
    pub fn new(config: UpfoundryLinkGraphWatIndexConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &UpfoundryLinkGraphWatIndexConfig {
        &self.config
    }

    fn fixture_dir() -> Option<String> {
        std::env::var("SKIPPR_WAT_INDEX_FIXTURE_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    async fn read_uri(
        uri: &str,
        s3_client: &S3Client,
        http_client: &reqwest::Client,
    ) -> Result<Vec<u8>, std::io::Error> {
        if let Some(path) = uri.strip_prefix("s3://") {
            let (bucket, key) = path.split_once('/').ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid s3 uri")
            })?;
            let resp = s3_client
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
            let bytes = http_client
                .get(uri)
                .send()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?
                .bytes()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            return Ok(bytes.to_vec());
        }
        std::fs::read(uri)
    }

    async fn load_wat_paths(
        manifest_uri: &str,
        s3_client: &S3Client,
        http_client: &reqwest::Client,
    ) -> Result<Vec<String>, std::io::Error> {
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
        let mut bytes = Self::read_uri(manifest_uri, s3_client, http_client).await?;
        if manifest_uri.ends_with(".gz") {
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

    fn resolve_manifest_uri(
        &self,
        crawl_id: &str,
        job_manifest_uri: Option<&str>,
        checkpoint: Option<&WatManifestCheckpoint>,
    ) -> String {
        job_manifest_uri
            .map(str::to_string)
            .or_else(|| self.config.wat_paths_manifest_uri.clone())
            .or_else(|| checkpoint.map(|cp| cp.manifest_uri.clone()))
            .unwrap_or_else(|| default_manifest_uri(crawl_id))
    }

    async fn receive_sqs_job(
        &self,
        sqs_client: &SqsClient,
    ) -> Result<Option<(WatIndexJobMessage, ActiveSqsJob)>, std::io::Error> {
        let queue_url = self
            .config
            .sqs_queue_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "sqs_queue_url is not configured",
                )
            })?;
        let response = sqs_client
            .receive_message()
            .queue_url(queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(5)
            .visibility_timeout(self.config.sqs_visibility_timeout_seconds)
            .send()
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        let Some(message) = response
            .messages
            .and_then(|messages| messages.into_iter().next())
        else {
            return Ok(None);
        };
        let body = message.body.unwrap_or_default();
        let job: WatIndexJobMessage = serde_json::from_str(&body).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid WAT index SQS job payload: {err}"),
            )
        })?;
        if job.crawl_id.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WAT index SQS job missing crawlId",
            ));
        }
        let receipt_handle = message.receipt_handle.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WAT index SQS job missing receipt handle",
            )
        })?;
        Ok(Some((
            job,
            ActiveSqsJob {
                queue_url: queue_url.to_string(),
                receipt_handle,
                visibility_timeout_seconds: self.config.sqs_visibility_timeout_seconds,
            },
        )))
    }

    async fn extend_sqs_visibility(&self, sqs_client: &SqsClient, job: &ActiveSqsJob) {
        if let Err(err) = sqs_client
            .change_message_visibility()
            .queue_url(&job.queue_url)
            .receipt_handle(&job.receipt_handle)
            .visibility_timeout(job.visibility_timeout_seconds)
            .send()
            .await
        {
            warn!(
                error = %err,
                "failed to extend WAT index SQS message visibility"
            );
        }
    }

    async fn delete_sqs_job(&self, sqs_client: &SqsClient, job: &ActiveSqsJob) {
        if let Err(err) = sqs_client
            .delete_message()
            .queue_url(&job.queue_url)
            .receipt_handle(&job.receipt_handle)
            .send()
            .await
        {
            warn!(error = %err, "failed to delete completed WAT index SQS job");
        }
    }

    async fn release_sqs_job_for_redelivery(&self, sqs_client: &SqsClient, job: &ActiveSqsJob) {
        if let Err(err) = sqs_client
            .change_message_visibility()
            .queue_url(&job.queue_url)
            .receipt_handle(&job.receipt_handle)
            .visibility_timeout(0)
            .send()
            .await
        {
            warn!(
                error = %err,
                "failed to release WAT index SQS job for immediate redelivery"
            );
        }
    }

    fn clear_manifest_checkpoint(
        &self,
        ctx: &dyn SourceSyncContext,
        crawl_id: &str,
        manifest_uri: &str,
    ) -> Result<(), std::io::Error> {
        store_checkpoint_payload(
            ctx,
            &checkpoint_key(crawl_id),
            &cleared_checkpoint(crawl_id, manifest_uri),
        )
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
        let aws_cfg = aws_config::load_defaults(BehaviorVersion::latest()).await;
        let s3_client = S3Client::new(&aws_cfg);
        let http_client = reqwest::Client::new();
        let sqs_client = if self.config.uses_sqs_jobs() {
            Some(SqsClient::new(&aws_cfg))
        } else {
            None
        };

        let (
            crawl_id,
            manifest_uri,
            sqs_job,
            reset_checkpoint,
            wat_path_start,
            wat_path_end,
            max_wat_objects_per_sync,
        ) = if let Some(ref client) = sqs_client {
            match self.receive_sqs_job(client).await? {
                Some((job, active)) => {
                    let manifest_uri =
                        self.resolve_manifest_uri(&job.crawl_id, job.manifest_uri.as_deref(), None);
                    (
                        job.crawl_id,
                        manifest_uri,
                        Some(active),
                        job.reset_checkpoint,
                        job.wat_path_start,
                        job.wat_path_end,
                        job.max_wat_objects_per_sync,
                    )
                }
                None => {
                    info!("no_wat_index_jobs");
                    return Ok(());
                }
            }
        } else {
            let crawl_id = self.config.crawl_id.clone();
            let manifest_uri = self
                .config
                .wat_paths_manifest_uri
                .clone()
                .unwrap_or_else(|| default_manifest_uri(&crawl_id));
            (crawl_id, manifest_uri, None, false, None, None, None)
        };

        let ckpt_key = checkpoint_key(&crawl_id);
        let checkpoint = effective_checkpoint(
            load_checkpoint_payload::<WatManifestCheckpoint>(ctx.as_ref(), &ckpt_key),
            reset_checkpoint,
        );
        let manifest_uri =
            self.resolve_manifest_uri(&crawl_id, Some(manifest_uri.as_str()), checkpoint.as_ref());

        let mut all_paths = Self::load_wat_paths(&manifest_uri, &s3_client, &http_client).await?;
        let start = wat_path_start
            .or(self.config.wat_path_start)
            .unwrap_or(0)
            .min(all_paths.len());
        let end = wat_path_end
            .or(self.config.wat_path_end)
            .unwrap_or(all_paths.len())
            .min(all_paths.len());
        all_paths = all_paths[start..end].to_vec();
        let total_paths = all_paths.len();

        let next_path_index = checkpoint
            .as_ref()
            .map(|cp| cp.next_path_index.min(total_paths))
            .unwrap_or(0);
        if next_path_index >= total_paths {
            info!(
                crawl_id = %crawl_id,
                total_paths,
                "WAT index crawl already complete for manifest slice"
            );
            if let (Some(client), Some(job)) = (&sqs_client, &sqs_job) {
                self.delete_sqs_job(client, job).await;
                self.clear_manifest_checkpoint(ctx.as_ref(), &crawl_id, &manifest_uri)?;
            }
            return Ok(());
        }

        let mut paths = all_paths[next_path_index..].to_vec();
        let max_objects = max_wat_objects_per_sync.or(self.config.max_wat_objects_per_sync);
        apply_path_cap(&mut paths, max_objects);

        let indexed_paths = paths
            .into_iter()
            .enumerate()
            .map(|(offset, path)| (next_path_index.saturating_add(offset), path))
            .collect::<Vec<_>>();

        let sqs_ref = sqs_job.as_ref();
        let final_index = ParallelProcessPaths::new(
            self.config.clone(),
            Arc::clone(&ctx),
            sqs_client.as_ref(),
            sqs_ref,
            crawl_id.clone(),
            manifest_uri.clone(),
            indexed_paths,
            total_paths,
            checkpoint.as_ref(),
            next_path_index,
        )
        .run()
        .await?;

        if final_index >= total_paths {
            info!(crawl_id = %crawl_id, total_paths, "WAT index crawl complete");
            if let (Some(client), Some(job)) = (&sqs_client, &sqs_job) {
                self.delete_sqs_job(client, job).await;
            }
            self.clear_manifest_checkpoint(ctx.as_ref(), &crawl_id, &manifest_uri)?;
        } else if let (Some(client), Some(job)) = (&sqs_client, &sqs_job) {
            self.release_sqs_job_for_redelivery(client, job).await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrow_batch::{encode_record_batch_ipc, TargetIndexBatchBuilder};
    use crate::extraction::{parse_wat_member_payload, rows_for_extraction_arrow, target_bucket};
    use crate::streams::NAMESPACE_TARGET_INDEX;
    use crate::wat_stream::WatGzipStream;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use serde_json::json;
    use skippr_plugin_shared_link_graph::{domain_id, id64_string, WatRecordLocation};
    use std::collections::BTreeMap;
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
            max_wat_objects_per_sync: Some(1),
            max_wat_object_bytes: 1024 * 1024,
            max_wat_records_per_object: None,
            include_subdomains: false,
            sqs_queue_url: None,
            sqs_visibility_timeout_seconds: 14_400,
        })
        .unwrap()
    }

    #[test]
    fn bucket_is_stable() {
        let plugin = test_plugin();
        let id = domain_id("example.com");
        assert_eq!(target_bucket(plugin.config(), id), (id % 32_768) as u32);
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
        let rows = rows_for_extraction_arrow(
            plugin.config(),
            "CC-MAIN-X",
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
    fn arrow_batch_encodes_target_index_rows() {
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
        let rows = rows_for_extraction_arrow(
            plugin.config(),
            "CC-MAIN-X",
            "crawl-data/CC-MAIN-X/segments/1/wat/source.warc.wat.gz",
            WatRecordLocation {
                filename: "crawl-data/CC-MAIN-X/segments/1/wat/source.warc.wat.gz".into(),
                record_offset: 10,
                record_length: 20,
            },
            &record,
        );
        assert_eq!(rows.len(), 1);
        let mut builder = TargetIndexBatchBuilder::new();
        for row in &rows {
            builder.append_row(row);
        }
        let batch = builder.finish_record_batch().unwrap().unwrap();
        let ipc = encode_record_batch_ipc(&batch).unwrap();
        assert!(!ipc.is_empty());
        assert_eq!(batch.num_rows(), rows.len());
        assert_eq!(
            batch.schema().field_with_name("crawl_id").unwrap().name(),
            "crawl_id"
        );
        let _ = NAMESPACE_TARGET_INDEX;
    }

    #[test]
    fn config_allows_sqs_without_static_crawl_id() {
        let config = UpfoundryLinkGraphWatIndexConfig {
            crawl_id: String::new(),
            wat_paths_manifest_uri: None,
            wat_path_start: None,
            wat_path_end: None,
            target_domain_bucket_count: 32_768,
            batch_size_bytes: 1024,
            max_records_per_batch: 10,
            max_links_per_page: 10,
            max_wat_objects_per_sync: None,
            max_wat_object_bytes: 1024 * 1024,
            max_wat_records_per_object: None,
            include_subdomains: false,
            sqs_queue_url: Some("https://sqs.eu-west-1.amazonaws.com/123/wat-index.fifo".into()),
            sqs_visibility_timeout_seconds: 14_400,
        };
        assert!(config.validate().is_ok());
        assert!(config.uses_sqs_jobs());
    }

    #[test]
    fn checkpoint_serializes_path_member_cursors() {
        let mut cursors = BTreeMap::new();
        cursors.insert(3, 12);
        let checkpoint = WatManifestCheckpoint {
            crawl_id: "CC-MAIN-X".into(),
            manifest_uri: "uri".into(),
            next_path_index: 2,
            total_paths: Some(10),
            cleared: false,
            path_member_cursors: cursors,
        };
        let json = serde_json::to_string(&checkpoint).unwrap();
        assert!(json.contains("path_member_cursors"));
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
            if let Some(value) = parse_wat_member_payload(&payload) {
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
                .and_then(serde_json::Value::as_str),
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
