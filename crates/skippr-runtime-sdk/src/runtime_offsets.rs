use std::sync::Arc;

use skippr_core::helpers::offsets::{OffsetKey, OffsetTypes};
use skippr_core::plugins::source_sync::{OffsetValidationEntry, SourceSyncContext};

/// Runtime source helper for plugins that still think in terms of per-key offset checks.
#[derive(Clone)]
pub struct RuntimeOffsetReader {
    ctx: Arc<dyn SourceSyncContext>,
}

impl RuntimeOffsetReader {
    pub fn new(ctx: Arc<dyn SourceSyncContext>) -> Self {
        Self { ctx }
    }

    pub fn validate(
        &self,
        key: &OffsetKey,
        offset_type: OffsetTypes,
        offset_value: u64,
    ) -> Option<bool> {
        let entry = OffsetValidationEntry {
            key: key.clone(),
            offset_type,
            offset_value,
        };
        self.ctx
            .validate_offset_batch(&[entry])
            .ok()
            .and_then(|mut results| results.pop())
    }
}
