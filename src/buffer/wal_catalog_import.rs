//! One-way legacy WAL import. This is the only module that may name
//! `mutation.log`, snapshots, completion bitmaps, manifest JSON, or `.seg.commit`.
//!
//! Leftover inventory is logged as counts. After owned pairs, bitmaps, and
//! compaction JSON are in the catalog, leftover files are archived under
//! `segment_buffer/legacy-wal-archive/` rather than deleted in place.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use sha2::{Digest, Sha256};
use skippr_lease::{DurableError, SegmentId};
use skippr_wal_catalog::{
    query_all, CatalogFence, CatalogKey, CatalogPrefix, CatalogRow, CatalogTxn, CompactionId,
    CompactionRow, CompactionState, FormatPhase, FormatRow, PayloadLocation, PutCond, SegmentRow,
    SeqRow, SliceBitmap, WalCatalogStore, FORMAT_VERSION, ROW_MAX_BYTES,
};

use crate::buffer::compaction_transaction::{CompactionTransaction, CompactionTransactionState};
use crate::buffer::segment_file::{SegmentFile, SegmentFileMetadata};
use crate::buffer::wal_handle::{PipelineWalHandle, WAL_CATALOG_PROGRESS_EVERY};
use crate::helpers::configuration::Config;
use crate::helpers::wal_storage::WalStorage;

const COMMIT_MAGIC: &[u8; 4] = b"SEGC";
const COMMIT_HEADER_VERSION: u32 = 1;
const COMMIT_HEADER_LEN: usize = 60;
/// Safety ceiling for a full paginated WAL prefix list. Hitting it is fail-closed,
/// not a silent truncate that would then GC unimported owned payloads.
const S3_WAL_LIST_CEILING: usize = 500_000;
const ARCHIVE_DIR_NAME: &str = "legacy-wal-archive";
const COMPLETION_MAGIC: &[u8; 4] = b"SCBL";
const COMPLETION_VERSION: u32 = 1;
const COMPLETION_HEADER_LEN: usize = 16;
const COMPLETION_CHECKSUM_LEN: usize = 32;

fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn parse_commit_header(bytes: &[u8]) -> io::Result<([u8; 32], u64, u32)> {
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
    Ok((
        sha256,
        u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
    ))
}

fn admit_owned_pair_bytes(body: &[u8], commit_bytes: &[u8]) -> io::Result<SegmentFileMetadata> {
    let claimed = SegmentFile::verify_body_checksum(body)?;
    let (sha256, total_bytes, parts_count) = parse_commit_header(commit_bytes)?;
    let meta = SegmentFile::read_metadata_from_bytes(body)?;
    if claimed != meta.body_sha256 {
        return Err(invalid_data("FOOT digest disagrees with metadata scan"));
    }
    if sha256 != meta.body_sha256 {
        return Err(invalid_data(
            "SEGC sha256 does not match FOOT; commit does not name this body",
        ));
    }
    if parts_count != meta.num_partitions {
        return Err(invalid_data("SEGC parts_count does not match FOOT"));
    }
    if total_bytes != meta.total_bytes {
        return Err(invalid_data("SEGC size does not match segment total_bytes"));
    }
    Ok(meta)
}

fn admit_owned_pair_path(seg_path: &Path) -> io::Result<SegmentFileMetadata> {
    let commit_path = seg_path.with_extension("seg.commit");
    let commit_bytes = fs::read(commit_path)?;
    let body = fs::read(seg_path)?;
    admit_owned_pair_bytes(&body, &commit_bytes)
}

#[cfg(test)]
fn build_commit_header_bytes(parts_count: u32, total_bytes: u64, sha256: &[u8; 32]) -> [u8; 60] {
    let mut buf = [0u8; 60];
    buf[0..4].copy_from_slice(COMMIT_MAGIC);
    buf[4..8].copy_from_slice(&COMMIT_HEADER_VERSION.to_le_bytes());
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    buf[8..16].copy_from_slice(&created_at.to_le_bytes());
    buf[16..24].copy_from_slice(&total_bytes.to_le_bytes());
    buf[24..28].copy_from_slice(&parts_count.to_le_bytes());
    buf[28..60].copy_from_slice(sha256);
    buf
}

pub fn import_if_needed(handle: &PipelineWalHandle, config: &Config) -> Result<(), DurableError> {
    let _ = handle.guard().require_recovery_epoch()?;
    let fence = handle.fence()?;
    let leftover = leftover_inventory(handle.paths());
    leftover.log();
    match handle
        .catalog()
        .get(&CatalogKey::Format)
        .map_err(|err| DurableError::Io(err.to_string()))?
    {
        Some(CatalogRow::Format(row)) if row.phase == FormatPhase::Clean => return Ok(()),
        Some(CatalogRow::Format(row)) if row.phase == FormatPhase::Imported => {
            finish_imported(handle, fence, config, row.row_version, leftover)?;
            return Ok(());
        }
        Some(CatalogRow::Format(_)) => {}
        Some(_) => {
            return Err(DurableError::ProtocolMismatch(
                "wal#format is not a FormatRow".into(),
            ))
        }
        None => {}
    }
    let started = Instant::now();
    let segments = import_owned_pairs(handle, fence, config)?;
    let bitmaps = import_completion_bitmaps(handle, fence)?;
    let compactions = import_compaction_json(handle, fence)?;
    write_format(handle.catalog(), fence, FormatPhase::Imported, None)?;
    archive_legacy_files(handle.paths())?;
    if matches!(config.get_wal_storage(), WalStorage::S3) {
        delete_legacy_s3_commits(config)?;
    }
    write_format(handle.catalog(), fence, FormatPhase::Clean, Some(0))?;
    tracing::info!(
        segments,
        bitmaps_applied = bitmaps.applied,
        bitmaps_stale = bitmaps.stale,
        compactions,
        leftover = leftover.summary(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "WAL catalog import complete"
    );
    Ok(())
}

fn finish_imported(
    handle: &PipelineWalHandle,
    fence: CatalogFence,
    config: &Config,
    row_version: u64,
    leftover: LeftoverInventory,
) -> Result<(), DurableError> {
    archive_legacy_files(handle.paths())?;
    if matches!(config.get_wal_storage(), WalStorage::S3) {
        delete_legacy_s3_commits(config)?;
    }
    write_format(
        handle.catalog(),
        fence,
        FormatPhase::Clean,
        Some(row_version),
    )?;
    tracing::info!(
        leftover = leftover.summary(),
        "WAL catalog import resumed Imported -> Clean after archive"
    );
    Ok(())
}

fn import_owned_pairs(
    handle: &PipelineWalHandle,
    fence: CatalogFence,
    config: &Config,
) -> Result<u64, DurableError> {
    let mut count = import_disk_owned_pairs(handle, fence)?;
    if matches!(config.get_wal_storage(), WalStorage::S3) {
        count = count.saturating_add(import_s3_owned_pairs(handle, fence, config)?);
    }
    Ok(count)
}

fn owned_pair_segment_id(stem: &str, source: &str) -> Result<SegmentId, DurableError> {
    SegmentId::new(stem).map_err(|err| {
        DurableError::Io(format!("owned pair has invalid segment id {source}: {err}"))
    })
}

fn pair_s3_owned_keys(keys: &[String]) -> Vec<(String, String)> {
    let mut bodies = Vec::new();
    let commits: std::collections::HashSet<&str> = keys
        .iter()
        .filter(|key| key.ends_with(".seg.commit"))
        .map(String::as_str)
        .collect();
    for key in keys {
        if !key.ends_with(".seg") || key.ends_with(".seg.commit") {
            continue;
        }
        let commit = format!("{key}.commit");
        if commits.contains(commit.as_str()) {
            bodies.push((key.clone(), commit));
        }
    }
    bodies
}

fn list_wal_prefix(config: &Config) -> Result<Vec<String>, DurableError> {
    let bucket = config.get_wal_s3_bucket();
    let prefix = config.get_wal_s3_prefix();
    let listed = crate::buffer::wal_store::block_on_async(async {
        crate::helpers::s3::list_object_keys(&bucket, &prefix, S3_WAL_LIST_CEILING)
            .await
            .map_err(std::io::Error::other)
    })
    .map_err(|err| DurableError::Io(err.to_string()))?;
    if listed.len() >= S3_WAL_LIST_CEILING {
        return Err(DurableError::Io(
            "S3 WAL prefix list hit ceiling; import/gc fail-closed".into(),
        ));
    }
    Ok(listed)
}

fn import_s3_owned_pairs(
    handle: &PipelineWalHandle,
    fence: CatalogFence,
    config: &Config,
) -> Result<u64, DurableError> {
    let bucket = config.get_wal_s3_bucket();
    let listed = list_wal_prefix(config)?;
    let mut count = 0u64;
    let mut max_seq = handle.hydrate_seq()?;
    for (body_key, commit_key) in pair_s3_owned_keys(&listed) {
        let stem = body_key
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".seg"))
            .ok_or_else(|| {
                DurableError::Io(format!("owned pair has unparseable key {body_key}"))
            })?;
        let id = owned_pair_segment_id(stem, &body_key)?;
        if handle
            .catalog()
            .get(&CatalogKey::Segment(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
            .is_some()
        {
            continue;
        }
        let get_bucket = bucket.clone();
        let get_body = body_key.clone();
        let get_commit = commit_key.clone();
        let (body, commit) = crate::buffer::wal_store::block_on_async(async move {
            let body =
                crate::helpers::s3::get_object_bytes_for_bucket(&get_bucket, &get_body).await?;
            let commit =
                crate::helpers::s3::get_object_bytes_for_bucket(&get_bucket, &get_commit).await?;
            Ok((body, commit))
        })
        .map_err(|err| {
            DurableError::Io(format!("S3 owned pair GET failed for {body_key}: {err}"))
        })?;
        let meta = admit_owned_pair_bytes(&body, &commit).map_err(|err| {
            DurableError::Io(format!("S3 owned pair admit failed for {body_key}: {err}"))
        })?;
        max_seq = commit_imported_segment(
            handle,
            fence,
            id,
            PayloadLocation::S3 {
                bucket: bucket.clone(),
                key: body_key,
            },
            &meta,
            max_seq,
        )?;
        count += 1;
        if count % WAL_CATALOG_PROGRESS_EVERY == 0 {
            tracing::info!(segments = count, "WAL catalog import progress");
        }
    }
    Ok(count)
}

fn delete_legacy_s3_commits(config: &Config) -> Result<(), DurableError> {
    let bucket = config.get_wal_s3_bucket();
    let listed = list_wal_prefix(config)?;
    for key in listed {
        if !key.ends_with(".seg.commit") {
            continue;
        }
        let bucket = bucket.clone();
        let _ = crate::buffer::wal_store::block_on_async(async move {
            crate::helpers::s3::delete_object(&bucket, &key)
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))
        });
    }
    Ok(())
}

fn commit_imported_segment(
    handle: &PipelineWalHandle,
    fence: CatalogFence,
    id: SegmentId,
    location: PayloadLocation,
    meta: &SegmentFileMetadata,
    max_seq: skippr_wal_catalog::CatalogSeq,
) -> Result<skippr_wal_catalog::CatalogSeq, DurableError> {
    let max_seq = max_seq
        .next()
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let slice_count = meta.num_partitions.max(1);
    let row = SegmentRow {
        segment_id: id.clone(),
        seq: max_seq,
        location,
        payload_len: meta.total_bytes,
        payload_sha256: meta.body_sha256,
        slice_count,
        done: SliceBitmap::new(slice_count),
        offsets: Vec::new(),
        checkpoints: Vec::new(),
        epoch: handle.fence()?.epoch,
        version: 0,
    };
    let mut txn = CatalogTxn::new();
    txn.put(
        CatalogKey::Segment(id.clone()),
        CatalogRow::Segment(row),
        PutCond::NotExists,
    )
    .map_err(|err| DurableError::Io(err.to_string()))?;
    crate::buffer::wal_handle::put_slice_index(&mut txn, &id, meta)?;
    txn.put(
        CatalogKey::Seq,
        CatalogRow::Seq(SeqRow { max_seq }),
        skippr_wal_catalog::PutCond::SeqAdvance,
    )
    .map_err(|err| DurableError::Io(err.to_string()))?;
    handle
        .catalog()
        .commit_durable(&txn, fence)
        .map_err(|err| DurableError::Io(err.to_string()))?;
    Ok(max_seq)
}

fn import_disk_owned_pairs(
    handle: &PipelineWalHandle,
    fence: CatalogFence,
) -> Result<u64, DurableError> {
    let segs = &handle.paths().segs;
    if !segs.exists() {
        return Ok(0);
    }
    let mut count = 0u64;
    let mut max_seq = handle.hydrate_seq()?;
    for entry in fs::read_dir(segs).map_err(DurableError::from)? {
        let entry = entry.map_err(DurableError::from)?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("seg") {
            continue;
        }
        let commit_path = path.with_extension("seg.commit");
        if !commit_path.exists() {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
            DurableError::Io(format!("owned pair has invalid name {}", path.display()))
        })?;
        let id = owned_pair_segment_id(stem, &path.display().to_string())?;
        if handle
            .catalog()
            .get(&CatalogKey::Segment(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
            .is_some()
        {
            continue;
        }
        let meta = admit_owned_pair_path(&path).map_err(|err| {
            DurableError::Io(format!(
                "owned pair admit failed for {}: {err}",
                path.display()
            ))
        })?;
        max_seq = commit_imported_segment(
            handle,
            fence,
            id,
            PayloadLocation::Disk {
                path: path.to_string_lossy().into_owned(),
            },
            &meta,
            max_seq,
        )?;
        count += 1;
        if count % WAL_CATALOG_PROGRESS_EVERY == 0 {
            tracing::info!(segments = count, "WAL catalog import progress");
        }
    }
    Ok(count)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct LeftoverInventory {
    owned_pairs: u64,
    body_only_segs: u64,
    mutation_log_bytes: u64,
    snapshot_bytes: u64,
    cluster_format: bool,
    completion_bitmaps: u64,
    completion_other: u64,
    compaction_json: u64,
}

impl LeftoverInventory {
    fn summary(&self) -> String {
        format!(
            "owned_pairs={} body_only={} mutation_log_bytes={} snapshot_bytes={} cluster_format={} completion_bitmaps={} completion_other={} compaction_json={}",
            self.owned_pairs,
            self.body_only_segs,
            self.mutation_log_bytes,
            self.snapshot_bytes,
            self.cluster_format,
            self.completion_bitmaps,
            self.completion_other,
            self.compaction_json
        )
    }

    fn log(&self) {
        tracing::info!(
            owned_pairs = self.owned_pairs,
            body_only = self.body_only_segs,
            mutation_log_bytes = self.mutation_log_bytes,
            snapshot_bytes = self.snapshot_bytes,
            cluster_format = self.cluster_format,
            completion_bitmaps = self.completion_bitmaps,
            completion_other = self.completion_other,
            compaction_json = self.compaction_json,
            "WAL catalog import leftover inventory"
        );
    }
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn leftover_inventory(paths: &skippr_lease::PipelinePaths) -> LeftoverInventory {
    let mut leftover = LeftoverInventory::default();
    if paths.segs.exists() {
        if let Ok(dir) = fs::read_dir(&paths.segs) {
            for entry in dir.flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.ends_with(".seg") && !name.ends_with(".seg.commit") {
                    if path.with_extension("seg.commit").exists() {
                        leftover.owned_pairs += 1;
                    } else {
                        leftover.body_only_segs += 1;
                    }
                }
            }
        }
    }
    let mutation = paths.durable.join("mutation.log");
    if mutation.exists() {
        leftover.mutation_log_bytes = file_len(&mutation);
    }
    let snapshot = paths.snapshots.join("current");
    if snapshot.exists() {
        leftover.snapshot_bytes = file_len(&snapshot);
    }
    leftover.cluster_format = paths.durable.join("CLUSTER_FORMAT_V1").exists();
    if paths.completions.exists() {
        if let Ok(dir) = fs::read_dir(&paths.completions) {
            for entry in dir.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(".seg.completion-bitmap") {
                    leftover.completion_bitmaps += 1;
                } else {
                    leftover.completion_other += 1;
                }
            }
        }
    }
    if paths.compactions.exists() {
        if let Ok(dir) = fs::read_dir(&paths.compactions) {
            leftover.compaction_json = dir
                .flatten()
                .filter(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("json"))
                .count() as u64;
        }
    }
    leftover
}

fn archive_root(paths: &skippr_lease::PipelinePaths) -> PathBuf {
    paths.root.join("segment_buffer").join(ARCHIVE_DIR_NAME)
}

fn take_to_archive(src: &Path, dest: &Path) -> Result<bool, DurableError> {
    if !src.exists() {
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(DurableError::from)?;
    }
    let dest = if dest.exists() {
        let mut n = 2u32;
        loop {
            let candidate = dest.with_file_name(format!(
                "{}.{}",
                dest.file_name().and_then(|s| s.to_str()).unwrap_or("item"),
                n
            ));
            if !candidate.exists() {
                break candidate;
            }
            n = n.saturating_add(1);
            if n > 32 {
                return Err(DurableError::Io(format!(
                    "archive dest busy for {}",
                    dest.file_name().and_then(|s| s.to_str()).unwrap_or("item")
                )));
            }
        }
    } else {
        dest.to_path_buf()
    };
    fs::rename(src, &dest).map_err(DurableError::from)?;
    Ok(true)
}

fn archive_legacy_files(paths: &skippr_lease::PipelinePaths) -> Result<(), DurableError> {
    let root = archive_root(paths);
    fs::create_dir_all(&root).map_err(DurableError::from)?;
    let mut commits = 0u64;
    let moved_durable = take_to_archive(&paths.durable, &root.join("durable"))?;
    let moved_done = take_to_archive(&paths.completions, &root.join("done"))?;
    let moved_compactions = take_to_archive(&paths.compactions, &root.join("compactions"))?;
    let commit_dir = root.join("seg-commits");
    if paths.segs.exists() {
        for entry in fs::read_dir(&paths.segs).map_err(DurableError::from)? {
            let path = entry.map_err(DurableError::from)?.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.ends_with(".seg.commit") {
                if take_to_archive(&path, &commit_dir.join(name))? {
                    commits += 1;
                }
            }
        }
    }
    tracing::info!(
        archive = %root.display(),
        durable = moved_durable,
        completions = moved_done,
        compactions = moved_compactions,
        seg_commits = commits,
        "WAL catalog import archived leftover"
    );
    Ok(())
}

struct BitmapImport {
    applied: u64,
    stale: u64,
}

fn import_completion_bitmaps(
    handle: &PipelineWalHandle,
    fence: CatalogFence,
) -> Result<BitmapImport, DurableError> {
    let mut out = BitmapImport {
        applied: 0,
        stale: 0,
    };
    let dir = &handle.paths().completions;
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir).map_err(DurableError::from)? {
        let path = entry.map_err(DurableError::from)?.path();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let Some(stem) = name.strip_suffix(".seg.completion-bitmap") else {
            continue;
        };
        let id = match SegmentId::new(stem) {
            Ok(id) => id,
            Err(err) => {
                return Err(DurableError::Io(format!(
                    "completion bitmap has invalid segment id {stem}: {err}"
                )))
            }
        };
        match handle
            .catalog()
            .get(&CatalogKey::Segment(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
        {
            Some(CatalogRow::Segment(mut row)) => {
                let bytes = fs::read(&path).map_err(DurableError::from)?;
                let bits =
                    decode_completion_bitmap(&bytes, row.slice_count as usize).map_err(|err| {
                        DurableError::Io(format!("completion bitmap {stem} unreadable: {err}"))
                    })?;
                row.done.or_bits(&bits);
                let version = row.version;
                row.version = version.saturating_add(1);
                let mut txn = CatalogTxn::new();
                txn.put(
                    CatalogKey::Segment(id),
                    CatalogRow::Segment(row),
                    PutCond::Version(version),
                )
                .map_err(|err| DurableError::Io(err.to_string()))?;
                handle
                    .catalog()
                    .commit_durable(&txn, fence)
                    .map_err(|err| DurableError::Io(err.to_string()))?;
                out.applied += 1;
            }
            Some(_) => {
                return Err(DurableError::ProtocolMismatch(
                    "wal#seg is not a SegmentRow".into(),
                ))
            }
            None => {
                out.stale += 1;
            }
        }
        if (out.applied + out.stale) % WAL_CATALOG_PROGRESS_EVERY == 0 {
            tracing::info!(
                applied = out.applied,
                stale = out.stale,
                "WAL catalog import bitmap progress"
            );
        }
    }
    Ok(out)
}

fn decode_completion_bitmap(bytes: &[u8], expected_bits: usize) -> io::Result<Vec<u32>> {
    if bytes.len() < COMPLETION_HEADER_LEN + COMPLETION_CHECKSUM_LEN {
        return Err(invalid_data("completion bitmap is truncated"));
    }
    if &bytes[0..4] != COMPLETION_MAGIC {
        return Err(invalid_data("bad completion bitmap magic"));
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != COMPLETION_VERSION {
        return Err(invalid_data(format!(
            "unsupported completion bitmap version {version}"
        )));
    }
    let bit_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let byte_count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let expected_byte_count = bit_count.div_ceil(8);
    let expected_len = COMPLETION_HEADER_LEN
        .checked_add(byte_count)
        .and_then(|length| length.checked_add(COMPLETION_CHECKSUM_LEN))
        .ok_or_else(|| invalid_data("completion bitmap length overflow"))?;
    if bit_count != expected_bits
        || byte_count != expected_byte_count
        || bytes.len() != expected_len
    {
        return Err(invalid_data(format!(
            "completion bitmap shape mismatch bits={bit_count} expected={expected_bits}"
        )));
    }
    let checksum_offset = COMPLETION_HEADER_LEN + byte_count;
    let expected_checksum = Sha256::digest(&bytes[..checksum_offset]);
    if expected_checksum.as_slice() != &bytes[checksum_offset..] {
        return Err(invalid_data("completion bitmap checksum mismatch"));
    }
    let bits = &bytes[COMPLETION_HEADER_LEN..checksum_offset];
    let mut ordinals = Vec::new();
    for i in 0..bit_count {
        if bits[i / 8] & (1 << (i % 8)) != 0 {
            ordinals.push(i as u32);
        }
    }
    Ok(ordinals)
}

#[cfg(test)]
fn encode_completion_bitmap(bit_count: u32, set: &[u32]) -> Vec<u8> {
    let byte_count = bit_count.div_ceil(8) as usize;
    let mut bits = vec![0u8; byte_count];
    for &ord in set {
        if ord < bit_count {
            let i = ord as usize;
            bits[i / 8] |= 1 << (i % 8);
        }
    }
    let mut bytes =
        Vec::with_capacity(COMPLETION_HEADER_LEN + byte_count + COMPLETION_CHECKSUM_LEN);
    bytes.extend_from_slice(COMPLETION_MAGIC);
    bytes.extend_from_slice(&COMPLETION_VERSION.to_le_bytes());
    bytes.extend_from_slice(&bit_count.to_le_bytes());
    bytes.extend_from_slice(&(byte_count as u32).to_le_bytes());
    bytes.extend_from_slice(&bits);
    let checksum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    bytes
}

fn import_compaction_json(
    handle: &PipelineWalHandle,
    fence: CatalogFence,
) -> Result<u64, DurableError> {
    let dir = &handle.paths().compactions;
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0u64;
    for entry in fs::read_dir(dir).map_err(DurableError::from)? {
        let path = entry.map_err(DurableError::from)?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("compaction.json")
            .to_string();
        let bytes = fs::read(&path).map_err(DurableError::from)?;
        if bytes.len() > ROW_MAX_BYTES {
            return Err(DurableError::Io(format!(
                "compaction json {name} exceeds row ceiling ({} bytes)",
                bytes.len()
            )));
        }
        let txn: CompactionTransaction = serde_json::from_slice(&bytes).map_err(|err| {
            DurableError::ProtocolMismatch(format!("unreadable compaction json {name}: {err}"))
        })?;
        let id = CompactionId::new(&txn.id).map_err(|err| {
            DurableError::Io(format!("compaction json {name} has invalid id: {err}"))
        })?;
        if handle
            .catalog()
            .get(&CatalogKey::Compaction(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
            .is_some()
        {
            count += 1;
            continue;
        }
        let state = match txn.state {
            CompactionTransactionState::Pending => CompactionState::Pending,
            CompactionTransactionState::Sent => CompactionState::Sent,
            CompactionTransactionState::Acked => CompactionState::Acked,
        };
        let row = CompactionRow {
            id: id.clone(),
            state,
            body: bytes,
            version: 0,
        };
        let mut catalog_txn = CatalogTxn::new();
        catalog_txn
            .put(
                CatalogKey::Compaction(id),
                CatalogRow::Compaction(row),
                PutCond::NotExists,
            )
            .map_err(|err| DurableError::Io(err.to_string()))?;
        handle
            .catalog()
            .commit_durable(&catalog_txn, fence)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        count += 1;
        if count % WAL_CATALOG_PROGRESS_EVERY == 0 {
            tracing::info!(
                compactions = count,
                "WAL catalog import compaction progress"
            );
        }
    }
    Ok(count)
}

fn write_format(
    catalog: &dyn WalCatalogStore,
    fence: CatalogFence,
    phase: FormatPhase,
    expected: Option<u64>,
) -> Result<(), DurableError> {
    let mut txn = CatalogTxn::new();
    let (cond, row_version) = match expected {
        None => (PutCond::NotExists, 0),
        Some(current) => (PutCond::Version(current), current.saturating_add(1)),
    };
    txn.put(
        CatalogKey::Format,
        CatalogRow::Format(FormatRow {
            version: FORMAT_VERSION,
            phase,
            row_version,
        }),
        cond,
    )
    .map_err(|err| DurableError::Io(err.to_string()))?;
    catalog
        .commit_durable(&txn, fence)
        .map_err(|err| DurableError::Io(err.to_string()))
}

pub fn gc_orphan_payloads(handle: &PipelineWalHandle, config: &Config) -> Result<(), DurableError> {
    let rows = query_all(handle.catalog(), CatalogPrefix::Segment)
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let mut live_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut live_s3_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (_, row) in rows {
        let CatalogRow::Segment(seg) = row else {
            continue;
        };
        live_ids.insert(seg.segment_id.as_str().to_string());
        if let PayloadLocation::S3 { key, .. } = seg.location {
            live_s3_keys.insert(key);
        }
    }
    let segs = &handle.paths().segs;
    if segs.exists() {
        for entry in fs::read_dir(segs).map_err(DurableError::from)? {
            let path = entry.map_err(DurableError::from)?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("seg") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if !live_ids.contains(id) {
                let _ = fs::remove_file(path);
            }
        }
    }
    if matches!(config.get_wal_storage(), WalStorage::S3) {
        gc_orphan_s3_payloads(config, &live_s3_keys)?;
    }
    Ok(())
}

fn gc_orphan_s3_payloads(
    config: &Config,
    live_keys: &std::collections::HashSet<String>,
) -> Result<(), DurableError> {
    let bucket = config.get_wal_s3_bucket();
    let listed = list_wal_prefix(config)?;
    for key in listed {
        if !key.ends_with(".seg") || live_keys.contains(&key) {
            continue;
        }
        let bucket = bucket.clone();
        let _ = crate::buffer::wal_store::block_on_async(async move {
            crate::helpers::s3::delete_object(&bucket, &key)
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::wal_handle::memory_catalog_for_test;
    use crate::helpers::offset_store::SledOffsetStore;
    use skippr_lease::{LeaseGuard, PipelineKey, PipelinePaths, SystemClock};
    use std::sync::Arc;

    use crate::buffer::segment_file::{PartitionKey, WalPayload};
    use arrow::array::Int32Array;
    use arrow::record_batch::RecordBatch;
    use arrow_schema::{DataType, Field, Schema};
    use skippr_lease::SegmentId;
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::SystemTime;

    fn test_handle(dir: &Path) -> Arc<PipelineWalHandle> {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::legacy_disk(dir);
        let guard = LeaseGuard::single_node(key.clone(), Arc::new(SystemClock::new()));
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.join("offsets")).unwrap();
        let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
            Arc::new(SledOffsetStore::new(db.open_tree("o").unwrap()));
        PipelineWalHandle::new(key, paths, guard, catalog, offsets)
    }

    fn write_seg(segs: &Path, name: &str) -> SegmentFileMetadata {
        fs::create_dir_all(segs).unwrap();
        let seg = SegmentFile::new(segs, name).unwrap();
        let mut batches: HashMap<PartitionKey, WalPayload> = HashMap::new();
        let key = PartitionKey {
            sink_ref: "out".into(),
            namespace: "ns".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        let schema = Schema::new(vec![Field::new("v", DataType::Int32, false)]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
        )
        .unwrap();
        batches.insert(key.clone(), WalPayload::Arrow(vec![batch]));
        let mut parts_meta = HashMap::new();
        parts_meta.insert(key, (0, SystemTime::now()));
        let (meta, _rows, _sha) = seg
            .write_snapshot(&HashMap::new(), &batches, &parts_meta, &HashMap::new())
            .unwrap();
        meta
    }

    #[test]
    fn import_is_noop_when_format_clean() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(handle.guard()).unwrap();
        crate::buffer::wal_handle::ensure_format_clean(handle.catalog(), fence).unwrap();
        import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap();
    }

    #[test]
    fn import_skips_payload_without_commit_marker() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        write_seg(&handle.paths().segs, "orphan");
        import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap();
        let id = SegmentId::new("orphan").unwrap();
        assert!(handle
            .catalog()
            .get(&CatalogKey::Segment(id))
            .unwrap()
            .is_none());
    }

    #[test]
    fn import_admits_owned_pair() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        let meta = write_seg(&handle.paths().segs, "owned");
        let seg_path = handle.paths().segs.join("owned.seg");
        let commit =
            build_commit_header_bytes(meta.num_partitions, meta.total_bytes, &meta.body_sha256);
        fs::write(seg_path.with_extension("seg.commit"), commit).unwrap();
        import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap();
        let id = SegmentId::new("owned").unwrap();
        match handle.catalog().get(&CatalogKey::Segment(id)).unwrap() {
            Some(CatalogRow::Segment(row)) => {
                assert_eq!(row.payload_sha256, meta.body_sha256);
                assert_eq!(row.payload_len, meta.total_bytes);
            }
            other => panic!("expected segment row, got {other:?}"),
        }
        assert!(!seg_path.with_extension("seg.commit").exists());
        assert!(archive_root(handle.paths())
            .join("seg-commits/owned.seg.commit")
            .exists());
        assert!(seg_path.exists());
    }

    fn write_owned(handle: &PipelineWalHandle, name: &str) -> SegmentFileMetadata {
        let meta = write_seg(&handle.paths().segs, name);
        let seg_path = handle.paths().segs.join(format!("{name}.seg"));
        let commit =
            build_commit_header_bytes(meta.num_partitions, meta.total_bytes, &meta.body_sha256);
        fs::write(seg_path.with_extension("seg.commit"), commit).unwrap();
        meta
    }

    #[test]
    fn leftover_inventory_summary_is_counts_not_paths() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        fs::create_dir_all(&handle.paths().compactions).unwrap();
        for i in 0..200 {
            fs::write(
                handle.paths().compactions.join(format!("cmp-{i:03}.json")),
                b"{}",
            )
            .unwrap();
        }
        fs::create_dir_all(&handle.paths().durable).unwrap();
        fs::write(handle.paths().durable.join("mutation.log"), vec![0u8; 32]).unwrap();
        let summary = leftover_inventory(handle.paths()).summary();
        assert!(summary.contains("compaction_json=200"), "{summary}");
        assert!(summary.contains("mutation_log_bytes=32"), "{summary}");
        assert!(
            !summary.contains("cmp-000.json"),
            "inventory must not list paths: {summary}"
        );
        assert!(
            summary.len() < 400,
            "inventory summary was {}",
            summary.len()
        );
    }

    #[test]
    fn import_archives_mutation_log_after_owned_pair() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        write_owned(&handle, "owned");
        fs::create_dir_all(&handle.paths().durable).unwrap();
        fs::write(handle.paths().durable.join("mutation.log"), b"legacy").unwrap();
        fs::write(
            handle.paths().durable.join("CLUSTER_FORMAT_V1"),
            b"CLUSTER_FORMAT_V1",
        )
        .unwrap();
        import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap();
        let id = SegmentId::new("owned").unwrap();
        assert!(matches!(
            handle.catalog().get(&CatalogKey::Segment(id)).unwrap(),
            Some(CatalogRow::Segment(_))
        ));
        assert!(!handle.paths().durable.join("mutation.log").exists());
        assert!(archive_root(handle.paths())
            .join("durable/mutation.log")
            .exists());
        match handle.catalog().get(&CatalogKey::Format).unwrap() {
            Some(CatalogRow::Format(row)) => assert_eq!(row.phase, FormatPhase::Clean),
            other => panic!("expected Clean format, got {other:?}"),
        }
    }

    #[test]
    fn import_archives_snapshot_after_owned_pair() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        write_owned(&handle, "owned");
        fs::create_dir_all(&handle.paths().snapshots).unwrap();
        fs::write(handle.paths().snapshots.join("current"), b"snap").unwrap();
        import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap();
        assert!(!handle.paths().snapshots.join("current").exists());
        assert!(archive_root(handle.paths())
            .join("durable/snapshots/current")
            .exists());
    }

    #[test]
    fn import_applies_completion_bitmap() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        let meta = write_owned(&handle, "owned");
        fs::create_dir_all(&handle.paths().completions).unwrap();
        let bitmap = encode_completion_bitmap(meta.num_partitions.max(1), &[0]);
        fs::write(
            handle
                .paths()
                .completions
                .join("owned.seg.completion-bitmap"),
            bitmap,
        )
        .unwrap();
        import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap();
        let id = SegmentId::new("owned").unwrap();
        match handle.catalog().get(&CatalogKey::Segment(id)).unwrap() {
            Some(CatalogRow::Segment(row)) => assert!(row.done.is_done(0)),
            other => panic!("expected segment row, got {other:?}"),
        }
        assert!(!handle.paths().completions.exists());
        assert!(archive_root(handle.paths())
            .join("done/owned.seg.completion-bitmap")
            .exists());
    }

    #[test]
    fn import_compaction_json_becomes_catalog_row() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        write_owned(&handle, "owned");
        let txn = crate::buffer::compaction_transaction::CompactionTransaction::new(
            "out".into(),
            "ns".into(),
            "schema".into(),
            crate::plugins::source_contract::WritePolicy::Append,
            crate::buffer::compaction_transaction::SinkWriteSemantics::AtLeastOnce,
            Vec::new(),
            "out.parquet".into(),
        );
        fs::create_dir_all(&handle.paths().compactions).unwrap();
        fs::write(
            handle.paths().compactions.join(format!("{}.json", txn.id)),
            serde_json::to_vec(&txn).unwrap(),
        )
        .unwrap();
        import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap();
        let id = skippr_wal_catalog::CompactionId::new(&txn.id).unwrap();
        match handle.catalog().get(&CatalogKey::Compaction(id)).unwrap() {
            Some(CatalogRow::Compaction(row)) => {
                assert_eq!(row.state, skippr_wal_catalog::CompactionState::Pending);
            }
            other => panic!("expected compaction row, got {other:?}"),
        }
        assert!(!handle.paths().compactions.exists());
        assert!(archive_root(handle.paths())
            .join(format!("compactions/{}.json", txn.id))
            .exists());
    }

    #[test]
    fn import_unreadable_compaction_json_is_fail_closed_and_short() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        write_owned(&handle, "owned");
        fs::create_dir_all(&handle.paths().compactions).unwrap();
        for i in 0..80 {
            fs::write(
                handle.paths().compactions.join(format!("ok-{i:03}.json")),
                serde_json::to_vec(
                    &crate::buffer::compaction_transaction::CompactionTransaction::new(
                        "out".into(),
                        format!("ns-{i}"),
                        "schema".into(),
                        crate::plugins::source_contract::WritePolicy::Append,
                        crate::buffer::compaction_transaction::SinkWriteSemantics::AtLeastOnce,
                        Vec::new(),
                        format!("out-{i}.parquet"),
                    ),
                )
                .unwrap(),
            )
            .unwrap();
        }
        fs::write(handle.paths().compactions.join("bad.json"), b"{}").unwrap();
        let err =
            import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.contains("unreadable compaction json bad.json"),
            "{rendered}"
        );
        assert!(
            rendered.len() < 600,
            "compaction import error listed leftovers: {} bytes",
            rendered.len()
        );
        assert!(!rendered.contains("ok-000.json"), "{rendered}");
        assert!(handle.catalog().get(&CatalogKey::Format).unwrap().is_none());
        assert!(handle.paths().compactions.join("bad.json").exists());
    }

    #[test]
    fn import_resumes_imported_phase_to_clean() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(handle.guard()).unwrap();
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Format,
            CatalogRow::Format(FormatRow {
                version: FORMAT_VERSION,
                phase: FormatPhase::Imported,
                row_version: 0,
            }),
            PutCond::NotExists,
        )
        .unwrap();
        handle.catalog().commit(&txn, fence).unwrap();
        import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap();
        match handle.catalog().get(&CatalogKey::Format).unwrap() {
            Some(CatalogRow::Format(row)) => assert_eq!(row.phase, FormatPhase::Clean),
            other => panic!("expected Clean format, got {other:?}"),
        }
    }

    #[test]
    fn s3_list_pairs_owned_body_with_commit_and_skips_body_only() {
        let pairs = pair_s3_owned_keys(&[
            "wal/a.seg".into(),
            "wal/a.seg.commit".into(),
            "wal/b.seg".into(),
            "wal/c.seg.commit".into(),
        ]);
        assert_eq!(pairs, vec![("wal/a.seg".into(), "wal/a.seg.commit".into())]);
    }

    #[test]
    fn import_s3_admitted_pair_writes_s3_location() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        let meta = write_seg(&handle.paths().segs, "s3owned");
        let body = fs::read(handle.paths().segs.join("s3owned.seg")).unwrap();
        let commit =
            build_commit_header_bytes(meta.num_partitions, meta.total_bytes, &meta.body_sha256);
        let admitted = admit_owned_pair_bytes(&body, &commit).unwrap();
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(handle.guard()).unwrap();
        let id = SegmentId::new("s3owned").unwrap();
        commit_imported_segment(
            &handle,
            fence,
            id.clone(),
            PayloadLocation::S3 {
                bucket: "wal-bucket".into(),
                key: "wal/s3owned.seg".into(),
            },
            &admitted,
            skippr_wal_catalog::CatalogSeq::ZERO,
        )
        .unwrap();
        match handle.catalog().get(&CatalogKey::Segment(id)).unwrap() {
            Some(CatalogRow::Segment(row)) => {
                assert_eq!(
                    row.location,
                    PayloadLocation::S3 {
                        bucket: "wal-bucket".into(),
                        key: "wal/s3owned.seg".into(),
                    }
                );
                assert_eq!(row.payload_sha256, meta.body_sha256);
            }
            other => panic!("expected S3 segment row, got {other:?}"),
        }
    }

    #[test]
    fn import_catalog_commits_flush_before_legacy_delete() {
        let src = include_str!("wal_catalog_import.rs");
        assert!(
            !src.contains(concat!(".commit(&txn, fence)\n            .map_err")),
            "import must commit_durable before deleting .seg.commit files/objects"
        );
    }

    #[test]
    fn import_unreadable_owned_pair_is_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        write_seg(&handle.paths().segs, "bad");
        fs::write(
            handle.paths().segs.join("bad.seg.commit"),
            b"not-a-commit-header",
        )
        .unwrap();
        let err =
            import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap_err();
        assert!(err.to_string().contains("owned pair admit failed"), "{err}");
        assert!(handle.catalog().get(&CatalogKey::Format).unwrap().is_none());
    }

    #[test]
    fn import_owned_pair_with_invalid_segment_id_is_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle(dir.path());
        let meta = write_seg(&handle.paths().segs, "bad#id");
        let seg_path = handle.paths().segs.join("bad#id.seg");
        let commit =
            build_commit_header_bytes(meta.num_partitions, meta.total_bytes, &meta.body_sha256);
        fs::write(seg_path.with_extension("seg.commit"), commit).unwrap();
        let err =
            import_if_needed(&handle, &crate::helpers::configuration::Config::new()).unwrap_err();
        assert!(err.to_string().contains("invalid segment id"), "{err}");
        assert!(handle.catalog().get(&CatalogKey::Format).unwrap().is_none());
        assert!(
            seg_path.exists(),
            "must not Clean+GC an unimported owned pair"
        );
    }

    #[test]
    fn s3_import_does_not_skip_unreadable_owned_pairs() {
        let src = include_str!("wal_catalog_import.rs");
        assert!(!src.contains(concat!("skipped unreadable S3", " owned pair")));
        assert!(src.contains(concat!("S3 WAL prefix list hit ceil", "ing")));
        assert!(!src.contains(concat!("Err(_) => ", "continue")));
        assert!(src.contains("owned_pair_segment_id"));
    }
}
