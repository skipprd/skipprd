use serde::{Deserialize, Serialize};
use skippr_plugin_shared_api_source::CheckpointPayload;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobPaginationCheckpoint {
    pub offset: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_after_token: Option<String>,
    pub pages_completed: u32,
}

impl CheckpointPayload for JobPaginationCheckpoint {
    const VERSION: u32 = 1;
}

pub fn checkpoint_key(job_id: &str, run_date: &str) -> String {
    format!("dataforseo_backlinks:job:{job_id}:{run_date}")
}
