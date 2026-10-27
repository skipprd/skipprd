//! Query-local Iceberg∪WAL read boundary. Pin at plan time; reuse at execute.
//!
//! `(S, E, W)` is a query-local ordinal list, not a reclaim lease. Slice-done
//! bits do not hide an ordinal until its `(segment_id, start)` is in Iceberg
//! `E` or the `SegmentRow` is reclaimed. Missing payloads fail at execute.

use std::collections::HashSet;

use skippr_lease::{DurableError, PipelineKey};

use crate::buffer::wal_reader::{wal_reader_for, PinnedSlice};
use crate::wal_slice_ref::WalSliceRef;

/// Iceberg snapshot captured with the WAL pin. `Empty` means no current snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LakeSnapshot {
    Empty,
    At(i64),
}

/// Pinned WAL side of a query-local `(S, E, W)`.
#[derive(Clone, Debug, PartialEq)]
pub enum PinnedWal {
    Local(Vec<PinnedSlice>),
    Clustered(Vec<skippr_query_plan::PinnedSliceRef>),
}

/// Pinned Iceberg snapshot plus live WAL slices for one table scan.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadBoundary {
    pub lake: LakeSnapshot,
    pub wal: PinnedWal,
    pub excluded: usize,
}

/// `W \ E`. Local pin and clustered ticket both subtract through this.
pub fn minus_excluded<T>(
    live: Vec<T>,
    excluded: &HashSet<WalSliceRef>,
    to_ref: impl Fn(&T) -> WalSliceRef,
) -> Vec<T> {
    live.into_iter()
        .filter(|item| !excluded.contains(&to_ref(item)))
        .collect()
}

impl ReadBoundary {
    pub fn new(
        lake: LakeSnapshot,
        live: Vec<PinnedSlice>,
        excluded: &HashSet<WalSliceRef>,
    ) -> Self {
        let live = minus_excluded(live, excluded, |slice| slice.slice_ref());
        Self {
            lake,
            wal: PinnedWal::Local(live),
            excluded: excluded.len(),
        }
    }

    pub fn for_clustered_ticket(
        lake: LakeSnapshot,
        live: Vec<skippr_query_plan::PinnedSliceRef>,
        excluded: &HashSet<WalSliceRef>,
    ) -> Result<Self, crate::wal_slice_ref::WalSliceRefError> {
        let tagged = live
            .into_iter()
            .map(|slice| {
                WalSliceRef::try_from_parts(&slice.segment_id, slice.start).map(|r| (r, slice))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let slices = minus_excluded(tagged, excluded, |(r, _)| r.clone())
            .into_iter()
            .map(|(_, slice)| slice)
            .collect();
        Ok(Self {
            lake,
            wal: PinnedWal::Clustered(slices),
            excluded: excluded.len(),
        })
    }

    pub fn pin_wal(
        pipeline: &PipelineKey,
        namespace: &str,
        exclude_slices: &HashSet<WalSliceRef>,
    ) -> Result<Self, DurableError> {
        let reader = wal_reader_for(pipeline).ok_or_else(|| {
            DurableError::Io(format!(
                "WAL catalog for pipeline {} is not open",
                pipeline.pipeline()
            ))
        })?;
        Ok(Self::new(
            LakeSnapshot::Empty,
            reader.live_slices(namespace)?,
            exclude_slices,
        ))
    }

    pub fn query_pin_fields(&self, namespace: &str) -> serde_json::Value {
        let snapshot = match self.lake {
            LakeSnapshot::Empty => serde_json::Value::Null,
            LakeSnapshot::At(id) => serde_json::json!(id),
        };
        let wal_slices = match &self.wal {
            PinnedWal::Local(slices) => slices.len(),
            PinnedWal::Clustered(slices) => slices.len(),
        };
        serde_json::json!({
            "namespace": namespace,
            "snapshot": snapshot,
            "wal_slices": wal_slices,
            "excluded_slices": self.excluded,
        })
    }

    /// One `query_pin` PERF event per frozen `(S, E, W)`; `wal_slices` is `|W \ E|`.
    pub fn emit_query_pin(&self, namespace: &str) {
        crate::helpers::perf::emit("query_pin", self.query_pin_fields(namespace));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::{PartitionKey, WalPayload};
    use crate::buffer::wal_handle::{
        install_wal_handle, memory_catalog_for_test, remove_wal_handle, PipelineWalHandle,
    };
    use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
    use crate::wal_slice_ref::WalSliceRef;
    use arrow::array::{RecordBatch, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    async fn persist(handle: &PipelineWalHandle, segment_id: &str, id: &str) {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let mut batches = HashMap::new();
        batches.insert(
            PartitionKey {
                sink_ref: "data_sinks.iceberg".into(),
                namespace: "ns".into(),
                partition: "p0".into(),
                time: None,
                schema_fingerprint: "fp".into(),
            },
            WalPayload::Arrow(vec![RecordBatch::try_new(
                schema,
                vec![Arc::new(StringArray::from(vec![id]))],
            )
            .unwrap()]),
        );
        handle
            .persist_segment(
                segment_id,
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
    }

    fn live_ids(boundary: &ReadBoundary) -> Vec<(String, u32)> {
        match &boundary.wal {
            PinnedWal::Local(live) => live
                .iter()
                .map(|slice| (slice.segment_id.as_str().to_string(), slice.entry.ordinal))
                .collect(),
            PinnedWal::Clustered(_) => panic!("expected local pin"),
        }
    }

    fn exclude_segment(handle: &PipelineWalHandle, segment_id: &str) -> HashSet<WalSliceRef> {
        handle
            .reader()
            .live_slices("ns")
            .unwrap()
            .into_iter()
            .filter(|slice| slice.segment_id.as_str() == segment_id)
            .map(|slice| slice.slice_ref())
            .collect()
    }

    #[tokio::test]
    async fn pin_ignores_later_compact_bits_and_new_segments() {
        let key = PipelineKey::new("t", "w", "rb-pin-later").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn OffsetStore> = Arc::new(SledOffsetStore::new(tree));
        let handle = PipelineWalHandle::new(key.clone(), paths, guard, catalog, offsets);
        install_wal_handle(handle.clone());
        persist(&handle, "live-seg", "a").await;
        persist(&handle, "gone-seg", "b").await;
        let pinned =
            ReadBoundary::pin_wal(&key, "ns", &exclude_segment(&handle, "gone-seg")).unwrap();
        assert_eq!(live_ids(&pinned), vec![("live-seg".to_string(), 0)]);
        handle
            .complete_slices_and_reclaim("live-seg", &[0])
            .await
            .unwrap();
        persist(&handle, "after-pin", "c").await;
        assert_eq!(
            live_ids(&pinned),
            vec![("live-seg".to_string(), 0)],
            "plan-time pin must not re-select after compact bits or new segments"
        );
        let later =
            ReadBoundary::pin_wal(&key, "ns", &exclude_segment(&handle, "gone-seg")).unwrap();
        assert_eq!(live_ids(&later), vec![("after-pin".to_string(), 0)]);
        remove_wal_handle(&key);
    }

    #[tokio::test]
    async fn exclude_set_hides_segment_not_slice_done_bits() {
        let key = PipelineKey::new("t", "w", "rb-exclude").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn OffsetStore> = Arc::new(SledOffsetStore::new(tree));
        let handle = PipelineWalHandle::new(key.clone(), paths, guard, catalog, offsets);
        install_wal_handle(handle.clone());
        persist(&handle, "live-seg", "a").await;
        persist(&handle, "named-seg", "b").await;
        let pinned =
            ReadBoundary::pin_wal(&key, "ns", &exclude_segment(&handle, "named-seg")).unwrap();
        assert_eq!(live_ids(&pinned), vec![("live-seg".to_string(), 0)]);
        assert!(!live_ids(&pinned).iter().any(|(id, _)| id == "named-seg"));
        remove_wal_handle(&key);
    }

    #[tokio::test]
    async fn slice_done_ordinal_stays_in_pin_until_exclude() {
        let key = PipelineKey::new("t", "w", "rb-slice-done").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn OffsetStore> = Arc::new(SledOffsetStore::new(tree));
        let handle = PipelineWalHandle::new(key.clone(), paths, guard, catalog, offsets);
        install_wal_handle(handle.clone());
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let mut batches = HashMap::new();
        for (i, id) in ["a", "b"].iter().enumerate() {
            batches.insert(
                PartitionKey {
                    sink_ref: "data_sinks.iceberg".into(),
                    namespace: "ns".into(),
                    partition: format!("p{i}"),
                    time: None,
                    schema_fingerprint: "fp".into(),
                },
                WalPayload::Arrow(vec![RecordBatch::try_new(
                    schema.clone(),
                    vec![Arc::new(StringArray::from(vec![*id]))],
                )
                .unwrap()]),
            );
        }
        handle
            .persist_segment(
                "live-seg",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        handle
            .complete_slices_and_reclaim("live-seg", &[0])
            .await
            .unwrap();
        let pinned = ReadBoundary::pin_wal(&key, "ns", &HashSet::new()).unwrap();
        let ids = live_ids(&pinned);
        assert!(ids.contains(&("live-seg".to_string(), 0)));
        assert!(ids.contains(&("live-seg".to_string(), 1)));
        assert!(live_ids(
            &ReadBoundary::pin_wal(&key, "ns", &exclude_segment(&handle, "live-seg")).unwrap()
        )
        .is_empty());
        remove_wal_handle(&key);
    }

    #[tokio::test]
    async fn exclude_one_slice_keeps_other_slice_of_same_segment() {
        let key = PipelineKey::new("t", "w", "rb-slice-exclude").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn OffsetStore> = Arc::new(SledOffsetStore::new(tree));
        let handle = PipelineWalHandle::new(key.clone(), paths, guard, catalog, offsets);
        install_wal_handle(handle.clone());
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let mut batches = HashMap::new();
        for (i, id) in ["a", "b"].iter().enumerate() {
            batches.insert(
                PartitionKey {
                    sink_ref: "data_sinks.iceberg".into(),
                    namespace: "ns".into(),
                    partition: format!("p{i}"),
                    time: None,
                    schema_fingerprint: "fp".into(),
                },
                WalPayload::Arrow(vec![RecordBatch::try_new(
                    schema.clone(),
                    vec![Arc::new(StringArray::from(vec![*id]))],
                )
                .unwrap()]),
            );
        }
        handle
            .persist_segment(
                "seg-part",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        let slices = handle.reader().live_slices("ns").unwrap();
        let p0 = slices
            .iter()
            .find(|slice| slice.entry.partition == "p0")
            .unwrap();
        let p1 = slices
            .iter()
            .find(|slice| slice.entry.partition == "p1")
            .unwrap();
        let exclude = HashSet::from([p0.slice_ref()]);
        let pinned = ReadBoundary::pin_wal(&key, "ns", &exclude).unwrap();
        assert_eq!(
            live_ids(&pinned),
            vec![(p1.segment_id.as_str().to_string(), p1.entry.ordinal)]
        );
        remove_wal_handle(&key);
    }

    #[test]
    fn pin_wal_fails_when_catalog_is_not_open() {
        let key = PipelineKey::new("t", "w", "rb-missing-reader").unwrap();
        let err = ReadBoundary::pin_wal(&key, "ns", &HashSet::new()).unwrap_err();
        assert!(
            matches!(err, DurableError::Io(ref msg) if msg.contains("is not open")),
            "got {err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn pin_wal_uses_query_reader_without_writer_handle() {
        use crate::buffer::wal_handle::wal_handle_for;
        use crate::buffer::wal_reader::{
            clear_query_readers, install_query_reader, reader_with_live_slice,
        };

        let key = PipelineKey::new("t", "w", "query-only-pin").unwrap();
        remove_wal_handle(&key);
        clear_query_readers();
        install_query_reader(reader_with_live_slice(key.clone(), "ns", "live-seg")).unwrap();
        assert!(
            wal_handle_for(&key).is_none(),
            "query pin must not require a writer handle"
        );
        let pinned = ReadBoundary::pin_wal(&key, "ns", &HashSet::new()).unwrap();
        assert_eq!(live_ids(&pinned), vec![("live-seg".to_string(), 0)]);
        clear_query_readers();
    }

    #[test]
    fn clustered_ticket_subtracts_e_through_read_boundary() {
        let keep = skippr_query_plan::PinnedSliceRef {
            segment_id: "seg-keep".into(),
            start: 0,
            seq: 1,
            ordinal: 0,
        };
        let drop = skippr_query_plan::PinnedSliceRef {
            segment_id: "seg-drop".into(),
            start: 10,
            seq: 2,
            ordinal: 1,
        };
        let excluded = HashSet::from([WalSliceRef::try_from_parts("seg-drop", 10).unwrap()]);
        let boundary = ReadBoundary::for_clustered_ticket(
            LakeSnapshot::At(7),
            vec![keep.clone(), drop],
            &excluded,
        )
        .unwrap();
        assert_eq!(boundary.lake, LakeSnapshot::At(7));
        assert_eq!(boundary.excluded, 1);
        match boundary.wal {
            PinnedWal::Clustered(slices) => assert_eq!(slices, vec![keep]),
            PinnedWal::Local(_) => panic!("clustered pin must live in ReadBoundary::wal"),
        }
    }

    #[test]
    fn query_pin_fields_report_snapshot_and_w_minus_e() {
        let keep = skippr_query_plan::PinnedSliceRef {
            segment_id: "seg-keep".into(),
            start: 0,
            seq: 1,
            ordinal: 0,
        };
        let excluded = HashSet::from([WalSliceRef::try_from_parts("seg-drop", 10).unwrap()]);
        let boundary =
            ReadBoundary::for_clustered_ticket(LakeSnapshot::At(7), vec![keep], &excluded).unwrap();
        let fields = boundary.query_pin_fields("ns");
        assert_eq!(fields["namespace"], "ns");
        assert_eq!(fields["snapshot"], 7);
        assert_eq!(fields["wal_slices"], 1);
        assert_eq!(fields["excluded_slices"], 1);

        let empty = ReadBoundary::new(LakeSnapshot::Empty, Vec::new(), &HashSet::new());
        let fields = empty.query_pin_fields("ns");
        assert!(fields["snapshot"].is_null());
        assert_eq!(fields["wal_slices"], 0);
    }

    #[test]
    fn every_scan_pin_emits_query_pin() {
        let tables = include_str!("tables.rs");
        let tables = tables.split("#[cfg(test)]").next().unwrap();
        assert_eq!(tables.matches(".emit_query_pin(").count(), 2);
        let wal = include_str!("wal_table.rs");
        let wal = wal.split("#[cfg(test)]").next().unwrap();
        assert_eq!(wal.matches(".emit_query_pin(").count(), 1);
    }
}
