use serde_json::{json, Map, Value};

use crate::config::BacklinkJob;

#[derive(Debug, Clone)]
pub struct TargetQueryOptions {
    pub include_subdomains: Option<bool>,
    pub exclude_internal_backlinks: Option<bool>,
    pub backlinks_status_type: Option<String>,
    pub internal_list_limit: Option<u32>,
}

impl TargetQueryOptions {
    pub fn from_job(job: &BacklinkJob) -> Self {
        Self {
            include_subdomains: job.include_subdomains,
            exclude_internal_backlinks: job.exclude_internal_backlinks,
            backlinks_status_type: job.backlinks_status_type.clone(),
            internal_list_limit: None,
        }
    }

    pub fn from_jobs(jobs: &[BacklinkJob]) -> Self {
        jobs.first()
            .map(Self::from_job)
            .unwrap_or(Self {
                include_subdomains: Some(true),
                exclude_internal_backlinks: Some(true),
                backlinks_status_type: Some("live".into()),
                internal_list_limit: Some(10),
            })
    }
}

pub fn build_target_task(
    target: &str,
    limit: u32,
    offset: u32,
    search_after_token: Option<&str>,
    rank_scale: Option<&str>,
    query: &TargetQueryOptions,
    extra: Map<String, Value>,
) -> Value {
    let mut task = Map::new();
    task.insert("target".into(), json!(target));
    if let Some(token) = search_after_token.filter(|t| !t.is_empty()) {
        task.insert("search_after_token".into(), json!(token));
    } else if limit > 0 {
        task.insert("limit".into(), json!(limit));
        task.insert("offset".into(), json!(offset));
    }
    if let Some(v) = query.include_subdomains {
        task.insert("include_subdomains".into(), json!(v));
    }
    if let Some(v) = query.exclude_internal_backlinks {
        task.insert("exclude_internal_backlinks".into(), json!(v));
    }
    if let Some(status) = query
        .backlinks_status_type
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        task.insert("backlinks_status_type".into(), json!(status));
    }
    if let Some(v) = query.internal_list_limit {
        task.insert("internal_list_limit".into(), json!(v));
    }
    if let Some(scale) = rank_scale.filter(|s| !s.is_empty()) {
        task.insert("rank_scale".into(), json!(scale));
    }
    for (k, v) in extra {
        task.insert(k, v);
    }
    Value::Object(task)
}
