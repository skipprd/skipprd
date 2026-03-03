use serde_json::Value;
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;
use std::time::Instant;

use super::{
    cache, CacheEntry, ControlStateEnvelope, ThreadLog, ThreadState, ThreadStep, ThreadStore,
    CONTROL_STATE_ENVELOPE_SCHEMA_VERSION, THREAD_SCHEMA_VERSION, THREAD_STATE_SCHEMA_VERSION,
};

impl ThreadStore {
    pub fn new(
        storage: Arc<dyn crate::storage::StorageAdapter>,
        scope: crate::scope::RequestScope,
        keyspace: Arc<dyn crate::keyspace::Keyspace>,
    ) -> Self {
        Self {
            storage,
            scope,
            keyspace,
        }
    }

    pub(crate) fn key(&self, thread_id: &str) -> String {
        self.keyspace
            .thread_key(&self.scope, thread_id)
            .unwrap_or_else(|_| format!("invalid/thread/{}.json", thread_id))
    }

    pub(crate) fn state_key(&self, thread_id: &str) -> String {
        self.keyspace
            .thread_state_key(&self.scope, thread_id)
            .unwrap_or_else(|_| format!("invalid/state/{}/state.json", thread_id))
    }

    fn list_prefix(&self) -> String {
        format!(
            "{}/",
            self.keyspace
                .threads_prefix(&self.scope)
                .trim_end_matches('/')
        )
    }

    fn cache_key_for(&self, key: &str) -> String {
        // Cache is process-global; include storage identity to avoid collisions in tests
        // (and in any multi-tenant multi-store deployments).
        format!("{:p}|{}", Arc::as_ptr(&self.storage), key)
    }

    fn ensure_thread_log_schema(log: &ThreadLog) -> Result<(), String> {
        if log.schema_version != THREAD_SCHEMA_VERSION {
            return Err(format!(
                "thread schema_version mismatch: expected {}, got {}",
                THREAD_SCHEMA_VERSION, log.schema_version
            ));
        }
        Ok(())
    }

    async fn load_thread_log_for_write(&self, key: &str) -> Result<ThreadLog, String> {
        let cache_key = self.cache_key_for(key);
        let log = if let Some(entry) = cache().get(&cache_key) {
            entry.log.clone()
        } else if let Ok(v) = self.storage.get_json(key).await {
            serde_json::from_value::<ThreadLog>(v)
                .map_err(|e| format!("failed to parse thread log: {e}"))?
        } else {
            ThreadLog::default()
        };
        Self::ensure_thread_log_schema(&log)?;
        Ok(log)
    }

    pub async fn append_step(&self, thread_id: &str, step: ThreadStep) -> Result<(), String> {
        let key = self.key(thread_id);
        let cache_key = self.cache_key_for(&key);
        let mut log = self.load_thread_log_for_write(&key).await?;
        log.steps.push(step.clone());
        let step_count = log.steps.len();
        let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
        self.storage.put_json(&key, &val).await?;
        cache().insert(
            cache_key,
            CacheEntry {
                log,
                ts: Instant::now(),
            },
        );

        // Keep thread_state strongly consistent with the persisted step sequence. We do not fail
        // append_step after the log write succeeds, but we must surface materialization failures.
        if let Err(e) = self
            .materialize_thread_state_incremental(thread_id, step_count, &step)
            .await
        {
            tracing::warn!(
                "thread_state_materialize_failed thread_id={} step_count={} error={}",
                thread_id,
                step_count,
                e
            );
        }
        Ok(())
    }

    pub async fn get_thread_state(&self, thread_id: &str) -> Result<ThreadState, String> {
        let key = self.state_key(thread_id);
        let v = self.storage.get_json(&key).await?;
        let s = serde_json::from_value::<ThreadState>(v)
            .map_err(|e| format!("failed to parse thread state: {e}"))?;
        if s.thread_state_schema_version != THREAD_STATE_SCHEMA_VERSION {
            return Err(format!(
                "thread_state schema_version mismatch: expected {}, got {}",
                THREAD_STATE_SCHEMA_VERSION, s.thread_state_schema_version
            ));
        }
        Ok(s)
    }

    pub async fn put_thread_state(&self, thread_id: &str, state: &ThreadState) -> Result<(), String> {
        if state.thread_state_schema_version != THREAD_STATE_SCHEMA_VERSION {
            return Err(format!(
                "thread_state schema_version mismatch: expected {}, got {}",
                THREAD_STATE_SCHEMA_VERSION, state.thread_state_schema_version
            ));
        }
        if state.thread_id != thread_id {
            return Err(format!(
                "thread_state thread_id mismatch: expected {}, got {}",
                thread_id, state.thread_id
            ));
        }
        // Non-destructive write: merge incoming patch with existing persisted state.
        let mut merged = self
            .get_thread_state(thread_id)
            .await
            .unwrap_or_else(|_| Self::new_thread_state(thread_id));
        if let Some(v) = state.suite_id.clone() {
            merged.suite_id = Some(v);
        }
        if let Some(v) = state.agent_type.clone() {
            merged.agent_type = Some(v);
        }
        if let Some(v) = state.current_phase.clone() {
            merged.current_phase = Some(v);
        }
        merged.last_materialized_step_count = merged
            .last_materialized_step_count
            .max(state.last_materialized_step_count);
        merged.total_runtime_ms = merged.total_runtime_ms.max(state.total_runtime_ms);
        for (k, v) in state.items.iter() {
            merged.items.insert(k.clone(), v.clone());
        }
        if let Some(v) = state.suite_state.clone() {
            merged.suite_state = Some(v);
        }
        if let Some(v) = state.control_state.clone() {
            merged.control_state = Some(v);
        }
        if state.bootstrap.catalog.is_some() {
            merged.bootstrap.catalog = state.bootstrap.catalog.clone();
        }
        merged.thread_state_schema_version = THREAD_STATE_SCHEMA_VERSION;
        merged.thread_id = thread_id.to_string();
        self.put_thread_state_replace(thread_id, &merged).await
    }

    pub async fn put_thread_state_replace(
        &self,
        thread_id: &str,
        state: &ThreadState,
    ) -> Result<(), String> {
        if state.thread_state_schema_version != THREAD_STATE_SCHEMA_VERSION {
            return Err(format!(
                "thread_state schema_version mismatch: expected {}, got {}",
                THREAD_STATE_SCHEMA_VERSION, state.thread_state_schema_version
            ));
        }
        if state.thread_id != thread_id {
            return Err(format!(
                "thread_state thread_id mismatch: expected {}, got {}",
                thread_id, state.thread_id
            ));
        }
        let key = self.state_key(thread_id);
        let v = serde_json::to_value(state).map_err(|e| e.to_string())?;
        self.storage.put_json(&key, &v).await
    }

    fn decode_control_state_payload(raw: &Value, expected_suite_id: &str) -> Option<Value> {
        if let Ok(env) = serde_json::from_value::<ControlStateEnvelope>(raw.clone()) {
            if env.schema_version == CONTROL_STATE_ENVELOPE_SCHEMA_VERSION
                && env.suite_id.trim() == expected_suite_id.trim()
            {
                return Some(env.payload);
            }
            return None;
        }
        None
    }

    fn encode_control_state_payload(suite_id: &str, payload: Value) -> Result<Value, String> {
        serde_json::to_value(ControlStateEnvelope {
            schema_version: CONTROL_STATE_ENVELOPE_SCHEMA_VERSION,
            suite_id: suite_id.trim().to_string(),
            payload,
        })
        .map_err(|e| e.to_string())
    }

    pub async fn load_control_state_payload(
        &self,
        thread_id: &str,
        suite_id: &str,
    ) -> Result<Option<Value>, String> {
        let state = self.get_thread_state(thread_id).await?;
        let Some(raw) = state.control_state else {
            return Ok(None);
        };
        Ok(Self::decode_control_state_payload(&raw, suite_id))
    }

    pub async fn save_control_state_payload(
        &self,
        thread_id: &str,
        suite_id: &str,
        payload: Value,
    ) -> Result<(), String> {
        let mut state = self
            .get_thread_state(thread_id)
            .await
            .unwrap_or_else(|_| Self::new_thread_state(thread_id));
        state.control_state = Some(Self::encode_control_state_payload(suite_id, payload)?);
        self.put_thread_state(thread_id, &state).await
    }

    pub async fn mutate_control_state_payload(
        &self,
        thread_id: &str,
        suite_id: &str,
        mutate: impl FnOnce(Option<Value>) -> Result<Value, String>,
    ) -> Result<Value, String> {
        let current = self.load_control_state_payload(thread_id, suite_id).await?;
        let next = mutate(current)?;
        self.save_control_state_payload(thread_id, suite_id, next.clone())
            .await?;
        Ok(next)
    }

    pub async fn load_typed_control_state<T: DeserializeOwned>(
        &self,
        thread_id: &str,
        suite_id: &str,
    ) -> Result<Option<T>, String> {
        let Some(payload) = self.load_control_state_payload(thread_id, suite_id).await? else {
            return Ok(None);
        };
        let parsed = serde_json::from_value::<T>(payload)
            .map_err(|e| format!("failed to parse typed control state payload: {e}"))?;
        Ok(Some(parsed))
    }

    pub async fn save_typed_control_state<T: Serialize>(
        &self,
        thread_id: &str,
        suite_id: &str,
        state: &T,
    ) -> Result<(), String> {
        let payload = serde_json::to_value(state)
            .map_err(|e| format!("failed to serialize typed control state payload: {e}"))?;
        self.save_control_state_payload(thread_id, suite_id, payload).await
    }

    pub async fn mutate_typed_control_state<T: Serialize + DeserializeOwned>(
        &self,
        thread_id: &str,
        suite_id: &str,
        mutate: impl FnOnce(Option<T>) -> Result<T, String>,
    ) -> Result<T, String> {
        let current = self.load_typed_control_state::<T>(thread_id, suite_id).await?;
        let next = mutate(current)?;
        self.save_typed_control_state(thread_id, suite_id, &next).await?;
        Ok(next)
    }

    pub async fn get_thread_events_from_log(&self, thread_id: &str) -> Result<Vec<super::ThreadEvent>, String> {
        let log = self.get(thread_id).await?;
        Ok(super::build_thread_events_from_log(&log, 200))
    }

    pub async fn get(&self, thread_id: &str) -> Result<ThreadLog, String> {
        let key = self.key(thread_id);
        let cache_key = self.cache_key_for(&key);
        // Serve from cache if fresh (5 seconds)
        if let Some(entry) = cache().get(&cache_key) {
            if entry.ts.elapsed().as_secs() < 5 {
                return Ok(entry.log.clone());
            }
        }
        let v = self
            .storage
            .get_json(&key)
            .await
            .map_err(|e| e.to_string())?;
        let log = serde_json::from_value::<ThreadLog>(v)
            .map_err(|e| format!("failed to parse thread log: {e}"))?;
        Self::ensure_thread_log_schema(&log)?;
        cache().insert(
            cache_key,
            CacheEntry {
                log: log.clone(),
                ts: Instant::now(),
            },
        );
        Ok(log)
    }

    pub async fn list(&self) -> Vec<String> {
        let prefix = self.list_prefix();
        let mut out: Vec<String> = Vec::new();
        if let Ok(keys) = self.storage.list_prefix(&prefix).await {
            for k in keys {
                // Backward-compat defensive filter: return only thread logs.
                if k.ends_with(".state.json") {
                    continue;
                }
                if let Some(name) = k
                    .strip_prefix(&prefix)
                    .and_then(|s| s.strip_suffix(".json"))
                {
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
        let cache_key = self.cache_key_for(&key);
        let mut log = self.load_thread_log_for_write(&key).await?;
        if log.title.is_none() || log.title.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
            log.title = Some(title.to_string());
            let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
            self.storage.put_json(&key, &val).await?;
            cache().insert(
                cache_key,
                CacheEntry {
                    log,
                    ts: Instant::now(),
                },
            );
        }
        Ok(())
    }

    pub async fn finalize_title(&self, thread_id: &str, title: &str) -> Result<(), String> {
        let key = self.key(thread_id);
        let cache_key = self.cache_key_for(&key);
        let mut log = self.load_thread_log_for_write(&key).await?;
        if !log.title_finalized {
            log.title = Some(title.to_string());
            log.title_finalized = true;
            let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
            self.storage.put_json(&key, &val).await?;
            cache().insert(
                cache_key,
                CacheEntry {
                    log,
                    ts: Instant::now(),
                },
            );
        }
        Ok(())
    }
}
