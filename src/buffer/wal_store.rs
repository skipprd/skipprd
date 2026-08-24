use crate::buffer::segment_file::{PartitionKey, SegmentFileMetadata};
use crate::buffer::segment_object::SegmentObject;
use crate::helpers::configuration::Config;
use crate::helpers::wal_storage::WalStorage;
use arrow::array::RecordBatch;
use async_trait::async_trait;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;
use tokio::runtime::Handle;

/// Where a segment was written -- compiler-enforced, no Options.
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

/// Run an async future to completion from synchronous code, regardless of whether
/// a tokio runtime is already active on this thread. Inside an existing runtime,
/// `block_in_place` parks the worker so nested `block_on` is legal.
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

/// Ingest-path lookup: the ActivePrimary store only.
/// Replica/query code must use [`crate::buffer::durable::durable_store_for`].
pub fn ingest_durable_store() -> Option<std::sync::Arc<crate::buffer::durable::PipelineDurableStore>>
{
    crate::buffer::durable::all_durable_stores()
        .into_iter()
        .find(|store| {
            matches!(
                store.guard().role(),
                skippr_lease::PipelineRole::ActivePrimary(_)
            )
        })
}

fn ingest_legacy_buffer_dir(leaf: &str) -> PathBuf {
    PathBuf::from(format!("{}/segment_buffer/{leaf}", Config::get_data_dir()))
}

/// Segment directory for the ingest pipeline: clustered `PipelinePaths` when an
/// ActivePrimary store is installed, otherwise the legacy `{DATA_DIR}/segment_buffer/...`
/// layout used by S3 WAL.
pub fn ingest_segment_dir() -> PathBuf {
    ingest_durable_store()
        .map(|store| store.paths().segs.clone())
        .unwrap_or_else(|| ingest_legacy_buffer_dir("segs"))
}

pub fn ingest_completion_dir() -> PathBuf {
    ingest_durable_store()
        .map(|store| store.paths().completions.clone())
        .unwrap_or_else(|| ingest_legacy_buffer_dir("done"))
}

pub fn ingest_compaction_dir() -> PathBuf {
    ingest_durable_store()
        .map(|store| store.paths().compactions.clone())
        .unwrap_or_else(|| ingest_legacy_buffer_dir("compactions"))
}

pub fn ingest_quarantine_dir() -> PathBuf {
    ingest_durable_store()
        .map(|store| store.paths().root.join("segment_buffer/quarantine"))
        .unwrap_or_else(|| ingest_legacy_buffer_dir("quarantine"))
}

/// Minimal WAL store interface.
#[async_trait]
pub trait WalStore {
    async fn write_snapshot_and_commit(
        &self,
        snapshot_id: &str,
        offsets: &HashMap<crate::helpers::offsets::OffsetKey, u64>,
        batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
        partitions_meta: &HashMap<PartitionKey, (u64 /*bytes*/, SystemTime /*updated*/)>,
        part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
        checkpoint_updates: &HashMap<String, crate::plugins::cdc::CheckpointEnvelope>,
    ) -> io::Result<SegmentWriteResult>;
}

pub struct S3WalStore {
    /// s3://bucket/prefix (without trailing slash)
    prefix_url: String,
}

impl S3WalStore {
    pub fn new(prefix_url: &str) -> Self {
        S3WalStore {
            prefix_url: prefix_url.to_string(),
        }
    }

    // no extra helpers; streaming lives in SegmentObject
}

#[async_trait]
impl WalStore for S3WalStore {
    async fn write_snapshot_and_commit(
        &self,
        snapshot_id: &str,
        offsets: &HashMap<crate::helpers::offsets::OffsetKey, u64>,
        batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
        partitions_meta: &HashMap<PartitionKey, (u64 /*bytes*/, SystemTime /*updated*/)>,
        part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
        _checkpoint_updates: &HashMap<String, crate::plugins::cdc::CheckpointEnvelope>,
    ) -> io::Result<SegmentWriteResult> {
        let client = crate::helpers::s3::get_s3_client().await;
        let (meta, total_rows, sha256, bucket, key) = SegmentObject::stream_snapshot_to_s3(
            &client,
            &self.prefix_url,
            snapshot_id,
            offsets,
            batches,
            partitions_meta,
            part_meta_blobs,
        )
        .await?;
        Ok(SegmentWriteResult {
            meta,
            total_rows,
            sha256,
            location: SegmentWriteLocation::S3 { key, bucket },
            offsets_published: false,
        })
    }
}

/// Disk-backed WAL uses [`PipelineDurableStore`] via [`ensure_disk_durable_store`].

/// Install a local-only [`PipelineDurableStore`] on the legacy disk layout.
/// Clustered mode must not call this; it uses tenant-scoped [`skippr_lease::PipelinePaths::new`].
pub fn ensure_disk_durable_store(
    offsets: std::sync::Arc<crate::helpers::offsets::Offsets>,
) -> io::Result<()> {
    if ingest_durable_store().is_some() {
        return Ok(());
    }
    let pipeline = Config::get_pipeline_name();
    if pipeline.is_empty() {
        return Err(io::Error::other(
            "disk durable store requires a pipeline name",
        ));
    }
    let key = skippr_lease::PipelineKey::new(
        Config::get_tenant(),
        Config::get_workspace_name(),
        pipeline,
    )
    .map_err(|err| io::Error::other(err.to_string()))?;
    let paths =
        skippr_lease::PipelinePaths::legacy_disk(std::path::Path::new(&Config::get_data_dir()));
    let clock = std::sync::Arc::new(skippr_lease::SystemClock::new());
    let guard = skippr_lease::LeaseGuard::single_node(key.clone(), clock);
    let log = crate::buffer::durable::log::MutationLog::open(paths.clone())
        .map_err(|err| io::Error::other(err.to_string()))?;
    let store = crate::buffer::durable::store::PipelineDurableStore::new(
        key,
        paths,
        guard,
        log,
        crate::buffer::durable::replicate::ReplicationMode::LocalOnly,
        crate::buffer::durable::store::OffsetMode::Sled(offsets),
    );
    crate::buffer::durable::install_durable_store(store);
    Ok(())
}

pub struct WalStoreFactory;

impl WalStoreFactory {
    pub fn for_batches(
        batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
    ) -> Box<dyn WalStore + Send + Sync> {
        let _ = batches; // unused; selection via root-level storage
        match Config::get_wal_storage() {
            WalStorage::S3 => {
                let bucket = Config::get_wal_s3_bucket();
                let base = Config::get_wal_s3_prefix();
                let prefix_url = format!("s3://{}/{}", bucket, base.trim_start_matches('/'));
                Box::new(S3WalStore::new(&prefix_url))
            }
            WalStorage::Disk | WalStorage::Clustered => {
                Box::new(crate::buffer::durable::store::ClusteredWalStore)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::durable::log::MutationLog;
    use crate::buffer::durable::replicate::ReplicationMode;
    use crate::buffer::durable::store::{
        install_durable_store, remove_durable_store, MemoryOffsetPublisher, OffsetMode,
        PipelineDurableStore,
    };
    use crate::helpers::configuration::{Config, PIPELINE_NAME};
    use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SystemClock};
    use std::sync::Arc;

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

    #[test]
    #[serial_test::serial]
    fn ingest_segment_dir_uses_clustered_store_paths() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("hla-e2e", "local", "hla_events").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let expected = paths.segs.clone();
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let log = MutationLog::open(paths.clone()).unwrap();
        let store = PipelineDurableStore::new(
            key.clone(),
            paths,
            guard,
            log,
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
        );
        install_durable_store(store);
        Config::setenv("TENANT", "hla-e2e");
        Config::setenv("WORKSPACE_NAME", "local");
        {
            let mut name = PIPELINE_NAME.write();
            name.clear();
            name.push_str("hla_events");
        }
        let got = ingest_segment_dir();
        remove_durable_store(&key);
        {
            let mut name = PIPELINE_NAME.write();
            name.clear();
        }
        Config::set_evncache("TENANT", "");
        Config::set_evncache("WORKSPACE_NAME", "");
        std::env::remove_var("TENANT");
        std::env::remove_var("WORKSPACE_NAME");
        assert_eq!(got, expected);
    }

    #[test]
    #[serial_test::serial]
    fn ingest_durable_store_requires_active_primary() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "idle-only").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            skippr_lease::LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let log = MutationLog::open(paths.clone()).unwrap();
        let store = PipelineDurableStore::new(
            key.clone(),
            paths,
            guard,
            log,
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
        );
        install_durable_store(store);
        assert!(ingest_durable_store().is_none());
        remove_durable_store(&key);
    }
}
