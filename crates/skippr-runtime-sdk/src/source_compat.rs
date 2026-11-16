pub fn take_row_count_batch<T>(current: &mut Vec<T>, cap: usize) -> Option<Vec<T>> {
    if current.len() >= cap {
        Some(std::mem::take(current))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn take_row_count_batch_splits_at_cap() {
        let mut rows = vec![1, 2];
        let full = super::take_row_count_batch(&mut rows, 2).unwrap();
        assert_eq!(full, vec![1, 2]);
        assert!(rows.is_empty());
        assert!(super::take_row_count_batch(&mut rows, 2).is_none());
    }
}

pub use crate::prefetch::ByteBoundedPrefetch;
pub use crate::source_sync::{
    collect_open_list_keys, load_checkpoint_payload, offset_validation_entry,
    partition_already_closed, source_payload_task, store_checkpoint_payload,
    submit_payload_batch_groups, submit_payload_batch_groups_durable, submit_payload_batches,
    submit_payload_batches_durable, validate_offset_key,
};
pub use skippr_core::ingest_work::{IngestBatch, ThroughputMetrics};
pub use skippr_core::plugins::source_sync::{
    OffsetValidationEntry, PayloadAck, PayloadSubmissionBatch, SourcePayloadTask, SourceSyncContext,
};
