use std::fs::{File, OpenOptions};
use std::{io, fs};
use std::io::{Seek, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use arrow::array::RecordBatch;
use arrow_schema::SchemaRef;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow::ipc::reader::StreamReader;
use crate::helpers::offsets::OffsetKey;
pub type PartitionKey = (String, String, Option<i64>, String);
use bincode;

const MAGIC: &[u8; 4] = b"SEGF";
const PART: &[u8; 4] = b"PART";
const VERSION: u32 = 1;

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
    pub fn new(dir: &Path, snapshot_id: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let p = dir.join(format!("{}.seg", snapshot_id));
        let _ = OpenOptions::new().create(true).write(true).truncate(true).open(&p)?;
        Ok(SegmentFile { path: p })
    }

    pub fn write_snapshot(
        &self,
        offsets: &std::collections::HashMap<OffsetKey, u64>,
        batches: &std::collections::HashMap<PartitionKey, Vec<RecordBatch>>,
        partitions_meta: &std::collections::HashMap<PartitionKey, (u64 /*bytes*/, SystemTime /*updated*/ )>,
    ) -> io::Result<(u64 /*bytes*/, u64 /*rows*/)> {
        let mut file = OpenOptions::new().write(true).read(true).open(&self.path)?;
        file.seek(io::SeekFrom::Start(0))?;

        // Header MAGIC + VERSION
        file.write_all(MAGIC)?;
        file.write_all(&VERSION.to_le_bytes())?;
        // created_at
        let created_at_secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
        file.write_all(&created_at_secs.to_le_bytes())?;

        // Offsets block
        let offsets_blob = bincode::serialize(offsets).unwrap();
        let offsets_len = offsets_blob.len() as u64;
        file.write_all(&offsets_len.to_le_bytes())?;
        file.write_all(&offsets_blob)?;

        // We'll write partitions sequentially with inline headers
        let mut total_rows: u64 = 0;
        let mut total_bytes: u64 = 0;
        for (key, rbatches) in batches.iter() {
            if rbatches.is_empty() { continue; }
            // Partition header
            file.write_all(PART)?;
            let key_blob = bincode::serialize(key).unwrap();
            let key_len = key_blob.len() as u64;
            file.write_all(&key_len.to_le_bytes())?;
            file.write_all(&key_blob)?;
            let (p_bytes, p_updated) = partitions_meta.get(key).cloned().unwrap_or((0, SystemTime::now()));
            let updated_secs = p_updated.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
            file.write_all(&p_bytes.to_le_bytes())?;
            file.write_all(&updated_secs.to_le_bytes())?;

            // Record start pos
            let start = file.stream_position()?;
            {
                // Write Arrow stream (scope to drop writer before querying file position)
                let options = IpcWriteOptions::default();
                let mut writer = StreamWriter::try_new_with_options(&mut file, &rbatches[0].schema(), options)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e)))?;
                for b in rbatches.iter() {
                    total_rows += b.num_rows() as u64;
                    writer.write(b).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e)))?;
                }
                writer.finish().map_err(|e| io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e)))?;
            }
            let end = file.stream_position()?;
            let len = end - start;
            total_bytes = total_bytes.saturating_add(len as u64);
            // For now we do not write a trailing index; we rely on scanning PART blocks for reading
        }

        file.sync_all()?;
        Ok((total_bytes, total_rows))
    }

    pub fn read_metadata(&self) -> io::Result<SegmentFileMetadata> {
        let mut file = OpenOptions::new().read(true).open(&self.path)?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC { return Err(io::Error::new(io::ErrorKind::InvalidData, "bad segment magic")); }
        let mut ver = [0u8; 4];
        file.read_exact(&mut ver)?;
        let _version = u32::from_le_bytes(ver);
        let mut created = [0u8; 8];
        file.read_exact(&mut created)?;
        let created_at_secs = u64::from_le_bytes(created);
        let mut off_len_buf = [0u8; 8];
        file.read_exact(&mut off_len_buf)?;
        let offsets_len = u64::from_le_bytes(off_len_buf);
        let mut offsets_blob = vec![0u8; offsets_len as usize];
        file.read_exact(&mut offsets_blob)?;
        let offsets: std::collections::HashMap<OffsetKey, u64> = bincode::deserialize(&offsets_blob).unwrap_or_default();

        let mut index: Vec<SegmentPartitionIndexEntry> = Vec::new();
        let mut total_bytes: u64 = 0;
        loop {
            let mut tag = [0u8; 4];
            match file.read_exact(&mut tag) { Ok(()) => {}, Err(e) => {
                if e.kind() == io::ErrorKind::UnexpectedEof { break; } else { return Err(e); }
            }}
            if &tag != PART { break; }
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
            let start = file.stream_position()?;
            // Use a dedicated handle to scan the Arrow stream from start and compute exact end
            let mut scan = OpenOptions::new().read(true).open(&self.path)?;
            scan.seek(io::SeekFrom::Start(start))?;
            let mut reader = io::BufReader::new(scan);
            let sr = StreamReader::try_new(&mut reader, None)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e)))?;
            for _ in sr { /* drain */ }
            let end = reader.stream_position()?;
            let len = end - start;
            total_bytes = total_bytes.saturating_add(len as u64);
            index.push(SegmentPartitionIndexEntry { key, bytes: part_bytes, updated_at_secs: upd_secs, start, len });
            // Advance the primary file handle to the end of this partition so we can read the next PART header
            file.seek(io::SeekFrom::Start(end))?;
        }

        Ok(SegmentFileMetadata { created_at_secs, total_bytes, num_partitions: index.len() as u32, offsets, index })
    }
}


