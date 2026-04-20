use crate::buffer::segment_file::{PartitionKey, SegmentFile, SegmentFileMetadata};
use crate::buffer::segment_object::SegmentObject;
use crate::helpers::configuration::Config;
use arrow::array::RecordBatch;
use arrow::ipc::reader::StreamReader;
use async_trait::async_trait;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::{Read, Seek};
use std::path::PathBuf;
use std::time::SystemTime;
use tokio::runtime::Handle;
use url::Url;

/// Where a segment was written -- compiler-enforced, no Options.
pub enum SegmentWriteLocation {
    Disk {
        path: PathBuf,
    },
    S3 {
        key: String,
        bucket: String,
        data: Vec<u8>,
    },
}

pub struct SegmentWriteResult {
    pub meta: SegmentFileMetadata,
    pub total_rows: u64,
    pub sha256: [u8; 32],
    pub location: SegmentWriteLocation,
}

/// Run an async future to completion from synchronous code, regardless of whether
/// a tokio runtime is already active on this thread. When inside an existing runtime
/// the work is offloaded to a helper thread to avoid nesting `block_on` calls.
fn block_on_async<F, T>(fut: F) -> io::Result<T>
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
    if let Ok(handle) = Handle::try_current() {
        handle.block_on(fut)
    } else {
        build()?.block_on(fut)
    }
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
    ) -> io::Result<SegmentWriteResult> {
        let client = crate::helpers::s3::get_s3_client().await;
        let (meta, total_rows, sha256, bucket, key, data) = SegmentObject::stream_snapshot_to_s3(
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
            location: SegmentWriteLocation::S3 { key, bucket, data },
        })
    }
}

/// Disk-backed WAL store using SegmentFile + local commit.
pub struct DiskWalStore;

#[async_trait]
impl WalStore for DiskWalStore {
    async fn write_snapshot_and_commit(
        &self,
        snapshot_id: &str,
        offsets: &HashMap<crate::helpers::offsets::OffsetKey, u64>,
        batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
        partitions_meta: &HashMap<PartitionKey, (u64 /*bytes*/, SystemTime /*updated*/)>,
        part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
    ) -> io::Result<SegmentWriteResult> {
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        let seg_file = SegmentFile::new(&seg_dir, snapshot_id)?;
        let (meta, total_rows, sha256) =
            seg_file.write_snapshot(offsets, batches, partitions_meta, part_meta_blobs)?;
        // Publish the commit marker with the same fsync discipline as the legacy
        // WAL path so offsets are not advanced before the marker is durable.
        crate::buffer::ingest_buffer::Buffers::write_seg_commit(
            &seg_file.path,
            &sha256,
            meta.num_partitions,
            meta.total_bytes,
        )?;
        Ok(SegmentWriteResult {
            meta,
            total_rows,
            sha256,
            location: SegmentWriteLocation::Disk {
                path: seg_file.path,
            },
        })
    }
}

pub struct WalStoreFactory;

impl WalStoreFactory {
    pub fn for_batches(
        batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
    ) -> Box<dyn WalStore + Send + Sync> {
        let _ = batches; // unused; selection via root-level storage
        if Config::get_wal_storage().eq_ignore_ascii_case("s3") {
            let bucket = Config::get_wal_s3_bucket();
            let base = Config::get_wal_s3_prefix();
            let prefix_url = format!("s3://{}/{}", bucket, base.trim_start_matches('/'));
            return Box::new(S3WalStore::new(&prefix_url));
        }
        Box::new(DiskWalStore)
    }
}

/// Read-side abstraction for WAL
pub trait WalReader {
    fn load_committed_batches(
        &self,
        pipeline: &str,
        limit_files: usize,
    ) -> io::Result<Vec<RecordBatch>>;
}

pub struct DiskWalReader;

impl WalReader for DiskWalReader {
    fn load_committed_batches(
        &self,
        pipeline: &str,
        limit_files: usize,
    ) -> io::Result<Vec<RecordBatch>> {
        let mut out: Vec<RecordBatch> = Vec::new();
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() {
            return Ok(out);
        }
        let mut used = 0usize;
        for entry in fs::read_dir(&seg_dir)? {
            if used >= limit_files {
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("seg") {
                continue;
            }
            let commit = path.with_extension("seg.commit");
            if !commit.exists() {
                continue;
            }
            let seg = SegmentFile { path: path.clone() };
            let meta = match seg.read_metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            for idx in meta.index.iter() {
                if idx.key.namespace != pipeline {
                    continue;
                }
                let mut file = match fs::OpenOptions::new().read(true).open(&path) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                if file.seek(std::io::SeekFrom::Start(idx.start)).is_err() {
                    continue;
                }
                let reader = std::io::BufReader::new(file);
                let mut take = reader.take(idx.len);
                if let Ok(sr) = StreamReader::try_new(&mut take, None) {
                    for it in sr {
                        if let Ok(b) = it {
                            out.push(b);
                        }
                    }
                }
                used += 1;
                if used >= limit_files {
                    break;
                }
            }
        }
        Ok(out)
    }
}

pub struct S3WalReader {
    pub prefix_url: String,
}

impl WalReader for S3WalReader {
    fn load_committed_batches(
        &self,
        pipeline: &str,
        limit_files: usize,
    ) -> io::Result<Vec<RecordBatch>> {
        let prefix = self.prefix_url.clone();
        let pipe = pipeline.to_string();
        let fut = async move {
            let mut out: Vec<RecordBatch> = Vec::new();
            let u = match Url::parse(&prefix) {
                Ok(u) => u,
                Err(_) => return Ok(out),
            };
            if u.scheme() != "s3" {
                return Ok(out);
            }
            let bucket = match u.host_str() {
                Some(b) => b.to_string(),
                None => return Ok(out),
            };
            let base = u
                .path()
                .trim_start_matches('/')
                .trim_end_matches('/')
                .to_string();
            let client = crate::helpers::s3::get_s3_client().await;
            // list objects
            let mut token: Option<String> = None;
            let mut all: Vec<String> = Vec::new();
            loop {
                let mut req = client.list_objects_v2().bucket(&bucket).prefix(&base);
                if let Some(t) = &token {
                    req = req.continuation_token(t);
                }
                match req.send().await {
                    Ok(resp) => {
                        if let Some(contents) = resp.contents {
                            for obj in contents {
                                if let Some(k) = obj.key() {
                                    all.push(k.to_string());
                                }
                            }
                        }
                        if resp.is_truncated.unwrap_or(false) {
                            token = resp.next_continuation_token;
                        } else {
                            break;
                        }
                    }
                    Err(_) => break,
                }
                if all.len() > 10_000 {
                    break;
                }
            }
            use std::collections::HashSet;
            let set: HashSet<String> = all.iter().cloned().collect();
            let mut used = 0usize;
            for k in all.into_iter().filter(|k| k.ends_with(".seg")) {
                if used >= limit_files {
                    break;
                }
                let commit = format!("{}.commit", k);
                if !set.contains(&commit) {
                    continue;
                }
                match client.get_object().bucket(&bucket).key(&k).send().await {
                    Ok(resp) => match resp.body.collect().await {
                        Ok(agg) => {
                            let bytes = agg.into_bytes().to_vec();
                            if let Ok(meta) = SegmentFile::read_metadata_from_bytes(&bytes) {
                                for idx in meta.index.iter() {
                                    if idx.key.namespace != pipe {
                                        continue;
                                    }
                                    let start = idx.start as usize;
                                    let end = start.saturating_add(idx.len as usize);
                                    if end > bytes.len() {
                                        continue;
                                    }
                                    let slice = &bytes[start..end];
                                    let mut cursor = std::io::Cursor::new(slice);
                                    if let Ok(sr) = StreamReader::try_new(&mut cursor, None) {
                                        for it in sr {
                                            if let Ok(b) = it {
                                                out.push(b);
                                            }
                                        }
                                    }
                                }
                                used += 1;
                            }
                        }
                        Err(_) => {}
                    },
                    Err(_) => {}
                }
            }
            Ok(out)
        };
        block_on_async(fut)
    }
}

pub struct WalReaderFactory;

impl WalReaderFactory {
    pub fn for_pipeline(_pipeline: &str) -> Box<dyn WalReader + Send + Sync> {
        if Config::get_wal_storage().eq_ignore_ascii_case("s3") {
            let bucket = Config::get_wal_s3_bucket();
            let base = Config::get_wal_s3_prefix();
            let prefix_url = format!("s3://{}/{}", bucket, base.trim_start_matches('/'));
            return Box::new(S3WalReader { prefix_url });
        }
        Box::new(DiskWalReader)
    }

    pub async fn for_pipeline_async(_pipeline: &str) -> Box<dyn WalReader + Send + Sync> {
        if Config::get_wal_storage().eq_ignore_ascii_case("s3") {
            let bucket = Config::get_wal_s3_bucket();
            let base = Config::get_wal_s3_prefix();
            let prefix_url = format!("s3://{}/{}", bucket, base.trim_start_matches('/'));
            return Box::new(S3WalReader { prefix_url });
        }
        Box::new(DiskWalReader)
    }
}
