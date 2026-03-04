use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, info};

use super::configuration::Config;

static CACHE: Lazy<Mutex<HashMap<String, HashSet<(String, String)>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub struct Manifest;

impl Manifest {
    pub fn s3_key(namespace: &str) -> (String, String) {
        let tenant = Config::get_tenant();
        let workspace = Config::get_workspace_name();
        let pipeline = Config::get_pipeline_name();
        let bucket = Config::get_skippr_s3_bucket();
        let filename = format!("{}.json", namespace);
        let key = format!("{}/{}/{}/manifest/{}", tenant, workspace, pipeline, filename);
        (bucket, key)
    }

    /// Build manifest key for a specific pipeline (no global pipeline state dependency).
    pub fn s3_key_for(pipeline: &str, namespace: &str) -> String {
        let tenant = Config::get_tenant();
        let workspace = Config::get_workspace_name();
        let filename = format!("{}.json", namespace);
        format!("{}/{}/{}/manifest/{}", tenant, workspace, pipeline, filename)
    }

    pub async fn read(namespace: &str) -> Option<Value> {
        let (bucket, key) = Self::s3_key(namespace);
        debug!("Reading manifest from s3://{}/{}", bucket, key);
        match crate::helpers::s3::get_json(&key).await {
            Ok(v) => {
                debug!("Manifest loaded for namespace '{}'", namespace);
                Some(v)
            }
            Err(_) => {
                debug!("Manifest not found at s3://{}/{}", bucket, key);
                None
            }
        }
    }

    pub async fn epoch(namespace: &str) -> Option<u64> {
        Self::read(namespace)
            .await
            .and_then(|v| v.get("epoch").and_then(|e| e.as_u64()))
    }

    fn resolve_abs_prefix(namespace: &str, dir_prefix: &str) -> String {
        if dir_prefix.starts_with("s3://") {
            dir_prefix.trim().to_string()
        } else {
            let bucket = Config::get_skippr_s3_bucket();
            let (_b, manifest_key) = Self::s3_key(namespace);
            format!("s3://{}/{}{}", bucket, manifest_key, dir_prefix)
        }
    }

    pub async fn ensure_prefix(namespace: &str, dir_prefix: &str) {
        Self::ensure_prefix_and_db(namespace, dir_prefix, "").await;
    }

    pub async fn ensure_prefix_and_db(namespace: &str, dir_prefix: &str, database: &str) {
        let abs_prefix = Self::resolve_abs_prefix(namespace, dir_prefix);
        let cache_key = (abs_prefix.clone(), database.to_string());

        // Fast path: already cached
        {
            let cache = CACHE.lock().await;
            if let Some(entries) = cache.get(namespace) {
                if entries.contains(&cache_key) {
                    return;
                }
            }
        }

        // Slow path: read from S3, populate cache, merge if needed
        let mut manifest = Self::read(namespace).await.unwrap_or(json!({
            "epoch": 0u64,
            "tables": {}
        }));

        // Populate cache from S3 state
        {
            let mut cache = CACHE.lock().await;
            let entries = cache.entry(namespace.to_string()).or_default();
            if let Some(tables) = manifest.get("tables").and_then(|t| t.as_object()) {
                if let Some(ns_obj) = tables.get(namespace).and_then(|v| v.as_object()) {
                    let existing_db =
                        ns_obj.get("database").and_then(|d| d.as_str()).unwrap_or("");
                    if let Some(arr) = ns_obj.get("prefixes").and_then(|a| a.as_array()) {
                        for p in arr {
                            if let Some(s) = p.as_str() {
                                entries.insert((s.to_string(), existing_db.to_string()));
                            }
                        }
                    }
                }
            }
            if entries.contains(&cache_key) {
                return;
            }
        }

        // Merge new prefix
        let mut changed = false;
        {
            let tables = manifest
                .as_object_mut()
                .unwrap()
                .entry("tables".to_string())
                .or_insert(json!({}));
            if !tables.is_object() {
                *tables = json!({});
            }
            let ns_entry = tables
                .as_object_mut()
                .unwrap()
                .entry(namespace.to_string())
                .or_insert(json!({"prefixes": []}));
            if !ns_entry.is_object() {
                *ns_entry = json!({"prefixes": []});
            }
            let arr = ns_entry
                .as_object_mut()
                .unwrap()
                .entry("prefixes".to_string())
                .or_insert(json!([]));
            if !arr.is_array() {
                *arr = json!([]);
            }
            let a = arr.as_array_mut().unwrap();
            if !a.iter().any(|v| v.as_str() == Some(&abs_prefix)) {
                a.push(Value::String(abs_prefix.clone()));
                changed = true;
            }
            if !database.is_empty() {
                let prev_db = ns_entry
                    .as_object()
                    .and_then(|o| o.get("database"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                if prev_db != database {
                    ns_entry
                        .as_object_mut()
                        .unwrap()
                        .insert("database".to_string(), json!(database));
                    changed = true;
                }
            }
        }

        if changed {
            let now_epoch = chrono::Utc::now().timestamp() as u64;
            if let Some(obj) = manifest.as_object_mut() {
                obj.insert("epoch".to_string(), json!(now_epoch));
            }
            let (_bucket, key) = Self::s3_key(namespace);
            let _ = crate::helpers::s3::put_json(&key, &manifest).await;
            info!(
                "Updated manifest for namespace '{}' (prefix={})",
                namespace, abs_prefix
            );
        }

        // Update cache
        {
            let mut cache = CACHE.lock().await;
            cache
                .entry(namespace.to_string())
                .or_default()
                .insert(cache_key);
        }
    }
}
