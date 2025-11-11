use std::sync::Arc;
use tokio::sync::RwLock;
use once_cell::sync::Lazy;

use serde::{Deserialize, Serialize};

use crate::helpers::configuration::Config;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NamespaceEntry {
    pub semantic_key: String,
    pub catalog_key: String,
    pub stats_key: String,
    pub last_updated_epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PipelineRegistry {
    pub pipeline: String,
    pub namespaces: std::collections::HashMap<String, NamespaceEntry>,
    pub last_updated_epoch: u64,
}

pub static REGISTRY_CACHE: Lazy<Arc<RwLock<Vec<PipelineRegistry>>>> = Lazy::new(|| Arc::new(RwLock::new(Vec::new())));

fn get_pipeline_registry_key(pipeline: &str) -> String {
    // {tenant}/{workspace}/{pipeline}/manifest/registry.json
    let tenant = Config::get_tenant();
    let workspace = Config::get_workspace_name();
    format!("{}/{}/{}/manifest/registry.json", tenant, workspace, pipeline)
}

async fn load_pipeline_registry(pipeline: &str) -> Option<PipelineRegistry> {
    let key = get_pipeline_registry_key(pipeline);
    match crate::helpers::s3::get_json(&key).await {
        Ok(val) => serde_json::from_value::<PipelineRegistry>(val).ok(),
        Err(e) => {
            if let aws_sdk_s3::error::SdkError::ServiceError(se) = &e { if se.err().is_no_such_key() { return None; } }
            None
        }
    }
}

async fn save_pipeline_registry(reg: &PipelineRegistry) -> Result<(), String> {
    let key = get_pipeline_registry_key(&reg.pipeline);
    let json_val = serde_json::to_value(reg).map_err(|e| e.to_string())?;
    match crate::helpers::s3::put_json(&key, &json_val).await {
        Ok(_) => {
            let mut guard = REGISTRY_CACHE.write().await;
            if let Some(existing) = guard.iter_mut().find(|p| p.pipeline == reg.pipeline) {
                *existing = reg.clone();
            } else {
                guard.push(reg.clone());
            }
            Ok(())
        }
        Err(e) => Err(format!("failed to save registry to S3: {:?}", e))
    }
}

pub async fn get_registry_cached() -> Vec<PipelineRegistry> {
    REGISTRY_CACHE.read().await.clone()
}

pub async fn list_namespaces(pipeline: &str) -> Vec<String> {
    // Try cache first
    {
        let reg = get_registry_cached().await;
        for p in &reg { if p.pipeline == pipeline { return p.namespaces.keys().cloned().collect(); } }
    }
    // Load this pipeline's registry from S3, cache and return
    if let Some(loaded) = load_pipeline_registry(pipeline).await {
        {
            let mut guard = REGISTRY_CACHE.write().await;
            if let Some(existing) = guard.iter_mut().find(|p| p.pipeline == pipeline) {
                *existing = loaded.clone();
            } else {
                guard.push(loaded.clone());
            }
        }
        return loaded.namespaces.keys().cloned().collect();
    }
    Vec::new()
}

pub async fn find_entry(pipeline: &str, namespace: &str) -> Option<NamespaceEntry> {
    {
        let reg = get_registry_cached().await;
        for p in &reg {
            if p.pipeline == pipeline {
                if let Some(ns) = p.namespaces.get(namespace) { return Some(ns.clone()); }
            }
        }
    }
    if let Some(loaded) = load_pipeline_registry(pipeline).await {
        {
            let mut guard = REGISTRY_CACHE.write().await;
            if let Some(existing) = guard.iter_mut().find(|p| p.pipeline == pipeline) {
                *existing = loaded.clone();
            } else {
                guard.push(loaded.clone());
            }
        }
        return loaded.namespaces.get(namespace).cloned();
    }
    None
}

pub async fn ensure_ns_entry(pipeline: &str, namespace: &str, mut updater: impl FnMut(Option<NamespaceEntry>) -> NamespaceEntry) -> Result<(), String> {
    // Get current registry for this pipeline (from cache or S3)
    let mut reg_vec = get_registry_cached().await;
    let mut maybe_idx = reg_vec.iter().position(|p| p.pipeline == pipeline);
    if maybe_idx.is_none() {
        if let Some(loaded) = load_pipeline_registry(pipeline).await {
            reg_vec.push(loaded);
            maybe_idx = reg_vec.iter().position(|p| p.pipeline == pipeline);
        } else {
            // push empty shell
            reg_vec.push(PipelineRegistry { pipeline: pipeline.to_string(), namespaces: std::collections::HashMap::new(), last_updated_epoch: 0 });
            maybe_idx = Some(reg_vec.len() - 1);
        }
    }
    let idx = maybe_idx.unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    {
        let pr = &mut reg_vec[idx];
        let current = pr.namespaces.get(namespace).cloned();
        let mut new_entry = updater(current);
        new_entry.last_updated_epoch = now;
        pr.namespaces.insert(namespace.to_string(), new_entry);
        pr.last_updated_epoch = now;
    }
    // Save just this pipeline's registry and refresh cache
    let pr_clone = reg_vec[idx].clone();
    save_pipeline_registry(&pr_clone).await
}


