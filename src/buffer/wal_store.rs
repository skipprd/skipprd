use crate::buffer::segment_file::{PartitionKey, SegmentFileMetadata, WalPayload};
use crate::buffer::wal_handle::{self, PipelineWalHandle};
use crate::buffer::wal_object_store::{parse_s3_prefix, S3WalObjectStore};
use crate::buffer::wal_persist::{persist_error_from_io, persist_snapshot_pair};
use crate::buffer::wal_reader::{install_query_reader, wal_reader_for, WalCatalogReader};
use crate::helpers::configuration::Config;
use crate::helpers::wal_storage::WalStorage;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::runtime::Handle;

pub enum SegmentWriteLocation {
    Disk { path: PathBuf },
    S3 { key: String, bucket: String },
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

fn ingest_primary_handle(config: &Config) -> Option<std::sync::Arc<PipelineWalHandle>> {
    let pipeline = config.get_pipeline_name();
    if pipeline.is_empty() {
        return None;
    }
    let key =
        skippr_lease::PipelineKey::new(config.get_tenant(), config.get_workspace_name(), pipeline)
            .ok()?;
    wal_handle::wal_handle_for(&key)
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
    ingest_primary_handle(&Config::new())
        .or_else(|| wal_handle::all_wal_handles().into_iter().next())
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
    created_at: SystemTime,
) -> io::Result<SegmentWriteResult> {
    let handle = ingest_wal_handle()
        .ok_or_else(|| io::Error::other("WAL write requires an installed catalog handle"))?;
    match config.get_wal_storage() {
        WalStorage::S3 => {
            let encoded = crate::buffer::segment_file::encode_snapshot(
                offsets,
                batches,
                partitions_meta,
                part_meta_blobs,
                created_at,
            )?;
            let id = skippr_lease::SegmentId::new(snapshot_id)
                .map_err(|err| io::Error::other(err.to_string()))?;
            let existing = handle
                .catalog()
                .get(&skippr_wal_catalog::CatalogKey::Segment(id))
                .map_err(|err| io::Error::other(err.to_string()))?;
            if let Some(skippr_wal_catalog::CatalogRow::Segment(row)) = existing {
                if row.payload_sha256 != encoded.sha256 {
                    return Err(io::Error::other(
                        "segment_id reuse with different payload sha",
                    ));
                }
                let skippr_wal_catalog::PayloadLocation::S3 { bucket, key } = row.location else {
                    return Err(io::Error::other(
                        "S3 persist found a disk catalog row for the same segment_id",
                    ));
                };
                let encoded = crate::buffer::segment_file::EncodedSnapshot {
                    bytes: Vec::new(),
                    sha256: encoded.sha256,
                    meta: encoded.meta,
                    total_rows: encoded.total_rows,
                };
                return handle
                    .persist_s3_segment(
                        snapshot_id,
                        &encoded,
                        &bucket,
                        &key,
                        offsets,
                        checkpoint_updates,
                    )
                    .await
                    .map_err(|err| io::Error::other(err.to_string()));
            }
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
                created_at,
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
        WalStorage::Disk => handle
            .persist_segment(
                snapshot_id,
                offsets,
                batches,
                partitions_meta,
                part_meta_blobs,
                checkpoint_updates,
                created_at,
            )
            .await
            .map_err(|err| io::Error::other(err.to_string())),
    }
}

pub fn ensure_disk_wal_handle(
    config: &Config,
    offsets: std::sync::Arc<crate::helpers::offsets::Offsets>,
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
    let paths =
        skippr_lease::PipelinePaths::legacy_disk(std::path::Path::new(&config.get_data_dir()));
    let fence = skippr_wal_catalog::CatalogFence::local();
    let catalog =
        crate::pipeline_backend::open_wal_catalog_for_config(config, Some(offsets.as_ref()), fence)
            .map_err(io::Error::other)?;
    let offset_store = offsets
        .bytes_store()
        .cloned()
        .ok_or_else(|| io::Error::other("WAL catalog requires an offset store"))?;
    let handle = PipelineWalHandle::new(key, paths, catalog, offset_store, fence);
    wal_handle::install_wal_handle(handle.clone());
    Ok(handle)
}

fn query_catalog_open_error(pipeline: &str, err: impl std::fmt::Display) -> io::Error {
    let text = err.to_string();
    if text.contains("already ingesting") || text.contains("could not acquire lock") {
        io::Error::other(format!(
            "WAL catalog for pipeline {pipeline} is locked by a running ingest; use a shared catalog backend"
        ))
    } else {
        io::Error::other(text)
    }
}

/// Open the Skippr Store WAL index for query. Never opens a writer catalog.
pub fn open_query_wal_reader(config: &Config) -> io::Result<WalCatalogReader> {
    let pipeline = config.get_pipeline_name();
    if pipeline.is_empty() {
        return Err(io::Error::other(
            "query WAL catalog requires a pipeline name",
        ));
    }
    let key = skippr_lease::PipelineKey::new(
        config.get_tenant(),
        config.get_workspace_name(),
        pipeline.as_str(),
    )
    .map_err(|err| io::Error::other(err.to_string()))?;
    if let Some(reader) = wal_reader_for(&key) {
        return Ok(reader);
    }
    let offsets = crate::helpers::offsets::Offsets::init(config)
        .map_err(|err| query_catalog_open_error(&pipeline, err))?;
    let catalog =
        crate::pipeline_backend::open_wal_catalog_reader_for_config(config, Some(&offsets))
            .map_err(|err| query_catalog_open_error(&pipeline, err))?;
    let reader = WalCatalogReader::new(key, catalog);
    install_query_reader(reader.clone())?;
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_snapshot_s3_checks_catalog_before_put() {
        let src = include_str!("wal_store.rs");
        let start = src
            .find("pub async fn persist_snapshot")
            .expect("persist_snapshot");
        let body = src[start..]
            .split("pub fn ensure_disk_wal_handle")
            .next()
            .expect("persist_snapshot body");
        let catalog_at = body.find("CatalogKey::Segment").expect("catalog get");
        let put_at = body.find("persist_snapshot_pair").expect("S3 PUT path");
        assert!(catalog_at < put_at);
        assert!(body.contains("created_at"));
    }

    #[test]
    fn query_reader_opens_skippr_store_without_writer_lease() {
        let src = include_str!("wal_store.rs");
        let start = src
            .find("pub fn open_query_wal_reader")
            .expect("open_query_wal_reader");
        let body = src[start..].split("#[cfg(test)]").next().unwrap();
        assert!(
            body.contains("open_wal_catalog_reader_for_config"),
            "query reader must use the Skippr Store reader opener"
        );
        assert!(
            !body.contains("open_wal_catalog_for_config"),
            "query reader must not open a writer WalCatalogStore"
        );
        assert!(
            !body.contains("query_reader_fence"),
            "query reader must not install a commit fence"
        );
        assert!(
            !body.contains("ensure_disk_wal_handle"),
            "query reader must not open a writer handle"
        );
        assert!(
            !body.contains("PipelineWalHandle"),
            "query reader must not install a writer handle"
        );
        assert!(
            !body.contains("install_wal_handle"),
            "query reader must not install a writer handle"
        );
    }

    #[test]
    fn query_wal_reader_rejects_unbound_pipeline() {
        let config = Config::new();
        let err = open_query_wal_reader(&config).unwrap_err();
        assert!(
            err.to_string().contains("requires a pipeline name"),
            "got {err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn open_query_wal_reader_reports_locked_sled() {
        use crate::helpers::offsets::SLED_NAME;
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "ws-lock", "tenant": "ten-lock" },
            "pipelines": {
                "orders": {
                    "data_source": "data_sources.sample",
                    "data_dir": dir.path().display().to_string()
                }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let config = config.bind_pipeline("orders");
        let data_dir = config.get_data_dir();
        let _holder = sled::open(format!("{data_dir}/{SLED_NAME}")).unwrap();
        let err = open_query_wal_reader(&config).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("locked by a running ingest"),
            "sled file lock must fail closed with the shared-catalog message, got {text}"
        );
        assert!(
            text.contains("orders"),
            "locked-ingest error must name the pipeline, got {text}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn open_query_wal_reader_installs_without_writer_handle() {
        use crate::buffer::wal_handle::wal_handle_for;
        use crate::buffer::wal_reader::clear_query_readers;
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "ws-open", "tenant": "ten-open" },
            "pipelines": {
                "orders": {
                    "data_source": "data_sources.sample",
                    "data_dir": dir.path().display().to_string()
                }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let config = config.bind_pipeline("orders");
        clear_query_readers();
        let reader = open_query_wal_reader(&config).unwrap();
        assert!(
            wal_handle_for(reader.pipeline()).is_none(),
            "query open must not install a writer handle"
        );
        assert!(reader.live_slices("ns").unwrap().is_empty());
        clear_query_readers();
    }

    #[test]
    #[serial_test::serial]
    fn open_query_wal_reader_reuses_in_process_writer() {
        use crate::buffer::wal_handle::{
            install_wal_handle, memory_catalog_for_test, remove_wal_handle, wal_handle_for,
            PipelineWalHandle,
        };
        use crate::buffer::wal_reader::{clear_query_readers, wal_reader_for};
        use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
        use serde_json::json;
        use skippr_lease::{PipelineKey, PipelinePaths};

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("ten-reuse", "ws-reuse", "orders").unwrap();
        remove_wal_handle(&key);
        clear_query_readers();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let fence = skippr_wal_catalog::CatalogFence::local();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn OffsetStore> = Arc::new(SledOffsetStore::new(tree));
        let handle = PipelineWalHandle::new(key.clone(), paths, catalog, offsets, fence);
        install_wal_handle(handle);
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "ws-reuse", "tenant": "ten-reuse" },
            "pipelines": {
                "orders": {
                    "data_source": "data_sources.sample",
                    "data_dir": dir.path().display().to_string()
                }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let config = config.bind_pipeline("orders");
        let reader = open_query_wal_reader(&config).unwrap();
        assert!(wal_handle_for(reader.pipeline()).is_some());
        assert!(reader.live_slices("ns").unwrap().is_empty());
        remove_wal_handle(&key);
        assert!(
            wal_reader_for(&key).is_none(),
            "in-process reuse must not install QUERY_READERS"
        );
        clear_query_readers();
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
