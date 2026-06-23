use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
use skippr_runtime_sdk::plugins::{OffsetValidationEntry, SourcePayloadTask, SourceSyncContext};
use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
use skippr_runtime_sdk::source_compat::ThroughputMetrics;

use crate::config::{AppStoreEntity, DataSourceAppleAppStoreSerpPluginConfig, TargetEntry};
use crate::itunes::FIXTURE_ENV;

pub fn sample_config() -> DataSourceAppleAppStoreSerpPluginConfig {
    DataSourceAppleAppStoreSerpPluginConfig {
        targets: vec![TargetEntry {
            app_id: "123456789".into(),
            bundle_id: Some("com.example.app".into()),
            aliases: vec![],
        }],
        keywords: vec!["fixture keyword".into()],
        storefronts: vec!["us".into()],
        entity: AppStoreEntity::Software,
        max_depth: 50,
        min_query_interval_ms: 3_000,
        max_queries_per_run: 20,
        stop_after_first_target_match: true,
        capture_results: false,
        force_refresh_today: false,
        user_agent: None,
    }
}

pub fn set_fixture_dir() {
    std::env::set_var(
        FIXTURE_ENV,
        concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures"),
    );
}

pub fn clear_fixture_dir() {
    std::env::remove_var(FIXTURE_ENV);
}

#[derive(Default)]
pub struct RecordingSyncContext {
    pub checkpoint_stores: Mutex<Vec<String>>,
    checkpoints: Mutex<HashMap<String, CheckpointEnvelope>>,
    payload_tasks: Mutex<Vec<SourcePayloadTask>>,
}

impl RecordingSyncContext {
    #[allow(dead_code)]
    pub fn submitted_namespaces(&self) -> Vec<String> {
        self.payload_tasks
            .lock()
            .unwrap()
            .iter()
            .flat_map(|task| {
                task.batches
                    .iter()
                    .filter_map(|batch| batch.namespace.clone())
            })
            .collect()
    }

    pub fn rows_for_namespace(&self, namespace: &str) -> Vec<Value> {
        self.payload_tasks
            .lock()
            .unwrap()
            .iter()
            .flat_map(|task| task.batches.iter())
            .filter(|batch| batch.namespace.as_deref() == Some(namespace))
            .flat_map(|batch| {
                batch
                    .data
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| {
                        serde_json::from_str(line).unwrap_or_else(|e| {
                            panic!("invalid row JSON in {namespace}: {e}\n{line}")
                        })
                    })
            })
            .collect()
    }
}

impl SourceSyncContext for RecordingSyncContext {
    fn submit_payload_tasks(
        &self,
        tasks: Vec<SourcePayloadTask>,
    ) -> Result<ThroughputMetrics, std::io::Error> {
        self.payload_tasks.lock().unwrap().extend(tasks);
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

    fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String> {
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
