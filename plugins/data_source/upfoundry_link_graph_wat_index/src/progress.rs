use std::collections::{BTreeMap, BTreeSet};

use skippr_runtime_sdk::plugins::SourceSyncContext;
use skippr_runtime_sdk::source_compat::store_checkpoint_payload;

use crate::job::{checkpoint_key, WatManifestCheckpoint};

#[derive(Debug, Default)]
pub struct CompletionTracker {
    completed_paths: BTreeSet<usize>,
    path_member_cursors: BTreeMap<usize, u64>,
    stored_next_path_index: usize,
}

impl CompletionTracker {
    pub fn new(stored_next_path_index: usize, cursors: BTreeMap<usize, u64>) -> Self {
        Self {
            completed_paths: BTreeSet::new(),
            path_member_cursors: cursors,
            stored_next_path_index,
        }
    }

    pub fn member_cursor(&self, path_index: usize) -> u64 {
        self.path_member_cursors
            .get(&path_index)
            .copied()
            .unwrap_or(0)
    }

    pub fn record_member_progress(&mut self, path_index: usize, member_index: u64) {
        self.path_member_cursors
            .entry(path_index)
            .and_modify(|cursor| {
                if member_index > *cursor {
                    *cursor = member_index;
                }
            })
            .or_insert(member_index);
    }

    pub fn mark_path_complete(&mut self, path_index: usize, final_member_index: u64) {
        self.path_member_cursors
            .insert(path_index, final_member_index);
        self.completed_paths.insert(path_index);
    }

    pub fn contiguous_complete_prefix(&self) -> usize {
        let mut prefix = self.stored_next_path_index;
        while self.completed_paths.contains(&prefix) {
            prefix = prefix.saturating_add(1);
        }
        prefix
    }

    pub fn maybe_store_manifest_checkpoint(
        &mut self,
        ctx: &dyn SourceSyncContext,
        crawl_id: &str,
        manifest_uri: &str,
        total_paths: usize,
    ) -> Result<(), std::io::Error> {
        let next = self.contiguous_complete_prefix();
        if next <= self.stored_next_path_index {
            return Ok(());
        }
        self.stored_next_path_index = next;
        let mut cursors = self.path_member_cursors.clone();
        for completed in &self.completed_paths {
            cursors.remove(completed);
        }
        store_checkpoint_payload(
            ctx,
            &checkpoint_key(crawl_id),
            &WatManifestCheckpoint {
                crawl_id: crawl_id.to_string(),
                manifest_uri: manifest_uri.to_string(),
                next_path_index: next,
                total_paths: Some(total_paths),
                cleared: false,
                path_member_cursors: cursors,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_cursor_from_checkpoint() {
        let mut cursors = BTreeMap::new();
        cursors.insert(3, 42);
        let tracker = CompletionTracker::new(0, cursors);
        assert_eq!(tracker.member_cursor(3), 42);
        assert_eq!(tracker.member_cursor(9), 0);
    }

    #[test]
    fn manifest_checkpoint_monotonic_under_out_of_order() {
        let mut tracker = CompletionTracker::new(0, BTreeMap::new());
        tracker.mark_path_complete(2, 10);
        assert_eq!(tracker.contiguous_complete_prefix(), 0);
        tracker.mark_path_complete(0, 5);
        assert_eq!(tracker.contiguous_complete_prefix(), 1);
        tracker.mark_path_complete(1, 8);
        assert_eq!(tracker.contiguous_complete_prefix(), 3);
    }

    #[test]
    fn contiguous_prefix_waits_for_out_of_order_completion() {
        let mut tracker = CompletionTracker::new(0, BTreeMap::new());
        tracker.mark_path_complete(1, 10);
        assert_eq!(tracker.contiguous_complete_prefix(), 0);
        tracker.mark_path_complete(0, 5);
        assert_eq!(tracker.contiguous_complete_prefix(), 2);
    }
}
