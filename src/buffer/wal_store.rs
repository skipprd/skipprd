use std::collections::HashMap;
use std::io;
use std::time::SystemTime;
use arrow::array::RecordBatch;
use url::Url;
use crate::helpers::configuration::Config;
use crate::buffer::segment_file::{PartitionKey, SegmentFile};
use crate::buffer::segment_object::SegmentObject;
use crate::helpers::offsets::OffsetKey;
use std::fs;
use std::path::PathBuf;
use arrow::ipc::reader::StreamReader;
use std::io::{Read, Seek};

/// Minimal WAL store interface (synchronous facade).
pub trait WalStore {
    /// Writes a snapshot and publishes a commit marker. Returns (total_bytes, total_rows, parts_count, sha256).
    fn write_snapshot_and_commit(
        &self,
        snapshot_id: &str,
        offsets: &HashMap<crate::helpers::offsets::OffsetKey, u64>,
        batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
        partitions_meta: &HashMap<PartitionKey, (u64 /*bytes*/, SystemTime /*updated*/ )>,
    ) -> io::Result<(u64, u64, u32, [u8;32])>;
}

pub struct S3WalStore {
    /// s3://bucket/prefix (without trailing slash)
    prefix_url: String,
}

impl S3WalStore {
    pub fn new(prefix_url: &str) -> Self {
        S3WalStore { prefix_url: prefix_url.to_string() }
    }

    // no extra helpers; streaming lives in SegmentObject
}

impl WalStore for S3WalStore {
    fn write_snapshot_and_commit(
        &self,
        snapshot_id: &str,
        offsets: &HashMap<crate::helpers::offsets::OffsetKey, u64>,
        batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
        partitions_meta: &HashMap<PartitionKey, (u64 /*bytes*/, SystemTime /*updated*/ )>,
    ) -> io::Result<(u64, u64, u32, [u8;32])> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("runtime: {}", e)))?;
        rt.block_on(async {
            let client = crate::helpers::s3::get_s3_client().await;
            SegmentObject::stream_snapshot_to_s3(&client, &self.prefix_url, snapshot_id, offsets, batches, partitions_meta).await
        })
    }
}

/// Disk-backed WAL store using SegmentFile + local commit.
pub struct DiskWalStore;

impl WalStore for DiskWalStore {
    fn write_snapshot_and_commit(
        &self,
        snapshot_id: &str,
        offsets: &HashMap<crate::helpers::offsets::OffsetKey, u64>,
        batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
        partitions_meta: &HashMap<PartitionKey, (u64 /*bytes*/, SystemTime /*updated*/ )>,
    ) -> io::Result<(u64, u64, u32, [u8;32])> {
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        let seg_file = SegmentFile::new(&seg_dir, snapshot_id)?;
        let (bytes, rows, parts, sha) = seg_file.write_snapshot(offsets, batches, partitions_meta)?;
        let header = SegmentFile::build_commit_header_bytes(parts, bytes, &sha);
        let commit_path = seg_file.path.with_extension("seg.commit");
        fs::write(&commit_path, &header)?;
        Ok((bytes, rows, parts, sha))
    }
}

pub struct WalStoreFactory;

impl WalStoreFactory {
    fn resolve_wal_prefix_for_namespace(ns: &str) -> Option<String> {
        // If already in a Tokio runtime, avoid blocking; use async variant or fall back
        if tokio::runtime::Handle::try_current().is_ok() {
            return None;
        }
        // No runtime: perform a small async read synchronously
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(_) => return None,
        };
        rt.block_on(async {
            if let Some(man) = Config::read_manifest(ns).await {
                if let Some(v) = man.get("wal_segments_prefix").and_then(|s| s.as_str()) {
                    if v.starts_with("s3://") { return Some(v.to_string()); }
                }
                if let Some(tables) = man.get("tables").and_then(|t| t.as_object()) {
                    if let Some(nsobj) = tables.get(ns).and_then(|v| v.as_object()) {
                        if let Some(pfx) = nsobj.get("wal_segments_prefix").and_then(|s| s.as_str()) {
                            if pfx.starts_with("s3://") { return Some(pfx.to_string()); }
                        }
                    }
                }
            }
            None
        })
    }

    pub fn for_batches(batches: &HashMap<PartitionKey, Vec<RecordBatch>>) -> Box<dyn WalStore + Send + Sync> {
        let storage = Config::getenv("WAL_STORAGE", "local");
        if storage.eq_ignore_ascii_case("s3") {
            if let Some(((ns, _p, _t, _s), _)) = batches.iter().next() {
                if let Some(pfx) = Self::resolve_wal_prefix_for_namespace(ns) {
                    return Box::new(S3WalStore::new(&pfx));
                }
            }
        }
        Box::new(DiskWalStore)
    }
}

/// Read-side abstraction for WAL
pub trait WalReader {
    fn load_committed_batches(&self, pipeline: &str, limit_files: usize) -> io::Result<Vec<RecordBatch>>;
}

pub struct DiskWalReader;

impl WalReader for DiskWalReader {
    fn load_committed_batches(&self, pipeline: &str, limit_files: usize) -> io::Result<Vec<RecordBatch>> {
        let mut out: Vec<RecordBatch> = Vec::new();
        let seg_dir = PathBuf::from(format!("{}/segment_buffer/segs", Config::get_data_dir()));
        if !seg_dir.exists() { return Ok(out); }
        let mut used = 0usize;
        for entry in fs::read_dir(&seg_dir)? {
            if used >= limit_files { break; }
            let entry = match entry { Ok(e) => e, Err(_) => continue };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
            let commit = path.with_extension("seg.commit");
            if !commit.exists() { continue; }
            let seg = SegmentFile { path: path.clone() };
            let meta = match seg.read_metadata() { Ok(m) => m, Err(_) => continue };
            for idx in meta.index.iter() {
                if idx.key.0 != pipeline { continue; }
                let mut file = match fs::OpenOptions::new().read(true).open(&path) { Ok(f) => f, Err(_) => continue };
                if file.seek(std::io::SeekFrom::Start(idx.start)).is_err() { continue; }
                let mut reader = std::io::BufReader::new(file);
                let mut take = reader.take(idx.len);
                if let Ok(sr) = StreamReader::try_new(&mut take, None) {
                    for it in sr {
                        if let Ok(b) = it { out.push(b); }
                    }
                }
                used += 1;
                if used >= limit_files { break; }
            }
        }
        Ok(out)
    }
}

pub struct S3WalReader {
    pub prefix_url: String,
}

impl WalReader for S3WalReader {
    fn load_committed_batches(&self, pipeline: &str, limit_files: usize) -> io::Result<Vec<RecordBatch>> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("runtime: {}", e)))?;
        rt.block_on(async {
            let mut out: Vec<RecordBatch> = Vec::new();
            let u = match Url::parse(&self.prefix_url) { Ok(u) => u, Err(_) => return Ok(out) };
            if u.scheme() != "s3" { return Ok(out); }
            let bucket = match u.host_str() { Some(b) => b.to_string(), None => return Ok(out) };
            let base = u.path().trim_start_matches('/').trim_end_matches('/').to_string();
            let client = crate::helpers::s3::get_s3_client().await;
            // list objects
            let mut token: Option<String> = None;
            let mut all: Vec<String> = Vec::new();
            loop {
                let mut req = client.list_objects_v2().bucket(&bucket).prefix(&base);
                if let Some(t) = &token { req = req.continuation_token(t); }
                match req.send().await {
                    Ok(resp) => {
                        if let Some(contents) = resp.contents {
                            for obj in contents {
                                if let Some(k) = obj.key() { all.push(k.to_string()); }
                            }
                        }
                        if resp.is_truncated.unwrap_or(false) { token = resp.next_continuation_token; } else { break; }
                    }
                    Err(_) => break,
                }
                if all.len() > 10_000 { break; }
            }
            use std::collections::HashSet;
            let set: HashSet<String> = all.iter().cloned().collect();
            let mut used = 0usize;
            for k in all.into_iter().filter(|k| k.ends_with(".seg")) {
                if used >= limit_files { break; }
                let commit = format!("{}.commit", k);
                if !set.contains(&commit) { continue; }
                match client.get_object().bucket(&bucket).key(&k).send().await {
                    Ok(resp) => {
                        match resp.body.collect().await {
                            Ok(agg) => {
                                let bytes = agg.into_bytes().to_vec();
                                if let Ok(meta) = SegmentFile::read_metadata_from_bytes(&bytes) {
                                    for idx in meta.index.iter() {
                                        if idx.key.0 != pipeline { continue; }
                                        let start = idx.start as usize;
                                        let end = start.saturating_add(idx.len as usize);
                                        if end > bytes.len() { continue; }
                                        let slice = &bytes[start..end];
                                        let mut cursor = std::io::Cursor::new(slice);
                                        if let Ok(sr) = StreamReader::try_new(&mut cursor, None) {
                                            for it in sr {
                                                if let Ok(b) = it { out.push(b); }
                                            }
                                        }
                                    }
                                    used += 1;
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    Err(_) => {}
                }
            }
            Ok(out)
        })
    }
}

pub struct WalReaderFactory;

impl WalReaderFactory {
    pub fn for_pipeline(pipeline: &str) -> Box<dyn WalReader + Send + Sync> {
        if let Some(pfx) = WalStoreFactory::resolve_wal_prefix_for_namespace(pipeline) {
            return Box::new(S3WalReader { prefix_url: pfx });
        }
        Box::new(DiskWalReader)
    }

    pub async fn for_pipeline_async(pipeline: &str) -> Box<dyn WalReader + Send + Sync> {
        // Manifest-based resolution asynchronously (faithful to current implementation)
        if let Some(man) = Config::read_manifest(pipeline).await {
            if let Some(v) = man.get("wal_segments_prefix").and_then(|s| s.as_str()) {
                if v.starts_with("s3://") { return Box::new(S3WalReader { prefix_url: v.to_string() }); }
            }
            if let Some(tables) = man.get("tables").and_then(|t| t.as_object()) {
                if let Some(nsobj) = tables.get(pipeline).and_then(|v| v.as_object()) {
                    if let Some(pfx) = nsobj.get("wal_segments_prefix").and_then(|s| s.as_str()) {
                        if pfx.starts_with("s3://") { return Box::new(S3WalReader { prefix_url: pfx.to_string() }); }
                    }
                }
            }
        }
        Box::new(DiskWalReader)
    }
}


