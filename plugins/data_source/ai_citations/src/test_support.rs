use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
use skippr_runtime_sdk::plugins::{OffsetValidationEntry, SourcePayloadTask, SourceSyncContext};
use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
use skippr_runtime_sdk::source_compat::{load_checkpoint_payload, ThroughputMetrics};

use crate::checkpoint::PromptCheckpoint;

#[derive(Default)]
pub struct RecordingSyncContext {
    pub checkpoint_stores: Mutex<Vec<String>>,
    checkpoints: Mutex<HashMap<String, CheckpointEnvelope>>,
    /// namespace -> NDJSON lines parsed as JSON values
    pub payloads: Mutex<HashMap<String, Vec<Value>>>,
}

impl RecordingSyncContext {
    pub fn rows(&self, namespace: &str) -> Vec<Value> {
        self.payloads
            .lock()
            .unwrap()
            .get(namespace)
            .cloned()
            .unwrap_or_default()
    }

    pub fn row_count(&self, namespace: &str) -> usize {
        self.rows(namespace).len()
    }

    #[allow(dead_code)]
    pub fn checkpoint_payload(&self, prompt_id: &str, model: &str) -> Option<PromptCheckpoint> {
        let key = crate::checkpoint::checkpoint_key(prompt_id, model);
        let ctx = self as &dyn SourceSyncContext;
        load_checkpoint_payload::<PromptCheckpoint>(ctx, &key)
    }
}

impl SourceSyncContext for RecordingSyncContext {
    fn submit_payload_tasks(
        &self,
        tasks: Vec<SourcePayloadTask>,
    ) -> Result<ThroughputMetrics, std::io::Error> {
        let mut payloads = self.payloads.lock().unwrap();
        for task in tasks {
            for batch in task.batches {
                let Some(namespace) = batch.namespace.as_deref() else {
                    continue;
                };
                let rows = batch
                    .data
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect::<Vec<_>>();
                payloads
                    .entry(namespace.to_string())
                    .or_default()
                    .extend(rows);
            }
        }
        Ok(ThroughputMetrics {
            bytes_per_second: 0,
            active_cores: 0,
            queue_length: 0,
            optimal_chunk_size: 0,
        })
    }

    fn validate_offset_batch(
        &self,
        entries: &[OffsetValidationEntry],
    ) -> Result<Vec<bool>, std::io::Error> {
        Ok(vec![false; entries.len()])
    }

    fn relay_offset_hints(
        &self,
        _hints: Vec<RuntimeOffsetMaterializationHint>,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn store_checkpoint(
        &self,
        key: &str,
        envelope: &CheckpointEnvelope,
    ) -> Result<(), String> {
        self.checkpoint_stores.lock().unwrap().push(key.to_string());
        self.checkpoints
            .lock()
            .unwrap()
            .insert(key.to_string(), envelope.clone());
        Ok(())
    }

    fn load_checkpoint_envelope(&self, key: &str) -> Option<CheckpointEnvelope> {
        self.checkpoints.lock().unwrap().get(key).cloned()
    }
}

pub fn checks_with_code<'a>(rows: &'a [Value], code: &str) -> Vec<&'a Value> {
    rows.iter()
        .filter(|row| row.get("check_code").and_then(|v| v.as_str()) == Some(code))
        .collect()
}
