use crate::buffer::direct_io::DirectIoFile;
use crate::helpers::offsets::OffsetKey;
use crate::metrics::counters as metrics_counters;
use crate::plugins::cdc::{WalPartKind, WalPartMeta};
use arrow::array::RecordBatch;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use bincode;
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::fs::File;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use std::{fs, io};

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct PartitionKey {
    pub sink_ref: String,
    pub namespace: String,
    pub partition: String,
    pub time: Option<i64>,
    pub schema_fingerprint: String,
}

const MAGIC: &[u8; 4] = b"SEGF";
const PART: &[u8; 4] = b"PART";
const FOOT: &[u8; 4] = b"FOOT";
const COMMIT_MAGIC: &[u8; 4] = b"SEGC";
/// Segment format version. Includes an optional `part_meta_blob` per PART
/// for CDC row-aligned metadata (mutation kind, event_id, order_token).
const VERSION: u32 = 3;
const COMMIT_HEADER_VERSION: u32 = 1;
const FOOTER_LEN: usize = 4 + 4 + 32;
const COMMIT_HEADER_LEN: usize = 60;

/// Compact, in-memory-only summary of a WAL PART metadata sidecar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentPartMetaSummary {
    Append,
    Cdc {
        row_count: u64,
        canonical_hash: [u8; 32],
    },
}

impl SegmentPartMetaSummary {
    pub fn is_cdc(&self) -> bool {
        matches!(self, Self::Cdc { .. })
    }

    pub fn row_count(&self) -> Option<u64> {
        match self {
            Self::Append => None,
            Self::Cdc { row_count, .. } => Some(*row_count),
        }
    }

    pub fn canonical_hash(&self) -> Option<[u8; 32]> {
        match self {
            Self::Append => None,
            Self::Cdc { canonical_hash, .. } => Some(*canonical_hash),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SegmentPartitionIndexEntry {
    pub key: PartitionKey,
    pub bytes: u64,
    pub updated_at_secs: u64,
    /// Stable position of this PART in the segment byte stream.
    pub slice_ordinal: u32,
    /// Direct byte range of the serialized `WalPartMeta` sidecar.
    pub part_meta_start: u64,
    pub part_meta_len: u64,
    pub part_meta_summary: SegmentPartMetaSummary,
    pub start: u64,
    pub len: u64,
}

#[derive(Clone, Debug)]
pub struct SegmentFileMetadata {
    pub created_at_secs: u64,
    pub total_bytes: u64,
    pub num_partitions: u32,
    pub offsets: std::collections::HashMap<OffsetKey, u64>,
    pub index: Vec<SegmentPartitionIndexEntry>,
    /// SHA-256 of the body prefix (bytes before FOOT). Same digest SEGC carries.
    pub body_sha256: [u8; 32],
}

/// 60-byte `SEGC` commit marker. Presence owns the pair; the digest binds the body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegCommitHeader {
    pub version: u32,
    pub created_at_secs: u64,
    pub total_bytes: u64,
    pub parts_count: u32,
    pub sha256: [u8; 32],
}

/// Result of un-owning then dropping a local `.seg` / `.seg.commit` pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalPairReclaim {
    Removed,
    AlreadyUnowned,
}

pub struct SegmentFile {
    pub path: PathBuf,
}

#[cfg(test)]
static FULL_PART_META_SCANS: AtomicUsize = AtomicUsize::new(0);

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn parse_part_meta_blob(blob: &[u8]) -> io::Result<(WalPartMeta, SegmentPartMetaSummary)> {
    let meta = bincode::deserialize::<WalPartMeta>(blob)
        .map_err(|err| invalid_data(format!("invalid WAL PART metadata: {err}")))?;
    meta.validate()
        .map_err(|err| invalid_data(format!("invalid WAL PART metadata: {err}")))?;
    let summary = match meta.kind {
        WalPartKind::Append => SegmentPartMetaSummary::Append,
        WalPartKind::Cdc => {
            // Preserve the established hash: deserialize, validate, then hash the
            // canonical bincode representation rather than arbitrary input bytes.
            let canonical = bincode::serialize(&meta)
                .map_err(|err| invalid_data(format!("serialize WAL PART metadata: {err}")))?;
            let digest = Sha256::digest(canonical);
            let mut canonical_hash = [0u8; 32];
            canonical_hash.copy_from_slice(&digest);
            SegmentPartMetaSummary::Cdc {
                row_count: meta.row_count,
                canonical_hash,
            }
        }
    };
    Ok((meta, summary))
}

pub(crate) fn summarize_part_meta_blob(blob: &[u8]) -> io::Result<SegmentPartMetaSummary> {
    if blob.is_empty() {
        return Ok(SegmentPartMetaSummary::Append);
    }
    parse_part_meta_blob(blob).map(|(_, summary)| summary)
}

impl SegmentFile {
    /// Build the 60-byte commit header used to gate visibility of a segment file.
    /// Layout: MAGIC("SEGC"), VERSION(u32 LE), created_at(u64 LE), size(u64 LE), parts(u32 LE), sha256([u8;32])
    pub fn build_commit_header_bytes(
        parts_count: u32,
        total_bytes: u64,
        sha256: &[u8; 32],
    ) -> [u8; 60] {
        let mut buf: [u8; 60] = [0u8; 60];
        buf[0..4].copy_from_slice(COMMIT_MAGIC);
        // VERSION=1
        buf[4..8].copy_from_slice(&(COMMIT_HEADER_VERSION).to_le_bytes());
        // created_at
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        buf[8..16].copy_from_slice(&created_at.to_le_bytes());
        // size
        buf[16..24].copy_from_slice(&total_bytes.to_le_bytes());
        // parts
        buf[24..28].copy_from_slice(&parts_count.to_le_bytes());
        // sha256
        buf[28..60].copy_from_slice(sha256);
        buf
    }

    pub fn commit_path_for_seg(seg_path: &Path) -> PathBuf {
        seg_path.with_extension("seg.commit")
    }

    pub fn parse_commit_header(bytes: &[u8]) -> io::Result<SegCommitHeader> {
        if bytes.len() < COMMIT_HEADER_LEN {
            return Err(invalid_data("commit header truncated"));
        }
        if &bytes[0..4] != COMMIT_MAGIC {
            return Err(invalid_data("bad commit magic"));
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if version != COMMIT_HEADER_VERSION {
            return Err(invalid_data(format!("commit refused version={version}")));
        }
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&bytes[28..60]);
        Ok(SegCommitHeader {
            version,
            created_at_secs: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            total_bytes: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            parts_count: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            sha256,
        })
    }

    pub fn bind_commit(commit: &SegCommitHeader, meta: &SegmentFileMetadata) -> io::Result<()> {
        if commit.sha256 != meta.body_sha256 {
            return Err(invalid_data(
                "SEGC sha256 does not match FOOT; commit does not name this body",
            ));
        }
        if commit.parts_count != meta.num_partitions {
            return Err(invalid_data("SEGC parts_count does not match FOOT"));
        }
        if commit.total_bytes != meta.total_bytes {
            return Err(invalid_data("SEGC size does not match segment total_bytes"));
        }
        Ok(())
    }

    /// Re-hash the materialized body prefix and require it equals FOOT.
    /// Call when the full object is already in RAM (S3 recovery, LRU fetch).
    pub fn verify_body_checksum(bytes: &[u8]) -> io::Result<[u8; 32]> {
        if bytes.len() < FOOTER_LEN {
            return Err(invalid_data("segment too short for FOOT"));
        }
        let prefix_len = bytes.len() - FOOTER_LEN;
        let footer = &bytes[prefix_len..];
        if &footer[0..4] != FOOT {
            return Err(invalid_data("missing FOOT"));
        }
        let mut claimed = [0u8; 32];
        claimed.copy_from_slice(&footer[8..40]);
        let digest = Sha256::digest(&bytes[..prefix_len]);
        if digest.as_slice() != claimed {
            return Err(invalid_data("FOOT sha256 does not match body"));
        }
        Ok(claimed)
    }

    /// Admit an owned pair when the body is already materialized (S3).
    /// Parses SEGC, rehashes FOOT, parses metadata, binds the marker to the body.
    pub fn admit_owned_pair_bytes(
        body: &[u8],
        commit_bytes: &[u8],
    ) -> io::Result<SegmentFileMetadata> {
        let claimed = Self::verify_body_checksum(body)?;
        let commit = Self::parse_commit_header(commit_bytes)?;
        let meta = Self::read_metadata_from_bytes(body)?;
        if claimed != meta.body_sha256 {
            return Err(invalid_data("FOOT digest disagrees with metadata scan"));
        }
        Self::bind_commit(&commit, &meta)?;
        Ok(meta)
    }

    /// Admit an owned pair from disk. Binds SEGC to FOOT without a full-file rehash.
    pub fn admit_owned_pair_path(seg_path: &Path) -> io::Result<SegmentFileMetadata> {
        let commit_path = Self::commit_path_for_seg(seg_path);
        let mut buf = [0u8; COMMIT_HEADER_LEN];
        let mut commit_file = DirectIoFile::open(&commit_path)?;
        commit_file.read_exact(&mut buf)?;
        let commit = Self::parse_commit_header(&buf)?;
        let mut body = DirectIoFile::open(seg_path)?;
        let meta = Self::read_metadata_from_reader(&mut body)?;
        Self::bind_commit(&commit, &meta)?;
        Ok(meta)
    }

    /// Un-own then drop body. Body is deleted only after the commit marker is gone
    /// (NotFound on the marker means already un-owned). A leftover `.seg` after a
    /// crash is an orphan; recovery ignores it. Do not sweep orphan bodies while
    /// a writer may still be publishing the commit.
    pub fn reclaim_local_pair(seg_path: &Path) -> io::Result<WalPairReclaim> {
        let commit_path = Self::commit_path_for_seg(seg_path);
        match fs::remove_file(&commit_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        match fs::remove_file(seg_path) {
            Ok(()) => Ok(WalPairReclaim::Removed),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(WalPairReclaim::AlreadyUnowned),
            Err(err) => Err(err),
        }
    }

    /// Read segment metadata from an in-memory buffer.
    /// This mirrors `read_metadata`, but operates on bytes, allowing S3-backed reads.
    pub fn read_metadata_from_bytes(bytes: &[u8]) -> io::Result<SegmentFileMetadata> {
        use std::io::Cursor;
        Self::read_metadata_from_reader(&mut Cursor::new(bytes))
    }

    /// Read segment metadata from any reader that implements Read+Seek.
    /// Used by `read_metadata_from_bytes` and disk Direct I/O readers.
    /// S3 compaction does not stream ranges; it materializes the body and admits it.
    pub fn read_metadata_from_reader<R: Read + Seek>(
        reader: &mut R,
    ) -> io::Result<SegmentFileMetadata> {
        let file_len = reader.seek(io::SeekFrom::End(0))?;
        reader.seek(io::SeekFrom::Start(0))?;
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(invalid_data("Compactor: bad segment magic"));
        }
        let mut ver = [0u8; 4];
        reader.read_exact(&mut ver)?;
        let version = u32::from_le_bytes(ver);
        if version != VERSION {
            return Err(invalid_data(format!(
                "Compactor: read refused version={version}"
            )));
        }
        let mut created = [0u8; 8];
        reader.read_exact(&mut created)?;
        let created_at_secs = u64::from_le_bytes(created);
        let mut off_len_buf = [0u8; 8];
        reader.read_exact(&mut off_len_buf)?;
        let offsets_len = u64::from_le_bytes(off_len_buf);
        let offsets_len_usize = usize::try_from(offsets_len)
            .map_err(|_| invalid_data("WAL offsets blob is too large"))?;
        let mut offsets_blob = vec![0u8; offsets_len_usize];
        reader.read_exact(&mut offsets_blob)?;
        let offsets: std::collections::HashMap<OffsetKey, u64> =
            bincode::deserialize(&offsets_blob)
                .map_err(|err| invalid_data(format!("invalid WAL offsets metadata: {err}")))?;

        let mut index: Vec<SegmentPartitionIndexEntry> = Vec::new();
        let mut total_bytes: u64 = 0;
        loop {
            let mut tag = [0u8; 4];
            match reader.read_exact(&mut tag) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(invalid_data("WAL segment missing FOOT"));
                }
                Err(e) => return Err(e),
            }
            if &tag == FOOT {
                break;
            }
            if &tag != PART {
                return Err(invalid_data("WAL segment expected PART or FOOT"));
            }
            let mut key_len_buf = [0u8; 8];
            reader.read_exact(&mut key_len_buf)?;
            let key_len = u64::from_le_bytes(key_len_buf);
            let key_len_usize = usize::try_from(key_len)
                .map_err(|_| invalid_data("WAL partition key is too large"))?;
            let mut key_blob = vec![0u8; key_len_usize];
            reader.read_exact(&mut key_blob)?;
            let key: PartitionKey = bincode::deserialize(&key_blob)
                .map_err(|err| invalid_data(format!("invalid WAL partition key: {err}")))?;
            let mut bytes_buf = [0u8; 8];
            reader.read_exact(&mut bytes_buf)?;
            let part_bytes = u64::from_le_bytes(bytes_buf);
            let mut upd_buf = [0u8; 8];
            reader.read_exact(&mut upd_buf)?;
            let upd_secs = u64::from_le_bytes(upd_buf);

            let mut meta_len_buf = [0u8; 8];
            reader.read_exact(&mut meta_len_buf)?;
            let meta_len = u64::from_le_bytes(meta_len_buf);
            let part_meta_start = reader.stream_position()?;
            let part_meta_summary = if meta_len == 0 {
                SegmentPartMetaSummary::Append
            } else {
                let meta_len_usize = usize::try_from(meta_len)
                    .map_err(|_| invalid_data("WAL PART metadata is too large"))?;
                let mut meta_blob = vec![0u8; meta_len_usize];
                reader.read_exact(&mut meta_blob)?;
                summarize_part_meta_blob(&meta_blob)?
            };
            if meta_len > 0 {
                let expected_position = part_meta_start
                    .checked_add(meta_len)
                    .ok_or_else(|| invalid_data("WAL PART metadata range overflow"))?;
                if reader.stream_position()? != expected_position {
                    return Err(invalid_data("WAL PART metadata length mismatch"));
                }
            }

            let mut len_buf = [0u8; 8];
            reader.read_exact(&mut len_buf)?;
            let data_len = u64::from_le_bytes(len_buf);
            let start = reader.stream_position()?;
            if start.saturating_add(data_len) > file_len {
                return Err(invalid_data(format!(
                    "Compactor: partition length beyond segment end start={start} len={data_len} file_len={file_len}"
                )));
            }
            total_bytes = total_bytes.saturating_add(data_len);
            let slice_ordinal = u32::try_from(index.len())
                .map_err(|_| invalid_data("WAL segment contains too many partitions"))?;
            index.push(SegmentPartitionIndexEntry {
                key,
                bytes: part_bytes,
                updated_at_secs: upd_secs,
                slice_ordinal,
                part_meta_start,
                part_meta_len: meta_len,
                part_meta_summary,
                start,
                len: data_len,
            });
            reader.seek(io::SeekFrom::Current(data_len as i64))?;
        }

        if index.iter().any(|entry| entry.part_meta_len > 0) {
            metrics_counters::record_cdc_metadata_segment_scan(file_len);
        }

        let mut parts_buf = [0u8; 4];
        reader.read_exact(&mut parts_buf)?;
        let footer_parts = u32::from_le_bytes(parts_buf);
        let mut body_sha256 = [0u8; 32];
        reader.read_exact(&mut body_sha256)?;
        let pos = reader.stream_position()?;
        if pos != file_len {
            return Err(invalid_data("WAL segment has trailing bytes after FOOT"));
        }
        let num_partitions = u32::try_from(index.len())
            .map_err(|_| invalid_data("WAL segment contains too many partitions"))?;
        if footer_parts != num_partitions {
            return Err(invalid_data(format!(
                "FOOT parts_count {footer_parts} does not match index {num_partitions}"
            )));
        }

        Ok(SegmentFileMetadata {
            created_at_secs,
            total_bytes,
            num_partitions,
            offsets,
            index,
            body_sha256,
        })
    }

    pub fn new(dir: &Path, snapshot_id: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        // Record the final path; we write directly and publish via a sibling .seg.commit
        let p_final = dir.join(format!("{}.seg", snapshot_id));
        Ok(SegmentFile { path: p_final })
    }

    /// Layout per PART:
    ///   PART | key_len | key_blob | part_bytes | updated_secs
    ///   | part_meta_len | part_meta_blob | data_len | Arrow IPC
    ///
    /// `part_meta_blobs` maps each `PartitionKey` to its serialized
    /// `WalPartMeta`. Partitions not present in the map get a zero-length
    /// meta blob (append-mode semantics).
    pub fn write_snapshot(
        &self,
        offsets: &std::collections::HashMap<OffsetKey, u64>,
        batches: &std::collections::HashMap<PartitionKey, Vec<RecordBatch>>,
        partitions_meta: &std::collections::HashMap<
            PartitionKey,
            (u64 /*bytes*/, SystemTime /*updated*/),
        >,
        part_meta_blobs: &std::collections::HashMap<PartitionKey, Vec<u8>>,
    ) -> io::Result<(
        SegmentFileMetadata,
        u64,      /*rows*/
        [u8; 32], /*sha256*/
    )> {
        let mut file = Cursor::new(Vec::new());

        file.write_all(MAGIC)?;
        file.write_all(&VERSION.to_le_bytes())?;
        let created_at_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        file.write_all(&created_at_secs.to_le_bytes())?;

        let offsets_blob = bincode::serialize(offsets).unwrap();
        let offsets_len = offsets_blob.len() as u64;
        file.write_all(&offsets_len.to_le_bytes())?;
        file.write_all(&offsets_blob)?;

        let mut total_rows: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut parts_count: u32 = 0;
        let mut index: Vec<SegmentPartitionIndexEntry> = Vec::with_capacity(batches.len());

        for (key, rbatches) in batches.iter() {
            if rbatches.is_empty() {
                continue;
            }
            parts_count = parts_count.saturating_add(1);
            file.write_all(PART)?;
            let key_blob = bincode::serialize(key).unwrap();
            let key_len = key_blob.len() as u64;
            file.write_all(&key_len.to_le_bytes())?;
            file.write_all(&key_blob)?;
            let (p_bytes, p_updated) = partitions_meta
                .get(key)
                .cloned()
                .unwrap_or((0, SystemTime::now()));
            let updated_secs = p_updated
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            file.write_all(&p_bytes.to_le_bytes())?;
            file.write_all(&updated_secs.to_le_bytes())?;

            // Write and index the metadata sidecar without retaining row metadata
            // in the scheduling index.
            let meta_blob = part_meta_blobs
                .get(key)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let part_meta_summary = summarize_part_meta_blob(meta_blob)?;
            let meta_len = meta_blob.len() as u64;
            file.write_all(&meta_len.to_le_bytes())?;
            let part_meta_start = file.stream_position()?;
            if meta_len > 0 {
                file.write_all(meta_blob)?;
            }

            let data_len_pos = file.stream_position()?;
            file.write_all(&0u64.to_le_bytes())?;

            let start = file.stream_position()?;
            {
                let options = IpcWriteOptions::default();
                let mut writer = StreamWriter::try_new_with_options(
                    &mut file,
                    &rbatches[0].schema(),
                    options,
                )
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e)))?;
                for b in rbatches.iter() {
                    total_rows += b.num_rows() as u64;
                    writer.write(b).map_err(|e| {
                        io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e))
                    })?;
                }
                writer
                    .finish()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e)))?;
            }
            let end = file.stream_position()?;
            let len = end - start;
            total_bytes = total_bytes.saturating_add(len);

            file.seek(io::SeekFrom::Start(data_len_pos))?;
            file.write_all(&len.to_le_bytes())?;
            file.seek(io::SeekFrom::Start(end))?;

            index.push(SegmentPartitionIndexEntry {
                key: key.clone(),
                bytes: p_bytes,
                updated_at_secs: updated_secs,
                slice_ordinal: parts_count - 1,
                part_meta_start,
                part_meta_len: meta_len,
                part_meta_summary,
                start,
                len,
            });
        }

        let end_before_footer = file.stream_position()?;
        let digest = Sha256::digest(&file.get_ref()[..end_before_footer as usize]);
        let mut sha_bytes: [u8; 32] = [0u8; 32];
        sha_bytes.copy_from_slice(&digest);

        file.write_all(FOOT)?;
        file.write_all(&parts_count.to_le_bytes())?;
        file.write_all(&sha_bytes)?;
        let mut durable = DirectIoFile::create(&self.path)?;
        durable.write_all(file.get_ref())?;
        durable.sync_data()?;

        let meta = SegmentFileMetadata {
            created_at_secs,
            total_bytes,
            num_partitions: parts_count,
            offsets: offsets.clone(),
            index,
            body_sha256: sha_bytes,
        };
        Ok((meta, total_rows, sha_bytes))
    }

    /// Read one selected PART metadata sidecar directly from its indexed range.
    pub fn read_part_meta_from_reader<R: Read + Seek>(
        reader: &mut R,
        index: &SegmentPartitionIndexEntry,
    ) -> io::Result<Option<WalPartMeta>> {
        if index.part_meta_len == 0 {
            if index.part_meta_summary != SegmentPartMetaSummary::Append {
                return Err(invalid_data(
                    "CDC WAL PART metadata summary has an empty sidecar",
                ));
            }
            return Ok(None);
        }

        let meta_len = usize::try_from(index.part_meta_len)
            .map_err(|_| invalid_data("WAL PART metadata is too large"))?;
        reader.seek(io::SeekFrom::Start(index.part_meta_start))?;
        let mut blob = vec![0u8; meta_len];
        reader.read_exact(&mut blob)?;
        let (meta, summary) = parse_part_meta_blob(&blob)?;
        if summary != index.part_meta_summary {
            return Err(invalid_data(format!(
                "WAL PART metadata summary mismatch at slice ordinal {}",
                index.slice_ordinal
            )));
        }
        Ok(Some(meta))
    }

    pub fn read_part_meta_from_bytes(
        bytes: &[u8],
        index: &SegmentPartitionIndexEntry,
    ) -> io::Result<Option<WalPartMeta>> {
        let end = index
            .part_meta_start
            .checked_add(index.part_meta_len)
            .ok_or_else(|| invalid_data("WAL PART metadata range overflow"))?;
        if end > bytes.len() as u64 {
            return Err(invalid_data(format!(
                "WAL PART metadata range beyond segment end start={} len={} file_len={}",
                index.part_meta_start,
                index.part_meta_len,
                bytes.len()
            )));
        }
        Self::read_part_meta_from_reader(&mut io::Cursor::new(bytes), index)
    }

    /// Read every per-partition CDC metadata blob from a segment.
    ///
    /// Scheduling must use `SegmentPartitionIndexEntry::part_meta_summary`;
    /// this compatibility helper intentionally performs a full-segment scan.
    pub fn read_part_meta_blobs_from_reader<R: Read + Seek>(
        reader: &mut R,
    ) -> io::Result<std::collections::HashMap<PartitionKey, Vec<u8>>> {
        #[cfg(test)]
        FULL_PART_META_SCANS.fetch_add(1, Ordering::Relaxed);

        let mut result: std::collections::HashMap<PartitionKey, Vec<u8>> =
            std::collections::HashMap::new();

        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Ok(result);
        }
        let mut ver = [0u8; 4];
        reader.read_exact(&mut ver)?;
        let version = u32::from_le_bytes(ver);
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("read_part_meta_blobs: refused version={}", version),
            ));
        }

        // Skip created_at + offsets blob
        reader.seek(io::SeekFrom::Current(8))?;
        let mut off_len_buf = [0u8; 8];
        reader.read_exact(&mut off_len_buf)?;
        let offsets_len = u64::from_le_bytes(off_len_buf);
        reader.seek(io::SeekFrom::Current(offsets_len as i64))?;

        loop {
            let mut tag = [0u8; 4];
            match reader.read_exact(&mut tag) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            if &tag != PART {
                break;
            }
            let mut key_len_buf = [0u8; 8];
            reader.read_exact(&mut key_len_buf)?;
            let key_len = u64::from_le_bytes(key_len_buf);
            let mut key_blob = vec![0u8; key_len as usize];
            reader.read_exact(&mut key_blob)?;
            let key: PartitionKey = bincode::deserialize(&key_blob).unwrap();

            // Skip part_bytes + updated_secs
            reader.seek(io::SeekFrom::Current(16))?;

            // Read part_meta_blob
            let mut meta_len_buf = [0u8; 8];
            reader.read_exact(&mut meta_len_buf)?;
            let meta_len = u64::from_le_bytes(meta_len_buf);
            if meta_len > 0 {
                let mut meta_blob = vec![0u8; meta_len as usize];
                reader.read_exact(&mut meta_blob)?;
                result.insert(key, meta_blob);
            }

            // Skip data_len + Arrow data
            let mut len_buf = [0u8; 8];
            reader.read_exact(&mut len_buf)?;
            let data_len = u64::from_le_bytes(len_buf);
            reader.seek(io::SeekFrom::Current(data_len as i64))?;
        }

        Ok(result)
    }

    #[cfg(test)]
    pub(crate) fn reset_full_part_meta_scan_count() {
        FULL_PART_META_SCANS.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn full_part_meta_scan_count() -> usize {
        FULL_PART_META_SCANS.load(Ordering::Relaxed)
    }

    pub fn read_metadata_durable(&self) -> io::Result<SegmentFileMetadata> {
        let mut file = DirectIoFile::open(&self.path)?;
        Self::read_metadata_from_reader(&mut file)
    }
}

#[cfg(test)]
mod tests_wal_writer {
    use super::*;
    use arrow::array::Int32Array;
    use arrow::record_batch::RecordBatch;
    use arrow_schema::{DataType, Field, Schema};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::SystemTime;

    fn temp_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("skippr_test_{}", rand::random::<u64>()));
        let _ = fs::create_dir_all(&base);
        base
    }

    fn make_batch() -> RecordBatch {
        let schema = Schema::new(vec![Field::new("v", DataType::Int32, false)]);
        let arr = Int32Array::from(vec![1, 2, 3]);
        RecordBatch::try_new(Arc::new(schema), vec![Arc::new(arr)]).unwrap()
    }

    #[test]
    fn test_write_snapshot_footer_and_hash() {
        let dir = temp_dir();
        let seg = SegmentFile::new(&dir, "t1").unwrap();
        let mut batches: HashMap<PartitionKey, Vec<RecordBatch>> = HashMap::new();
        let key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "ns".to_string(),
            partition: "".to_string(),
            time: Some(0),
            schema_fingerprint: "schema".to_string(),
        };
        batches.insert(key.clone(), vec![make_batch()]);
        let mut parts_meta: HashMap<PartitionKey, (u64, SystemTime)> = HashMap::new();
        parts_meta.insert(key.clone(), (0, SystemTime::now()));
        let offsets: HashMap<crate::helpers::offsets::OffsetKey, u64> = HashMap::new();

        let empty_blobs: HashMap<PartitionKey, Vec<u8>> = HashMap::new();
        let (meta, _rows, sha) = seg
            .write_snapshot(&offsets, &batches, &parts_meta, &empty_blobs)
            .unwrap();
        let parts_count = meta.num_partitions;
        assert_eq!(parts_count, 1);
        let append_index = &meta.index[0];
        assert_eq!(append_index.slice_ordinal, 0);
        assert_eq!(append_index.part_meta_len, 0);
        assert_eq!(
            append_index.part_meta_summary,
            SegmentPartMetaSummary::Append
        );
        let mut append_file = File::open(&seg.path).unwrap();
        assert!(
            SegmentFile::read_part_meta_from_reader(&mut append_file, append_index)
                .unwrap()
                .is_none()
        );

        // Read footer and verify
        let mut f = File::open(&seg.path).unwrap();
        let file_len = f.metadata().unwrap().len();
        let footer_len = 4 + 4 + 32; // FOOT + parts(u32) + sha256
        assert!(file_len > footer_len);
        f.seek(io::SeekFrom::Start(file_len - footer_len)).unwrap();
        let mut tag = [0u8; 4];
        f.read_exact(&mut tag).unwrap();
        assert_eq!(&tag, FOOT);
        let mut pc = [0u8; 4];
        f.read_exact(&mut pc).unwrap();
        let got_parts = u32::from_le_bytes(pc);
        assert_eq!(got_parts, parts_count);
        let mut got_sha = [0u8; 32];
        f.read_exact(&mut got_sha).unwrap();
        assert_eq!(&got_sha, &sha);

        // Recompute sha over content up to footer
        let mut f2 = File::open(&seg.path).unwrap();
        let mut hasher = Sha256::new();
        let mut remaining = (file_len - footer_len) as i64;
        let mut buf = vec![0u8; 1 << 16];
        while remaining > 0 {
            let to_read = std::cmp::min(remaining as usize, buf.len());
            let n = f2.read(&mut buf[..to_read]).unwrap();
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            remaining -= n as i64;
        }
        let digest = hasher.finalize();
        assert_eq!(&digest[..], &sha);
    }

    #[test]
    fn test_write_snapshot_with_meta_blob() {
        use crate::plugins::cdc::{MutationKind, WalPartKind, WalPartMeta, WalRowMeta};

        let dir = temp_dir();
        let seg = SegmentFile::new(&dir, "t_cdc").unwrap();
        let key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "ns".to_string(),
            partition: "".to_string(),
            time: Some(0),
            schema_fingerprint: "schema".to_string(),
        };
        let batch = make_batch();
        let mut batches: HashMap<PartitionKey, Vec<RecordBatch>> = HashMap::new();
        batches.insert(key.clone(), vec![batch]);
        let mut parts_meta: HashMap<PartitionKey, (u64, SystemTime)> = HashMap::new();
        parts_meta.insert(key.clone(), (0, SystemTime::now()));
        let offsets: HashMap<crate::helpers::offsets::OffsetKey, u64> = HashMap::new();

        let wal_part_meta = WalPartMeta {
            kind: WalPartKind::Cdc,
            row_count: 3,
            rows: vec![
                WalRowMeta {
                    mutation: MutationKind::Insert,
                    event_id: vec![1],
                    order_token: vec![0, 0, 0, 1],
                },
                WalRowMeta {
                    mutation: MutationKind::Update,
                    event_id: vec![2],
                    order_token: vec![0, 0, 0, 2],
                },
                WalRowMeta {
                    mutation: MutationKind::Delete,
                    event_id: vec![3],
                    order_token: vec![0, 0, 0, 3],
                },
            ],
        };
        let meta_blob = bincode::serialize(&wal_part_meta).unwrap();
        let mut part_meta_blobs: HashMap<PartitionKey, Vec<u8>> = HashMap::new();
        part_meta_blobs.insert(key.clone(), meta_blob.clone());

        let (meta, _rows, _sha) = seg
            .write_snapshot(&offsets, &batches, &parts_meta, &part_meta_blobs)
            .unwrap();
        assert_eq!(meta.num_partitions, 1);
        let index = &meta.index[0];
        assert_eq!(index.slice_ordinal, 0);
        assert_eq!(index.part_meta_len, meta_blob.len() as u64);
        let expected_hash = {
            let digest = Sha256::digest(&meta_blob);
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&digest);
            hash
        };
        assert_eq!(
            index.part_meta_summary,
            SegmentPartMetaSummary::Cdc {
                row_count: 3,
                canonical_hash: expected_hash,
            }
        );

        let read_meta = seg.read_metadata_durable().unwrap();
        assert_eq!(read_meta.num_partitions, 1);
        assert_eq!(read_meta.index.len(), 1);
        assert_eq!(read_meta.index[0].part_meta_start, index.part_meta_start);
        assert_eq!(
            read_meta.index[0].part_meta_summary,
            index.part_meta_summary
        );

        let mut direct_file = File::open(&seg.path).unwrap();
        let direct = SegmentFile::read_part_meta_from_reader(&mut direct_file, &read_meta.index[0])
            .unwrap()
            .unwrap();
        assert_eq!(direct.kind, WalPartKind::Cdc);
        assert_eq!(direct.row_count, 3);
        assert_eq!(direct.rows[0].event_id, vec![1]);
        assert_eq!(direct.rows[2].event_id, vec![3]);

        let mut f = File::open(&seg.path).unwrap();
        let blobs = SegmentFile::read_part_meta_blobs_from_reader(&mut f).unwrap();
        assert_eq!(blobs.len(), 1);
        let got_blob = blobs.get(&key).unwrap();
        let decoded: WalPartMeta = bincode::deserialize(got_blob).unwrap();
        assert_eq!(decoded.kind, WalPartKind::Cdc);
        assert_eq!(decoded.row_count, 3);
        assert_eq!(decoded.rows[0].mutation, MutationKind::Insert);
        assert_eq!(decoded.rows[2].mutation, MutationKind::Delete);
    }

    #[test]
    fn test_from_bytes_reader() {
        use crate::plugins::cdc::{MutationKind, WalPartKind, WalPartMeta, WalRowMeta};

        let dir = temp_dir();
        let seg = SegmentFile::new(&dir, "t_bytes").unwrap();
        let key = PartitionKey {
            sink_ref: "out".to_string(),
            namespace: "tbl".to_string(),
            partition: "".to_string(),
            time: None,
            schema_fingerprint: "".to_string(),
        };
        let batch = make_batch();
        let mut batches: HashMap<PartitionKey, Vec<RecordBatch>> = HashMap::new();
        batches.insert(key.clone(), vec![batch]);
        let mut parts_meta: HashMap<PartitionKey, (u64, SystemTime)> = HashMap::new();
        parts_meta.insert(key.clone(), (100, SystemTime::now()));
        let offsets: HashMap<crate::helpers::offsets::OffsetKey, u64> = HashMap::new();

        let wal_meta = WalPartMeta {
            kind: WalPartKind::Cdc,
            row_count: 3,
            rows: (0..3)
                .map(|i| WalRowMeta {
                    mutation: MutationKind::Snapshot,
                    event_id: vec![i],
                    order_token: vec![0, 0, 0, i],
                })
                .collect(),
        };
        let meta_blob = bincode::serialize(&wal_meta).unwrap();
        let mut blobs: HashMap<PartitionKey, Vec<u8>> = HashMap::new();
        blobs.insert(key.clone(), meta_blob);

        seg.write_snapshot(&offsets, &batches, &parts_meta, &blobs)
            .unwrap();

        let bytes = fs::read(&seg.path).unwrap();
        let meta = SegmentFile::read_metadata_from_bytes(&bytes).unwrap();
        assert_eq!(meta.num_partitions, 1);
        assert_eq!(meta.index[0].key, key);
    }

    #[test]
    fn metadata_read_without_foot_is_refused() {
        let dir = temp_dir();
        let seg = SegmentFile::new(&dir, "no_foot").unwrap();
        let mut batches: HashMap<PartitionKey, Vec<RecordBatch>> = HashMap::new();
        let key = PartitionKey {
            sink_ref: "out".into(),
            namespace: "ns".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        batches.insert(key.clone(), vec![make_batch()]);
        let mut parts_meta = HashMap::new();
        parts_meta.insert(key, (0, SystemTime::now()));
        let offsets = HashMap::new();
        let empty = HashMap::new();
        seg.write_snapshot(&offsets, &batches, &parts_meta, &empty)
            .unwrap();
        let mut bytes = fs::read(&seg.path).unwrap();
        bytes.truncate(bytes.len() - FOOTER_LEN);
        let err = SegmentFile::read_metadata_from_bytes(&bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("FOOT"));
    }

    #[test]
    fn admit_owned_pair_binds_segc_to_foot() {
        let dir = temp_dir();
        let seg = SegmentFile::new(&dir, "bind").unwrap();
        let mut batches: HashMap<PartitionKey, Vec<RecordBatch>> = HashMap::new();
        let key = PartitionKey {
            sink_ref: "out".into(),
            namespace: "ns".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        batches.insert(key.clone(), vec![make_batch()]);
        let mut parts_meta = HashMap::new();
        parts_meta.insert(key, (0, SystemTime::now()));
        let offsets = HashMap::new();
        let empty = HashMap::new();
        let (meta, _rows, sha) = seg
            .write_snapshot(&offsets, &batches, &parts_meta, &empty)
            .unwrap();
        let commit =
            SegmentFile::build_commit_header_bytes(meta.num_partitions, meta.total_bytes, &sha);
        fs::write(SegmentFile::commit_path_for_seg(&seg.path), commit).unwrap();
        let admitted = SegmentFile::admit_owned_pair_path(&seg.path).unwrap();
        assert_eq!(admitted.body_sha256, sha);
        let body = fs::read(&seg.path).unwrap();
        let from_bytes = SegmentFile::admit_owned_pair_bytes(&body, &commit).unwrap();
        assert_eq!(from_bytes.body_sha256, sha);
    }

    #[test]
    fn admit_owned_pair_refuses_mismatched_segc_digest() {
        let dir = temp_dir();
        let seg = SegmentFile::new(&dir, "mismatch").unwrap();
        let mut batches: HashMap<PartitionKey, Vec<RecordBatch>> = HashMap::new();
        let key = PartitionKey {
            sink_ref: "out".into(),
            namespace: "ns".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        batches.insert(key.clone(), vec![make_batch()]);
        let mut parts_meta = HashMap::new();
        parts_meta.insert(key, (0, SystemTime::now()));
        let offsets = HashMap::new();
        let empty = HashMap::new();
        let (meta, _rows, _sha) = seg
            .write_snapshot(&offsets, &batches, &parts_meta, &empty)
            .unwrap();
        let wrong = [0u8; 32];
        let commit =
            SegmentFile::build_commit_header_bytes(meta.num_partitions, meta.total_bytes, &wrong);
        let body = fs::read(&seg.path).unwrap();
        let err = SegmentFile::admit_owned_pair_bytes(&body, &commit).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("SEGC"));
    }

    #[test]
    fn verify_body_checksum_refuses_flipped_payload() {
        let dir = temp_dir();
        let seg = SegmentFile::new(&dir, "flip").unwrap();
        let mut batches: HashMap<PartitionKey, Vec<RecordBatch>> = HashMap::new();
        let key = PartitionKey {
            sink_ref: "out".into(),
            namespace: "ns".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        batches.insert(key.clone(), vec![make_batch()]);
        let mut parts_meta = HashMap::new();
        parts_meta.insert(key, (0, SystemTime::now()));
        let offsets = HashMap::new();
        let empty = HashMap::new();
        seg.write_snapshot(&offsets, &batches, &parts_meta, &empty)
            .unwrap();
        let mut body = fs::read(&seg.path).unwrap();
        let flip_at = body.len() - FOOTER_LEN - 1;
        body[flip_at] ^= 0xff;
        let err = SegmentFile::verify_body_checksum(&body).unwrap_err();
        assert!(err.to_string().contains("FOOT sha256"));
    }

    #[test]
    fn reclaim_local_pair_unowns_before_dropping_body() {
        let dir = temp_dir();
        let seg_path = dir.join("gate.seg");
        let commit_path = SegmentFile::commit_path_for_seg(&seg_path);
        fs::create_dir(&seg_path).unwrap();
        fs::write(&commit_path, b"SEGC").unwrap();
        let err = SegmentFile::reclaim_local_pair(&seg_path).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::NotFound);
        assert!(!commit_path.exists(), "commit must be un-owned first");
        assert!(seg_path.exists(), "body must not be deleted if drop fails");
    }

    #[test]
    fn reclaim_local_pair_retries_body_after_commit_already_gone() {
        let dir = temp_dir();
        let seg_path = dir.join("orphan-body.seg");
        fs::write(&seg_path, b"body").unwrap();
        let result = SegmentFile::reclaim_local_pair(&seg_path).unwrap();
        assert_eq!(result, WalPairReclaim::Removed);
        assert!(!seg_path.exists());
        assert!(!SegmentFile::commit_path_for_seg(&seg_path).exists());
    }

    #[test]
    fn reclaim_local_pair_deletes_commit_then_body() {
        let dir = temp_dir();
        let seg_path = dir.join("ok.seg");
        let commit_path = SegmentFile::commit_path_for_seg(&seg_path);
        fs::write(&seg_path, b"body").unwrap();
        fs::write(&commit_path, b"commit").unwrap();
        let result = SegmentFile::reclaim_local_pair(&seg_path).unwrap();
        assert_eq!(result, WalPairReclaim::Removed);
        assert!(!seg_path.exists());
        assert!(!commit_path.exists());
    }
}
