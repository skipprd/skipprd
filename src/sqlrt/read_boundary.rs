//! Query-local Iceberg∪WAL read boundary. Pin at plan time; reuse at execute.
//!
//! `(S, E, W)` is a query-local ordinal list, not a reclaim lease. Slice-done
//! bits do not hide an ordinal until its segment id is in Iceberg `E` or the
//! `SegmentRow` is reclaimed. Missing payloads skip that ordinal.

use skippr_lease::PipelineKey;

use crate::query_flight::live_wal::select_live_ordinals_for;

/// Pinned Iceberg snapshot plus live WAL ordinals for one table scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadBoundary {
    pub snapshot_id: Option<i64>,
    pub exclude_segment_ids: Vec<String>,
    pub wal_ordinals: Vec<(String, u32)>,
}

impl ReadBoundary {
    pub fn pin_wal(pipeline: &PipelineKey, exclude_segment_ids: &[String]) -> Self {
        Self {
            snapshot_id: None,
            exclude_segment_ids: exclude_segment_ids.to_vec(),
            wal_ordinals: select_live_ordinals_for(pipeline, exclude_segment_ids),
        }
    }

    pub fn with_snapshot(mut self, snapshot_id: i64) -> Self {
        self.snapshot_id = Some(snapshot_id);
        self
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
    use arrow::array::{RecordBatch, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
    use std::collections::HashMap;
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
            )
            .await
            .unwrap();
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
        let pinned = ReadBoundary::pin_wal(&key, &["gone-seg".into()]);
        assert_eq!(pinned.wal_ordinals, vec![("live-seg".to_string(), 0)]);
        handle
            .complete_slices_and_reclaim("live-seg", &[0])
            .await
            .unwrap();
        persist(&handle, "after-pin", "c").await;
        assert_eq!(
            pinned.wal_ordinals,
            vec![("live-seg".to_string(), 0)],
            "plan-time pin must not re-select after compact bits or new segments"
        );
        let later = ReadBoundary::pin_wal(&key, &["gone-seg".into()]);
        assert_eq!(later.wal_ordinals, vec![("after-pin".to_string(), 0)]);
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
        let pinned = ReadBoundary::pin_wal(&key, &["named-seg".into()]);
        assert_eq!(pinned.wal_ordinals, vec![("live-seg".to_string(), 0)]);
        assert!(!pinned.wal_ordinals.iter().any(|(id, _)| id == "named-seg"));
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
            )
            .await
            .unwrap();
        handle
            .complete_slices_and_reclaim("live-seg", &[0])
            .await
            .unwrap();
        let pinned = ReadBoundary::pin_wal(&key, &[]);
        assert!(pinned.wal_ordinals.contains(&("live-seg".to_string(), 0)));
        assert!(pinned.wal_ordinals.contains(&("live-seg".to_string(), 1)));
        assert!(ReadBoundary::pin_wal(&key, &["live-seg".into()])
            .wal_ordinals
            .is_empty());
        remove_wal_handle(&key);
    }
}
