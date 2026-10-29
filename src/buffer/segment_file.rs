use crate::buffer::direct_io::DirectIoFile;
use crate::helpers::offsets::OffsetKey;
use crate::metrics::counters as metrics_counters;
use crate::plugins::cdc::{WalPartKind, WalPartMeta};
use crate::serdes::copy::CopyWalRecord;
use arrow::array::RecordBatch;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use bincode;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
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

/// Arrow IPC PART (SEGF v3 payload and v4 codec 0).
pub const PART_CODEC_ARROW: u8 = 0;
/// Compressed bincode of `CopyWalRecord` frames (v4 codec 1).
pub const PART_CODEC_COPY: u8 = 1;
const SEGF_VERSION_V3: u32 = 3;

/// One partition's WAL bytes. Copy and Arrow never share a map entry
/// (`schema_fingerprint` for copy is always `"copy"`).
#[derive(Clone, Debug)]
pub enum WalPayload {
    Arrow(Vec<RecordBatch>),
    Raw(Vec<CopyWalRecord>),
}

impl WalPayload {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Arrow(batches) => batches.is_empty(),
            Self::Raw(records) => records.is_empty(),
        }
    }

    pub fn estimated_bytes(&self) -> u64 {
        match self {
            Self::Arrow(batches) => batches
                .iter()
                .map(|batch| batch.get_array_memory_size() as u64)
                .sum(),
            Self::Raw(records) => records.iter().map(CopyWalRecord::estimated_wal_bytes).sum(),
        }
    }

    pub fn row_count(&self) -> u64 {
        match self {
            Self::Arrow(batches) => batches.iter().map(|batch| batch.num_rows() as u64).sum(),
            Self::Raw(records) => records.len() as u64,
        }
    }

    pub fn codec(&self) -> u8 {
        match self {
            Self::Arrow(_) => PART_CODEC_ARROW,
            Self::Raw(_) => PART_CODEC_COPY,
        }
    }

    pub fn extend(&mut self, other: WalPayload) -> Result<(), String> {
        match (self, other) {
            (Self::Arrow(dst), Self::Arrow(src)) => {
                dst.extend(src);
                Ok(())
            }
            (Self::Raw(dst), Self::Raw(src)) => {
                dst.extend(src);
                Ok(())
            }
            _ => Err("WAL partition mixed Arrow and copy payloads".to_string()),
        }
    }
}

fn accepted_segf_version(version: u32) -> bool {
    version == SEGF_VERSION_V3 || version == VERSION
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
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
/// Segment format version. v4 writes a PART `codec` u8 before `data_len`.
/// v3 (Arrow IPC, no codec byte) remains readable.
const VERSION: u32 = 4;
const FOOTER_LEN: usize = 4 + 4 + 32;

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
    /// `PART_CODEC_ARROW` or `PART_CODEC_COPY`. v3 segments are Arrow.
    pub codec: u8,
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
    /// SHA-256 of the body prefix (bytes before FOOT). Catalog rows store this digest.
    pub body_sha256: [u8; 32],
}

/// Encoded SEGF body plus FOOT. Offsets live in the body; SEGC is a sibling object.
pub struct EncodedSnapshot {
    pub bytes: Vec<u8>,
    pub meta: SegmentFileMetadata,
    pub total_rows: u64,
    pub sha256: [u8; 32],
}

pub struct SegmentFile {
    pub path: PathBuf,
}

#[cfg(test)]
static FULL_PART_META_SCANS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    static INDEX_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn claimed_len_capped_by_remaining(
    claimed: u64,
    pos: u64,
    file_len: u64,
    what: &'static str,
) -> io::Result<usize> {
    let remaining = file_len.saturating_sub(pos);
    if claimed > remaining {
        return Err(invalid_data(format!(
            "{what} length {claimed} exceeds remaining {remaining}"
        )));
    }
    usize::try_from(claimed).map_err(|_| invalid_data(format!("{what} is too large")))
}

fn read_len_capped_by_remaining<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    what: &'static str,
) -> io::Result<usize> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    claimed_len_capped_by_remaining(
        u64::from_le_bytes(buf),
        reader.stream_position()?,
        file_len,
        what,
    )
}

fn read_blob_capped_by_remaining<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    what: &'static str,
) -> io::Result<Vec<u8>> {
    let len = read_len_capped_by_remaining(reader, file_len, what)?;
    let mut blob = vec![0u8; len];
    reader.read_exact(&mut blob)?;
    Ok(blob)
}

fn skip_claimed_len<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    what: &'static str,
) -> io::Result<()> {
    let len = read_len_capped_by_remaining(reader, file_len, what)? as u64;
    let end = reader
        .stream_position()?
        .checked_add(len)
        .ok_or_else(|| invalid_data(format!("{what} range overflow")))?;
    reader.seek(io::SeekFrom::Start(end))?;
    Ok(())
}

fn skip_payload<R: Seek>(reader: &mut R, data_len: u64, file_len: u64) -> io::Result<()> {
    let start = reader.stream_position()?;
    let end = start
        .checked_add(data_len)
        .ok_or_else(|| invalid_data("WAL PART payload range overflow"))?;
    if end > file_len {
        return Err(invalid_data(format!(
            "Compactor: partition length beyond segment end start={start} len={data_len} file_len={file_len}"
        )));
    }
    reader.seek(io::SeekFrom::Start(end))?;
    Ok(())
}

fn read_part_codec_and_len<R: Read>(reader: &mut R, version: u32) -> io::Result<(u8, u64)> {
    let codec = if version >= VERSION {
        let mut codec = [0u8; 1];
        reader.read_exact(&mut codec)?;
        codec[0]
    } else {
        PART_CODEC_ARROW
    };
    if codec != PART_CODEC_ARROW && codec != PART_CODEC_COPY {
        return Err(invalid_data(format!("unknown WAL PART codec {codec}")));
    }
    let mut len_buf = [0u8; 8];
    reader.read_exact(&mut len_buf)?;
    Ok((codec, u64::from_le_bytes(len_buf)))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CopyWalFrame {
    value: Vec<u8>,
    identity: Option<Vec<u8>>,
}

fn encode_copy_part(records: &[CopyWalRecord]) -> io::Result<Vec<u8>> {
    let frames: Vec<CopyWalFrame> = records
        .iter()
        .map(|record| {
            Ok(CopyWalFrame {
                value: serde_json::to_vec(&record.value)
                    .map_err(|err| invalid_data(format!("encode copy WAL value: {err}")))?,
                identity: record.identity.clone(),
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let raw = bincode::serialize(&frames)
        .map_err(|err| invalid_data(format!("encode copy WAL: {err}")))?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&raw)?;
    encoder.finish()
}

/// Decompress a copy PART payload into format-agnostic WAL frames.
pub fn decode_copy_part(bytes: &[u8]) -> io::Result<Vec<CopyWalRecord>> {
    let mut decoder = GzDecoder::new(bytes);
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw)?;
    let frames: Vec<CopyWalFrame> = bincode::deserialize(&raw)
        .map_err(|err| invalid_data(format!("decode copy WAL: {err}")))?;
    frames
        .into_iter()
        .map(|frame| {
            Ok(CopyWalRecord {
                value: serde_json::from_slice(&frame.value)
                    .map_err(|err| invalid_data(format!("decode copy WAL value: {err}")))?,
                identity: frame.identity,
            })
        })
        .collect()
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

    /// SEGF/PART headers. Seeks past payloads and never hashes the body:
    /// payload integrity is proven against the catalog sha on admit.
    pub fn read_index_path(seg_path: &Path) -> io::Result<SegmentFileMetadata> {
        #[cfg(test)]
        INDEX_READS.with(|reads| reads.set(reads.get() + 1));
        let mut reader = io::BufReader::new(fs::File::open(seg_path)?);
        Self::read_metadata_from_reader(&mut reader)
    }

    pub fn admit_payload_path(
        seg_path: &Path,
        expected_sha: &[u8; 32],
    ) -> io::Result<SegmentFileMetadata> {
        let mut body_file = DirectIoFile::open(seg_path)?;
        let mut body = Vec::new();
        body_file.read_to_end(&mut body)?;
        let claimed = Self::verify_body_checksum(&body)?;
        if &claimed != expected_sha {
            return Err(invalid_data("payload sha256 does not match catalog row"));
        }
        let meta = Self::read_metadata_from_bytes(&body)?;
        if claimed != meta.body_sha256 {
            return Err(invalid_data("FOOT digest disagrees with metadata scan"));
        }
        Ok(meta)
    }

    #[cfg(test)]
    pub(crate) fn index_read_count() -> usize {
        INDEX_READS.with(std::cell::Cell::get)
    }

    /// Read segment metadata from an in-memory buffer.
    /// This mirrors `read_metadata`, but operates on bytes, allowing S3-backed reads.
    pub fn read_metadata_from_bytes(bytes: &[u8]) -> io::Result<SegmentFileMetadata> {
        use std::io::Cursor;
        Self::read_metadata_from_reader(&mut Cursor::new(bytes))
    }

    /// Read segment metadata from any reader that implements Read+Seek.
    /// Used by `read_metadata_from_bytes`, and can also be used by external callers for S3 streaming.
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
        if !accepted_segf_version(version) {
            return Err(invalid_data(format!(
                "Compactor: read refused version={version}"
            )));
        }
        let mut created = [0u8; 8];
        reader.read_exact(&mut created)?;
        let created_at_secs = u64::from_le_bytes(created);
        let offsets_blob = read_blob_capped_by_remaining(reader, file_len, "WAL offsets blob")?;
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
            let key_blob = read_blob_capped_by_remaining(reader, file_len, "WAL partition key")?;
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
            let remaining = file_len.saturating_sub(part_meta_start);
            if meta_len > remaining {
                return Err(invalid_data(format!(
                    "WAL PART metadata length {meta_len} exceeds remaining {remaining}"
                )));
            }
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

            let (codec, data_len) = read_part_codec_and_len(reader, version)?;
            let start = reader.stream_position()?;
            skip_payload(reader, data_len, file_len)?;
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
                codec,
                start,
                len: data_len,
            });
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
        // Record the final payload path; catalog commit is the ownership record.
        let p_final = dir.join(format!("{}.seg", snapshot_id));
        Ok(SegmentFile { path: p_final })
    }

    /// Layout per PART (v4):
    ///   PART | key_len | key_blob | part_bytes | updated_secs
    ///   | part_meta_len | part_meta_blob | codec | data_len | payload
    ///
    /// v3 omits `codec` and the payload is Arrow IPC. `part_meta_blobs` maps
    /// each `PartitionKey` to serialized `WalPartMeta`.
    pub fn write_snapshot(
        &self,
        offsets: &std::collections::HashMap<OffsetKey, u64>,
        batches: &std::collections::HashMap<PartitionKey, WalPayload>,
        partitions_meta: &std::collections::HashMap<
            PartitionKey,
            (u64 /*bytes*/, SystemTime /*updated*/),
        >,
        part_meta_blobs: &std::collections::HashMap<PartitionKey, Vec<u8>>,
        created_at: SystemTime,
    ) -> io::Result<(
        SegmentFileMetadata,
        u64,      /*rows*/
        [u8; 32], /*sha256*/
    )> {
        let encoded = encode_snapshot(
            offsets,
            batches,
            partitions_meta,
            part_meta_blobs,
            created_at,
        )?;
        write_payload_atomically(&self.path, &encoded.bytes)?;
        Ok((encoded.meta, encoded.total_rows, encoded.sha256))
    }
}

/// Write `.seg` bytes via tmp + fsync + rename so a crash cannot leave a
/// truncated live payload.
pub fn write_payload_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("seg.tmp");
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    {
        let mut durable = DirectIoFile::create_new(&tmp)?;
        durable.write_all(bytes)?;
        durable.sync_data()?;
    }
    fs::rename(&tmp, path)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "segment path has no parent"))?;
    crate::helpers::fsync::fsync_dir(parent)
}

/// Encode a SEGF v4 body + FOOT once. Disk and S3 persist the same bytes.
pub fn encode_snapshot(
    offsets: &std::collections::HashMap<OffsetKey, u64>,
    batches: &std::collections::HashMap<PartitionKey, WalPayload>,
    partitions_meta: &std::collections::HashMap<
        PartitionKey,
        (u64 /*bytes*/, SystemTime /*updated*/),
    >,
    part_meta_blobs: &std::collections::HashMap<PartitionKey, Vec<u8>>,
    created_at: SystemTime,
) -> io::Result<EncodedSnapshot> {
    let mut file = Cursor::new(Vec::new());

    file.write_all(MAGIC)?;
    file.write_all(&VERSION.to_le_bytes())?;
    let created_at_secs = created_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    file.write_all(&created_at_secs.to_le_bytes())?;

    let offsets_blob =
        bincode::serialize(&offsets.iter().collect::<std::collections::BTreeMap<_, _>>())
            .map_err(|err| invalid_data(format!("encode WAL offsets: {err}")))?;
    let offsets_len = offsets_blob.len() as u64;
    file.write_all(&offsets_len.to_le_bytes())?;
    file.write_all(&offsets_blob)?;

    let mut total_rows: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut parts_count: u32 = 0;
    let mut index: Vec<SegmentPartitionIndexEntry> = Vec::with_capacity(batches.len());

    let mut parts: Vec<_> = batches.iter().collect();
    parts.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (key, payload) in parts {
        if payload.is_empty() {
            continue;
        }
        parts_count = parts_count.saturating_add(1);
        file.write_all(PART)?;
        let key_blob = bincode::serialize(key)
            .map_err(|err| invalid_data(format!("encode WAL key: {err}")))?;
        let key_len = key_blob.len() as u64;
        file.write_all(&key_len.to_le_bytes())?;
        file.write_all(&key_blob)?;
        let (p_bytes, p_updated) = partitions_meta.get(key).cloned().unwrap_or((0, created_at));
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

        let codec = payload.codec();
        file.write_all(&[codec])?;
        let data_len_pos = file.stream_position()?;
        file.write_all(&0u64.to_le_bytes())?;

        let start = file.stream_position()?;
        match payload {
            WalPayload::Arrow(rbatches) => {
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
            WalPayload::Raw(records) => {
                let encoded = encode_copy_part(records)?;
                total_rows = total_rows.saturating_add(records.len() as u64);
                file.write_all(&encoded)?;
            }
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
            codec,
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

    let meta = SegmentFileMetadata {
        created_at_secs,
        total_bytes,
        num_partitions: parts_count,
        offsets: offsets.clone(),
        index,
        body_sha256: sha_bytes,
    };
    Ok(EncodedSnapshot {
        bytes: file.into_inner(),
        meta,
        total_rows,
        sha256: sha_bytes,
    })
}

impl SegmentFile {
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

        let file_len = reader.seek(io::SeekFrom::End(0))?;
        reader.seek(io::SeekFrom::Start(0))?;
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Ok(result);
        }
        let mut ver = [0u8; 4];
        reader.read_exact(&mut ver)?;
        let version = u32::from_le_bytes(ver);
        if !accepted_segf_version(version) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("read_part_meta_blobs: refused version={}", version),
            ));
        }

        reader.seek(io::SeekFrom::Current(8))?;
        skip_claimed_len(reader, file_len, "WAL offsets blob")?;

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
            let key_blob = read_blob_capped_by_remaining(reader, file_len, "WAL partition key")?;
            let key: PartitionKey = bincode::deserialize(&key_blob).map_err(|err| {
                invalid_data(format!(
                    "invalid WAL partition key in part-meta scan: {err}"
                ))
            })?;

            reader.seek(io::SeekFrom::Current(16))?;

            let mut meta_len_buf = [0u8; 8];
            reader.read_exact(&mut meta_len_buf)?;
            let meta_len = u64::from_le_bytes(meta_len_buf);
            let remaining = file_len.saturating_sub(reader.stream_position()?);
            if meta_len > remaining {
                return Err(invalid_data(format!(
                    "WAL PART metadata length {meta_len} exceeds remaining {remaining}"
                )));
            }
            if meta_len > 0 {
                let meta_len_usize = usize::try_from(meta_len)
                    .map_err(|_| invalid_data("WAL PART metadata is too large"))?;
                let mut meta_blob = vec![0u8; meta_len_usize];
                reader.read_exact(&mut meta_blob)?;
                result.insert(key, meta_blob);
            }

            let (_codec, data_len) = read_part_codec_and_len(reader, version)?;
            skip_payload(reader, data_len, file_len)?;
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

    #[cfg(test)]
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
        let mut batches: HashMap<PartitionKey, WalPayload> = HashMap::new();
        let key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "ns".to_string(),
            partition: "".to_string(),
            time: Some(0),
            schema_fingerprint: "schema".to_string(),
        };
        batches.insert(key.clone(), WalPayload::Arrow(vec![make_batch()]));
        let mut parts_meta: HashMap<PartitionKey, (u64, SystemTime)> = HashMap::new();
        parts_meta.insert(key.clone(), (0, SystemTime::now()));
        let offsets: HashMap<crate::helpers::offsets::OffsetKey, u64> = HashMap::new();

        let empty_blobs: HashMap<PartitionKey, Vec<u8>> = HashMap::new();
        let (meta, _rows, sha) = seg
            .write_snapshot(
                &offsets,
                &batches,
                &parts_meta,
                &empty_blobs,
                SystemTime::UNIX_EPOCH,
            )
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
        let mut batches: HashMap<PartitionKey, WalPayload> = HashMap::new();
        batches.insert(key.clone(), WalPayload::Arrow(vec![batch]));
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
            .write_snapshot(
                &offsets,
                &batches,
                &parts_meta,
                &part_meta_blobs,
                SystemTime::UNIX_EPOCH,
            )
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
        let mut batches: HashMap<PartitionKey, WalPayload> = HashMap::new();
        batches.insert(key.clone(), WalPayload::Arrow(vec![batch]));
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

        seg.write_snapshot(
            &offsets,
            &batches,
            &parts_meta,
            &blobs,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();

        let bytes = fs::read(&seg.path).unwrap();
        let meta = SegmentFile::read_metadata_from_bytes(&bytes).unwrap();
        assert_eq!(meta.num_partitions, 1);
        assert_eq!(meta.index[0].key, key);
    }

    fn snapshot_one_part(name: &str) -> (PathBuf, SegmentFile, [u8; 32], SegmentFileMetadata) {
        let dir = temp_dir();
        let seg = SegmentFile::new(&dir, name).unwrap();
        let mut batches: HashMap<PartitionKey, WalPayload> = HashMap::new();
        let key = PartitionKey {
            sink_ref: "out".into(),
            namespace: "ns".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        batches.insert(key.clone(), WalPayload::Arrow(vec![make_batch()]));
        let mut parts_meta = HashMap::new();
        parts_meta.insert(key, (0, SystemTime::now()));
        let offsets = HashMap::new();
        let empty = HashMap::new();
        let (meta, _rows, sha) = seg
            .write_snapshot(
                &offsets,
                &batches,
                &parts_meta,
                &empty,
                SystemTime::UNIX_EPOCH,
            )
            .unwrap();
        (dir, seg, sha, meta)
    }

    #[test]
    fn metadata_read_without_foot_is_refused() {
        let (_dir, seg, _sha, _meta) = snapshot_one_part("no_foot");
        let mut bytes = fs::read(&seg.path).unwrap();
        bytes.truncate(bytes.len() - FOOTER_LEN);
        let err = SegmentFile::read_metadata_from_bytes(&bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("FOOT"));
    }

    #[test]
    fn admit_payload_path_binds_catalog_sha() {
        let (_dir, seg, sha, _meta) = snapshot_one_part("bind");
        let admitted = SegmentFile::admit_payload_path(&seg.path, &sha).unwrap();
        assert_eq!(admitted.body_sha256, sha);
    }

    #[test]
    fn admit_payload_path_refuses_mismatched_catalog_sha() {
        let (_dir, seg, _sha, _meta) = snapshot_one_part("mismatch");
        let err = SegmentFile::admit_payload_path(&seg.path, &[0u8; 32]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("catalog row"));
    }

    #[test]
    fn verify_body_checksum_refuses_flipped_payload() {
        let (_dir, seg, _sha, _meta) = snapshot_one_part("flip");
        let mut body = fs::read(&seg.path).unwrap();
        let flip_at = body.len() - FOOTER_LEN - 1;
        body[flip_at] ^= 0xff;
        let err = SegmentFile::verify_body_checksum(&body).unwrap_err();
        assert!(err.to_string().contains("FOOT sha256"));
    }

    #[test]
    fn admit_payload_path_refuses_flipped_payload() {
        let (_dir, seg, sha, _meta) = snapshot_one_part("flip-path");
        let mut body = fs::read(&seg.path).unwrap();
        let flip_at = body.len() - FOOTER_LEN - 1;
        body[flip_at] ^= 0xff;
        fs::write(&seg.path, &body).unwrap();
        let err = SegmentFile::admit_payload_path(&seg.path, &sha).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("FOOT sha256"));
    }

    #[test]
    fn read_index_path_does_not_hash_the_body() {
        let (_dir, seg, sha, meta) = snapshot_one_part("index-only");
        let mut body = fs::read(&seg.path).unwrap();
        let flip_at = body.len() - FOOTER_LEN - 1;
        body[flip_at] ^= 0xff;
        fs::write(&seg.path, &body).unwrap();

        let indexed = SegmentFile::read_index_path(&seg.path).unwrap();
        assert_eq!(indexed.body_sha256, sha);
        assert_eq!(indexed.index.len(), meta.index.len());
        assert_eq!(indexed.index[0].start, meta.index[0].start);
        assert!(SegmentFile::admit_payload_path(&seg.path, &sha).is_err());
    }

    #[test]
    fn read_index_path_reads_payload_bytes() {
        let (_dir, seg, _sha, meta) = snapshot_one_part("index-payload");
        let admitted = SegmentFile::read_index_path(&seg.path).unwrap();
        assert_eq!(admitted.num_partitions, meta.num_partitions);
    }

    #[test]
    fn v4_mixed_arrow_and_copy_parts_roundtrip() {
        let dir = temp_dir();
        let seg = SegmentFile::new(&dir, "mixed").unwrap();
        let arrow_key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "_dl_copy".to_string(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "arrow-fp".to_string(),
        };
        let copy_key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "events".to_string(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: crate::pipeline_type::COPY_WAL_FINGERPRINT.to_string(),
        };
        let mut batches = HashMap::new();
        batches.insert(arrow_key.clone(), WalPayload::Arrow(vec![make_batch()]));
        batches.insert(
            copy_key.clone(),
            WalPayload::Raw(vec![crate::serdes::copy::CopyWalRecord {
                value: serde_json::json!({"id": 1}),
                identity: Some(b"{\"id\":1}\n".to_vec()),
            }]),
        );
        let mut parts_meta = HashMap::new();
        parts_meta.insert(arrow_key.clone(), (0, SystemTime::now()));
        parts_meta.insert(copy_key.clone(), (0, SystemTime::now()));
        let (meta, rows, _sha) = seg
            .write_snapshot(
                &HashMap::new(),
                &batches,
                &parts_meta,
                &HashMap::new(),
                SystemTime::UNIX_EPOCH,
            )
            .unwrap();
        assert_eq!(meta.num_partitions, 2);
        assert_eq!(rows, 4);
        let read = SegmentFile::read_metadata_from_bytes(&fs::read(&seg.path).unwrap()).unwrap();
        let copy_idx = read
            .index
            .iter()
            .find(|idx| idx.key.schema_fingerprint == crate::pipeline_type::COPY_WAL_FINGERPRINT)
            .unwrap();
        let arrow_idx = read
            .index
            .iter()
            .find(|idx| idx.key.schema_fingerprint == "arrow-fp")
            .unwrap();
        assert_eq!(copy_idx.codec, PART_CODEC_COPY);
        assert_eq!(arrow_idx.codec, PART_CODEC_ARROW);
        let bytes = fs::read(&seg.path).unwrap();
        let start = copy_idx.start as usize;
        let end = start + copy_idx.len as usize;
        let decoded = decode_copy_part(&bytes[start..end]).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].value["id"], 1);
    }

    #[test]
    fn v3_arrow_segment_still_reads() {
        let dir = temp_dir();
        let path = dir.join("legacy.seg");
        let key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "ns".to_string(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".to_string(),
        };
        let mut file = Cursor::new(Vec::new());
        file.write_all(MAGIC).unwrap();
        file.write_all(&SEGF_VERSION_V3.to_le_bytes()).unwrap();
        file.write_all(&0u64.to_le_bytes()).unwrap();
        let offsets =
            bincode::serialize(&HashMap::<crate::helpers::offsets::OffsetKey, u64>::new()).unwrap();
        file.write_all(&(offsets.len() as u64).to_le_bytes())
            .unwrap();
        file.write_all(&offsets).unwrap();
        file.write_all(PART).unwrap();
        let key_blob = bincode::serialize(&key).unwrap();
        file.write_all(&(key_blob.len() as u64).to_le_bytes())
            .unwrap();
        file.write_all(&key_blob).unwrap();
        file.write_all(&0u64.to_le_bytes()).unwrap();
        file.write_all(&0u64.to_le_bytes()).unwrap();
        file.write_all(&0u64.to_le_bytes()).unwrap();
        let data_len_pos = file.stream_position().unwrap();
        file.write_all(&0u64.to_le_bytes()).unwrap();
        let start = file.stream_position().unwrap();
        {
            let batch = make_batch();
            let options = IpcWriteOptions::default();
            let mut writer =
                StreamWriter::try_new_with_options(&mut file, &batch.schema(), options).unwrap();
            writer.write(&batch).unwrap();
            writer.finish().unwrap();
        }
        let end = file.stream_position().unwrap();
        let len = end - start;
        file.seek(std::io::SeekFrom::Start(data_len_pos)).unwrap();
        file.write_all(&len.to_le_bytes()).unwrap();
        file.seek(std::io::SeekFrom::Start(end)).unwrap();
        let digest = Sha256::digest(&file.get_ref()[..end as usize]);
        file.write_all(FOOT).unwrap();
        file.write_all(&1u32.to_le_bytes()).unwrap();
        file.write_all(&digest).unwrap();
        fs::write(&path, file.into_inner()).unwrap();
        let meta = SegmentFile::read_metadata_from_bytes(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(meta.num_partitions, 1);
        assert_eq!(meta.index[0].codec, PART_CODEC_ARROW);
    }

    #[test]
    fn copy_estimated_bytes_tracks_identity_without_json_display() {
        let identity = b"{\"id\":1}\n".to_vec();
        let payload = WalPayload::Raw(vec![crate::serdes::copy::CopyWalRecord {
            value: serde_json::json!({"id": 1}),
            identity: Some(identity.clone()),
        }]);
        let bytes = payload.estimated_bytes();
        assert!(bytes >= identity.len() as u64);
        assert!(bytes < 64);
    }

    #[test]
    fn encode_snapshot_is_deterministic_for_created_at() {
        let src = include_str!("segment_file.rs");
        let start = src.find("pub fn encode_snapshot").expect("encode_snapshot");
        let body = src[start..]
            .split("\nimpl SegmentFile")
            .next()
            .expect("encode_snapshot body");
        assert!(
            !body.contains("SystemTime::now()"),
            "encode_snapshot must not read the wall clock"
        );
        let key = PartitionKey {
            sink_ref: "data_outputs.test".to_string(),
            namespace: "ns".to_string(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".to_string(),
        };
        let mut batches = HashMap::new();
        batches.insert(key, WalPayload::Arrow(vec![make_batch()]));
        let created = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let a = encode_snapshot(
            &HashMap::new(),
            &batches,
            &HashMap::new(),
            &HashMap::new(),
            created,
        )
        .unwrap();
        let b = encode_snapshot(
            &HashMap::new(),
            &batches,
            &HashMap::new(),
            &HashMap::new(),
            created,
        )
        .unwrap();
        assert_eq!(a.sha256, b.sha256);
        let later = encode_snapshot(
            &HashMap::new(),
            &batches,
            &HashMap::new(),
            &HashMap::new(),
            created + std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert_ne!(a.sha256, later.sha256);
        let created = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut first = HashMap::new();
        first.insert(
            PartitionKey {
                sink_ref: "s".into(),
                namespace: "ns".into(),
                partition: "b".into(),
                time: None,
                schema_fingerprint: "fp".into(),
            },
            WalPayload::Arrow(vec![make_batch()]),
        );
        first.insert(
            PartitionKey {
                sink_ref: "s".into(),
                namespace: "ns".into(),
                partition: "a".into(),
                time: None,
                schema_fingerprint: "fp".into(),
            },
            WalPayload::Arrow(vec![make_batch()]),
        );
        let mut second = HashMap::new();
        second.insert(
            PartitionKey {
                sink_ref: "s".into(),
                namespace: "ns".into(),
                partition: "a".into(),
                time: None,
                schema_fingerprint: "fp".into(),
            },
            WalPayload::Arrow(vec![make_batch()]),
        );
        second.insert(
            PartitionKey {
                sink_ref: "s".into(),
                namespace: "ns".into(),
                partition: "b".into(),
                time: None,
                schema_fingerprint: "fp".into(),
            },
            WalPayload::Arrow(vec![make_batch()]),
        );
        let left = encode_snapshot(
            &HashMap::new(),
            &first,
            &HashMap::new(),
            &HashMap::new(),
            created,
        )
        .unwrap();
        let right = encode_snapshot(
            &HashMap::new(),
            &second,
            &HashMap::new(),
            &HashMap::new(),
            created,
        )
        .unwrap();
        assert_eq!(left.sha256, right.sha256);
    }

    #[test]
    fn write_payload_atomically_renames_tmp_to_seg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.seg");
        write_payload_atomically(&path, b"payload").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"payload");
        assert!(!dir.path().join("snap.seg.tmp").exists());
        let src = include_str!("segment_file.rs");
        let start = src.find("pub fn write_snapshot").expect("write_snapshot");
        let body = src[start..]
            .split("pub fn write_payload_atomically")
            .next()
            .expect("write_snapshot body");
        assert!(body.contains("write_payload_atomically"));
        assert!(!body.contains("DirectIoFile::create("));
    }

    #[test]
    fn huge_segf_offsets_len_fails_closed() {
        // cargo-fuzz wal_slices crash-3ec3c9b046a3bd6ac6e281b6d88b42eb60a924a2
        let bytes: &[u8] = include_bytes!("../../fuzz/corpus/wal_slices/crash_3ec3c9b0");
        assert_eq!(&bytes[..4], b"SEGF");
        let err = SegmentFile::read_metadata_from_bytes(bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let err =
            SegmentFile::read_part_meta_blobs_from_reader(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
