use serde::{Deserialize, Serialize};
use serde_json::Value;
use once_cell::sync::OnceCell;
use dashmap::DashMap;
use std::time::Instant;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::storage::StorageAdapter;
use crate::keyspace::Keyspace;
use crate::scope::RequestScope;

pub const THREAD_SCHEMA_VERSION: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub ok: bool,
    pub errors: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Observation {
    pub fn ok() -> Self {
        Self { ok: true, errors: Vec::new(), warnings: Vec::new() }
    }

    pub fn fail(errors: Vec<String>) -> Self {
        Self { ok: false, errors, warnings: Vec::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolObservation {
    pub ok: bool,
    pub errors: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Tool-specific payload (written_keys, rows, etc.).
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl ToolObservation {
    pub fn ok(extra: BTreeMap<String, Value>) -> Self {
        Self { ok: true, errors: Vec::new(), warnings: Vec::new(), extra }
    }

    pub fn fail(errors: Vec<String>, extra: BTreeMap<String, Value>) -> Self {
        Self { ok: false, errors, warnings: Vec::new(), extra }
    }

    /// Convert any legacy tool output `Value` into the canonical envelope:
    /// - `errors` is ALWAYS present (even if 0/1)
    /// - legacy `error: string` is converted into `errors: [error]` and removed from `extra`
    pub fn normalize(v: Value) -> Self {
        let mut extra: BTreeMap<String, Value> = match v {
            Value::Object(m) => m.into_iter().collect(),
            other => {
                let mut m = BTreeMap::new();
                m.insert("raw".to_string(), other);
                m
            }
        };

        let ok = extra
            .get("ok")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);

        let errors = if let Some(Value::Array(arr)) = extra.get("errors") {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
        } else if let Some(err) = extra.get("error").and_then(|x| x.as_str()) {
            let s = err.trim().to_string();
            if s.is_empty() { Vec::new() } else { vec![s] }
        } else {
            Vec::new()
        };

        let warnings = if let Some(Value::Array(arr)) = extra.get("warnings") {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        // Remove canonical envelope keys from extra (and legacy `error`).
        extra.remove("ok");
        extra.remove("errors");
        extra.remove("warnings");
        extra.remove("error");

        // If this is a failure and we still have no errors, force one.
        let mut errors = errors;
        if !ok && errors.is_empty() {
            errors.push("unknown error".to_string());
        }

        Self { ok, errors, warnings, extra }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadStep {
    SwitchSuite {
        from: Option<String>,
        to: String,
        observation: Observation,
        ts: String,
        agent: String,
    },
    SwitchAgent {
        from: Option<String>,
        to: String,
        observation: Observation,
        ts: String,
        agent: String,
    },
    User {
        text: String,
        observation: Observation,
        ts: String,
        agent: String,
    },
    Tool {
        name: String,
        args: Value,
        observation: ToolObservation,
        ts: String,
        agent: String,
    },
    /// LLM call observability (hashed/deduped prompt parts + response).
    ///
    /// Notes:
    /// - `parts` MUST constitute the entire prompt (including system prompts, hardcoded strings, etc.).
    /// - Each element of `parts` is a JSON object with (at minimum) `{name, hash, text}` where
    ///   `text` is either the full (redacted) part content or the literal string `"unchanged"`.
    /// - `prompt_hash` is sha256 over the exact serialized message list used in the call.
    LlmCall {
        call_id: u64,
        /// Best-effort model identifier (provider/model name).
        model: String,
        /// Best-effort phase identifier (suite phase or core react loop label).
        phase: String,
        /// sha256 hex of the exact serialized message list (system/user/tool).
        prompt_hash: String,
        /// Prompt parts in order; each part may include `"text":"unchanged"` when deduped.
        parts: Vec<Value>,
        /// Stable part hashes keyed by part name.
        part_hashes: BTreeMap<String, String>,
        /// sha256 hex of the raw response text.
        response_hash: String,
        /// Full (redacted) response text when enabled.
        #[serde(default)]
        response_text: Option<String>,
        observation: Observation,
        ts: String,
        agent: String,
    },
    Phase {
        phase: String,
        from_phase: Option<String>,
        reason_code: Option<String>,
        reason_detail: Option<Value>,
        observation: Observation,
        ts: String,
        agent: String,
    },
    GuardBlock {
        phase: String,
        kind: String,
        reason: String,
        observation: Observation,
        ts: String,
        agent: String,
    },
    ArtifactFocus {
        kind: String,
        name: String,
        dataset_id: Option<String>,
        exists: bool,
        observation: Observation,
        ts: String,
        agent: String,
    },
    ArtifactSaved {
        kind: String,
        name: String,
        dataset_id: Option<String>,
        key: String,
        status: String,
        lines_added: u64,
        lines_removed: u64,
        observation: Observation,
        ts: String,
        agent: String,
    },
    AskUser {
        prompt: String,
        observation: Observation,
        ts: String,
        agent: String,
    },
    AskApproval {
        prompt: String,
        observation: Observation,
        ts: String,
        agent: String,
    },
    ReviewResponse {
        text: String,
        #[serde(default)]
        meta: Option<Value>,
        observation: Observation,
        ts: String,
        agent: String,
    },
    Final {
        kind: String,
        payload: Value,
        #[serde(default)]
        display: Option<String>,
        observation: Observation,
        ts: String,
        agent: String,
    },
}

impl ThreadStep {
    pub fn ts(&self) -> &str {
        match self {
            ThreadStep::SwitchSuite { ts, .. } => ts,
            ThreadStep::SwitchAgent { ts, .. } => ts,
            ThreadStep::User { ts, .. } => ts,
            ThreadStep::Tool { ts, .. } => ts,
            ThreadStep::LlmCall { ts, .. } => ts,
            ThreadStep::Phase { ts, .. } => ts,
            ThreadStep::GuardBlock { ts, .. } => ts,
            ThreadStep::ArtifactFocus { ts, .. } => ts,
            ThreadStep::ArtifactSaved { ts, .. } => ts,
            ThreadStep::AskUser { ts, .. } => ts,
            ThreadStep::AskApproval { ts, .. } => ts,
            ThreadStep::ReviewResponse { ts, .. } => ts,
            ThreadStep::Final { ts, .. } => ts,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ThreadLog {
    pub schema_version: u32,
    pub steps: Vec<ThreadStep>,
    pub result: Option<ThreadResult>,
    pub title: Option<String>,
    pub title_finalized: bool,
}

impl Default for ThreadLog {
    fn default() -> Self {
        Self {
            schema_version: THREAD_SCHEMA_VERSION,
            steps: Vec::new(),
            result: None,
            title: None,
            title_finalized: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ThreadResult {
    pub kind: String,
    pub payload: Value,
    #[serde(default)]
    pub display: Option<String>,
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
    /// Last published curated relations (materialized in the warehouse), best-effort.
    pub published_relations: Vec<String>, // dataset FQN list (e.g. catalog.db.table)
    /// Digest of the manifest that produced `published_relations`, best-effort.
    pub published_manifest_sha256: Option<String>,
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

    pub fn update_published(thread_id: &str, manifest_sha256: &str, relations: Vec<String>) {
        let mut entry = ctx_cache().get(thread_id).map(|e| e.clone()).unwrap_or_default();
        entry.published_relations = relations;
        entry.published_manifest_sha256 = Some(manifest_sha256.to_string());
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
        let mut log = if let Some(entry) = cache().get(thread_id) {
            entry.log.clone()
        } else if let Ok(v) = self.storage.get_json(&key).await {
            serde_json::from_value::<ThreadLog>(v)
                .map_err(|e| format!("failed to parse thread log: {e}"))?
        } else {
            ThreadLog::default()
        };
        if log.schema_version != THREAD_SCHEMA_VERSION {
            return Err(format!(
                "thread schema_version mismatch: expected {}, got {}",
                THREAD_SCHEMA_VERSION, log.schema_version
            ));
        }
        log.steps.push(step);
        let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
        self.storage.put_json(&key, &val).await?;
        // Update cache
        cache().insert(thread_id.to_string(), CacheEntry { log, ts: Instant::now() });
        Ok(())
    }

    pub async fn get(&self, thread_id: &str) -> Result<ThreadLog, String> {
        let key = self.key(thread_id);
        // Serve from cache if fresh (5 seconds)
        if let Some(entry) = cache().get(thread_id) {
            if entry.ts.elapsed().as_secs() < 5 {
                return Ok(entry.log.clone());
            }
        }
        let v = self.storage.get_json(&key).await.map_err(|e| e.to_string())?;
        let log = serde_json::from_value::<ThreadLog>(v).map_err(|e| format!("failed to parse thread log: {e}"))?;
        if log.schema_version != THREAD_SCHEMA_VERSION {
            return Err(format!(
                "thread schema_version mismatch: expected {}, got {}",
                THREAD_SCHEMA_VERSION, log.schema_version
            ));
        }
        cache().insert(thread_id.to_string(), CacheEntry { log: log.clone(), ts: Instant::now() });
        Ok(log)
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
        let mut log = match cache().get(thread_id) {
            Some(e) => e.log.clone(),
            None => {
                let v = self.storage.get_json(&key).await.map_err(|e| e.to_string())?;
                serde_json::from_value::<ThreadLog>(v).map_err(|e| format!("failed to parse thread log: {e}"))?
            }
        };
        if log.schema_version != THREAD_SCHEMA_VERSION {
            return Err(format!(
                "thread schema_version mismatch: expected {}, got {}",
                THREAD_SCHEMA_VERSION, log.schema_version
            ));
        }
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
        let mut log = match cache().get(thread_id) {
            Some(e) => e.log.clone(),
            None => {
                let v = self.storage.get_json(&key).await.map_err(|e| e.to_string())?;
                serde_json::from_value::<ThreadLog>(v).map_err(|e| format!("failed to parse thread log: {e}"))?
            }
        };
        if log.schema_version != THREAD_SCHEMA_VERSION {
            return Err(format!(
                "thread schema_version mismatch: expected {}, got {}",
                THREAD_SCHEMA_VERSION, log.schema_version
            ));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_observation_normalizes_legacy_error_field_into_errors_array() {
        let obs = ToolObservation::normalize(serde_json::json!({"ok": false, "error": "boom"}));
        assert!(!obs.ok);
        assert_eq!(obs.errors, vec!["boom".to_string()]);
        assert!(!obs.extra.contains_key("error"));
    }

    #[test]
    fn thread_log_v1_missing_schema_version_fails_to_deserialize() {
        // This mimics the old v1 persisted shape: no schema_version, and stringly-typed steps.
        let v1 = serde_json::json!({
            "steps": [{
                "action": "user",
                "args": {"text": "hi"},
                "observation": {"ok": true},
                "ts": "t",
                "agent": "ask"
            }],
            "result": null
        });
        assert!(serde_json::from_value::<ThreadLog>(v1).is_err());
    }

    #[test]
    fn unknown_fields_in_thread_step_fail_to_deserialize() {
        let bad = serde_json::json!({
            "schema_version": THREAD_SCHEMA_VERSION,
            "steps": [{
                "type": "user",
                "text": "hi",
                "observation": { "ok": true, "errors": [], "warnings": [] },
                "ts": "t",
                "agent": "ask",
                "unexpected": 123
            }],
            "result": null,
            "title": null,
            "title_finalized": false
        });
        assert!(serde_json::from_value::<ThreadLog>(bad).is_err());
    }

    #[test]
    fn thread_log_round_trips_with_llm_call_step() {
        let log = ThreadLog {
            schema_version: THREAD_SCHEMA_VERSION,
            steps: vec![ThreadStep::LlmCall {
                call_id: 1,
                model: "unknown".to_string(),
                phase: "test".to_string(),
                prompt_hash: "p".to_string(),
                parts: vec![serde_json::json!({"name":"system","hash":"h","text":"hello"})],
                part_hashes: BTreeMap::from([("system".to_string(), "h".to_string())]),
                response_hash: "r".to_string(),
                response_text: Some("ok".to_string()),
                observation: Observation::ok(),
                ts: "t".to_string(),
                agent: "a".to_string(),
            }],
            result: None,
            title: None,
            title_finalized: false,
        };
        let v = serde_json::to_value(&log).unwrap();
        let parsed: ThreadLog = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.schema_version, THREAD_SCHEMA_VERSION);
        assert_eq!(parsed.steps.len(), 1);
        match &parsed.steps[0] {
            ThreadStep::LlmCall { call_id, phase, response_text, .. } => {
                assert_eq!(*call_id, 1);
                assert_eq!(phase, "test");
                assert_eq!(response_text.as_deref(), Some("ok"));
            }
            _ => panic!("expected llm_call step"),
        }
    }

    #[test]
    fn thread_log_serializes_and_deserializes_llm_call_step() {
        let step = ThreadStep::LlmCall {
            call_id: 1,
            model: "m".to_string(),
            phase: "p".to_string(),
            prompt_hash: "ph".to_string(),
            parts: vec![serde_json::json!({"name":"system","hash":"h","text":"x"})],
            part_hashes: BTreeMap::from([("system".to_string(), "h".to_string())]),
            response_hash: "rh".to_string(),
            response_text: Some("resp".to_string()),
            observation: Observation::ok(),
            ts: "t".to_string(),
            agent: "a".to_string(),
        };
        let log = ThreadLog {
            schema_version: THREAD_SCHEMA_VERSION,
            steps: vec![step],
            result: None,
            title: None,
            title_finalized: false,
        };
        let v = serde_json::to_value(&log).unwrap();
        let parsed: ThreadLog = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.steps.len(), 1);
        match &parsed.steps[0] {
            ThreadStep::LlmCall { call_id, model, phase, response_text, .. } => {
                assert_eq!(*call_id, 1);
                assert_eq!(model, "m");
                assert_eq!(phase, "p");
                assert_eq!(response_text.as_deref(), Some("resp"));
            }
            _ => panic!("expected llm_call"),
        }
    }
}
