use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
use skippr_runtime_sdk::plugins::{OffsetValidationEntry, SourcePayloadTask, SourceSyncContext};
use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
use skippr_runtime_sdk::source_compat::ThroughputMetrics;

use crate::shopify::DataSourceShopifyAdminPluginConfig;
use crate::shopify_api::FIXTURE_ENV;
use crate::streams::StreamProfile;

pub fn sample_config() -> DataSourceShopifyAdminPluginConfig {
    DataSourceShopifyAdminPluginConfig {
        shop_domain: "fixture.myshopify.com".into(),
        api_version: Some("2026-04".into()),
        start_date: "2024-01-01".into(),
        lookback_days: 30,
        stream_profile: StreamProfile::ConsoleDefault,
        streams: None,
        min_query_interval_ms: 0,
        max_queries_per_run: 200,
        use_bulk_operations: true,
        oauth_client_id: None,
        oauth_client_secret: None,
        oauth_access_token: None,
    }
}

pub fn set_fixture_dir() {
    std::env::set_var(
        FIXTURE_ENV,
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"),
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
