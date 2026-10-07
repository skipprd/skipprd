//! Bounded Iceberg table history. A snapshot expires only when it is older
//! than [`SNAPSHOT_RETAIN_AGE_MS`], outside the newest
//! [`SNAPSHOT_RETAIN_LATEST`], written by this pipeline, and names no WAL
//! segment the host still holds. Retained snapshots carry the grouped-commit
//! idempotency gate and the live-WAL query dedupe set.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;

use iceberg::io::FileIO;
use iceberg::spec::{ManifestStatus, Snapshot, TableMetadata};
use skippr_runtime_sdk::plugins::LiveWalSegments;

pub(crate) const SNAPSHOT_RETAIN_AGE_MS: i64 = 24 * 60 * 60 * 1000;
pub(crate) const SNAPSHOT_RETAIN_LATEST: usize = 100;
/// Expiry commits are batched so steady-state history does not double the
/// commit rate.
pub(crate) const SNAPSHOT_EXPIRE_MIN_BATCH: usize = 16;
pub(crate) const SNAPSHOT_EXPIRE_MAX_PER_PASS: usize = 256;

pub(crate) const SNAPSHOT_PIPELINE: &str = "skippr.pipeline";
pub(crate) const SNAPSHOT_WAL_SEGMENT_IDS: &str = "skippr.wal-segment-ids";
pub(crate) const DELETE_AFTER_COMMIT: &str = "write.metadata.delete-after-commit.enabled";

const DELETED_DATA_FILES: &str = "deleted-data-files";
const REMOVED_DELETE_FILES: &str = "removed-delete-files";

pub(crate) struct ExpiryFence<'a> {
    pub pipeline: &'a str,
    pub live: &'a LiveWalSegments,
    pub now_ms: i64,
}

#[derive(Debug)]
struct SnapshotFacts<'a> {
    id: i64,
    sequence_number: i64,
    timestamp_ms: i64,
    referenced: bool,
    pipeline: Option<&'a str>,
    segment_ids: Vec<&'a str>,
    removes_files: bool,
}

impl<'a> SnapshotFacts<'a> {
    fn of(snapshot: &'a Snapshot, referenced: &HashSet<i64>) -> Self {
        let props = &snapshot.summary().additional_properties;
        let count = |key: &str| {
            props
                .get(key)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
        };
        Self {
            id: snapshot.snapshot_id(),
            sequence_number: snapshot.sequence_number(),
            timestamp_ms: snapshot.timestamp_ms(),
            referenced: referenced.contains(&snapshot.snapshot_id()),
            pipeline: props.get(SNAPSHOT_PIPELINE).map(String::as_str),
            segment_ids: props
                .get(SNAPSHOT_WAL_SEGMENT_IDS)
                .map(|ids| {
                    ids.split(',')
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            removes_files: count(DELETED_DATA_FILES) > 0 || count(REMOVED_DELETE_FILES) > 0,
        }
    }

    fn eligible(&self, fence: &ExpiryFence<'_>) -> bool {
        !self.referenced
            && self.timestamp_ms <= fence.now_ms.saturating_sub(SNAPSHOT_RETAIN_AGE_MS)
            && self.pipeline == Some(fence.pipeline)
            && !self.segment_ids.iter().any(|id| fence.live.contains(id))
    }
}

/// Snapshot ids to expire this pass, oldest first. Empty below
/// [`SNAPSHOT_EXPIRE_MIN_BATCH`].
pub(crate) fn expirable_snapshot_ids(
    metadata: &TableMetadata,
    fence: &ExpiryFence<'_>,
) -> Vec<i64> {
    let referenced: HashSet<i64> = metadata
        .referenced_snapshot_ids()
        .chain(metadata.current_snapshot_id())
        .collect();
    let mut facts: Vec<SnapshotFacts<'_>> = metadata
        .snapshots()
        .map(|snapshot| SnapshotFacts::of(snapshot, &referenced))
        .collect();
    select_expirable(&mut facts, fence)
}

fn select_expirable(facts: &mut [SnapshotFacts<'_>], fence: &ExpiryFence<'_>) -> Vec<i64> {
    facts.sort_by_key(|facts| facts.sequence_number);
    let candidates = facts.len().saturating_sub(SNAPSHOT_RETAIN_LATEST);
    let mut expired = Vec::new();
    let mut retained_older = false;
    for snapshot in &facts[..candidates] {
        // A snapshot that removed files expires only once nothing older is
        // kept, so the files it removed are referenced by no retained snapshot.
        let expire = expired.len() < SNAPSHOT_EXPIRE_MAX_PER_PASS
            && snapshot.eligible(fence)
            && !(snapshot.removes_files && retained_older);
        if expire {
            expired.push(snapshot.id);
        } else {
            retained_older = true;
        }
    }
    if expired.len() < SNAPSHOT_EXPIRE_MIN_BATCH {
        return Vec::new();
    }
    expired
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CleanupStats {
    pub manifest_lists: usize,
    pub manifests: usize,
    pub files: usize,
}

/// Deletes what only the expired snapshots referenced, given the metadata they
/// were expired from. History is linear on `main`, so a manifest lives on a
/// contiguous run of snapshots: it is unreferenced once neither retained
/// neighbour of an expired snapshot lists it.
pub(crate) async fn delete_expired_files(
    file_io: &FileIO,
    before: &TableMetadata,
    expired: &[i64],
) -> io::Result<CleanupStats> {
    let expired_ids: HashSet<i64> = expired.iter().copied().collect();
    let mut ordered: Vec<&Snapshot> = before.snapshots().map(AsRef::as_ref).collect();
    ordered.sort_by_key(|snapshot| snapshot.sequence_number());
    let mut lists: HashMap<i64, Vec<iceberg::spec::ManifestFile>> = HashMap::new();
    let mut manifests = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut manifest_lists = Vec::new();
    for (index, snapshot) in ordered.iter().enumerate() {
        if !expired_ids.contains(&snapshot.snapshot_id()) {
            continue;
        }
        let previous = ordered[..index]
            .iter()
            .rev()
            .find(|candidate| !expired_ids.contains(&candidate.snapshot_id()));
        let next = ordered[index + 1..]
            .iter()
            .find(|candidate| !expired_ids.contains(&candidate.snapshot_id()));
        let mut kept = HashSet::new();
        for neighbour in previous.into_iter().chain(next) {
            for manifest in manifest_list(file_io, before, neighbour, &mut lists).await? {
                kept.insert(manifest.manifest_path.clone());
            }
        }
        let own = manifest_list(file_io, before, snapshot, &mut lists)
            .await?
            .to_vec();
        let removes_files = SnapshotFacts::of(snapshot, &HashSet::new()).removes_files;
        for manifest in &own {
            if removes_files && manifest.added_snapshot_id == snapshot.snapshot_id() {
                let loaded = manifest
                    .load_manifest(file_io)
                    .await
                    .map_err(|err| io::Error::other(err.to_string()))?;
                for entry in loaded.entries() {
                    if entry.status() == ManifestStatus::Deleted {
                        files.insert(entry.file_path().to_string());
                    }
                }
            }
            if !kept.contains(&manifest.manifest_path) {
                manifests.insert(manifest.manifest_path.clone());
            }
        }
        manifest_lists.push(snapshot.manifest_list().to_string());
    }
    let stats = CleanupStats {
        manifest_lists: manifest_lists.len(),
        manifests: manifests.len(),
        files: files.len(),
    };
    for path in files.iter().chain(&manifests).chain(&manifest_lists) {
        file_io
            .delete(path)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
    }
    Ok(stats)
}

async fn manifest_list<'a>(
    file_io: &FileIO,
    metadata: &TableMetadata,
    snapshot: &Snapshot,
    lists: &'a mut HashMap<i64, Vec<iceberg::spec::ManifestFile>>,
) -> io::Result<&'a [iceberg::spec::ManifestFile]> {
    if !lists.contains_key(&snapshot.snapshot_id()) {
        let list = snapshot
            .load_manifest_list(file_io, metadata)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        lists.insert(snapshot.snapshot_id(), list.entries().to_vec());
    }
    Ok(&lists[&snapshot.snapshot_id()])
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: i64 = 60 * 60 * 1000;
    const NOW_MS: i64 = 1_000 * HOUR_MS;

    fn facts<'a>(sequence_number: i64, age_ms: i64, segment: &'a str) -> SnapshotFacts<'a> {
        SnapshotFacts {
            id: sequence_number,
            sequence_number,
            timestamp_ms: NOW_MS - age_ms,
            referenced: false,
            pipeline: Some("p"),
            segment_ids: vec![segment],
            removes_files: false,
        }
    }

    fn history(len: i64, age_ms: i64) -> Vec<SnapshotFacts<'static>> {
        (1..=len).map(|seq| facts(seq, age_ms, "seg-old")).collect()
    }

    fn select(facts: &mut [SnapshotFacts<'_>], live: &[&str]) -> Vec<i64> {
        let live = LiveWalSegments::new(live.iter().map(|id| id.to_string()));
        select_expirable(
            facts,
            &ExpiryFence {
                pipeline: "p",
                live: &live,
                now_ms: NOW_MS,
            },
        )
    }

    #[test]
    fn keeps_the_newest_hundred_even_when_old() {
        let mut snapshots = history(150, 48 * HOUR_MS);

        assert_eq!(select(&mut snapshots, &[]), (1..=50).collect::<Vec<_>>());
    }

    #[test]
    fn keeps_snapshots_younger_than_a_day() {
        let mut snapshots = history(150, 48 * HOUR_MS);
        for snapshot in &mut snapshots[20..] {
            snapshot.timestamp_ms = NOW_MS - 23 * HOUR_MS;
        }

        assert_eq!(select(&mut snapshots, &[]), (1..=20).collect::<Vec<_>>());
    }

    #[test]
    fn keeps_snapshots_naming_a_live_wal_segment() {
        let mut snapshots = history(150, 48 * HOUR_MS);
        snapshots[4].segment_ids = vec!["seg-a", "seg-live"];

        let expired = select(&mut snapshots, &["seg-live"]);

        assert!(!expired.contains(&5));
        assert_eq!(expired.len(), 49);
    }

    #[test]
    fn keeps_other_pipelines_and_unlabelled_snapshots() {
        let mut snapshots = history(150, 48 * HOUR_MS);
        snapshots[0].pipeline = Some("other");
        snapshots[1].pipeline = None;

        let expired = select(&mut snapshots, &[]);

        assert_eq!(expired, (3..=50).collect::<Vec<_>>());
    }

    #[test]
    fn keeps_referenced_snapshots() {
        let mut snapshots = history(150, 48 * HOUR_MS);
        snapshots[9].referenced = true;

        assert!(!select(&mut snapshots, &[]).contains(&10));
    }

    #[test]
    fn a_file_removing_snapshot_waits_for_everything_older() {
        let mut snapshots = history(150, 48 * HOUR_MS);
        snapshots[0].segment_ids = vec!["seg-live"];
        snapshots[30].removes_files = true;

        let expired = select(&mut snapshots, &["seg-live"]);

        assert!(!expired.contains(&1));
        assert!(!expired.contains(&31));
        assert_eq!(expired.len(), 48);
    }

    #[test]
    fn small_batches_wait() {
        let mut snapshots = history(100 + SNAPSHOT_EXPIRE_MIN_BATCH as i64 - 1, 48 * HOUR_MS);

        assert!(select(&mut snapshots, &[]).is_empty());
    }

    #[test]
    fn one_pass_is_bounded() {
        let mut snapshots = history(100 + 2 * SNAPSHOT_EXPIRE_MAX_PER_PASS as i64, 48 * HOUR_MS);

        assert_eq!(
            select(&mut snapshots, &[]).len(),
            SNAPSHOT_EXPIRE_MAX_PER_PASS
        );
    }
}
