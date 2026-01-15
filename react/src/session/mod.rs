use serde::{Deserialize, Serialize};
use serde_json::Value;
use once_cell::sync::OnceCell;
use dashmap::DashMap;
use std::time::Instant;
use std::collections::HashMap;
use std::sync::Arc;

use crate::adapters::storage::StorageAdapter;
use crate::providers::{Keyspace, RequestScope};

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

#[derive(Clone)]
pub struct ThreadStore {
    storage: Arc<dyn StorageAdapter>,
    scope: RequestScope,
    keyspace: Arc<dyn Keyspace>,
}

#[derive(Clone)]
struct CacheEntry { log: ThreadLog, ts: Instant }
static THREAD_CACHE: OnceCell<DashMap<String, CacheEntry>> = OnceCell::new();
fn cache() -> &'static DashMap<String, CacheEntry> { THREAD_CACHE.get_or_init(|| DashMap::new()) }

// Per-thread, in-memory context cache (not persisted)
#[derive(Clone, Debug, Default)]
pub struct ThreadCache {
    pub candidates: Vec<(String, String, f32)>, // (project_id, dataset_id, score) [legacy cache shape; best-effort only]
    pub schemas: HashMap<String, Vec<(String, String)>>, // dataset FQN -> [(name, type)]
    pub samples: HashMap<String, Vec<Vec<String>>>, // dataset FQN -> rows
    pub updated_at: Option<Instant>,
}

static THREAD_CTX_CACHE: OnceCell<DashMap<String, ThreadCache>> = OnceCell::new();
fn ctx_cache() -> &'static DashMap<String, ThreadCache> { THREAD_CTX_CACHE.get_or_init(|| DashMap::new()) }

impl ThreadCache {
    pub fn ttl_fresh(&self, secs: u64) -> bool {
        match self.updated_at {
            Some(t) => t.elapsed().as_secs() < secs,
            None => false,
        }
    }
}

pub struct ThreadCacheStore;

impl ThreadCacheStore {
    pub fn get(thread_id: &str) -> Option<ThreadCache> {
        ctx_cache().get(thread_id).map(|c| c.clone())
    }
    pub fn set(thread_id: &str, cache: ThreadCache) {
        ctx_cache().insert(thread_id.to_string(), cache);
    }
    pub fn update_candidates(thread_id: &str, cands: Vec<(String, String, f32)>) {
        let mut entry = ctx_cache().get(thread_id).map(|e| e.clone()).unwrap_or_default();
        entry.candidates = cands;
        entry.updated_at = Some(Instant::now());
        ctx_cache().insert(thread_id.to_string(), entry);
    }
    pub fn update_schema(thread_id: &str, dataset_fqn: &str, cols: Vec<(String, String)>) {
        let mut entry = ctx_cache().get(thread_id).map(|e| e.clone()).unwrap_or_default();
        entry.schemas.insert(dataset_fqn.to_string(), cols);
        entry.updated_at = Some(Instant::now());
        ctx_cache().insert(thread_id.to_string(), entry);
    }
    pub fn update_samples(thread_id: &str, dataset_fqn: &str, rows: Vec<Vec<String>>) {
        let mut entry = ctx_cache().get(thread_id).map(|e| e.clone()).unwrap_or_default();
        entry.samples.insert(dataset_fqn.to_string(), rows);
        entry.updated_at = Some(Instant::now());
        ctx_cache().insert(thread_id.to_string(), entry);
    }
}

impl ThreadStore {
    pub fn new(storage: Arc<dyn StorageAdapter>, scope: RequestScope, keyspace: Arc<dyn Keyspace>) -> Self {
        Self { storage, scope, keyspace }
    }

    fn key(&self, thread_id: &str) -> String {
        self.keyspace
            .thread_key(&self.scope, thread_id)
            .unwrap_or_else(|_| format!("invalid/thread/{}.json", thread_id))
    }

    fn list_prefix(&self) -> String {
        format!("{}/", self.keyspace.threads_prefix(&self.scope).trim_end_matches('/'))
    }

    pub async fn append_step(&self, thread_id: &str, step: ThreadStep) -> Result<(), String> {
        let key = self.key(thread_id);
        // Try cache first to avoid extra GETs
        let mut log = match cache().get(thread_id) {
            Some(entry) => entry.log.clone(),
            None => {
                if let Ok(v) = self.storage.get_json(&key).await {
                    serde_json::from_value::<ThreadLog>(v).unwrap_or_default()
                } else {
                    ThreadLog::default()
                }
            }
        };
        log.steps.push(step);
        let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
        self.storage.put_json(&key, &val).await?;
        // Update cache
        cache().insert(thread_id.to_string(), CacheEntry { log, ts: Instant::now() });
        Ok(())
    }

    pub async fn get(&self, thread_id: &str) -> Option<ThreadLog> {
        let key = self.key(thread_id);
        // Serve from cache if fresh (5 seconds)
        if let Some(entry) = cache().get(thread_id) {
            if entry.ts.elapsed().as_secs() < 5 {
                return Some(entry.log.clone());
            }
        }
        if let Ok(v) = self.storage.get_json(&key).await {
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
            if let Some(ref log) = log_opt {
                cache().insert(thread_id.to_string(), CacheEntry { log: log.clone(), ts: Instant::now() });
            }
            log_opt
        } else {
            None
        }
    }

    pub async fn list(&self) -> Vec<String> {
        let prefix = self.list_prefix();
        let mut out: Vec<String> = Vec::new();
        if let Ok(keys) = self.storage.list_prefix(&prefix).await {
            for k in keys {
                if let Some(name) = k.strip_prefix(&prefix).and_then(|s| s.strip_suffix(".json")) {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        out
    }

    pub async fn delete(&self, thread_id: &str) -> Result<(), String> {
        let key = self.key(thread_id);
        self.storage.delete_object(&key).await?;
        Ok(())
    }

    pub async fn set_title_if_absent(&self, thread_id: &str, title: &str) -> Result<(), String> {
        let key = self.key(thread_id);
        let mut log = cache().get(thread_id).map(|e| e.log.clone()).unwrap_or_else(|| ThreadLog::default());
        if log.title.is_none() || log.title.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
            log.title = Some(title.to_string());
            let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
            self.storage.put_json(&key, &val).await?;
            cache().insert(thread_id.to_string(), CacheEntry { log, ts: Instant::now() });
        }
        Ok(())
    }

    pub async fn finalize_title(&self, thread_id: &str, title: &str) -> Result<(), String> {
        let key = self.key(thread_id);
        let mut log = cache().get(thread_id).map(|e| e.log.clone()).unwrap_or_else(|| ThreadLog::default());
        if !log.title_finalized {
            log.title = Some(title.to_string());
            log.title_finalized = true;
            let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
            self.storage.put_json(&key, &val).await?;
            cache().insert(thread_id.to_string(), CacheEntry { log, ts: Instant::now() });
        }
        Ok(())
    }
}


