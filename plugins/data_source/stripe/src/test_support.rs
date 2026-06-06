use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use skippr_runtime_sdk::plugins::cdc::CheckpointEnvelope;
use skippr_runtime_sdk::plugins::{OffsetValidationEntry, SourcePayloadTask, SourceSyncContext};
use skippr_runtime_sdk::protocol::RuntimeOffsetMaterializationHint;
use skippr_runtime_sdk::source_compat::ThroughputMetrics;

use crate::privacy::{PrivacyConfig, PrivacyMode, PrivacyProfile};
use crate::streams::StreamProfile;
use crate::stripe::DataSourceStripePluginConfig;
use crate::stripe_api::FIXTURE_ENV;

pub fn sample_config() -> DataSourceStripePluginConfig {
    DataSourceStripePluginConfig {
        stripe_account_id: "acct_fixture".into(),
        start_date: "2024-01-01".into(),
        lookback_days: 30,
        stream_profile: StreamProfile::ConsoleDefault,
        streams: None,
        min_query_interval_ms: 0,
        write_policy: None,
        oauth_token_url: None,
        oauth_client_id: None,
        oauth_client_secret: None,
        oauth_refresh_token: None,
        access_token: None,
        privacy: PrivacyConfig {
            mode: PrivacyMode::Profile,
            profile: PrivacyProfile::UpfoundrySafe,
            ..Default::default()
        },
    }
}

pub fn set_fixture_dir(dir: &str) {
    std::env::set_var(FIXTURE_ENV, dir);
}

pub fn clear_fixture_dir() {
    std::env::remove_var(FIXTURE_ENV);
}

#[derive(Default)]
pub struct RecordingSyncContext {
    payload_tasks: Mutex<Vec<SourcePayloadTask>>,
    checkpoints: Mutex<HashMap<String, CheckpointEnvelope>>,
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
                    .map(|line| serde_json::from_str(line).expect("invalid row JSON"))
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
