use dashmap::DashMap;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use crate::control_flow::{GuardBlockKind, PhaseReasonCode};
use crate::keyspace::Keyspace;
use crate::scope::RequestScope;
use crate::storage::StorageAdapter;

pub const THREAD_SCHEMA_VERSION: u32 = 4;
pub const THREAD_STATE_SCHEMA_VERSION: u32 = 1;

/// Materialized, reloadable thread state (stable summary, not raw streaming events).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThreadState {
    pub thread_state_schema_version: u32,
    pub thread_id: String,
    #[serde(default)]
    pub suite_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub current_phase: Option<String>,
    /// Number of thread steps that have been materialized into this state snapshot.
    #[serde(default)]
    pub last_materialized_step_count: usize,
    /// Sum of completed phase runtimes in milliseconds.
    #[serde(default)]
    pub total_runtime_ms: u64,
    /// Per-item semaphore/state keyed by stable item ids.
    #[serde(default)]
    pub items: BTreeMap<String, ThreadItemState>,
    /// Recent, bounded timeline events (tool_start/tool_end) for "post reconnect" UIs.
    #[serde(default)]
    pub events: Vec<ThreadEvent>,
    /// Opaque suite-owned state snapshot.
    #[serde(default)]
    pub suite_state: Option<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThreadEvent {
    pub step_idx: usize,
    pub event_kind: ThreadEventKind,
    pub ts: String,
    #[serde(default)]
    pub tool_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub clean_name: Option<String>,
    #[serde(default)]
    pub status: Option<ThreadEventStatus>,
    #[serde(default)]
    pub runtime_ms: Option<u64>,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub call_id: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub phase: Option<String>,

    pub ctx: Option<ExecutionContext>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ThreadEventKind {
    #[default]
    ToolStart,
    ToolEnd,
    LlmStart,
    LlmEnd,
}

impl ThreadEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadEventKind::ToolStart => "tool_start",
            ThreadEventKind::ToolEnd => "tool_end",
            ThreadEventKind::LlmStart => "llm_start",
            ThreadEventKind::LlmEnd => "llm_end",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ThreadEventStatus {
    Running,
    Ok,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ToolStepStatus {
    Running,
    Ok,
    Failed,
}

impl ToolStepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolStepStatus::Running => "running",
            ToolStepStatus::Ok => "ok",
            ToolStepStatus::Failed => "failed",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LlmStepStatus {
    Ok,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FinalKind {
    Generic,
    Ask,
    Kb,
    Other(String),
}

impl FinalKind {
    pub fn as_str(&self) -> &str {
        match self {
            FinalKind::Generic => "generic",
            FinalKind::Ask => "ask",
            FinalKind::Kb => "kb",
            FinalKind::Other(s) => s.as_str(),
        }
    }
}

impl Default for FinalKind {
    fn default() -> Self {
        FinalKind::Generic
    }
}

impl From<String> for FinalKind {
    fn from(value: String) -> Self {
        match value.as_str() {
            "generic" => FinalKind::Generic,
            "ask" => FinalKind::Ask,
            "kb" => FinalKind::Kb,
            _ => FinalKind::Other(value),
        }
    }
}

impl From<FinalKind> for String {
    fn from(value: FinalKind) -> Self {
        match value {
            FinalKind::Generic => "generic".to_string(),
            FinalKind::Ask => "ask".to_string(),
            FinalKind::Kb => "kb".to_string(),
            FinalKind::Other(s) => s,
        }
    }
}

impl Serialize for FinalKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FinalKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(FinalKind::from(s))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    Model,
    Metric,
    Other(String),
}

impl ArtifactKind {
    pub fn as_str(&self) -> &str {
        match self {
            ArtifactKind::Model => "model",
            ArtifactKind::Metric => "metric",
            ArtifactKind::Other(s) => s.as_str(),
        }
    }
}

impl From<String> for ArtifactKind {
    fn from(value: String) -> Self {
        match value.as_str() {
            "model" => ArtifactKind::Model,
            "metric" => ArtifactKind::Metric,
            _ => ArtifactKind::Other(value),
        }
    }
}

impl From<ArtifactKind> for String {
    fn from(value: ArtifactKind) -> Self {
        match value {
            ArtifactKind::Model => "model".to_string(),
            ArtifactKind::Metric => "metric".to_string(),
            ArtifactKind::Other(s) => s,
        }
    }
}

impl Serialize for ArtifactKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(ArtifactKind::from(s))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactSaveStatus {
    Added,
    Modified,
    Ok,
    Failed,
    Other(String),
}

impl ArtifactSaveStatus {
    pub fn as_str(&self) -> &str {
        match self {
            ArtifactSaveStatus::Added => "added",
            ArtifactSaveStatus::Modified => "modified",
            ArtifactSaveStatus::Ok => "ok",
            ArtifactSaveStatus::Failed => "failed",
            ArtifactSaveStatus::Other(s) => s.as_str(),
        }
    }
}

impl From<String> for ArtifactSaveStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "added" => ArtifactSaveStatus::Added,
            "modified" => ArtifactSaveStatus::Modified,
            "ok" => ArtifactSaveStatus::Ok,
            "failed" => ArtifactSaveStatus::Failed,
            _ => ArtifactSaveStatus::Other(value),
        }
    }
}

impl From<ArtifactSaveStatus> for String {
    fn from(value: ArtifactSaveStatus) -> Self {
        match value {
            ArtifactSaveStatus::Added => "added".to_string(),
            ArtifactSaveStatus::Modified => "modified".to_string(),
            ArtifactSaveStatus::Ok => "ok".to_string(),
            ArtifactSaveStatus::Failed => "failed".to_string(),
            ArtifactSaveStatus::Other(s) => s,
        }
    }
}

impl Serialize for ArtifactSaveStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactSaveStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(ArtifactSaveStatus::from(s))
    }
}

/// Opaque execution context for hierarchical UI rendering.
///
/// Suites own the schema; core only persists/forwards this payload.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContext {
    #[serde(default)]
    pub plan_kind: Option<ExecutionPlanKind>,
    #[serde(default)]
    pub plan_key: Option<String>,
    #[serde(default)]
    pub workgroup_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub checklist_item_id: Option<String>,
    #[serde(default)]
    pub data: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct ExecutionPlanKind(pub String);

impl ExecutionPlanKind {
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ThreadItemKind {
    #[default]
    Phase,
    Tool,
    Task,
}

impl ThreadItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadItemKind::Phase => "phase",
            ThreadItemKind::Tool => "tool",
            ThreadItemKind::Task => "task",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ThreadItemStatus {
    Queued,
    #[default]
    Running,
    Ok,
    Failed,
    Blocked,
}

impl ThreadItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadItemStatus::Queued => "queued",
            ThreadItemStatus::Running => "running",
            ThreadItemStatus::Ok => "ok",
            ThreadItemStatus::Failed => "failed",
            ThreadItemStatus::Blocked => "blocked",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThreadItemState {
    pub kind: ThreadItemKind,
    pub status: ThreadItemStatus,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    /// Runtime/duration in milliseconds (best-effort).
    #[serde(default)]
    pub runtime_ms: Option<u64>,
    #[serde(default)]
    pub last_error: Option<ThreadItemError>,
    #[serde(default)]
    pub outputs: Option<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThreadItemError {
    pub summary: String,
    #[serde(default)]
    pub tool_step_idx: Option<usize>,
    #[serde(default)]
    pub step_ts: Option<String>,
}

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
        Self {
            ok: true,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn fail(errors: Vec<String>) -> Self {
        Self {
            ok: false,
            errors,
            warnings: Vec::new(),
        }
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
        Self {
            ok: true,
            errors: Vec::new(),
            warnings: Vec::new(),
            extra,
        }
    }

    pub fn fail(errors: Vec<String>, extra: BTreeMap<String, Value>) -> Self {
        Self {
            ok: false,
            errors,
            warnings: Vec::new(),
            extra,
        }
    }

    fn non_empty_string_field(extra: &BTreeMap<String, Value>, key: &str) -> Option<String> {
        extra
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn extract_errors_from_extra(extra: &BTreeMap<String, Value>) -> Vec<String> {
        if let Some(Value::Array(arr)) = extra.get("errors") {
            let out: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect();
            if !out.is_empty() {
                return out;
            }
        }
        if let Some(v) = extra.get("error") {
            match v {
                Value::String(s) => {
                    let t = s.trim();
                    if !t.is_empty() {
                        return vec![t.to_string()];
                    }
                }
                other => {
                    let s = other.to_string();
                    if !s.trim().is_empty() {
                        return vec![s];
                    }
                }
            }
        }
        for key in ["message", "reason", "detail"] {
            if let Some(s) = Self::non_empty_string_field(extra, key) {
                return vec![s];
            }
        }
        Vec::new()
    }

    pub fn first_error_or_context(&self) -> Option<String> {
        self.errors.first().cloned().or_else(|| {
            for key in ["message", "reason", "detail"] {
                if let Some(s) = Self::non_empty_string_field(&self.extra, key) {
                    return Some(s);
                }
            }
            None
        })
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

        let ok = extra.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);

        let errors = Self::extract_errors_from_extra(&extra);

        let warnings = if let Some(Value::Array(arr)) = extra.get("warnings") {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let mut errors = errors;

        // Remove canonical envelope keys from extra (and legacy `error`).
        extra.remove("ok");
        extra.remove("errors");
        extra.remove("warnings");
        extra.remove("error");

        // If this is a failure and we still have no errors, force one.
        if !ok && errors.is_empty() {
            errors.push("no error details were captured".to_string());
        }

        Self {
            ok,
            errors,
            warnings,
            extra,
        }
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
    ToolStart {
        tool_id: String,
        name: String,
        /// Human-readable, short label for UI (e.g. "Read dbt_project.yml").
        #[serde(default)]
        clean_name: String,
        args: Value,
        /// running|ok|failed (tool_start should be running)
        status: ToolStepStatus,
        #[serde(default)]
        payload: Option<Value>,
        ctx: Option<ExecutionContext>,
        ts: String,
        agent: String,
    },
    ToolEnd {
        tool_id: String,
        name: String,
        /// Human-readable, short label for UI (e.g. "Read dbt_project.yml").
        #[serde(default)]
        clean_name: String,
        #[serde(default)]
        args: Value,
        /// running|ok|failed (tool_end should be ok|failed)
        status: ToolStepStatus,
        #[serde(default)]
        payload: Option<Value>,
        ctx: Option<ExecutionContext>,
        observation: ToolObservation,
        ts: String,
        agent: String,
    },
    LlmStart {
        call_id: u64,
        #[serde(default)]
        model: Option<String>,
        phase: String,
        ctx: Option<ExecutionContext>,
        ts: String,
        agent: String,
    },
    LlmEnd {
        call_id: u64,
        #[serde(default)]
        model: Option<String>,
        phase: String,
        status: LlmStepStatus, // ok|failed
        #[serde(default)]
        error: Option<String>,
        ctx: Option<ExecutionContext>,
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
        #[serde(default)]
        reason_code: Option<PhaseReasonCode>,
        reason_detail: Option<Value>,
        observation: Observation,
        ts: String,
        agent: String,
    },
    GuardBlock {
        phase: String,
        kind: GuardBlockKind,
        reason: String,
        observation: Observation,
        ts: String,
        agent: String,
    },
    ArtifactFocus {
        kind: ArtifactKind,
        name: String,
        dataset_id: Option<String>,
        exists: bool,
        observation: Observation,
        ts: String,
        agent: String,
    },
    ArtifactSaved {
        kind: ArtifactKind,
        name: String,
        dataset_id: Option<String>,
        key: String,
        status: ArtifactSaveStatus,
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
        kind: FinalKind,
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
            ThreadStep::ToolStart { ts, .. } => ts,
            ThreadStep::ToolEnd { ts, .. } => ts,
            ThreadStep::LlmStart { ts, .. } => ts,
            ThreadStep::LlmEnd { ts, .. } => ts,
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
    pub kind: FinalKind,
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
struct CacheEntry {
    log: ThreadLog,
    ts: Instant,
}
static THREAD_CACHE: OnceCell<DashMap<String, CacheEntry>> = OnceCell::new();
fn cache() -> &'static DashMap<String, CacheEntry> {
    THREAD_CACHE.get_or_init(|| DashMap::new())
}

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
fn ctx_cache() -> &'static DashMap<String, ThreadCache> {
    THREAD_CTX_CACHE.get_or_init(|| DashMap::new())
}

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
        let mut entry = ctx_cache()
            .get(thread_id)
            .map(|e| e.clone())
            .unwrap_or_default();
        entry.candidates = cands;
        entry.updated_at = Some(Instant::now());
        ctx_cache().insert(thread_id.to_string(), entry);
    }
    pub fn update_schema(thread_id: &str, dataset_fqn: &str, cols: Vec<(String, String)>) {
        let mut entry = ctx_cache()
            .get(thread_id)
            .map(|e| e.clone())
            .unwrap_or_default();
        entry.schemas.insert(dataset_fqn.to_string(), cols);
        entry.updated_at = Some(Instant::now());
        ctx_cache().insert(thread_id.to_string(), entry);
    }
    pub fn update_samples(thread_id: &str, dataset_fqn: &str, rows: Vec<Vec<String>>) {
        let mut entry = ctx_cache()
            .get(thread_id)
            .map(|e| e.clone())
            .unwrap_or_default();
        entry.samples.insert(dataset_fqn.to_string(), rows);
        entry.updated_at = Some(Instant::now());
        ctx_cache().insert(thread_id.to_string(), entry);
    }

    pub fn update_published(thread_id: &str, manifest_sha256: &str, relations: Vec<String>) {
        let mut entry = ctx_cache()
            .get(thread_id)
            .map(|e| e.clone())
            .unwrap_or_default();
        entry.published_relations = relations;
        entry.published_manifest_sha256 = Some(manifest_sha256.to_string());
        entry.updated_at = Some(Instant::now());
        ctx_cache().insert(thread_id.to_string(), entry);
    }
}

impl ThreadStore {
    pub fn new(
        storage: Arc<dyn StorageAdapter>,
        scope: RequestScope,
        keyspace: Arc<dyn Keyspace>,
    ) -> Self {
        Self {
            storage,
            scope,
            keyspace,
        }
    }

    fn key(&self, thread_id: &str) -> String {
        self.keyspace
            .thread_key(&self.scope, thread_id)
            .unwrap_or_else(|_| format!("invalid/thread/{}.json", thread_id))
    }

    fn state_key(&self, thread_id: &str) -> String {
        self.keyspace
            .thread_state_key(&self.scope, thread_id)
            .unwrap_or_else(|_| format!("invalid/thread/{}.state.json", thread_id))
    }

    fn artifact_key(&self, thread_id: &str, artifact_id: &str) -> String {
        self.keyspace
            .thread_artifact_key(&self.scope, thread_id, artifact_id)
            .unwrap_or_else(|_| format!("invalid/thread/{}.{}.json", thread_id, artifact_id))
    }

    fn list_prefix(&self) -> String {
        format!(
            "{}/",
            self.keyspace
                .threads_prefix(&self.scope)
                .trim_end_matches('/')
        )
    }

    pub async fn append_step(&self, thread_id: &str, step: ThreadStep) -> Result<(), String> {
        let key = self.key(thread_id);
        // Cache is process-global; include storage identity to avoid collisions in tests
        // (and in any multi-tenant multi-store deployments).
        let cache_key = format!("{:p}|{}", Arc::as_ptr(&self.storage), key);
        // Try cache first to avoid extra GETs
        let mut log = if let Some(entry) = cache().get(&cache_key) {
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
        log.steps.push(step.clone());
        let step_count = log.steps.len();
        let val = serde_json::to_value(&log).map_err(|e| e.to_string())?;
        self.storage.put_json(&key, &val).await?;
        // Update cache
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

    pub async fn put_thread_state(
        &self,
        thread_id: &str,
        state: &ThreadState,
    ) -> Result<(), String> {
        let key = self.state_key(thread_id);
        let v = serde_json::to_value(state).map_err(|e| e.to_string())?;
        self.storage.put_json(&key, &v).await
    }

    pub async fn get_thread_artifact_json(
        &self,
        thread_id: &str,
        artifact_id: &str,
    ) -> Result<Value, String> {
        let key = self.artifact_key(thread_id, artifact_id);
        self.storage.get_json(&key).await.map_err(|e| e.to_string())
    }

    pub async fn put_thread_artifact_json(
        &self,
        thread_id: &str,
        artifact_id: &str,
        value: &Value,
    ) -> Result<(), String> {
        let key = self.artifact_key(thread_id, artifact_id);
        self.storage.put_json(&key, value).await
    }

    pub async fn get_thread_artifact_typed<T>(
        &self,
        thread_id: &str,
        artifact_id: &str,
    ) -> Option<T>
    where
        T: crate::state::ThreadStateArtifact,
    {
        let v = self
            .get_thread_artifact_json(thread_id, artifact_id)
            .await
            .ok()?;
        let s = serde_json::from_value::<T>(v).ok()?;
        if s.schema_version() != T::SCHEMA_VERSION {
            return None;
        }
        Some(s)
    }

    pub async fn put_thread_artifact_typed<T>(
        &self,
        thread_id: &str,
        artifact_id: &str,
        value: &T,
    ) -> Result<(), String>
    where
        T: crate::state::ThreadStateArtifact,
    {
        if value.schema_version() != T::SCHEMA_VERSION {
            return Err(format!(
                "artifact schema_version mismatch for '{}': expected {}, got {}",
                artifact_id,
                T::SCHEMA_VERSION,
                value.schema_version()
            ));
        }
        let v = serde_json::to_value(value).map_err(|e| e.to_string())?;
        self.put_thread_artifact_json(thread_id, artifact_id, &v).await
    }

    fn new_thread_state(thread_id: &str) -> ThreadState {
        ThreadState {
            thread_state_schema_version: THREAD_STATE_SCHEMA_VERSION,
            thread_id: thread_id.to_string(),
            ..ThreadState::default()
        }
    }

    async fn materialize_thread_state_incremental(
        &self,
        thread_id: &str,
        want_step_count: usize,
        step: &ThreadStep,
    ) -> Result<(), String> {
        let mut state = self
            .get_thread_state(thread_id)
            .await
            .unwrap_or_else(|_| Self::new_thread_state(thread_id));
        if state.last_materialized_step_count > want_step_count.saturating_sub(1) {
            return Err(format!(
                "thread_state materialization mismatch: state_count={} want_step_count={}",
                state.last_materialized_step_count, want_step_count
            ));
        }
        if state.last_materialized_step_count < want_step_count.saturating_sub(1) {
            // Hard cutover: do not replay thread logs for state reconstruction.
            // If a gap is detected, reset to an empty state snapshot and continue incrementally.
            state = Self::new_thread_state(thread_id);
        }
        let mut paired: HashSet<String> = HashSet::new();
        match step {
            ThreadStep::ToolStart { tool_id, .. } | ThreadStep::ToolEnd { tool_id, .. } => {
                paired.insert(tool_id.clone());
            }
            _ => {}
        }
        apply_step_to_state(&mut state, want_step_count.saturating_sub(1), step, &paired);
        state.thread_state_schema_version = THREAD_STATE_SCHEMA_VERSION;
        state.thread_id = thread_id.to_string();
        state.last_materialized_step_count = want_step_count;
        state.total_runtime_ms = state
            .items
            .values()
            .filter(|it| it.kind == ThreadItemKind::Phase)
            .filter_map(|it| it.runtime_ms)
            .sum();
        self.put_thread_state(thread_id, &state).await?;
        Ok(())
    }

    pub async fn get(&self, thread_id: &str) -> Result<ThreadLog, String> {
        let key = self.key(thread_id);
        let cache_key = format!("{:p}|{}", Arc::as_ptr(&self.storage), key);
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
        if log.schema_version != THREAD_SCHEMA_VERSION {
            return Err(format!(
                "thread schema_version mismatch: expected {}, got {}",
                THREAD_SCHEMA_VERSION, log.schema_version
            ));
        }
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
                // Thread state snapshots live alongside logs under the same prefix.
                // The list endpoint must return ONLY thread logs.
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
        let cache_key = format!("{:p}|{}", Arc::as_ptr(&self.storage), key);
        let mut log = match cache().get(&cache_key) {
            Some(e) => e.log.clone(),
            None => {
                let v = self
                    .storage
                    .get_json(&key)
                    .await
                    .map_err(|e| e.to_string())?;
                serde_json::from_value::<ThreadLog>(v)
                    .map_err(|e| format!("failed to parse thread log: {e}"))?
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
        let cache_key = format!("{:p}|{}", Arc::as_ptr(&self.storage), key);
        let mut log = match cache().get(&cache_key) {
            Some(e) => e.log.clone(),
            None => {
                let v = self
                    .storage
                    .get_json(&key)
                    .await
                    .map_err(|e| e.to_string())?;
                serde_json::from_value::<ThreadLog>(v)
                    .map_err(|e| format!("failed to parse thread log: {e}"))?
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

fn build_thread_state_from_log(thread_id: &str, log: &ThreadLog) -> ThreadState {
    // For UI timelines, only emit tool events that have both a start+end in the persisted log.
    // This prevents orphan spans when the store drops one side of the pair.
    let paired_tool_ids: HashSet<String> = {
        let mut starts: HashSet<String> = HashSet::new();
        let mut ends: HashSet<String> = HashSet::new();
        for s in log.steps.iter() {
            match s {
                ThreadStep::ToolStart { tool_id, .. } => {
                    starts.insert(tool_id.clone());
                }
                ThreadStep::ToolEnd { tool_id, .. } => {
                    ends.insert(tool_id.clone());
                }
                _ => {}
            }
        }
        starts.intersection(&ends).cloned().collect()
    };

    let mut st = ThreadState {
        thread_state_schema_version: THREAD_STATE_SCHEMA_VERSION,
        thread_id: thread_id.to_string(),
        suite_id: None,
        agent_type: None,
        current_phase: None,
        last_materialized_step_count: 0,
        total_runtime_ms: 0,
        items: BTreeMap::new(),
        events: Vec::new(),
        suite_state: None,
    };

    for (idx, step) in log.steps.iter().enumerate() {
        apply_step_to_state(&mut st, idx, step, &paired_tool_ids);
        st.last_materialized_step_count = idx + 1;
    }

    // Total runtime = sum of completed phase runtimes.
    st.total_runtime_ms = st
        .items
        .values()
        .filter(|it| it.kind == ThreadItemKind::Phase)
        .filter_map(|it| it.runtime_ms)
        .sum();
    st
}

fn duration_ms(start_ts: &str, end_ts: &str) -> Option<u64> {
    let start = chrono::DateTime::parse_from_rfc3339(start_ts).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(end_ts).ok()?;
    let delta = end.signed_duration_since(start);
    let ms = delta.num_milliseconds();
    if ms <= 0 {
        return Some(0);
    }
    Some(ms as u64)
}

fn apply_step_to_state(
    st: &mut ThreadState,
    step_idx: usize,
    step: &ThreadStep,
    paired_tool_ids: &HashSet<String>,
) {
    fn push_event(st: &mut ThreadState, ev: ThreadEvent) {
        const MAX_EVENTS: usize = 200;
        st.events.push(ev);
        if st.events.len() > MAX_EVENTS {
            let drop_n = st.events.len() - MAX_EVENTS;
            st.events.drain(0..drop_n);
        }
    }

    match step {
        ThreadStep::SwitchSuite { to, .. } => {
            if !to.trim().is_empty() {
                st.suite_id = Some(to.to_string());
            }
        }
        ThreadStep::SwitchAgent { to, .. } => {
            if !to.trim().is_empty() {
                st.agent_type = Some(to.to_string());
            }
        }
        ThreadStep::Phase {
            phase,
            from_phase,
            ts,
            ..
        } => {
            let ph = phase.trim().to_string();
            if ph.is_empty() {
                return;
            }

            if let Some(prev) = from_phase
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                let key = format!("phase:{}", prev);
                let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
                    kind: ThreadItemKind::Phase,
                    status: ThreadItemStatus::Ok,
                    started_at: None,
                    finished_at: Some(ts.clone()),
                    runtime_ms: None,
                    last_error: None,
                    outputs: None,
                });
                ent.kind = ThreadItemKind::Phase;
                ent.status = ThreadItemStatus::Ok;
                ent.finished_at.get_or_insert_with(|| ts.clone());
                if ent.runtime_ms.is_none() {
                    if let (Some(ref started), Some(ref finished)) =
                        (&ent.started_at, &ent.finished_at)
                    {
                        ent.runtime_ms = duration_ms(started, finished);
                    }
                }
            }

            let key = format!("phase:{}", ph);
            let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
                kind: ThreadItemKind::Phase,
                status: ThreadItemStatus::Running,
                started_at: Some(ts.clone()),
                finished_at: None,
                runtime_ms: None,
                last_error: None,
                outputs: None,
            });
            ent.kind = ThreadItemKind::Phase;
            ent.status = ThreadItemStatus::Running;
            ent.started_at.get_or_insert_with(|| ts.clone());
            st.current_phase = Some(ph);
        }
        ThreadStep::ToolStart {
            tool_id,
            name,
            clean_name,
            args: _,
            status: _,
            payload,
            ctx,
            ts,
            ..
        } => {
            let key = format!("tool:{}", tool_id);
            let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
                kind: ThreadItemKind::Tool,
                status: ThreadItemStatus::Running,
                started_at: Some(ts.clone()),
                finished_at: None,
                runtime_ms: None,
                last_error: None,
                outputs: None,
            });
            ent.kind = ThreadItemKind::Tool;
            ent.status = ThreadItemStatus::Running;
            ent.started_at.get_or_insert_with(|| ts.clone());
            if let Some(p) = payload.clone() {
                ent.outputs = Some(p);
            }

            // Only emit timeline events when we have a complete span in the log.
            if paired_tool_ids.contains(tool_id) {
                push_event(
                    st,
                    ThreadEvent {
                        step_idx,
                        event_kind: ThreadEventKind::ToolStart,
                        ts: ts.clone(),
                        tool_id: Some(tool_id.clone()),
                        name: Some(name.clone()),
                        clean_name: Some(clean_name.clone()),
                        status: Some(ThreadEventStatus::Running),
                        runtime_ms: None,
                        payload: payload.clone(),
                        error: None,
                        call_id: None,
                        model: None,
                        phase: st
                            .current_phase
                            .clone()
                            .or_else(|| Some("preflight".to_string())),
                        ctx: ctx.clone(),
                    },
                );
            }
        }
        ThreadStep::ToolEnd {
            tool_id,
            name,
            clean_name,
            args: _,
            status,
            payload,
            ctx,
            observation,
            ts,
            ..
        } => {
            let key = format!("tool:{}", tool_id);
            let (runtime_ms, payload_out, err_out) = {
                let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
                    kind: ThreadItemKind::Tool,
                    status: if *status == ToolStepStatus::Failed {
                        ThreadItemStatus::Failed
                    } else {
                        ThreadItemStatus::Ok
                    },
                    started_at: Some(ts.clone()),
                    finished_at: None,
                    runtime_ms: None,
                    last_error: None,
                    outputs: None,
                });
                ent.kind = ThreadItemKind::Tool;
                ent.status = if *status == ToolStepStatus::Failed {
                    ThreadItemStatus::Failed
                } else {
                    ThreadItemStatus::Ok
                };
                ent.started_at.get_or_insert_with(|| ts.clone());
                ent.finished_at = Some(ts.clone());
                if ent.runtime_ms.is_none() {
                    if let (Some(ref started), Some(ref finished)) =
                        (&ent.started_at, &ent.finished_at)
                    {
                        ent.runtime_ms = duration_ms(started, finished);
                    }
                }

                // Prefer explicit payload (tool-owned) for outputs; otherwise keep a small generic subset.
                if let Some(p) = payload.clone() {
                    ent.outputs = Some(p);
                } else {
                    let mut outputs = serde_json::Map::new();
                    for k in [
                        "written_keys",
                        "key",
                        "uploaded_target_files",
                        "runtime_failures",
                    ] {
                        if let Some(v) = observation.extra.get(k) {
                            outputs.insert(k.to_string(), v.clone());
                        }
                    }
                    if !outputs.is_empty() {
                        ent.outputs = Some(Value::Object(outputs));
                    }
                }

                if ent.status == ThreadItemStatus::Failed || !observation.ok {
                    let summary = observation
                        .first_error_or_context()
                        .unwrap_or_else(|| "no error details were captured".to_string());
                    ent.last_error = Some(ThreadItemError {
                        summary,
                        tool_step_idx: None,
                        step_ts: Some(ts.clone()),
                    });
                }

                let err_out = if observation.ok {
                    None
                } else {
                    observation.first_error_or_context()
                };
                (ent.runtime_ms, ent.outputs.clone(), err_out)
            };

            // Only emit timeline events when we have a complete span in the log.
            if paired_tool_ids.contains(tool_id) {
                push_event(
                    st,
                    ThreadEvent {
                        step_idx,
                        event_kind: ThreadEventKind::ToolEnd,
                        ts: ts.clone(),
                        tool_id: Some(tool_id.clone()),
                        name: Some(name.clone()),
                        clean_name: Some(clean_name.clone()),
                        status: Some(if *status == ToolStepStatus::Failed {
                            ThreadEventStatus::Failed
                        } else {
                            ThreadEventStatus::Ok
                        }),
                        runtime_ms,
                        payload: payload_out,
                        error: err_out,
                        call_id: None,
                        model: None,
                        phase: st
                            .current_phase
                            .clone()
                            .or_else(|| Some("preflight".to_string())),
                        ctx: ctx.clone(),
                    },
                );
            }
        }
        ThreadStep::LlmStart {
            call_id,
            model,
            phase,
            ctx,
            ts,
            ..
        } => {
            push_event(
                st,
                ThreadEvent {
                    step_idx,
                    event_kind: ThreadEventKind::LlmStart,
                    ts: ts.clone(),
                    tool_id: None,
                    name: None,
                    clean_name: None,
                    status: Some(ThreadEventStatus::Running),
                    runtime_ms: None,
                    payload: None,
                    error: None,
                    call_id: Some(*call_id),
                    model: model.clone(),
                    phase: Some(phase.clone()),
                    ctx: ctx.clone(),
                },
            );
        }
        ThreadStep::LlmEnd {
            call_id,
            model,
            phase,
            status,
            error,
            ctx,
            ts,
            ..
        } => {
            push_event(
                st,
                ThreadEvent {
                    step_idx,
                    event_kind: ThreadEventKind::LlmEnd,
                    ts: ts.clone(),
                    tool_id: None,
                    name: None,
                    clean_name: None,
                    status: Some(if *status == LlmStepStatus::Failed {
                        ThreadEventStatus::Failed
                    } else {
                        ThreadEventStatus::Ok
                    }),
                    runtime_ms: None,
                    payload: None,
                    error: error.clone(),
                    call_id: Some(*call_id),
                    model: model.clone(),
                    phase: Some(phase.clone()),
                    ctx: ctx.clone(),
                },
            );
        }
        ThreadStep::Final {
            ts, observation, ..
        } => {
            // A `final` marks the end of a run. Close out the currently-running phase so
            // UIs can mark the terminal phase (often `done`) as completed.
            let Some(ph) = st
                .current_phase
                .clone()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                return;
            };

            let key = format!("phase:{}", ph);
            let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
                kind: ThreadItemKind::Phase,
                status: ThreadItemStatus::Running,
                started_at: Some(ts.clone()),
                finished_at: None,
                runtime_ms: None,
                last_error: None,
                outputs: None,
            });
            ent.kind = ThreadItemKind::Phase;
            ent.status = if observation.ok {
                ThreadItemStatus::Ok
            } else {
                ThreadItemStatus::Failed
            };
            ent.finished_at = Some(ts.clone());
            if ent.runtime_ms.is_none() {
                if let (Some(ref started), Some(ref finished)) = (&ent.started_at, &ent.finished_at)
                {
                    ent.runtime_ms = duration_ms(started, finished);
                }
            }
        }
        ThreadStep::AskUser { prompt, ts, .. } => {
            block_current_phase(st, prompt, ts);
        }
        ThreadStep::AskApproval { prompt, ts, .. } => {
            block_current_phase(st, prompt, ts);
        }
        ThreadStep::GuardBlock { reason, ts, .. } => {
            block_current_phase(st, reason, ts);
        }
        _ => {}
    }
}

fn block_current_phase(st: &mut ThreadState, msg: &str, ts: &str) {
    let Some(ph) = st.current_phase.clone() else {
        return;
    };
    let key = format!("phase:{}", ph);
    let ent = st.items.entry(key).or_insert_with(|| ThreadItemState {
        kind: ThreadItemKind::Phase,
        status: ThreadItemStatus::Blocked,
        started_at: Some(ts.to_string()),
        finished_at: None,
        runtime_ms: None,
        last_error: None,
        outputs: None,
    });
    ent.kind = ThreadItemKind::Phase;
    ent.status = ThreadItemStatus::Blocked;
    ent.last_error = Some(ThreadItemError {
        summary: msg.to_string(),
        tool_step_idx: None,
        step_ts: Some(ts.to_string()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyspace::DefaultKeyspace;
    use crate::storage::InMemoryStorageAdapter;

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
    fn orphan_tool_end_does_not_emit_timeline_event() {
        let log = ThreadLog {
            schema_version: THREAD_SCHEMA_VERSION,
            steps: vec![ThreadStep::ToolEnd {
                tool_id: "orphan".to_string(),
                name: "run_sql".to_string(),
                clean_name: "Run SQL".to_string(),
                args: serde_json::json!({}),
                status: ToolStepStatus::Ok,
                payload: None,
                ctx: None,
                observation: ToolObservation::normalize(serde_json::json!({"ok": true})),
                ts: "t".to_string(),
                agent: "ask".to_string(),
            }],
            result: None,
            title: None,
            title_finalized: false,
        };
        let st = build_thread_state_from_log("tid", &log);
        assert!(
            st.events.is_empty(),
            "orphan tool_end should not appear in timeline events"
        );
    }

    #[test]
    fn orphan_tool_start_does_not_emit_timeline_event() {
        let log = ThreadLog {
            schema_version: THREAD_SCHEMA_VERSION,
            steps: vec![ThreadStep::ToolStart {
                tool_id: "orphan".to_string(),
                name: "run_sql".to_string(),
                clean_name: "Run SQL".to_string(),
                args: serde_json::json!({}),
                status: ToolStepStatus::Running,
                payload: None,
                ctx: None,
                ts: "t".to_string(),
                agent: "ask".to_string(),
            }],
            result: None,
            title: None,
            title_finalized: false,
        };
        let st = build_thread_state_from_log("tid", &log);
        assert!(
            st.events.is_empty(),
            "orphan tool_start should not appear in timeline events"
        );
    }

    #[test]
    fn tool_timeline_events_embed_execution_ctx() {
        let ctx = ExecutionContext {
            data: BTreeMap::from([
                ("suite".to_string(), serde_json::json!("suite_x")),
                ("flow".to_string(), serde_json::json!("plan_a")),
                ("task_id".to_string(), serde_json::json!("task1")),
            ]),
            ..Default::default()
        };
        let log = ThreadLog {
            schema_version: THREAD_SCHEMA_VERSION,
            steps: vec![
                ThreadStep::ToolStart {
                    tool_id: "t1".to_string(),
                    name: "file".to_string(),
                    clean_name: "Read file".to_string(),
                    args: serde_json::json!({"op":"get"}),
                    status: ToolStepStatus::Running,
                    payload: None,
                    ctx: Some(ctx.clone()),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
                ThreadStep::ToolEnd {
                    tool_id: "t1".to_string(),
                    name: "file".to_string(),
                    clean_name: "Read file".to_string(),
                    args: serde_json::json!({"op":"get"}),
                    status: ToolStepStatus::Ok,
                    payload: None,
                    ctx: Some(ctx.clone()),
                    observation: ToolObservation::normalize(serde_json::json!({"ok": true})),
                    ts: "t".to_string(),
                    agent: "agent".to_string(),
                },
            ],
            result: None,
            title: None,
            title_finalized: false,
        };
        let st = build_thread_state_from_log("tid", &log);
        assert!(
            st.events
                .iter()
                .any(|e| e.event_kind == ThreadEventKind::ToolStart),
            "expected tool_start event"
        );
        let ev = st
            .events
            .iter()
            .find(|e| e.event_kind == ThreadEventKind::ToolStart)
            .and_then(|e| e.ctx.as_ref())
            .and_then(|c| c.data.get("flow"))
            .and_then(|v| v.as_str());
        assert_eq!(ev, Some("plan_a"));
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
            ThreadStep::LlmCall {
                call_id,
                phase,
                response_text,
                ..
            } => {
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
            ThreadStep::LlmCall {
                call_id,
                model,
                phase,
                response_text,
                ..
            } => {
                assert_eq!(*call_id, 1);
                assert_eq!(model, "m");
                assert_eq!(phase, "p");
                assert_eq!(response_text.as_deref(), Some("resp"));
            }
            _ => panic!("expected llm_call"),
        }
    }

    #[tokio::test]
    async fn thread_state_is_materialized_from_appended_steps() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let store = ThreadStore::new(storage, scope, keyspace);

        let tid = "tid";
        let t0 = chrono::DateTime::parse_from_rfc3339("2026-01-26T00:00:00Z")
            .unwrap()
            .to_rfc3339();
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-01-26T00:00:05Z")
            .unwrap()
            .to_rfc3339();
        let t2 = chrono::DateTime::parse_from_rfc3339("2026-01-26T00:00:06Z")
            .unwrap()
            .to_rfc3339();

        store
            .append_step(
                tid,
                ThreadStep::Phase {
                    phase: "model_plan".to_string(),
                    from_phase: None,
                    reason_code: None,
                    reason_detail: None,
                    observation: Observation::ok(),
                    ts: t0.clone(),
                    agent: "agent".to_string(),
                },
            )
            .await
            .unwrap();

        store
            .append_step(
                tid,
                ThreadStep::Phase {
                    phase: "model_author".to_string(),
                    from_phase: Some("model_plan".to_string()),
                    reason_code: None,
                    reason_detail: None,
                    observation: Observation::ok(),
                    ts: t1.clone(),
                    agent: "agent".to_string(),
                },
            )
            .await
            .unwrap();

        store
            .append_step(
                tid,
                ThreadStep::ToolStart {
                    tool_id: "t1".to_string(),
                    name: "dbt_validate".to_string(),
                    clean_name: "Validate DBT".to_string(),
                    args: serde_json::json!({"build": true}),
                    status: ToolStepStatus::Running,
                    payload: None,
                    ctx: None,
                    ts: t2.clone(),
                    agent: "agent".to_string(),
                },
            )
            .await
            .unwrap();
        store
            .append_step(
                tid,
                ThreadStep::ToolEnd {
                    tool_id: "t1".to_string(),
                    name: "dbt_validate".to_string(),
                    clean_name: "Validate DBT".to_string(),
                    args: serde_json::json!({"build": true}),
                    status: ToolStepStatus::Failed,
                    payload: None,
                    ctx: None,
                    observation: ToolObservation::normalize(serde_json::json!({"ok": false, "errors": ["boom"], "logs": {"run_or_build": {"stdout": "line 1:1 error"}}})),
                    ts: t2.clone(),
                    agent: "agent".to_string(),
                },
            )
            .await
            .unwrap();

        let st = store.get_thread_state(tid).await.unwrap();
        assert_eq!(st.thread_state_schema_version, THREAD_STATE_SCHEMA_VERSION);
        assert_eq!(st.thread_id, tid);
        assert_eq!(st.current_phase.as_deref(), Some("model_author"));
        let ph = st
            .items
            .get("phase:model_plan")
            .expect("phase:model_plan present");
        assert_eq!(ph.runtime_ms, Some(5_000));
        assert!(st.total_runtime_ms >= 5_000);
        // tool step should be materialized as failed
        let tool_item = st.items.get("tool:t1").expect("tool:t1 present");
        assert_eq!(tool_item.status, ThreadItemStatus::Failed);
        assert!(tool_item.last_error.as_ref().map(|e| e.summary.as_str()) == Some("boom"));
    }

    #[tokio::test]
    async fn final_closes_out_current_phase_as_completed() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let store = ThreadStore::new(storage, scope, keyspace);

        let tid = "tid2";
        let t0 = chrono::DateTime::parse_from_rfc3339("2026-01-26T00:00:00Z")
            .unwrap()
            .to_rfc3339();
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-01-26T00:00:01Z")
            .unwrap()
            .to_rfc3339();

        store
            .append_step(
                tid,
                ThreadStep::Phase {
                    phase: "done".to_string(),
                    from_phase: None,
                    reason_code: None,
                    reason_detail: None,
                    observation: Observation::ok(),
                    ts: t0.clone(),
                    agent: "agent".to_string(),
                },
            )
            .await
            .unwrap();

        store
            .append_step(
                tid,
                ThreadStep::Final {
                    kind: FinalKind::Generic,
                    payload: serde_json::json!({"text":"ok"}),
                    display: Some("ok".to_string()),
                    observation: Observation::ok(),
                    ts: t1.clone(),
                    agent: "agent".to_string(),
                },
            )
            .await
            .unwrap();

        let st = store.get_thread_state(tid).await.unwrap();
        assert_eq!(st.current_phase.as_deref(), Some("done"));
        let ph = st.items.get("phase:done").expect("phase:done present");
        assert_eq!(ph.status, ThreadItemStatus::Ok);
        assert_eq!(ph.finished_at.as_deref(), Some(t1.as_str()));
    }

    #[tokio::test]
    async fn list_returns_only_thread_logs_not_thread_state_snapshots() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let store = ThreadStore::new(storage.clone(), scope.clone(), keyspace.clone());

        // Write a real thread log.
        let tid = "123";
        store
            .append_step(
                tid,
                ThreadStep::User {
                    text: "hi".to_string(),
                    observation: Observation::ok(),
                    ts: "t".to_string(),
                    agent: "ask".to_string(),
                },
            )
            .await
            .unwrap();

        // Also write a thread_state snapshot object under the same prefix.
        let state_key = keyspace.thread_state_key(&scope, tid).unwrap();
        let st = ThreadState {
            thread_state_schema_version: THREAD_STATE_SCHEMA_VERSION,
            thread_id: tid.to_string(),
            ..Default::default()
        };
        storage
            .put_json(&state_key, &serde_json::to_value(&st).unwrap())
            .await
            .unwrap();

        let ids = store.list().await;
        assert_eq!(ids, vec![tid.to_string()]);
    }

    #[tokio::test]
    async fn get_thread_state_does_not_fallback_to_thread_log() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("b".to_string()));
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let store = ThreadStore::new(storage.clone(), scope.clone(), keyspace.clone());
        let tid = "tid-no-state";

        // Seed only a thread log object; no state snapshot.
        let key = keyspace.thread_key(&scope, tid).unwrap();
        let log = ThreadLog {
            schema_version: THREAD_SCHEMA_VERSION,
            steps: vec![],
            result: None,
            title: None,
            title_finalized: false,
        };
        storage
            .put_json(&key, &serde_json::to_value(log).unwrap())
            .await
            .unwrap();

        let got = store.get_thread_state(tid).await;
        assert!(got.is_err(), "thread state should not be reconstructed from logs");
    }
}
