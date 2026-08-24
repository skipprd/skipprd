use crate::buffer::direct_io::DirectIoFile;
use crate::buffer::segment_file::{PartitionKey, SegmentPartitionIndexEntry};
use crate::metrics::counters as metrics_counters;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// V1: magic[4], version(u32 LE), indexed-slice count(u32 LE),
// bitmap-byte count(u32 LE), bitmap bytes, SHA-256 of all preceding bytes.
const MAGIC: &[u8; 4] = b"SCBL";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 16;
const CHECKSUM_LEN: usize = 32;

static SEGMENT_LOCKS: Lazy<DashMap<PathBuf, Arc<Mutex<()>>>> = Lazy::new(DashMap::new);
static SEGMENT_CACHE: Lazy<DashMap<PathBuf, CachedState>> = Lazy::new(DashMap::new);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
enum CachedState {
    Valid(Arc<CompletionBitmap>),
    Corrupt(String),
}

#[derive(Clone, Debug)]
struct CompletionBitmap {
    bit_count: usize,
    bits: Vec<u8>,
}

impl CompletionBitmap {
    fn empty(bit_count: usize) -> Self {
        Self {
            bit_count,
            bits: vec![0; bit_count.saturating_add(7) / 8],
        }
    }

    fn is_set(&self, ordinal: usize) -> bool {
        ordinal < self.bit_count && self.bits[ordinal / 8] & (1 << (ordinal % 8)) != 0
    }

    fn set(&mut self, ordinal: usize) -> io::Result<bool> {
        if ordinal >= self.bit_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "completion ordinal {ordinal} outside segment index of {} entries",
                    self.bit_count
                ),
            ));
        }
        let byte = &mut self.bits[ordinal / 8];
        let mask = 1 << (ordinal % 8);
        let changed = *byte & mask == 0;
        *byte |= mask;
        Ok(changed)
    }

    fn all_complete(&self) -> bool {
        (0..self.bit_count).all(|ordinal| self.is_set(ordinal))
    }

    fn completed_count(&self) -> usize {
        (0..self.bit_count)
            .filter(|ordinal| self.is_set(*ordinal))
            .count()
    }
}

/// One per-segment update for [`SegmentCompletionLedger::mark_complete_batch`].
///
/// `index` is the stable index order persisted in the v3 WAL segment. Completion
/// bits are addressed by ordinal in that order.
pub struct SegmentCompletionUpdate<'a> {
    pub segment_id: &'a str,
    pub index: &'a [SegmentPartitionIndexEntry],
    pub ordinals: &'a [usize],
}

/// Durable completion state stored adjacent to the legacy per-slice tombstones.
///
/// The process-wide cache keeps scheduling checks off the filesystem after a
/// segment's first access. A process-wide per-sidecar lock serializes read/OR/
/// replace updates from concurrent compactions.
#[derive(Clone, Debug)]
pub struct SegmentCompletionLedger {
    done_dir: PathBuf,
}

impl SegmentCompletionLedger {
    pub fn new(done_dir: PathBuf) -> Self {
        Self { done_dir }
    }

    pub fn bitmap_path(&self, segment_id: &str) -> PathBuf {
        self.done_dir
            .join(format!("{segment_id}.seg.completion-bitmap"))
    }

    pub fn legacy_tombstone_path(&self, segment_id: &str, key: &PartitionKey) -> PathBuf {
        let time = key.time.unwrap_or(0);
        let safe = |value: &str| value.replace('/', "_");
        self.done_dir.join(format!(
            "{}.seg.{}.{}.{}.{}.{}.tombstone",
            segment_id,
            safe(&key.sink_ref),
            safe(&key.namespace),
            safe(&key.partition),
            time,
            safe(&key.schema_fingerprint)
        ))
    }

    pub fn is_complete(
        &self,
        segment_id: &str,
        index: &[SegmentPartitionIndexEntry],
        ordinal: usize,
    ) -> io::Result<bool> {
        if ordinal >= index.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "completion ordinal {ordinal} outside segment index of {} entries",
                    index.len()
                ),
            ));
        }
        Ok(self
            .load_cached_or_migrate(segment_id, index)?
            .is_set(ordinal))
    }

    pub fn is_complete_key(
        &self,
        segment_id: &str,
        index: &[SegmentPartitionIndexEntry],
        key: &PartitionKey,
    ) -> io::Result<bool> {
        let ordinal = index
            .iter()
            .position(|entry| &entry.key == key)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "completion key is not present in the segment index",
                )
            })?;
        self.is_complete(segment_id, index, ordinal)
    }

    pub fn all_complete(
        &self,
        segment_id: &str,
        index: &[SegmentPartitionIndexEntry],
    ) -> io::Result<bool> {
        Ok(self
            .load_cached_or_migrate(segment_id, index)?
            .all_complete())
    }

    /// Materialize a bitmap and merge any legacy tombstones on first access.
    pub fn migrate_legacy(
        &self,
        segment_id: &str,
        index: &[SegmentPartitionIndexEntry],
    ) -> io::Result<usize> {
        Ok(self
            .load_cached_or_migrate(segment_id, index)?
            .completed_count())
    }

    /// Durably OR completion bits, grouped by segment.
    ///
    /// Legacy tombstones are written and synced first so a crash at any later
    /// point remains readable by 15.10.1. The bitmap then uses temp-file write,
    /// file fsync, atomic rename, and parent-directory fsync.
    pub fn mark_complete_batch(&self, updates: &[SegmentCompletionUpdate<'_>]) -> io::Result<()> {
        let mut first_error = None;
        for update in updates {
            if let Err(error) = self.mark_segment_complete(update) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Remove completion sidecars after the corresponding segment is gone.
    pub fn remove_segment(
        &self,
        segment_id: &str,
        index: &[SegmentPartitionIndexEntry],
    ) -> io::Result<()> {
        let bitmap_path = self.bitmap_path(segment_id);
        let lock = segment_lock(&bitmap_path);
        let _guard = lock_segment(&lock)?;
        let mut changed = false;
        let mut first_error = None;

        for path in std::iter::once(bitmap_path.clone()).chain(
            index
                .iter()
                .map(|entry| self.legacy_tombstone_path(segment_id, &entry.key)),
        ) {
            match fs::remove_file(&path) {
                Ok(()) => changed = true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if changed {
            if let Err(error) = fsync_dir(&self.done_dir) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        SEGMENT_CACHE.remove(&bitmap_path);
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn forget_segment(&self, segment_id: &str) {
        SEGMENT_CACHE.remove(&self.bitmap_path(segment_id));
    }

    fn mark_segment_complete(&self, update: &SegmentCompletionUpdate<'_>) -> io::Result<()> {
        let bitmap_path = self.bitmap_path(update.segment_id);
        let lock = segment_lock(&bitmap_path);
        let _guard = lock_segment(&lock)?;

        let mut bitmap = match self.load_from_disk_and_migrate(update.segment_id, update.index) {
            Ok(bitmap) => bitmap,
            Err(error) => {
                SEGMENT_CACHE.insert(bitmap_path, CachedState::Corrupt(error.to_string()));
                return Err(error);
            }
        };

        let mut completed_ordinals = Vec::with_capacity(update.ordinals.len());
        let mut first_error = None;
        for &ordinal in update.ordinals {
            let Some(entry) = update.index.get(ordinal) else {
                if first_error.is_none() {
                    first_error = Some(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "completion ordinal {ordinal} outside segment index of {} entries",
                            update.index.len()
                        ),
                    ));
                }
                continue;
            };
            let tombstone = self.legacy_tombstone_path(update.segment_id, &entry.key);
            match write_and_sync_legacy_tombstone(&tombstone) {
                Ok(()) => {
                    completed_ordinals.push(ordinal);
                    metrics_counters::add_compaction_tombstone_write(1);
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if !completed_ordinals.is_empty() {
            if let Err(error) = fsync_dir(&self.done_dir) {
                SEGMENT_CACHE.remove(&bitmap_path);
                return Err(error);
            }
        }

        let mut changed = false;
        for ordinal in completed_ordinals {
            changed |= bitmap.set(ordinal)?;
        }
        if changed {
            if let Err(error) = persist_bitmap(&bitmap_path, &bitmap) {
                SEGMENT_CACHE.remove(&bitmap_path);
                return Err(error);
            }
            metrics_counters::add_compaction_completion_ledger_write(1);
        }
        SEGMENT_CACHE.insert(bitmap_path, CachedState::Valid(Arc::new(bitmap)));

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn load_cached_or_migrate(
        &self,
        segment_id: &str,
        index: &[SegmentPartitionIndexEntry],
    ) -> io::Result<Arc<CompletionBitmap>> {
        let bitmap_path = self.bitmap_path(segment_id);
        let lock = segment_lock(&bitmap_path);
        let _guard = lock_segment(&lock)?;

        if let Some(cached) = SEGMENT_CACHE.get(&bitmap_path) {
            return cached_result(cached.value(), index.len());
        }

        match self.load_from_disk_and_migrate(segment_id, index) {
            Ok(bitmap) => {
                let bitmap = Arc::new(bitmap);
                SEGMENT_CACHE.insert(bitmap_path, CachedState::Valid(bitmap.clone()));
                Ok(bitmap)
            }
            Err(error) => {
                SEGMENT_CACHE.insert(bitmap_path, CachedState::Corrupt(error.to_string()));
                Err(error)
            }
        }
    }

    fn load_from_disk_and_migrate(
        &self,
        segment_id: &str,
        index: &[SegmentPartitionIndexEntry],
    ) -> io::Result<CompletionBitmap> {
        fs::create_dir_all(&self.done_dir)?;
        let bitmap_path = self.bitmap_path(segment_id);
        let (mut bitmap, mut changed) = match DirectIoFile::read_path(&bitmap_path) {
            Ok(bytes) => (decode_bitmap(&bytes, index.len())?, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (CompletionBitmap::empty(index.len()), true)
            }
            Err(error) => return Err(error),
        };

        for (ordinal, entry) in index.iter().enumerate() {
            let tombstone = self.legacy_tombstone_path(segment_id, &entry.key);
            match fs::metadata(tombstone) {
                Ok(_) => changed |= bitmap.set(ordinal)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }

        if changed {
            persist_bitmap(&bitmap_path, &bitmap)?;
            metrics_counters::add_compaction_completion_ledger_write(1);
        }
        Ok(bitmap)
    }
}

fn segment_lock(path: &Path) -> Arc<Mutex<()>> {
    SEGMENT_LOCKS
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn lock_segment(lock: &Arc<Mutex<()>>) -> io::Result<std::sync::MutexGuard<'_, ()>> {
    lock.lock()
        .map_err(|_| io::Error::other("segment completion lock poisoned"))
}

fn cached_result(state: &CachedState, expected_bits: usize) -> io::Result<Arc<CompletionBitmap>> {
    match state {
        CachedState::Valid(bitmap) if bitmap.bit_count == expected_bits => Ok(bitmap.clone()),
        CachedState::Valid(bitmap) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "completion bitmap indexes {} slices but segment has {expected_bits}",
                bitmap.bit_count
            ),
        )),
        CachedState::Corrupt(message) => {
            Err(io::Error::new(io::ErrorKind::InvalidData, message.clone()))
        }
    }
}

fn write_and_sync_legacy_tombstone(path: &Path) -> io::Result<()> {
    DirectIoFile::write_path_sync(path, &[])
}

fn encode_bitmap(bitmap: &CompletionBitmap) -> io::Result<Vec<u8>> {
    let bit_count = u32::try_from(bitmap.bit_count).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "completion bitmap has more than u32::MAX bits",
        )
    })?;
    let byte_count = u32::try_from(bitmap.bits.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "completion bitmap payload is too large",
        )
    })?;
    let mut bytes = Vec::with_capacity(HEADER_LEN + bitmap.bits.len() + CHECKSUM_LEN);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&bit_count.to_le_bytes());
    bytes.extend_from_slice(&byte_count.to_le_bytes());
    bytes.extend_from_slice(&bitmap.bits);
    let checksum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    Ok(bytes)
}

fn decode_bitmap(bytes: &[u8], expected_bits: usize) -> io::Result<CompletionBitmap> {
    if bytes.len() < HEADER_LEN + CHECKSUM_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "completion bitmap is truncated",
        ));
    }
    if &bytes[0..4] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad completion bitmap magic",
        ));
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported completion bitmap version {version}"),
        ));
    }
    let bit_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let byte_count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let expected_byte_count = bit_count.saturating_add(7) / 8;
    let expected_len = HEADER_LEN
        .checked_add(byte_count)
        .and_then(|length| length.checked_add(CHECKSUM_LEN))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "completion bitmap length overflow",
            )
        })?;
    if bit_count != expected_bits
        || byte_count != expected_byte_count
        || bytes.len() != expected_len
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "completion bitmap shape mismatch: bits={bit_count} bytes={byte_count} segment_bits={expected_bits}"
            ),
        ));
    }
    let checksum_offset = HEADER_LEN + byte_count;
    let expected_checksum = Sha256::digest(&bytes[..checksum_offset]);
    if expected_checksum.as_slice() != &bytes[checksum_offset..] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "completion bitmap checksum mismatch",
        ));
    }
    let bits = bytes[HEADER_LEN..checksum_offset].to_vec();
    if bit_count % 8 != 0 {
        let valid_mask = (1u8 << (bit_count % 8)) - 1;
        if bits.last().copied().unwrap_or(0) & !valid_mask != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "completion bitmap has non-zero padding bits",
            ));
        }
    }
    Ok(CompletionBitmap { bit_count, bits })
}

fn persist_bitmap(path: &Path, bitmap: &CompletionBitmap) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "completion bitmap path has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    let bytes = encode_bitmap(bitmap)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("completion-bitmap");
    for _ in 0..16 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            sequence
        ));
        match DirectIoFile::create_new(&candidate) {
            Ok(mut file) => {
                let result = (|| {
                    file.write_all(&bytes)?;
                    file.sync_data()?;
                    drop(file);
                    fs::rename(&candidate, path)?;
                    fsync_dir(parent)
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&candidate);
                }
                return result;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a completion bitmap temp file",
    ))
}

#[cfg(not(windows))]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(windows)]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::SegmentPartMetaSummary;
    use std::sync::Barrier;

    fn index(count: usize) -> Vec<SegmentPartitionIndexEntry> {
        (0..count)
            .map(|ordinal| SegmentPartitionIndexEntry {
                key: PartitionKey {
                    sink_ref: "data_sinks.test".to_string(),
                    namespace: "ledger_test".to_string(),
                    partition: format!("slice-{ordinal}"),
                    time: Some(1_700_000_000),
                    schema_fingerprint: "schema-v1".to_string(),
                },
                bytes: 1,
                updated_at_secs: 0,
                slice_ordinal: ordinal as u32,
                part_meta_start: 0,
                part_meta_len: 0,
                part_meta_summary: SegmentPartMetaSummary::Append,
                start: ordinal as u64,
                len: 1,
            })
            .collect()
    }

    #[test]
    fn concurrent_updates_merge_monotonically() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = Arc::new(SegmentCompletionLedger::new(temp.path().join("done")));
        let index = Arc::new(index(2));
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for ordinal in 0..2 {
            let ledger = ledger.clone();
            let index = index.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let ordinals = [ordinal];
                ledger
                    .mark_complete_batch(&[SegmentCompletionUpdate {
                        segment_id: "concurrent",
                        index: &index,
                        ordinals: &ordinals,
                    }])
                    .unwrap();
            }));
        }
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        assert!(ledger.all_complete("concurrent", &index).unwrap());
    }

    #[test]
    fn truncated_bitmap_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = SegmentCompletionLedger::new(temp.path().join("done"));
        fs::create_dir_all(&ledger.done_dir).unwrap();
        fs::write(ledger.bitmap_path("truncated"), b"SCBL\x01").unwrap();

        assert!(ledger.all_complete("truncated", &index(1)).is_err());
    }

    #[test]
    fn checksum_corruption_is_rejected_even_with_legacy_tombstones() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = SegmentCompletionLedger::new(temp.path().join("done"));
        let index = index(1);
        ledger
            .mark_complete_batch(&[SegmentCompletionUpdate {
                segment_id: "corrupt",
                index: &index,
                ordinals: &[0],
            }])
            .unwrap();
        let path = ledger.bitmap_path("corrupt");
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        fs::write(&path, bytes).unwrap();
        ledger.forget_segment("corrupt");

        assert!(ledger.all_complete("corrupt", &index).is_err());
    }

    #[test]
    fn legacy_tombstones_are_migrated_on_first_access() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = SegmentCompletionLedger::new(temp.path().join("done"));
        let index = index(2);
        fs::create_dir_all(&ledger.done_dir).unwrap();
        fs::write(ledger.legacy_tombstone_path("legacy", &index[1].key), b"").unwrap();

        assert!(!ledger.is_complete("legacy", &index, 0).unwrap());
        assert!(ledger.is_complete("legacy", &index, 1).unwrap());
        assert!(ledger.bitmap_path("legacy").is_file());
    }

    #[test]
    fn completion_marks_dual_write_legacy_tombstones() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = SegmentCompletionLedger::new(temp.path().join("done"));
        let index = index(1);
        ledger
            .mark_complete_batch(&[SegmentCompletionUpdate {
                segment_id: "rollback",
                index: &index,
                ordinals: &[0],
            }])
            .unwrap();

        assert!(ledger
            .legacy_tombstone_path("rollback", &index[0].key)
            .is_file());
        assert!(ledger.is_complete("rollback", &index, 0).unwrap());
    }
}
