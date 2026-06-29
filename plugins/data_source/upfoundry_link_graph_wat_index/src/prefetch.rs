use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aws_sdk_s3::Client as S3Client;
use aws_sdk_sqs::Client as SqsClient;
use skippr_plugin_shared_link_graph::WatRecordLocation;
use skippr_runtime_sdk::plugins::SourceSyncContext;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing::{info, warn};

use crate::arrow_batch::{TargetIndexArrowRow, TargetIndexBatchBuilder};
use crate::config::UpfoundryLinkGraphWatIndexConfig;
use crate::extraction::rows_for_extraction_arrow;
use crate::ingest_pipeline::{
    ingest_backpressure_pressure, source_pending_max_bytes, ArrowIngestPipeline,
};
use crate::job::WatManifestCheckpoint;
use crate::progress::CompletionTracker;
use crate::wat_stream::{download_wat_bytes, open_wat_stream_from_bytes, WatStreamOpenError};

const SQS_VISIBILITY_EXTEND_EVERY_PATHS: usize = 25;

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn env_usize(name: &str, default: usize) -> usize {
    env_string(name)
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn download_concurrency_limit() -> usize {
    env_usize("WAT_DOWNLOAD_CONCURRENCY", (num_cpus::get() / 4).max(2).min(32))
}

fn parse_concurrency_limit() -> usize {
    env_usize(
        "WAT_PARSE_CONCURRENCY",
        (num_cpus::get() / 2).max(2).min(64),
    )
}

fn prefetch_depth_limit() -> usize {
    env_usize("WAT_PREFETCH_DEPTH", 8)
}

fn download_memory_budget_bytes() -> usize {
    env_usize("WAT_DOWNLOAD_MEMORY_MB", 4096) * 1024 * 1024
}

#[derive(Clone, Debug, Default)]
struct ConcurrencyLimits {
    download: Arc<AtomicUsize>,
    parse: Arc<AtomicUsize>,
}

impl ConcurrencyLimits {
    fn new(download_base: usize, parse_base: usize) -> Self {
        Self {
            download: Arc::new(AtomicUsize::new(download_base)),
            parse: Arc::new(AtomicUsize::new(parse_base)),
        }
    }

    fn spawn_autotune(self: Arc<Self>, ctx: Arc<dyn SourceSyncContext>) {
        let limits = Arc::clone(&self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let pressure = ingest_backpressure_pressure(ctx.as_ref());
                let download_scale = if pressure >= 0.9 {
                    0.25
                } else if pressure >= 0.7 {
                    0.5
                } else {
                    1.0
                };
                let parse_scale = if pressure >= 0.85 {
                    0.25
                } else if pressure >= 0.65 {
                    0.5
                } else {
                    1.0
                };
                limits.download.store(
                    ((download_concurrency_limit() as f64) * download_scale)
                        .round()
                        .max(1.0) as usize,
                    Ordering::Relaxed,
                );
                limits.parse.store(
                    ((parse_concurrency_limit() as f64) * parse_scale)
                        .round()
                        .max(1.0) as usize,
                    Ordering::Relaxed,
                );
            }
        });
    }
}

struct ReadyWat {
    path_index: usize,
    path: String,
    bytes: Vec<u8>,
}

struct PathParseResult {
    path_index: usize,
    final_member_index: u64,
    records_seen: u64,
    rows_emitted: u64,
    skipped: bool,
}

use crate::job::ActiveSqsJob;

pub struct ParallelProcessPaths<'a> {
    config: UpfoundryLinkGraphWatIndexConfig,
    ctx: Arc<dyn SourceSyncContext>,
    sqs_client: Option<&'a SqsClient>,
    sqs_job: Option<&'a ActiveSqsJob>,
    crawl_id: String,
    manifest_uri: String,
    paths: Vec<(usize, String)>,
    total_paths: usize,
    member_cursors: std::collections::BTreeMap<usize, u64>,
    start_next_path_index: usize,
}

impl<'a> ParallelProcessPaths<'a> {
    pub fn new(
        config: UpfoundryLinkGraphWatIndexConfig,
        ctx: Arc<dyn SourceSyncContext>,
        sqs_client: Option<&'a SqsClient>,
        sqs_job: Option<&'a ActiveSqsJob>,
        crawl_id: String,
        manifest_uri: String,
        paths: Vec<(usize, String)>,
        total_paths: usize,
        checkpoint: Option<&WatManifestCheckpoint>,
        start_next_path_index: usize,
    ) -> Self {
        let member_cursors = checkpoint
            .map(|cp| cp.path_member_cursors.clone())
            .unwrap_or_default();
        Self {
            config,
            ctx,
            sqs_client,
            sqs_job,
            crawl_id,
            manifest_uri,
            paths,
            total_paths,
            member_cursors,
            start_next_path_index,
        }
    }

    pub async fn run(self) -> Result<usize, std::io::Error> {
        let config = self.config.clone();
        let aws_cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let s3_client = S3Client::new(&aws_cfg);
        let http_client = reqwest::Client::new();

        let ready_count = Arc::new(AtomicUsize::new(0));
        let downloads_in_flight = Arc::new(AtomicUsize::new(0));
        let parses_in_flight = Arc::new(AtomicUsize::new(0));
        let concurrency_limits = Arc::new(ConcurrencyLimits::new(
            download_concurrency_limit(),
            parse_concurrency_limit(),
        ));
        concurrency_limits.clone().spawn_autotune(Arc::clone(&self.ctx));

        let (ready_tx, ready_rx) = mpsc::channel::<ReadyWat>(prefetch_depth_limit());
        let (result_tx, mut result_rx) = mpsc::channel::<PathParseResult>(self.paths.len().max(1));

        let download_sem = Arc::new(Semaphore::new(download_concurrency_limit()));
        let parse_sem = Arc::new(Semaphore::new(parse_concurrency_limit()));
        let ready_bytes = Arc::new(AtomicUsize::new(0));
        let pipeline = Arc::new(Mutex::new(ArrowIngestPipeline::new(Arc::clone(&self.ctx))));
        let tracker = Arc::new(Mutex::new(CompletionTracker::new(
            self.start_next_path_index,
            self.member_cursors.clone(),
        )));

        let download_paths = self.paths.clone();
        let crawl_id_dl = self.crawl_id.clone();
        let max_object_bytes = config.max_wat_object_bytes;
        let ctx_tune = Arc::clone(&self.ctx);
        let limits_dl = Arc::clone(&concurrency_limits);

        let ready_count_dl = Arc::clone(&ready_count);
        let result_tx_dl = result_tx.clone();
        let ready_bytes_dl = Arc::clone(&ready_bytes);
        let downloads_in_flight_dl = Arc::clone(&downloads_in_flight);
        let downloader = tokio::spawn(async move {
            for (path_index, path) in download_paths {
                loop {
                    let pressure = ingest_backpressure_pressure(ctx_tune.as_ref());
                    let ready_depth = ready_count_dl.load(Ordering::Relaxed);
                    let budget_used = ready_bytes_dl.load(Ordering::Relaxed);
                    let effective_downloads = limits_dl.download.load(Ordering::Relaxed);
                    if pressure < 0.85
                        && ready_depth < prefetch_depth_limit()
                        && budget_used < download_memory_budget_bytes()
                        && downloads_in_flight_dl.load(Ordering::Relaxed) < effective_downloads
                    {
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }

                let permit = match download_sem.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                downloads_in_flight_dl.fetch_add(1, Ordering::Relaxed);

                let bytes = match download_wat_bytes(
                    &path,
                    max_object_bytes,
                    Some(&s3_client),
                    Some(&http_client),
                )
                .await
                {
                    Ok(bytes) => bytes,
                    Err(WatStreamOpenError::TooLarge {
                        compressed_bytes,
                        max_wat_object_bytes,
                    }) => {
                        warn!(
                            crawl_id = %crawl_id_dl,
                            wat_path = %path,
                            reason = "wat_object_too_large",
                            compressed_bytes,
                            max_wat_object_bytes,
                            "skipping WAT object"
                        );
                        let _ = result_tx_dl
                            .send(PathParseResult {
                                path_index,
                                final_member_index: 0,
                                records_seen: 0,
                                rows_emitted: 0,
                                skipped: true,
                            })
                            .await;
                        drop(permit);
                        downloads_in_flight_dl.fetch_sub(1, Ordering::Relaxed);
                        continue;
                    }
                    Err(err) => {
                        warn!(
                            crawl_id = %crawl_id_dl,
                            wat_path = %path,
                            reason = "read_failed",
                            error = %err,
                            "skipping WAT object"
                        );
                        let _ = result_tx_dl
                            .send(PathParseResult {
                                path_index,
                                final_member_index: 0,
                                records_seen: 0,
                                rows_emitted: 0,
                                skipped: true,
                            })
                            .await;
                        drop(permit);
                        downloads_in_flight_dl.fetch_sub(1, Ordering::Relaxed);
                        continue;
                    }
                };

                ready_bytes_dl.fetch_add(bytes.len(), Ordering::Relaxed);
                ready_count_dl.fetch_add(1, Ordering::Relaxed);
                if ready_tx
                    .send(ReadyWat {
                        path_index,
                        path,
                        bytes,
                    })
                    .await
                    .is_err()
                {
                    drop(permit);
                    downloads_in_flight_dl.fetch_sub(1, Ordering::Relaxed);
                    break;
                }
                drop(permit);
                downloads_in_flight_dl.fetch_sub(1, Ordering::Relaxed);
            }
        });

        let mut ready_rx = ready_rx;
        let parse_workers = {
            let config = config.clone();
            let crawl_id = self.crawl_id.clone();
            let pipeline = Arc::clone(&pipeline);
            let result_tx = result_tx.clone();
            let parse_sem = Arc::clone(&parse_sem);
            let ready_bytes = Arc::clone(&ready_bytes);
            let ready_count = Arc::clone(&ready_count);
            let member_cursors = self.member_cursors.clone();
            let limits_parse = Arc::clone(&concurrency_limits);
            let parses_in_flight_parse = Arc::clone(&parses_in_flight);
            let ctx_parse = Arc::clone(&self.ctx);

            tokio::spawn(async move {
                let mut join_set = tokio::task::JoinSet::new();
                while let Some(ready) = ready_rx.recv().await {
                    loop {
                        let pressure = ingest_backpressure_pressure(ctx_parse.as_ref());
                        let effective_parses = limits_parse.parse.load(Ordering::Relaxed);
                        if pressure < 0.9
                            && parses_in_flight_parse.load(Ordering::Relaxed) < effective_parses
                        {
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                    let permit = match parse_sem.clone().acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => break,
                    };
                    parses_in_flight_parse.fetch_add(1, Ordering::Relaxed);
                    ready_count.fetch_sub(1, Ordering::Relaxed);
                    ready_bytes.fetch_sub(ready.bytes.len(), Ordering::Relaxed);
                    let start_member = member_cursors.get(&ready.path_index).copied().unwrap_or(0);

                    let config = config.clone();
                    let crawl_id = crawl_id.clone();
                    let pipeline = Arc::clone(&pipeline);
                    let result_tx = result_tx.clone();
                    let parses_in_flight_spawn = Arc::clone(&parses_in_flight_parse);
                    join_set.spawn(async move {
                        let _permit = permit;
                        let result = parse_ready_wat(
                            &config,
                            &crawl_id,
                            ready.path_index,
                            &ready.path,
                            ready.bytes,
                            start_member,
                            pipeline,
                        )
                        .await;
                        match result {
                            Ok(stats) => {
                                let _ = result_tx.send(stats).await;
                            }
                            Err(err) => {
                                warn!(
                                    crawl_id = %crawl_id,
                                    wat_path = %ready.path,
                                    error = %err,
                                    "parse worker failed"
                                );
                                let _ = result_tx
                                    .send(PathParseResult {
                                        path_index: ready.path_index,
                                        final_member_index: 0,
                                        records_seen: 0,
                                        rows_emitted: 0,
                                        skipped: true,
                                    })
                                    .await;
                            }
                        }
                        parses_in_flight_spawn.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                while join_set.join_next().await.is_some() {}
            })
        };

        drop(result_tx);

        let mut files_processed = 0u32;
        let mut records_seen = 0u64;
        let mut rows_emitted = 0u64;
        let mut skips_logged = 0u64;
        let expected = self.paths.len();

        for _ in 0..expected {
            let Some(result) = result_rx.recv().await else {
                break;
            };
            if result.skipped {
                skips_logged = skips_logged.saturating_add(1);
            } else {
                records_seen = records_seen.saturating_add(result.records_seen);
                rows_emitted = rows_emitted.saturating_add(result.rows_emitted);
            }
            files_processed = files_processed.saturating_add(1);

            {
                let mut pipeline_guard = pipeline.lock().await;
                pipeline_guard.await_durable_submissions()?;
            }

            {
                let mut tracker = tracker.lock().await;
                tracker.mark_path_complete(result.path_index, result.final_member_index);
                tracker.maybe_store_manifest_checkpoint(
                    self.ctx.as_ref(),
                    &self.crawl_id,
                    &self.manifest_uri,
                    self.total_paths,
                )?;
            }

            if let (Some(sqs_client), Some(sqs_job)) = (self.sqs_client, self.sqs_job) {
                if files_processed as usize % SQS_VISIBILITY_EXTEND_EVERY_PATHS == 0 {
                    extend_sqs_visibility(sqs_client, sqs_job).await;
                }
            }
        }

        let _ = downloader.await;
        let _ = parse_workers.await;

        let final_index = {
            let tracker = tracker.lock().await;
            tracker.contiguous_complete_prefix()
        };

        finish_shared_pipeline(pipeline, Arc::clone(&self.ctx)).await?;

        info!(
            crawl_id = %self.crawl_id,
            files_processed,
            records_seen,
            rows_emitted,
            skips_logged,
            wat_files_selected = expected,
            next_path_index = final_index,
            total_paths = self.total_paths,
            target_domain_bucket_count = config.target_domain_bucket_count,
            "wat index build complete"
        );

        Ok(final_index)
    }
}

async fn parse_ready_wat(
    config: &UpfoundryLinkGraphWatIndexConfig,
    crawl_id: &str,
    path_index: usize,
    path: &str,
    bytes: Vec<u8>,
    start_member: u64,
    pipeline: Arc<Mutex<ArrowIngestPipeline>>,
) -> Result<PathParseResult, std::io::Error> {
    let mut stream = open_wat_stream_from_bytes(bytes, config.max_wat_object_bytes)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let mut pending: HashMap<u32, TargetIndexBatchBuilder> = HashMap::new();
    let mut pending_bytes = 0usize;
    let mut records_seen = 0u64;
    let mut rows_emitted = 0u64;
    let mut records_seen_in_object = 0u64;
    let mut member_index = 0u64;

    loop {
        if config
            .max_wat_records_per_object
            .is_some_and(|limit| records_seen_in_object >= limit as u64)
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
                    "stopping WAT object after parse failure"
                );
                break;
            }
        };

        member_index = member_index.saturating_add(1);
        if member_index <= start_member {
            continue;
        }

        let (record_offset, record_length, payload) = member;
        let Some(value) = crate::extraction::parse_wat_member_payload(&payload) else {
            continue;
        };
        records_seen = records_seen.saturating_add(1);
        records_seen_in_object = records_seen_in_object.saturating_add(1);

        let location = WatRecordLocation {
            filename: path.to_string(),
            record_offset: i64::try_from(record_offset).unwrap_or(0),
            record_length: i64::try_from(record_length).unwrap_or(0),
        };

        for row in rows_for_extraction_arrow(config, crawl_id, path, location, &value) {
            rows_emitted = rows_emitted.saturating_add(1);
            push_arrow_row(
                crawl_id,
                path_index,
                member_index,
                config,
                &row,
                &mut pending,
                &mut pending_bytes,
                &pipeline,
            )
            .await?;
        }

        {
            let mut tracker_pipeline = pipeline.lock().await;
            while pending_bytes >= source_pending_max_bytes() {
                flush_largest_bucket(
                    crawl_id,
                    path_index,
                    member_index,
                    &mut pending,
                    &mut pending_bytes,
                    &mut tracker_pipeline,
                )
                .await?;
            }
        }
    }

    for bucket in pending.keys().copied().collect::<Vec<_>>() {
        flush_bucket(
            crawl_id,
            path_index,
            member_index,
            bucket,
            &mut pending,
            &mut pending_bytes,
            &pipeline,
        )
        .await?;
    }

    Ok(PathParseResult {
        path_index,
        final_member_index: member_index,
        records_seen,
        rows_emitted,
        skipped: false,
    })
}

async fn push_arrow_row(
    crawl_id: &str,
    path_index: usize,
    member_index: u64,
    config: &UpfoundryLinkGraphWatIndexConfig,
    row: &TargetIndexArrowRow,
    pending: &mut HashMap<u32, TargetIndexBatchBuilder>,
    pending_bytes: &mut usize,
    pipeline: &Arc<Mutex<ArrowIngestPipeline>>,
) -> Result<(), std::io::Error> {
    let bucket = row
        .target_domain_hash_bucket
        .parse::<u32>()
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    let builder = pending.entry(bucket).or_default();
    let before = builder.approx_bytes();
    builder.append_row(row);
    *pending_bytes = pending_bytes.saturating_add(builder.approx_bytes().saturating_sub(before));

    if builder.rows() >= config.max_records_per_batch
        || builder.approx_bytes() >= config.batch_size_bytes
    {
        flush_bucket(
            crawl_id,
            path_index,
            member_index,
            bucket,
            pending,
            pending_bytes,
            pipeline,
        )
        .await?;
    }
    Ok(())
}

async fn flush_largest_bucket(
    crawl_id: &str,
    path_index: usize,
    member_index: u64,
    pending: &mut HashMap<u32, TargetIndexBatchBuilder>,
    pending_bytes: &mut usize,
    pipeline: &mut ArrowIngestPipeline,
) -> Result<(), std::io::Error> {
    let Some(bucket) = pending
        .iter()
        .max_by_key(|(_, batch)| batch.approx_bytes())
        .map(|(bucket, _)| *bucket)
    else {
        return Ok(());
    };
    let mut builder = pending.remove(&bucket).unwrap_or_default();
    let bytes = builder.approx_bytes();
    *pending_bytes = pending_bytes.saturating_sub(bytes);
    pipeline.queue_arrow_batch(crawl_id, bucket, path_index, member_index, &mut builder)
}

async fn flush_bucket(
    crawl_id: &str,
    path_index: usize,
    member_index: u64,
    bucket: u32,
    pending: &mut HashMap<u32, TargetIndexBatchBuilder>,
    pending_bytes: &mut usize,
    pipeline: &Arc<Mutex<ArrowIngestPipeline>>,
) -> Result<(), std::io::Error> {
    let Some(mut builder) = pending.remove(&bucket) else {
        return Ok(());
    };
    let bytes = builder.approx_bytes();
    *pending_bytes = pending_bytes.saturating_sub(bytes);
    let mut pipeline = pipeline.lock().await;
    pipeline.queue_arrow_batch(crawl_id, bucket, path_index, member_index, &mut builder)
}

async fn extend_sqs_visibility(sqs_client: &SqsClient, job: &ActiveSqsJob) {
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

pub async fn finish_shared_pipeline(
    pipeline: Arc<Mutex<ArrowIngestPipeline>>,
    ctx: Arc<dyn SourceSyncContext>,
) -> Result<(), std::io::Error> {
    let mut guard = pipeline.lock().await;
    guard.flush_pending()?;
    let finished = std::mem::replace(&mut *guard, ArrowIngestPipeline::new(Arc::clone(&ctx)));
    drop(guard);
    finished.finish()
}
