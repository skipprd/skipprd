use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThreadStep {
    pub action: String,
    pub args: Value,
    pub observation: Value,
    pub ts: String,
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ThreadLog {
    pub steps: Vec<ThreadStep>,
    pub result: Option<ThreadResult>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub title_finalized: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThreadResult {
    pub sql: Option<String>,
    pub answer: String,
}

pub struct ThreadStore {
}

impl ThreadStore {
    pub fn new() -> Self {
        Self { }
    }

    fn s3_key(&self, thread_id: &str) -> String {
        let tenant = crate::helpers::configuration::Config::get_tenant();
        let workspace = crate::helpers::configuration::Config::get_workspace_name();
        format!("{}/{}/threads/{}.json", tenant, workspace, thread_id)
    }

    fn threads_prefix(&self) -> String {
        let tenant = crate::helpers::configuration::Config::get_tenant();
        let workspace = crate::helpers::configuration::Config::get_workspace_name();
        format!("{}/{}/threads/", tenant, workspace)
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
            let mut log_opt = serde_json::from_value::<ThreadLog>(v).ok();
            if let Some(ref mut log) = log_opt {
                // Back-compat: default missing agent to "ask"
                for step in log.steps.iter_mut() {
                    if step.agent.is_none() {
                        step.agent = Some("ask".to_string());
                    }
                }
                // Ensure defaults for new fields
                if log.title.is_none() && !log.steps.is_empty() {
                    // no-op default; title set explicitly by server
                }
            }
            log_opt
        } else {
            None
        }
    }

    pub async fn list(&self) -> Vec<String> {
        use aws_sdk_s3::primitives::DateTime as S3DateTime;
        let bucket = crate::helpers::configuration::Config::get_skippr_s3_bucket();
        let client = crate::helpers::s3::get_s3_client().await;
        let prefix = self.threads_prefix();
        let mut token: Option<String> = None;
        let mut out: Vec<String> = Vec::new();
        loop {
            let mut req = client.list_objects_v2().bucket(&bucket).prefix(&prefix).max_keys(1000);
            if let Some(t) = token.as_ref() { req = req.continuation_token(t); }
            match req.send().await {
                Ok(resp) => {
                    for obj in resp.contents() {
                        if let Some(k) = obj.key() {
                            if let Some(name) = k.strip_prefix(&prefix).and_then(|s| s.strip_suffix(".json")) {
                                out.push(name.to_string());
                            }
                        }
                    }
                    if resp.next_continuation_token().is_none() { break; }
                    token = resp.next_continuation_token().map(|s| s.to_string());
                }
                Err(_) => { break; }
            }
        }
        out.sort();
        out
    }

    pub async fn delete(&self, thread_id: &str) -> Result<(), String> {
        let key = self.s3_key(thread_id);
        crate::helpers::s3::delete_object(&key).await.map_err(|e| format!("{:?}", e))?;
        Ok(())
    }

    pub async fn set_title_if_absent(&self, thread_id: &str, title: &str) -> Result<(), String> {
        let key = self.s3_key(thread_id);
        let mut log = if let Ok(v) = crate::helpers::s3::get_json(&key).await {
            serde_json::from_value::<ThreadLog>(v).unwrap_or_default()
        } else {
            ThreadLog::default()
        };
        if log.title.is_none() || log.title.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
            log.title = Some(title.to_string());
            let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
            crate::helpers::s3::put_json(&key, &val).await.map_err(|e| format!("{:?}", e))?;
        }
        Ok(())
    }

    pub async fn finalize_title(&self, thread_id: &str, title: &str) -> Result<(), String> {
        let key = self.s3_key(thread_id);
        let mut log = if let Ok(v) = crate::helpers::s3::get_json(&key).await {
            serde_json::from_value::<ThreadLog>(v).unwrap_or_default()
        } else {
            ThreadLog::default()
        };
        if !log.title_finalized {
            log.title = Some(title.to_string());
            log.title_finalized = true;
            let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
            crate::helpers::s3::put_json(&key, &val).await.map_err(|e| format!("{:?}", e))?;
        }
        Ok(())
    }
}


