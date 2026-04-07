use crate::helpers::offsets::OffsetKey;
use arrow::array::RecordBatch;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use bincode;
use serde_derive::{Deserialize, Serialize};
#[cfg(test)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::{fs, io};

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct PartitionKey {
    pub sink_ref: String,
    pub namespace: String,
    pub partition: String,
    pub time: Option<i64>,
    pub shard: String,
}

const MAGIC: &[u8; 4] = b"SEGF";
const PART: &[u8; 4] = b"PART";
const FOOT: &[u8; 4] = b"FOOT";
/// Segment format version. Includes an optional `part_meta_blob` per PART
/// for CDC row-aligned metadata (mutation kind, event_id, order_token).
const VERSION: u32 = 3;
const COMMIT_HEADER_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct SegmentPartitionIndexEntry {
    pub key: PartitionKey,
    pub bytes: u64,
    pub updated_at_secs: u64,
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
}

pub struct SegmentFile {
    pub path: PathBuf,
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
        // MAGIC
        buf[0..4].copy_from_slice(b"SEGC");
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
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Compactor: bad segment magic",
            ));
        }
        let mut ver = [0u8; 4];
        reader.read_exact(&mut ver)?;
        let version = u32::from_le_bytes(ver);
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Compactor: read refused version={}", version),
            ));
        }
        let mut created = [0u8; 8];
        reader.read_exact(&mut created)?;
        let created_at_secs = u64::from_le_bytes(created);
        let mut off_len_buf = [0u8; 8];
        reader.read_exact(&mut off_len_buf)?;
        let offsets_len = u64::from_le_bytes(off_len_buf);
        let mut offsets_blob = vec![0u8; offsets_len as usize];
        reader.read_exact(&mut offsets_blob)?;
        let offsets: std::collections::HashMap<OffsetKey, u64> =
            bincode::deserialize(&offsets_blob).unwrap_or_default();

        let mut index: Vec<SegmentPartitionIndexEntry> = Vec::new();
        let mut total_bytes: u64 = 0;
        loop {
            let mut tag = [0u8; 4];
            match reader.read_exact(&mut tag) {
                Ok(()) => {}
                Err(e) => {
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        break;
                    } else {
                        return Err(e);
                    }
                }
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
            let mut bytes_buf = [0u8; 8];
            reader.read_exact(&mut bytes_buf)?;
            let part_bytes = u64::from_le_bytes(bytes_buf);
            let mut upd_buf = [0u8; 8];
            reader.read_exact(&mut upd_buf)?;
            let upd_secs = u64::from_le_bytes(upd_buf);

            // Skip part_meta_blob sidecar
            let mut meta_len_buf = [0u8; 8];
            reader.read_exact(&mut meta_len_buf)?;
            let meta_len = u64::from_le_bytes(meta_len_buf);
            if meta_len > 0 {
                reader.seek(io::SeekFrom::Current(meta_len as i64))?;
            }

            let mut len_buf = [0u8; 8];
            reader.read_exact(&mut len_buf)?;
            let data_len = u64::from_le_bytes(len_buf);
            let start = reader.stream_position()?;
            total_bytes = total_bytes.saturating_add(data_len);
            index.push(SegmentPartitionIndexEntry {
                key,
                bytes: part_bytes,
                updated_at_secs: upd_secs,
                start,
                len: data_len,
            });
            reader.seek(io::SeekFrom::Current(data_len as i64))?;
        }

        Ok(SegmentFileMetadata {
            created_at_secs,
            total_bytes,
            num_partitions: index.len() as u32,
            offsets,
            index,
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
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(true)
            .open(&self.path)?;
        file.seek(io::SeekFrom::Start(0))?;

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

            // write part_meta_blob sidecar
            let meta_blob = part_meta_blobs.get(key).cloned().unwrap_or_default();
            let meta_len = meta_blob.len() as u64;
            file.write_all(&meta_len.to_le_bytes())?;
            if meta_len > 0 {
                file.write_all(&meta_blob)?;
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
                start,
                len,
            });
        }

        let end_before_footer = file.stream_position()?;
        let mut f2 = OpenOptions::new().read(true).open(&self.path)?;
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut remaining = end_before_footer as i64;
        loop {
            if remaining <= 0 {
                break;
            }
            let to_read = std::cmp::min(remaining as usize, buf.len());
            let n = f2.read(&mut buf[..to_read])?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            remaining -= n as i64;
        }
        let digest = hasher.finalize();
        let mut sha_bytes: [u8; 32] = [0u8; 32];
        sha_bytes.copy_from_slice(&digest[..]);

        file.write_all(FOOT)?;
        file.write_all(&parts_count.to_le_bytes())?;
        file.write_all(&sha_bytes)?;
        file.sync_all()?;

        let meta = SegmentFileMetadata {
            created_at_secs,
            total_bytes,
            num_partitions: parts_count,
            offsets: offsets.clone(),
            index,
        };
        Ok((meta, total_rows, sha_bytes))
    }

    /// Read per-partition CDC metadata blobs from a segment.
    pub fn read_part_meta_blobs_from_reader<R: Read + Seek>(
        reader: &mut R,
    ) -> io::Result<std::collections::HashMap<PartitionKey, Vec<u8>>> {
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

    pub fn read_metadata(&self) -> io::Result<SegmentFileMetadata> {
        let mut file = OpenOptions::new().read(true).open(&self.path)?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Compactor: bad segment magic",
            ));
        }
        let mut ver = [0u8; 4];
        file.read_exact(&mut ver)?;
        let version = u32::from_le_bytes(ver);
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Compactor: read refused seg={} version={}",
                    self.path.to_string_lossy(),
                    version
                ),
            ));
        }
        let mut created = [0u8; 8];
        file.read_exact(&mut created)?;
        let created_at_secs = u64::from_le_bytes(created);
        let mut off_len_buf = [0u8; 8];
        file.read_exact(&mut off_len_buf)?;
        let offsets_len = u64::from_le_bytes(off_len_buf);
        let mut offsets_blob = vec![0u8; offsets_len as usize];
        file.read_exact(&mut offsets_blob)?;
        let offsets: std::collections::HashMap<OffsetKey, u64> =
            bincode::deserialize(&offsets_blob).unwrap_or_default();

        let mut index: Vec<SegmentPartitionIndexEntry> = Vec::new();
        let mut total_bytes: u64 = 0;
        loop {
            let mut tag = [0u8; 4];
            match file.read_exact(&mut tag) {
                Ok(()) => {}
                Err(e) => {
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        break;
                    } else {
                        return Err(e);
                    }
                }
            }
            if &tag != PART {
                break;
            }
            let mut key_len_buf = [0u8; 8];
            file.read_exact(&mut key_len_buf)?;
            let key_len = u64::from_le_bytes(key_len_buf);
            let mut key_blob = vec![0u8; key_len as usize];
            file.read_exact(&mut key_blob)?;
            let key: PartitionKey = bincode::deserialize(&key_blob).unwrap();
            let mut bytes_buf = [0u8; 8];
            file.read_exact(&mut bytes_buf)?;
            let part_bytes = u64::from_le_bytes(bytes_buf);
            let mut upd_buf = [0u8; 8];
            file.read_exact(&mut upd_buf)?;
            let upd_secs = u64::from_le_bytes(upd_buf);

            // Skip part_meta_blob sidecar
            let mut meta_len_buf = [0u8; 8];
            file.read_exact(&mut meta_len_buf)?;
            let meta_len = u64::from_le_bytes(meta_len_buf);
            if meta_len > 0 {
                file.seek(io::SeekFrom::Current(meta_len as i64))?;
            }

            let mut len_buf = [0u8; 8];
            file.read_exact(&mut len_buf)?;
            let data_len = u64::from_le_bytes(len_buf);
            let start = file.stream_position()?;
            let file_len = file.metadata()?.len();
            if start.saturating_add(data_len) > file_len {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!(
                    "Compactor: partition length beyond file end for {} start={} len={} file_len={}",
                    self.path.to_string_lossy(), start, data_len, file_len
                )));
            }
            total_bytes = total_bytes.saturating_add(data_len);
            index.push(SegmentPartitionIndexEntry {
                key,
                bytes: part_bytes,
                updated_at_secs: upd_secs,
                start,
                len: data_len,
            });
            // Seek over the Arrow stream to the next PART header
            file.seek(io::SeekFrom::Current(data_len as i64))?;
        }

        Ok(SegmentFileMetadata {
            created_at_secs,
            total_bytes,
            num_partitions: index.len() as u32,
            offsets,
            index,
        })
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
            shard: "shard".to_string(),
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
            shard: "shard".to_string(),
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

        let read_meta = seg.read_metadata().unwrap();
        assert_eq!(read_meta.num_partitions, 1);
        assert_eq!(read_meta.index.len(), 1);

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
            shard: "".to_string(),
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
}
