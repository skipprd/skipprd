//! LIST-based S3 WAL recovery. Only bound body+commit pairs rebuild offsets.
//! Body-only and commit-only keys stay unowned. Invalid listed pairs fail closed.

use std::collections::HashSet;
use std::io;

use skippr_lease::LeaseGuard;

use crate::buffer::segment_file::{SegmentFile, SegmentFileMetadata};
use crate::buffer::wal_object_store::{GetOutcome, ListOutcome, WalObjectStore};
use crate::helpers::offsets::{OffsetTypes, Offsets};

pub struct AdmittedPair {
    pub body_key: String,
    pub meta: SegmentFileMetadata,
}

pub struct RecoveredWal {
    pub processed: u64,
    pub committed_offsets: u64,
    pub namespaces: HashSet<String>,
    pub bytes_total: u64,
    pub listed_segments: u64,
    pub pairs: Vec<AdmittedPair>,
}

pub async fn recover_owned_pairs(
    store: &dyn WalObjectStore,
    prefix: &str,
    offsets_db: &Offsets,
    lease: &LeaseGuard,
) -> io::Result<RecoveredWal> {
    let epoch = lease
        .require_offset_epoch()
        .map_err(|err| io::Error::other(format!("s3 wal recover fenced: {err}")))?;
    let keys = match store.list(prefix).await {
        ListOutcome::Keys(keys) => keys,
        ListOutcome::Failed(message) => {
            return Err(io::Error::other(format!(
                "s3 wal recover list failed: {message}"
            )));
        }
    };

    let mut segs: HashSet<String> = HashSet::new();
    let mut commits: HashSet<String> = HashSet::new();
    for key in keys {
        if key.ends_with(".seg") {
            segs.insert(key);
        } else if let Some(seg) = key.strip_suffix(".commit") {
            if seg.ends_with(".seg") {
                commits.insert(seg.to_string());
            }
        }
    }

    let mut owned: Vec<String> = segs
        .iter()
        .filter(|seg_key| commits.contains(*seg_key))
        .cloned()
        .collect();
    owned.sort();

    let mut processed = 0u64;
    let mut committed_offsets = 0u64;
    let mut namespaces = HashSet::new();
    let mut bytes_total = 0u64;
    let mut pairs = Vec::new();

    for seg_key in owned {
        let commit_key = format!("{seg_key}.commit");
        let commit_bytes = match store.get(&commit_key).await {
            GetOutcome::Found(bytes) => bytes,
            GetOutcome::Missing => continue,
            GetOutcome::Unknown => {
                return Err(io::Error::other(format!(
                    "s3 wal recover commit GET unknown: {commit_key}"
                )));
            }
        };
        let body_bytes = match store.get(&seg_key).await {
            GetOutcome::Found(bytes) => bytes,
            GetOutcome::Missing => continue,
            GetOutcome::Unknown => {
                return Err(io::Error::other(format!(
                    "s3 wal recover body GET unknown: {seg_key}"
                )));
            }
        };
        let meta =
            SegmentFile::admit_owned_pair_bytes(&body_bytes, &commit_bytes).map_err(|err| {
                io::Error::other(format!(
                    "s3 wal recover refusing invalid listed pair {seg_key}: {err}"
                ))
            })?;
        lease
            .require_same_offset_epoch(epoch)
            .map_err(|err| io::Error::other(format!("s3 wal recover fenced: {err}")))?;
        for idx in meta.index.iter() {
            namespaces.insert(idx.key.namespace.clone());
        }
        mark_offsets_durable(offsets_db, meta.offsets.iter())?;
        lease
            .require_same_offset_epoch(epoch)
            .map_err(|err| io::Error::other(format!("s3 wal recover fenced: {err}")))?;
        committed_offsets = committed_offsets.saturating_add(meta.offsets.len() as u64);
        bytes_total = bytes_total.saturating_add(meta.total_bytes);
        processed = processed.saturating_add(1);
        pairs.push(AdmittedPair {
            body_key: seg_key.clone(),
            meta,
        });
    }

    Ok(RecoveredWal {
        processed,
        committed_offsets,
        namespaces,
        bytes_total,
        listed_segments: segs.len() as u64,
        pairs,
    })
}

fn mark_offsets_durable<'a, I>(offsets_db: &Offsets, offsets: I) -> io::Result<()>
where
    I: IntoIterator<Item = (&'a crate::helpers::offsets::OffsetKey, &'a u64)>,
{
    for (offset, position) in offsets {
        let key = crate::helpers::offsets::OffsetKey {
            namespace: offset.namespace.clone(),
            partition: offset.partition.clone(),
        };
        offsets_db
            .insert(&key, OffsetTypes::Closed, 1)
            .map_err(|err| io::Error::other(format!("offset closed publish failed: {err}")))?;
        offsets_db
            .insert(&key, OffsetTypes::Position, *position)
            .map_err(|err| io::Error::other(format!("offset position publish failed: {err}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::{encode_snapshot, PartitionKey};
    use crate::buffer::wal_object_store::{MemoryObjectStore, ScriptedGet, ScriptedObjectStore};
    use crate::helpers::offsets::{OffsetKey, OffsetValue};
    use arrow::array::Int32Array;
    use arrow::array::RecordBatch;
    use arrow_schema::{DataType, Field, Schema};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::SystemTime;
    use zerocopy::LayoutVerified;

    fn batch() -> RecordBatch {
        let schema = Schema::new(vec![Field::new("v", DataType::Int32, false)]);
        let arr = Int32Array::from(vec![1, 2, 3]);
        RecordBatch::try_new(Arc::new(schema), vec![Arc::new(arr)]).unwrap()
    }

    fn encoded_pair() -> (Vec<u8>, Vec<u8>) {
        encoded_pair_at(11)
    }

    fn encoded_pair_at(position: u64) -> (Vec<u8>, Vec<u8>) {
        let key = PartitionKey {
            sink_ref: "sink".into(),
            namespace: "ns".into(),
            partition: "p".into(),
            time: Some(0),
            schema_fingerprint: "fp".into(),
        };
        let mut batches = HashMap::new();
        batches.insert(key.clone(), vec![batch()]);
        let mut meta = HashMap::new();
        meta.insert(key, (12, SystemTime::now()));
        let mut offsets = HashMap::new();
        offsets.insert(OffsetKey::new("ns", "src-a"), position);
        let encoded = encode_snapshot(&offsets, &batches, &meta, &HashMap::new()).unwrap();
        let commit = SegmentFile::build_commit_header_bytes(
            encoded.meta.num_partitions,
            encoded.meta.total_bytes,
            &encoded.sha256,
        )
        .to_vec();
        (encoded.bytes, commit)
    }

    use skippr_lease::{LeaseGuard, PipelineKey, SystemClock};

    fn test_guard() -> Arc<LeaseGuard> {
        let key = PipelineKey::new("t", "w", "p").expect("pipeline key");
        LeaseGuard::single_node(key, Arc::new(SystemClock::new()))
    }

    async fn recover(
        store: &dyn crate::buffer::wal_object_store::WalObjectStore,
        prefix: &str,
        offsets: &Offsets,
    ) -> Result<RecoveredWal, std::io::Error> {
        let guard = test_guard();
        recover_owned_pairs(store, prefix, offsets, guard.as_ref()).await
    }

    fn recover_err(result: Result<RecoveredWal, std::io::Error>) -> std::io::Error {
        match result {
            Ok(_) => panic!("expected recovery error"),
            Err(err) => err,
        }
    }

    fn test_offsets() -> Offsets {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        Offsets::from_local_db_for_test(db)
    }

    fn closed_value(offsets: &Offsets, key: &OffsetKey) -> Option<u64> {
        let bytes = offsets.get(key).ok().flatten()?;
        let mut backing = bytes.to_vec();
        let layout: LayoutVerified<&mut [u8], OffsetValue> =
            LayoutVerified::new_unaligned(backing.as_mut_slice())?;
        Some(layout.into_ref().closed.get())
    }

    fn position_value(offsets: &Offsets, key: &OffsetKey) -> Option<u64> {
        let bytes = offsets.get(key).ok().flatten()?;
        let mut backing = bytes.to_vec();
        let layout: LayoutVerified<&mut [u8], OffsetValue> =
            LayoutVerified::new_unaligned(backing.as_mut_slice())?;
        Some(layout.into_ref().line.get())
    }

    #[tokio::test]
    async fn list_failure_fails_closed() {
        let store = ScriptedObjectStore::new();
        store.fail_list("list timeout");
        let offsets = test_offsets();
        let err = recover_err(recover(&store, "", &offsets).await);
        assert!(err.to_string().contains("list failed"));
    }

    #[tokio::test]
    async fn body_only_and_commit_only_stay_unowned() {
        let store = MemoryObjectStore::new();
        let (body, commit) = encoded_pair();
        store.put("orphan.seg", body).await;
        store.put("ghost.seg.commit", commit).await;
        let offsets = test_offsets();
        let recovered = recover(&store, "", &offsets).await.unwrap();
        assert_eq!(recovered.processed, 0);
        assert_eq!(closed_value(&offsets, &OffsetKey::new("ns", "src-a")), None);
    }

    #[tokio::test]
    async fn bound_pair_publishes_offsets() {
        let store = MemoryObjectStore::new();
        let (body, commit) = encoded_pair();
        store.put("owned.seg", body).await;
        store.put("owned.seg.commit", commit).await;
        let offsets = test_offsets();
        let recovered = recover(&store, "", &offsets).await.unwrap();
        assert_eq!(recovered.processed, 1);
        assert_eq!(
            closed_value(&offsets, &OffsetKey::new("ns", "src-a")),
            Some(1)
        );
    }

    #[tokio::test]
    async fn later_segment_offsets_max_merge_and_do_not_regress() {
        let store = MemoryObjectStore::new();
        let (older_body, older_commit) = encoded_pair_at(1);
        let (newer_body, newer_commit) = encoded_pair_at(9);
        store.put("z-older.seg", older_body).await;
        store.put("z-older.seg.commit", older_commit).await;
        store.put("a-newer.seg", newer_body).await;
        store.put("a-newer.seg.commit", newer_commit).await;
        let offsets = test_offsets();
        let recovered = recover(&store, "", &offsets).await.unwrap();
        assert_eq!(recovered.processed, 2);
        assert_eq!(
            position_value(&offsets, &OffsetKey::new("ns", "src-a")),
            Some(9)
        );
    }

    #[tokio::test]
    async fn invalid_listed_pair_fails_closed() {
        let store = MemoryObjectStore::new();
        let (body, _) = encoded_pair();
        store.put("bad.seg", body).await;
        store.put("bad.seg.commit", vec![0u8; 60]).await;
        let offsets = test_offsets();
        let err = recover_err(recover(&store, "", &offsets).await);
        assert!(err.to_string().contains("invalid listed pair"));
    }

    #[tokio::test]
    async fn get_unknown_fails_closed() {
        let store = ScriptedObjectStore::new();
        let (body, commit) = encoded_pair();
        store.put("g.seg", body).await;
        store.put("g.seg.commit", commit).await;
        store.script_get("g.seg", ScriptedGet::AlwaysFail);
        let offsets = test_offsets();
        let err = recover_err(recover(&store, "", &offsets).await);
        assert!(err.to_string().contains("GET unknown"));
    }

    #[tokio::test]
    async fn sparse_and_unrelated_keys_are_discovered_from_list() {
        let store = MemoryObjectStore::new();
        let (body, commit) = encoded_pair();
        store.put("noise.txt", b"nope".to_vec()).await;
        store.put("p/aabbccddeeff0011.seg", body).await;
        store.put("p/aabbccddeeff0011.seg.commit", commit).await;
        let offsets = test_offsets();
        let recovered = recover(&store, "p/", &offsets).await.unwrap();
        assert_eq!(recovered.processed, 1);
        assert_eq!(recovered.listed_segments, 1);
    }
}
