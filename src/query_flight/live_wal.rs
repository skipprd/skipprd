use skippr_lease::PipelineKey;

use crate::buffer::wal_handle::{self, PipelineWalHandle};

#[derive(Clone, Debug)]
pub struct LiveWalScanRequest {
    pub pipeline: PipelineKey,
    pub namespace: String,
    pub exclude_segment_ids: Vec<String>,
}

/// Live incomplete ordinals from catalog SegmentRows, minus Iceberg-named
/// excludes. Unreadable payloads are skipped. Slice-complete ordinals stay
/// visible until Iceberg lists the segment (the row is deleted only when every
/// slice is done).
pub fn select_live_ordinals(
    handle: &PipelineWalHandle,
    exclude_segment_ids: &[String],
) -> Vec<(String, u32)> {
    handle.select_live_ordinals(exclude_segment_ids)
}

pub fn select_live_ordinals_for(
    pipeline: &PipelineKey,
    exclude_segment_ids: &[String],
) -> Vec<(String, u32)> {
    match wal_handle::wal_handle_for(pipeline) {
        Some(handle) => handle.select_live_ordinals(exclude_segment_ids),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::{PartitionKey, WalPayload};
    use crate::buffer::wal_handle::memory_catalog_for_test;
    use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
    use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn leased_handle(dir: &std::path::Path) -> Arc<PipelineWalHandle> {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::legacy_disk(dir);
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn OffsetStore> = Arc::new(SledOffsetStore::new(tree));
        PipelineWalHandle::new(key, paths, guard, catalog, offsets)
    }

    async fn persist_named(handle: &PipelineWalHandle, segment_id: &str, ids: &[&str]) {
        let schema = Arc::new(arrow::datatypes::Schema::new(vec![
            arrow::datatypes::Field::new("id", arrow::datatypes::DataType::Utf8, true),
        ]));
        let mut batches = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            let part = PartitionKey {
                sink_ref: "data_sinks.iceberg".into(),
                namespace: "events".into(),
                partition: format!("p{i}"),
                time: None,
                schema_fingerprint: "fp".into(),
            };
            let batch = arrow::record_batch::RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(arrow::array::StringArray::from(vec![*id]))],
            )
            .unwrap();
            batches.insert(part, WalPayload::Arrow(vec![batch]));
        }
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
    async fn iceberg_named_segment_is_not_selected() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        persist_named(&handle, "iceberg-seg", &["evt-a"]).await;
        persist_named(&handle, "live-seg", &["evt-b"]).await;
        let selected = select_live_ordinals(&handle, &["iceberg-seg".into()]);
        assert_eq!(selected, vec![("live-seg".to_string(), 0)]);
    }

    #[tokio::test]
    async fn complete_ordinal_stays_visible_until_iceberg_exclude() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        persist_named(&handle, "live-seg", &["evt-0", "evt-1"]).await;
        handle
            .complete_slices_and_reclaim("live-seg", &[0])
            .await
            .unwrap();
        let selected = select_live_ordinals(&handle, &[]);
        assert!(selected.contains(&("live-seg".to_string(), 0)));
        assert!(selected.contains(&("live-seg".to_string(), 1)));
        assert!(select_live_ordinals(&handle, &["live-seg".into()]).is_empty());
    }

    #[tokio::test]
    async fn fully_completed_row_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        persist_named(&handle, "live-seg", &["evt-23"]).await;
        handle
            .complete_slices_and_reclaim("live-seg", &[0])
            .await
            .unwrap();
        assert!(select_live_ordinals(&handle, &[]).is_empty());
    }

    #[tokio::test]
    async fn missing_payload_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        persist_named(&handle, "gone-seg", &["evt-a"]).await;
        let path = handle.paths().segs.join("gone-seg.seg");
        let _ = std::fs::remove_file(&path);
        assert!(select_live_ordinals(&handle, &[]).is_empty());
    }

    #[tokio::test]
    async fn catalog_segment_ids_are_catalog_rows() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        persist_named(&handle, "old", &["evt-old"]).await;
        persist_named(&handle, "new", &["evt-new"]).await;
        persist_named(&handle, "gone", &["evt-gone"]).await;
        handle
            .complete_slices_and_reclaim("gone", &[0])
            .await
            .unwrap();
        let mut ids = handle.catalog_segment_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["new".to_string(), "old".to_string()]);
    }

    #[tokio::test]
    async fn reclaimed_segment_is_not_selected() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        persist_named(&handle, "live-seg", &["evt-a"]).await;
        handle
            .complete_slices_and_reclaim("live-seg", &[0])
            .await
            .unwrap();
        assert!(select_live_ordinals(&handle, &[]).is_empty());
    }
}
