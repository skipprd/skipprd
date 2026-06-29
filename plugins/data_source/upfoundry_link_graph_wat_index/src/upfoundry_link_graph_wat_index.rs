use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sqs::Client as SqsClient;
use serde::Serialize;
use serde_json::Value;
use skippr_plugin_shared_link_graph::{
    canonicalize_url, domain_id, id64_string, parse_wat_metadata_record, WatRecordLocation,
};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::source_compat::{
    load_checkpoint_payload, source_payload_task, store_checkpoint_payload, IngestBatch,
    SourcePayloadTask,
};
use tracing::{info, warn};

use crate::config::UpfoundryLinkGraphWatIndexConfig;
use crate::job::{
    apply_path_cap, checkpoint_key, cleared_checkpoint, default_manifest_uri, effective_checkpoint,
    WatIndexJobMessage, WatManifestCheckpoint,
};
use crate::streams::{all_namespace_contracts, NAMESPACE_TARGET_INDEX};
use crate::wat_stream::{open_wat_stream, WatStreamOpenError};

const SQS_VISIBILITY_EXTEND_EVERY_PATHS: usize = 25;

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn env_truthy(name: &str) -> bool {
    env_string(name).is_some_and(|value| {
        value == "1"
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
    })
}

fn runtime_is_ci() -> bool {
    env_truthy("GITHUB_ACTIONS") || env_truthy("CI")
}

fn ingest_pending_max_bytes() -> usize {
    env_string("INGEST_PENDING_MAX_BYTES")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| {
            if runtime_is_ci() {
                512 * 1024 * 1024
            } else {
                2 * 1024 * 1024 * 1024
            }
        })
}

struct IngestPipeline<'a> {
    ctx: &'a dyn SourceSyncContext,
    dispatch_cpus: usize,
    pending_cap: usize,
    pending_tasks: Vec<SourcePayloadTask>,
    accepted_submissions: Vec<skippr_runtime_sdk::source_compat::PayloadSubmissionBatch>,
    pending_bytes_sum: usize,
}

impl<'a> IngestPipeline<'a> {
    fn new(ctx: &'a dyn SourceSyncContext) -> Self {
        let dispatch_cpus = num_cpus::get().max(1);
        Self {
            ctx,
            dispatch_cpus,
            pending_cap: ingest_pending_max_bytes(),
            pending_tasks: Vec::with_capacity(dispatch_cpus),
            accepted_submissions: Vec::new(),
            pending_bytes_sum: 0,
        }
    }

    fn queue_batch(&mut self, batch: IngestBatch) -> Result<(), std::io::Error> {
        self.pending_bytes_sum = self.pending_bytes_sum.saturating_add(batch.bytes);
        self.pending_tasks.push(source_payload_task(vec![batch]));
        self.dispatch_if_ready()
    }

    fn dispatch_if_ready(&mut self) -> Result<(), std::io::Error> {
        if self.pending_tasks.len() < self.dispatch_cpus
            && self.pending_bytes_sum < self.pending_cap
        {
            return Ok(());
        }
        self.dispatch_pending()
    }

    fn dispatch_pending(&mut self) -> Result<(), std::io::Error> {
        if self.pending_tasks.is_empty() {
            return Ok(());
        }
        let submission = self
            .ctx
            .submit_payload_tasks_accepted(std::mem::take(&mut self.pending_tasks))?;
        self.accepted_submissions.push(submission);
        self.pending_bytes_sum = 0;
        if self.accepted_submissions.len() >= 1024 {
            self.ctx.wait_payload_acks(&self.accepted_submissions)?;
            self.accepted_submissions.clear();
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), std::io::Error> {
        self.dispatch_pending()?;
        if !self.accepted_submissions.is_empty() {
            self.ctx.wait_payload_acks(&self.accepted_submissions)?;
        }
        self.ctx.drain_payload_acks()
    }
}

#[derive(Debug, Clone, Serialize)]
struct TargetIndexRow {
    crawl_id: String,
    target_domain_hash_bucket: String,
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

struct ActiveSqsJob {
    queue_url: String,
    receipt_handle: String,
    visibility_timeout_seconds: i32,
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
        self.config.batch_size_bytes
    }

    fn bucket_ingest_batch(
        &self,
        crawl_id: &str,
        bucket: u32,
        batch: PendingBatch,
    ) -> Option<IngestBatch> {
        if batch.rows.is_empty() {
            return None;
        }
        let data = batch.rows.join("\n");
        Some(IngestBatch {
            offset_key: OffsetKey::new(
                NAMESPACE_TARGET_INDEX,
                format!("{crawl_id}#{bucket:05}"),
            ),
            bytes: data.len(),
            data,
            namespace: Some(NAMESPACE_TARGET_INDEX.to_string()),
            source_uri: format!("commoncrawl-wat://{crawl_id}"),
            offset_pos: None,
            cdc_rows: None,
        })
    }

    fn push_row(
        &self,
        crawl_id: &str,
        batches: &mut HashMap<u32, PendingBatch>,
        row: TargetIndexRow,
        pipeline: &mut IngestPipeline<'_>,
    ) -> Result<(), std::io::Error> {
        let bucket = row
            .target_domain_hash_bucket
            .parse::<u32>()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        let line = serde_json::to_string(&row)?;
        let batch = batches.entry(bucket).or_default();
        batch.bytes = batch.bytes.saturating_add(line.len() + 1);
        batch.rows.push(line);
        if batch.bytes >= self.effective_batch_size_bytes()
            || batch.rows.len() >= self.config.max_records_per_batch
        {
            self.flush_bucket(crawl_id, bucket, batches, pipeline)?;
        }
        Ok(())
    }

    fn flush_bucket(
        &self,
        crawl_id: &str,
        bucket: u32,
        batches: &mut HashMap<u32, PendingBatch>,
        pipeline: &mut IngestPipeline<'_>,
    ) -> Result<(), std::io::Error> {
        let Some(pending) = batches.remove(&bucket) else {
            return Ok(());
        };
        if let Some(batch) = self.bucket_ingest_batch(crawl_id, bucket, pending) {
            pipeline.queue_batch(batch)?;
        }
        Ok(())
    }

    fn rows_for_extraction(
        &self,
        crawl_id: &str,
        wat_path: &str,
        location: WatRecordLocation,
        json: &Value,
    ) -> Vec<TargetIndexRow> {
        let Some(extraction) =
            parse_wat_metadata_record(crawl_id, location, json, self.config.max_links_per_page)
        else {
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
                        crawl_id: crawl_id.to_string(),
                        target_domain_hash_bucket: self.target_bucket(target_domain_id).to_string(),
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
        let Some(message) = response.messages.and_then(|messages| messages.into_iter().next())
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

    async fn process_paths(
        &self,
        ctx: Arc<dyn SourceSyncContext>,
        sqs_client: Option<&SqsClient>,
        sqs_job: Option<&ActiveSqsJob>,
        crawl_id: &str,
        manifest_uri: &str,
        paths: &[String],
        mut next_path_index: usize,
        total_paths: usize,
    ) -> Result<usize, std::io::Error> {
        let mut pending: HashMap<u32, PendingBatch> = HashMap::new();
        let mut pipeline = IngestPipeline::new(ctx.as_ref());
        let s3_client = S3Client::new(&aws_config::load_defaults(BehaviorVersion::latest()).await);
        let http_client = reqwest::Client::new();
        let mut files_processed = 0u32;
        let mut records_seen = 0u64;
        let mut rows_emitted = 0u64;
        let mut skips_logged = 0u64;
        let ckpt_key = checkpoint_key(crawl_id);

        for path in paths {
            let mut records_seen_in_object = 0usize;
            let mut stream = match open_wat_stream(
                path,
                self.config.max_wat_object_bytes,
                Some(&s3_client),
                Some(&http_client),
            )
            .await
            {
                Ok(stream) => stream,
                Err(WatStreamOpenError::TooLarge {
                    compressed_bytes,
                    max_wat_object_bytes,
                }) => {
                    warn!(
                        crawl_id = %crawl_id,
                        wat_path = %path,
                        reason = "wat_object_too_large",
                        compressed_bytes,
                        max_wat_object_bytes,
                        "skipping WAT object"
                    );
                    skips_logged = skips_logged.saturating_add(1);
                    next_path_index = next_path_index.saturating_add(1);
                    store_checkpoint_payload(
                        ctx.as_ref(),
                        &ckpt_key,
                        &WatManifestCheckpoint {
                            crawl_id: crawl_id.to_string(),
                            manifest_uri: manifest_uri.to_string(),
                            next_path_index,
                            total_paths: Some(total_paths),
                            cleared: false,
                        },
                    )?;
                    continue;
                }
                Err(err) => {
                    warn!(
                        crawl_id = %crawl_id,
                        wat_path = %path,
                        reason = "read_failed",
                        error = %err,
                        "skipping WAT object"
                    );
                    skips_logged = skips_logged.saturating_add(1);
                    next_path_index = next_path_index.saturating_add(1);
                    store_checkpoint_payload(
                        ctx.as_ref(),
                        &ckpt_key,
                        &WatManifestCheckpoint {
                            crawl_id: crawl_id.to_string(),
                            manifest_uri: manifest_uri.to_string(),
                            next_path_index,
                            total_paths: Some(total_paths),
                            cleared: false,
                        },
                    )?;
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
                        warn!(
                            crawl_id = %crawl_id,
                            wat_path = %path,
                            reason = "parse_records_failed",
                            error = %err,
                            compressed_bytes_read = stream.bytes_read(),
                            "stopping WAT object after parse failure"
                        );
                        skips_logged = skips_logged.saturating_add(1);
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
                for row in self.rows_for_extraction(crawl_id, path, location, &value) {
                    rows_emitted = rows_emitted.saturating_add(1);
                    self.push_row(crawl_id, &mut pending, row, &mut pipeline)?;
                }
            }

            files_processed = files_processed.saturating_add(1);
            next_path_index = next_path_index.saturating_add(1);
            store_checkpoint_payload(
                ctx.as_ref(),
                &ckpt_key,
                &WatManifestCheckpoint {
                    crawl_id: crawl_id.to_string(),
                    manifest_uri: manifest_uri.to_string(),
                    next_path_index,
                    total_paths: Some(total_paths),
                    cleared: false,
                },
            )?;

            if let (Some(sqs_client), Some(sqs_job)) = (sqs_client, sqs_job) {
                if files_processed as usize % SQS_VISIBILITY_EXTEND_EVERY_PATHS == 0 {
                    self.extend_sqs_visibility(sqs_client, sqs_job).await;
                }
            }
        }

        let buckets = pending.keys().copied().collect::<Vec<_>>();
        for bucket in buckets {
            self.flush_bucket(crawl_id, bucket, &mut pending, &mut pipeline)?;
        }
        pipeline.finish()?;

        info!(
            crawl_id = %crawl_id,
            files_processed,
            records_seen,
            rows_emitted,
            skips_logged,
            wat_files_selected = paths.len(),
            next_path_index,
            total_paths,
            target_domain_bucket_count = self.config.target_domain_bucket_count,
            "wat index build complete"
        );
        Ok(next_path_index)
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
                    let manifest_uri = self.resolve_manifest_uri(
                        &job.crawl_id,
                        job.manifest_uri.as_deref(),
                        None,
                    );
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
            (
                crawl_id,
                manifest_uri,
                None,
                false,
                None,
                None,
                None,
            )
        };

        let ckpt_key = checkpoint_key(&crawl_id);
        let checkpoint = effective_checkpoint(
            load_checkpoint_payload::<WatManifestCheckpoint>(ctx.as_ref(), &ckpt_key),
            reset_checkpoint,
        );
        let manifest_uri = self.resolve_manifest_uri(
            &crawl_id,
            Some(manifest_uri.as_str()),
            checkpoint.as_ref(),
        );

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

        let final_index = self
            .process_paths(
                ctx.clone(),
                sqs_client.as_ref(),
                sqs_job.as_ref(),
                &crawl_id,
                &manifest_uri,
                &paths,
                next_path_index,
                total_paths,
            )
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
        assert_eq!(plugin.target_bucket(id), (id % 32_768) as u32);
    }

    #[test]
    fn effective_batch_size_uses_configured_value() {
        let mut plugin = test_plugin();
        plugin.config.batch_size_bytes = 512 * 1024 * 1024;

        assert_eq!(plugin.effective_batch_size_bytes(), 512 * 1024 * 1024);
    }

    #[test]
    fn bucket_ingest_batch_builds_target_index_payload() {
        let plugin = test_plugin();
        let batch = plugin
            .bucket_ingest_batch(
                "CC-MAIN-X",
                42,
                PendingBatch {
                    rows: vec!["{\"crawl_id\":\"CC-MAIN-X\"}".to_string()],
                    bytes: 24,
                },
            )
            .unwrap();
        assert_eq!(batch.namespace.as_deref(), Some(NAMESPACE_TARGET_INDEX));
        assert!(batch.data.contains("CC-MAIN-X"));
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
            "CC-MAIN-X",
            "crawl-data/CC-MAIN-X/segments/1/wat/source.warc.wat.gz",
            WatRecordLocation {
                filename: "crawl-data/CC-MAIN-X/segments/1/wat/source.warc.wat.gz".into(),
                record_offset: 10,
                record_length: 20,
            },
            &record,
        );
        let value: Value = serde_json::to_value(&rows[0]).unwrap();
        assert!(value["target_domain_hash_bucket"].is_string());
        assert!(value["target_domain_id"].is_string());
        assert!(value["source_url_id"].is_string());
        assert!(value["source_domain_id"].is_string());
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
