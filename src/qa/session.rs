use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThreadStep {
    pub action: String,
    pub args: Value,
    pub observation: Value,
    pub ts: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ThreadLog {
    pub steps: Vec<ThreadStep>,
    pub result: Option<ThreadResult>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThreadResult {
    pub sql: Option<String>,
    pub answer: String,
}

pub struct ThreadStore {
    pipeline: String,
}

impl ThreadStore {
    pub fn new(pipeline: &str) -> Self {
        Self { pipeline: pipeline.to_string() }
    }

    fn s3_key(&self, thread_id: &str) -> String {
        let tenant = crate::helpers::configuration::Config::get_tenant();
        let workspace = crate::helpers::configuration::Config::get_workspace_name();
        format!("{}/{}/{}/threads/{}.json", tenant, workspace, self.pipeline, thread_id)
    }

    pub async fn append_step(&self, thread_id: &str, step: ThreadStep) -> Result<(), String> {
        let key = self.s3_key(thread_id);
        let mut log = if let Ok(v) = crate::helpers::s3::get_json(&key).await {
            serde_json::from_value::<ThreadLog>(v).unwrap_or_default()
        } else {
            ThreadLog::default()
        };
        log.steps.push(step);
        let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
        crate::helpers::s3::put_json(&key, &val).await.map_err(|e| format!("{:?}", e))?;
        Ok(())
    }

    pub async fn get(&self, thread_id: &str) -> Option<ThreadLog> {
        let key = self.s3_key(thread_id);
        if let Ok(v) = crate::helpers::s3::get_json(&key).await {
            serde_json::from_value::<ThreadLog>(v).ok()
        } else {
            None
        }
    }
}


