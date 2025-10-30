use std::sync::Arc;
use tokio::sync::RwLock;
use once_cell::sync::Lazy;

use serde::{Deserialize, Serialize};

use crate::helpers::configuration::Config;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NamespaceEntry {
    pub data_prefixes: Vec<String>,
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

fn get_registry_key_only() -> String {
    // {tenant}/{workspace}/_skippr/registry.json
    let tenant = Config::get_tenant();
    let workspace = Config::get_workspace_name();
    format!("{}/{}/_skippr/registry.json", tenant, workspace)
}

pub async fn load_registry() -> Result<Vec<PipelineRegistry>, String> {
    let key = get_registry_key_only();
    match crate::helpers::s3::get_json(&key).await {
        Ok(val) => {
            let reg: Vec<PipelineRegistry> = serde_json::from_value(val).map_err(|e| e.to_string())?;
            {
                let mut guard = REGISTRY_CACHE.write().await;
                *guard = reg.clone();
            }
            Ok(reg)
        }
        Err(e) => {
            // If 404, treat as empty registry
            if let aws_sdk_s3::error::SdkError::ServiceError(se) = &e { if se.err().is_no_such_key() { let empty: Vec<PipelineRegistry> = Vec::new(); let mut guard = REGISTRY_CACHE.write().await; *guard = empty.clone(); return Ok(empty); } }
            Err(format!("failed to load registry from S3: {:?}", e))
        }
    }
}

pub async fn save_registry(new_val: &Vec<PipelineRegistry>) -> Result<(), String> {
    let key = get_registry_key_only();
    let json_val = serde_json::to_value(new_val).map_err(|e| e.to_string())?;
    match crate::helpers::s3::put_json(&key, &json_val).await {
        Ok(_) => {
            let mut guard = REGISTRY_CACHE.write().await;
            *guard = new_val.clone();
            Ok(())
        }
        Err(e) => Err(format!("failed to save registry to S3: {:?}", e))
    }
}

pub async fn get_registry_cached() -> Vec<PipelineRegistry> {
    REGISTRY_CACHE.read().await.clone()
}

pub async fn list_namespaces(pipeline: &str) -> Vec<String> {
    let mut reg = get_registry_cached().await;
    if reg.is_empty() {
        let _ = load_registry().await;
        reg = get_registry_cached().await;
    }
    for p in &reg { if p.pipeline == pipeline { return p.namespaces.keys().cloned().collect(); } }
    Vec::new()
}

pub async fn find_entry(pipeline: &str, namespace: &str) -> Option<NamespaceEntry> {
    let mut reg = get_registry_cached().await;
    if reg.is_empty() {
        let _ = load_registry().await;
        reg = get_registry_cached().await;
    }
    for p in &reg {
        if p.pipeline == pipeline {
            if let Some(ns) = p.namespaces.get(namespace) { return Some(ns.clone()); }
        }
    }
    None
}

pub async fn ensure_ns_entry(pipeline: &str, namespace: &str, mut updater: impl FnMut(Option<NamespaceEntry>) -> NamespaceEntry) -> Result<(), String> {
    let mut reg = get_registry_cached().await;
    let mut found = false;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    for p in reg.iter_mut() {
        if p.pipeline == pipeline {
            let current = p.namespaces.get(namespace).cloned();
            let mut new_entry = updater(current);
            new_entry.last_updated_epoch = now;
            p.namespaces.insert(namespace.to_string(), new_entry);
            p.last_updated_epoch = now;
            found = true;
            break;
        }
    }
    if !found {
        let mut map = std::collections::HashMap::new();
        let mut new_entry = updater(None);
        new_entry.last_updated_epoch = now;
        map.insert(namespace.to_string(), new_entry);
        reg.push(PipelineRegistry { pipeline: pipeline.to_string(), namespaces: map, last_updated_epoch: now });
    }
    save_registry(&reg).await
}


