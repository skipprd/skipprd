use crate::buffer::segment_file::{PartitionKey, SegmentFileMetadata, WalPayload};
use crate::buffer::wal_handle::{self, PipelineWalHandle};
use crate::buffer::wal_object_store::{parse_s3_prefix, S3WalObjectStore};
use crate::buffer::wal_persist::{persist_error_from_io, persist_snapshot_pair};
use crate::helpers::configuration::Config;
use crate::helpers::wal_storage::WalStorage;
use skippr_lease::LeaseGuard;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::runtime::Handle;

pub enum SegmentWriteLocation {
    Disk { path: PathBuf },
    S3 { key: String, bucket: String },
    Clustered { path: PathBuf, uri: String },
}

pub struct SegmentWriteResult {
    pub meta: SegmentFileMetadata,
    pub total_rows: u64,
    pub sha256: [u8; 32],
    pub location: SegmentWriteLocation,
    pub offsets_published: bool,
}

pub(crate) fn block_on_async<F, T>(fut: F) -> io::Result<T>
where
    F: std::future::Future<Output = io::Result<T>>,
{
    let build = || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("runtime: {}", e)))
    };
    if Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| Handle::current().block_on(fut))
    } else {
        build()?.block_on(fut)
    }
}

fn ingest_writer_handle(handle: &PipelineWalHandle) -> bool {
    !matches!(
        handle.guard().role(),
        skippr_lease::PipelineRole::Replica { .. }
    )
}

fn ingest_primary_handle(config: &Config) -> Option<std::sync::Arc<PipelineWalHandle>> {
    let pipeline = config.get_pipeline_name();
    if pipeline.is_empty() {
        return None;
    }
    let key =
        skippr_lease::PipelineKey::new(config.get_tenant(), config.get_workspace_name(), pipeline)
            .ok()?;
    wal_handle::wal_handle_for(&key).filter(|handle| ingest_writer_handle(handle))
}

pub(crate) async fn live_wal_segments(config: &Config) -> Option<crate::plugins::LiveWalSegments> {
    let handle = ingest_primary_handle(config)?;
    match handle.live_wal_segments().await {
        Ok(live) => Some(live),
        Err(err) => {
            tracing::warn!(
                "WAL live-segment fence unavailable for pipeline {:?}: {err}",
                handle.key()
            );
            None
        }
    }
}

pub fn ingest_wal_handle() -> Option<std::sync::Arc<PipelineWalHandle>> {
    ingest_primary_handle(&Config::new()).or_else(|| {
        wal_handle::all_wal_handles()
            .into_iter()
            .find(|handle| ingest_writer_handle(handle))
    })
}

fn ingest_handle_for_paths(config: &Config) -> Option<std::sync::Arc<PipelineWalHandle>> {
    if let Some(handle) = ingest_primary_handle(config) {
        return Some(handle);
    }
    let data_dir = PathBuf::from(config.get_data_dir());
    ingest_wal_handle().filter(|handle| {
        handle.paths().root.starts_with(&data_dir) || handle.paths().segs.starts_with(&data_dir)
    })
}

pub fn ingest_segment_dir(config: &Config) -> Option<PathBuf> {
    ingest_handle_for_paths(config).map(|handle| handle.paths().segs.clone())
}

pub fn ingest_quarantine_dir(config: &Config) -> Option<PathBuf> {
    ingest_handle_for_paths(config)
        .map(|handle| handle.paths().root.join("segment_buffer/quarantine"))
}

pub async fn persist_snapshot(
    config: &Config,
    snapshot_id: &str,
    offsets: &HashMap<crate::helpers::offsets::OffsetKey, u64>,
    batches: &HashMap<PartitionKey, WalPayload>,
    partitions_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
    part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
    checkpoint_updates: &HashMap<String, crate::plugins::cdc::CheckpointEnvelope>,
) -> io::Result<SegmentWriteResult> {
    let handle = ingest_wal_handle()
        .ok_or_else(|| io::Error::other("WAL write requires an installed catalog handle"))?;
    match config.get_wal_storage() {
        WalStorage::S3 => {
            let lease = Arc::clone(handle.guard());
            let client = crate::helpers::s3::get_s3_client().await;
            let bucket = config.get_wal_s3_bucket();
            let base = config.get_wal_s3_prefix();
            let prefix_url = format!("s3://{}/{}", bucket, base.trim_start_matches('/'));
            let (bucket, _) = parse_s3_prefix(&prefix_url)?;
            let store = S3WalObjectStore::new(client, bucket.clone());
            let payload = persist_snapshot_pair(
                &store,
                &prefix_url,
                snapshot_id,
                offsets,
                batches,
                partitions_meta,
                part_meta_blobs,
                lease.as_ref(),
            )
            .await
            .map_err(|err| persist_error_from_io(err, snapshot_id))?;
            let crate::buffer::wal_store::SegmentWriteLocation::S3 { key, bucket } =
                &payload.location
            else {
                return Err(io::Error::other("S3 persist returned a non-S3 location"));
            };
            let encoded = crate::buffer::segment_file::EncodedSnapshot {
                bytes: Vec::new(),
                sha256: payload.sha256,
                meta: payload.meta.clone(),
                total_rows: payload.total_rows,
            };
            handle
                .persist_s3_segment(
                    snapshot_id,
                    &encoded,
                    bucket,
                    key,
                    offsets,
                    checkpoint_updates,
                )
                .await
                .map_err(|err| io::Error::other(err.to_string()))
        }
        WalStorage::Disk | WalStorage::Clustered => handle
            .persist_segment(
                snapshot_id,
                offsets,
                batches,
                partitions_meta,
                part_meta_blobs,
                checkpoint_updates,
            )
            .await
            .map_err(|err| io::Error::other(err.to_string())),
    }
}

pub fn ingest_writer_lease() -> io::Result<Arc<LeaseGuard>> {
    ingest_wal_handle()
        .map(|handle| Arc::clone(handle.guard()))
        .ok_or_else(|| io::Error::other("pipeline writer lease is not installed"))
}

pub fn ensure_disk_wal_handle(
    config: &Config,
    offsets: std::sync::Arc<crate::helpers::offsets::Offsets>,
    guard: Arc<LeaseGuard>,
) -> io::Result<Arc<PipelineWalHandle>> {
    if let Some(handle) = ingest_primary_handle(config) {
        return Ok(handle);
    }
    let pipeline = config.get_pipeline_name();
    if pipeline.is_empty() {
        return Err(io::Error::other(
            "disk WAL catalog requires a pipeline name",
        ));
    }
    let key =
        skippr_lease::PipelineKey::new(config.get_tenant(), config.get_workspace_name(), pipeline)
            .map_err(|err| io::Error::other(err.to_string()))?;
    if guard.key() != &key {
        return Err(io::Error::other(
            "installed writer lease does not match this pipeline",
        ));
    }
    let paths =
        skippr_lease::PipelinePaths::legacy_disk(std::path::Path::new(&config.get_data_dir()));
    let session = guard
        .leased_session()
        .ok_or_else(|| io::Error::other("pipeline writer lease has no session"))?;
    let fence = skippr_wal_catalog::CatalogFence {
        owner: session.owner,
        epoch: session.epoch,
    };
    let catalog =
        crate::pipeline_backend::open_wal_catalog_for_config(config, Some(offsets.as_ref()), fence)
            .map_err(io::Error::other)?;
    let offset_store = offsets
        .bytes_store()
        .cloned()
        .ok_or_else(|| io::Error::other("WAL catalog requires an offset store"))?;
    let handle = PipelineWalHandle::new(key, paths, guard, catalog, offset_store);
    wal_handle::install_wal_handle(handle.clone());
    Ok(handle)
}

/// Open the WAL catalog for `skipprd query` without taking the ingest writer lease.
pub fn ensure_query_wal_handle(config: &Config) -> io::Result<Option<Arc<PipelineWalHandle>>> {
    let pipeline = config.get_pipeline_name();
    if pipeline.is_empty() {
        return Ok(None);
    }
    let key =
        skippr_lease::PipelineKey::new(config.get_tenant(), config.get_workspace_name(), pipeline)
            .map_err(|err| io::Error::other(err.to_string()))?;
    if let Some(handle) = wal_handle::wal_handle_for(&key) {
        return Ok(Some(handle));
    }
    let offsets = crate::helpers::offsets::Offsets::init(config)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let offsets = Arc::new(offsets);
    let clock = Arc::new(skippr_lease::SystemClock::new());
    let guard = skippr_lease::LeaseGuard::single_node(key.clone(), clock);
    let fence = wal_handle::catalog_fence_from_guard(&guard)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let catalog =
        crate::pipeline_backend::open_wal_catalog_for_config(config, Some(offsets.as_ref()), fence)
            .map_err(io::Error::other)?;
    let offset_store = offsets
        .bytes_store()
        .cloned()
        .ok_or_else(|| io::Error::other("WAL catalog requires an offset store"))?;
    let paths =
        skippr_lease::PipelinePaths::legacy_disk(std::path::Path::new(&config.get_data_dir()));
    let handle = PipelineWalHandle::new(key, paths, guard, catalog, offset_store);
    wal_handle::install_wal_handle(handle.clone());
    Ok(Some(handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_wal_handle_skips_unbound_pipeline() {
        let config = Config::new();
        assert!(ensure_query_wal_handle(&config).unwrap().is_none());
    }

    #[test]
    fn block_on_async_completes_without_runtime() {
        assert_eq!(block_on_async(async { Ok::<_, io::Error>(3) }).unwrap(), 3);
    }

    #[test]
    fn block_on_async_completes_inside_multi_thread_runtime() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let value = rt.block_on(async {
            tokio::spawn(async { block_on_async(async { Ok::<_, io::Error>(7) }).unwrap() })
                .await
                .unwrap()
        });
        assert_eq!(value, 7);
    }
}
