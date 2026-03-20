use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::helpers::configuration::Config;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NamespaceEntry {
    pub semantic_key: String,
    pub catalog_key: String,
    pub stats_key: String,
    pub last_updated_epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct Registry {
    pub pipelines: Vec<String>,
    pub namespaces_by_pipeline: HashMap<String, HashMap<String, NamespaceEntry>>,
    #[serde(default)]
    pub embeddings_uri_by_pipeline: HashMap<String, String>,
    pub last_updated_epoch: u64,
}

static REGISTRY_CACHE: Lazy<Arc<RwLock<Option<Registry>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

fn central_registry_key() -> String {
    let tenant = Config::get_tenant();
    let workspace = Config::get_workspace_name();
    format!("{}/{}/registry.json", tenant, workspace)
}

async fn load_registry() -> Option<Registry> {
    let key = central_registry_key();
    let storage = crate::adapters::storage::get_storage();
    match storage.get_json_opt(&key).await {
        Ok(Some(val)) => serde_json::from_value::<Registry>(val).ok(),
        Ok(None) => None,
        Err(e) => {
            warn!("load_registry: failed to fetch '{}': {}", key, e);
            None
        }
    }
}

async fn save_registry(reg: &Registry) -> Result<(), String> {
    let key = central_registry_key();
    let json_val = serde_json::to_value(reg).map_err(|e| e.to_string())?;
    let storage = crate::adapters::storage::get_storage();
    storage.put_json(&key, &json_val).await?;
    {
        let mut guard = REGISTRY_CACHE.write().await;
        *guard = Some(reg.clone());
    }
    info!("Updated central registry: {}", key);
    Ok(())
}

pub async fn read_registry() -> Option<Registry> {
    if let Some(r) = REGISTRY_CACHE.read().await.as_ref() {
        return Some(r.clone());
    }
    let loaded = load_registry().await;
    if loaded.is_some() {
        let mut guard = REGISTRY_CACHE.write().await;
        *guard = loaded.clone();
    }
    loaded
}

pub async fn write_registry(mut reg: Registry) -> Result<(), String> {
    reg.last_updated_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    save_registry(&reg).await
}

pub async fn list_pipelines() -> Vec<String> {
    read_registry()
        .await
        .map(|r| {
            let mut v = r.pipelines.clone();
            v.sort();
            v.dedup();
            v
        })
        .unwrap_or_default()
}

pub async fn list_namespaces(pipeline: &str) -> Vec<String> {
    if let Some(r) = read_registry().await {
        if let Some(nsmap) = r.namespaces_by_pipeline.get(pipeline) {
            let mut v: Vec<String> = nsmap.keys().cloned().collect();
            v.sort();
            return v;
        }
    }
    Vec::new()
}

pub async fn find_entry(pipeline: &str, namespace: &str) -> Option<NamespaceEntry> {
    read_registry().await.and_then(|r| {
        r.namespaces_by_pipeline
            .get(pipeline)
            .and_then(|m| m.get(namespace).cloned())
    })
}

pub async fn ensure_ns_entry(
    pipeline: &str,
    namespace: &str,
    mut updater: impl FnMut(Option<NamespaceEntry>) -> NamespaceEntry,
) -> Result<(), String> {
    let mut reg = read_registry().await.unwrap_or_default();
    if !reg.pipelines.iter().any(|p| p == pipeline) {
        reg.pipelines.push(pipeline.to_string());
    }
    let nsmap = reg
        .namespaces_by_pipeline
        .entry(pipeline.to_string())
        .or_insert_with(HashMap::new);
    let current = nsmap.get(namespace).cloned();
    let mut new_entry = updater(current);
    new_entry.last_updated_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    nsmap.insert(namespace.to_string(), new_entry);
    write_registry(reg).await
}

pub async fn set_embeddings_uri(pipeline: &str, uri: &str) -> Result<(), String> {
    let mut reg = read_registry().await.unwrap_or_default();
    if !reg.pipelines.iter().any(|p| p == pipeline) {
        reg.pipelines.push(pipeline.to_string());
    }
    reg.embeddings_uri_by_pipeline
        .insert(pipeline.to_string(), uri.to_string());
    write_registry(reg).await
}

/// Build the S3 key for a namespace manifest for a given pipeline, independent of any global pipeline state.
pub fn manifest_key_for(pipeline: &str, namespace: &str) -> String {
    crate::helpers::manifest::Manifest::s3_key_for(pipeline, namespace)
}
