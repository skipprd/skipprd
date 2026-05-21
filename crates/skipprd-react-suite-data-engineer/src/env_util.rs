/// Resolve a `${VAR_NAME}` env-var reference.
///
/// If the entire string is `${…}`, returns the env-var value (or the
/// original string when the variable is unset).  Non-ref strings pass
/// through unchanged.
pub fn resolve_env_ref(value: &str) -> String {
    if value.starts_with("${") && value.ends_with('}') {
        let var_name = &value[2..value.len() - 1];
        std::env::var(var_name).unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    }
}

pub fn getenv_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|v| {
        let t = v.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

pub fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

pub fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
}

pub fn env_bool_truthy(key: &str) -> Option<bool> {
    std::env::var(key).ok().map(|v| {
        let t = v.trim().to_ascii_lowercase();
        !(t.is_empty() || t == "0" || t == "false" || t == "no")
    })
}

pub fn parse_reasoning_effort_env(key: &str) -> Option<react_core::llm::ReasoningEffort> {
    match std::env::var(key)
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("none") => Some(react_core::llm::ReasoningEffort::None),
        Some("low") => Some(react_core::llm::ReasoningEffort::Low),
        Some("medium") => Some(react_core::llm::ReasoningEffort::Medium),
        Some("high") => Some(react_core::llm::ReasoningEffort::High),
        Some("extra_high") | Some("xhigh") => Some(react_core::llm::ReasoningEffort::High),
        _ => None,
    }
}

pub fn author_reasoning_effort(is_cleanse: bool) -> react_core::llm::ReasoningEffort {
    let track_key = if is_cleanse {
        env_keys::LLM_AUTHOR_REASONING_EFFORT_CLEANSE
    } else {
        env_keys::LLM_AUTHOR_REASONING_EFFORT_MODEL
    };
    parse_reasoning_effort_env(track_key)
        .or_else(|| parse_reasoning_effort_env(env_keys::LLM_AUTHOR_REASONING_EFFORT))
        .unwrap_or(react_core::llm::ReasoningEffort::Low)
}

pub fn repair_reasoning_effort() -> react_core::llm::ReasoningEffort {
    parse_reasoning_effort_env(env_keys::LLM_REPAIR_REASONING_EFFORT)
        .unwrap_or(react_core::llm::ReasoningEffort::Medium)
}

pub fn review_batch_concurrency() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        env_usize(env_keys::LLM_REVIEW_BATCH_CONCURRENCY)
            .unwrap_or(DEFAULT_REVIEW_BATCH_CONCURRENCY)
            .clamp(1, MAX_REVIEW_BATCH_CONCURRENCY)
    })
}

// ---------------------------------------------------------------------------
// Centralized env-var key names
// ---------------------------------------------------------------------------

pub mod env_keys {
    // Agent orchestration
    pub const AGENT_MAX_PHASE_STEPS: &str = "AGENT_MAX_PHASE_STEPS";
    pub const AGENT_MAX_REPLAN_BACKTRACKS: &str = "AGENT_MAX_REPLAN_BACKTRACKS";
    pub const AGENT_MAX_PUBLISH_RETRIES: &str = "AGENT_MAX_PUBLISH_RETRIES";
    pub const AGENT_MAX_SUBJECTIVE_RETRIES: &str = "AGENT_MAX_SUBJECTIVE_RETRIES";

    // Suite runtime
    pub const REACT_HEADLESS: &str = "REACT_HEADLESS";
    pub const DE_CATALOG_BOOTSTRAP_TIMEOUT_SECS: &str = "DE_CATALOG_BOOTSTRAP_TIMEOUT_SECS";
    pub const DE_CATALOG_LLM_ENRICHMENT: &str = "DE_CATALOG_LLM_ENRICHMENT";

    // Planning LLM tokens
    pub const LLM_PLAN_MAX_TOKENS_CLEANSE: &str = "LLM_PLAN_MAX_TOKENS_CLEANSE";
    pub const LLM_PLAN_MAX_TOKENS_MODEL: &str = "LLM_PLAN_MAX_TOKENS_MODEL";
    pub const LLM_PLAN_MEMO_MAX_TOKENS: &str = "LLM_PLAN_MEMO_MAX_TOKENS";
    pub const LLM_PLAN_CRITIQUE_MAX_TOKENS: &str = "LLM_PLAN_CRITIQUE_MAX_TOKENS";
    pub const LLM_PLAN_SKELETON_MAX_TOKENS: &str = "LLM_PLAN_SKELETON_MAX_TOKENS";
    pub const LLM_PLAN_ENRICH_MAX_TOKENS: &str = "LLM_PLAN_ENRICH_MAX_TOKENS";
    pub const LLM_PLAN_ENRICH_REASON_MAX_TOKENS: &str = "LLM_PLAN_ENRICH_REASON_MAX_TOKENS";
    pub const LLM_PLAN_ENRICH_CHUNK_SIZE: &str = "LLM_PLAN_ENRICH_CHUNK_SIZE";
    pub const LLM_MODEL_PLAN_MIN_SCORE: &str = "LLM_MODEL_PLAN_MIN_SCORE";

    // Planning reasoning effort
    pub const LLM_PLAN_REASONING_EFFORT: &str = "LLM_PLAN_REASONING_EFFORT";
    pub const LLM_PLAN_REASONING_EFFORT_CLEANSE: &str = "LLM_PLAN_REASONING_EFFORT_CLEANSE";
    pub const LLM_PLAN_REASONING_EFFORT_MODEL: &str = "LLM_PLAN_REASONING_EFFORT_MODEL";

    // Authoring LLM tokens
    pub const LLM_AUTHOR_MAX_TOKENS_CLEANSE: &str = "LLM_AUTHOR_MAX_TOKENS_CLEANSE";
    pub const LLM_AUTHOR_MAX_TOKENS_MODEL: &str = "LLM_AUTHOR_MAX_TOKENS_MODEL";
    pub const LLM_AUTHOR_REASONING_EFFORT: &str = "LLM_AUTHOR_REASONING_EFFORT";
    pub const LLM_AUTHOR_REASONING_EFFORT_CLEANSE: &str = "LLM_AUTHOR_REASONING_EFFORT_CLEANSE";
    pub const LLM_AUTHOR_REASONING_EFFORT_MODEL: &str = "LLM_AUTHOR_REASONING_EFFORT_MODEL";

    // Ask/review LLM tokens
    pub const LLM_ASK_REASONING_EFFORT: &str = "LLM_ASK_REASONING_EFFORT";
    pub const LLM_ASK_MAX_TOKENS: &str = "LLM_ASK_MAX_TOKENS";
    pub const LLM_REVIEW_REASONING_EFFORT: &str = "LLM_REVIEW_REASONING_EFFORT";
    pub const LLM_REVIEW_MAX_TOKENS: &str = "LLM_REVIEW_MAX_TOKENS";
    pub const LLM_REVIEW_MAX_TOKENS_UNIFY: &str = "LLM_REVIEW_MAX_TOKENS_UNIFY";
    pub const LLM_REVIEW_MAX_TOKENS_CLEANSE: &str = "LLM_REVIEW_MAX_TOKENS_CLEANSE";
    pub const LLM_REVIEW_MAX_TOKENS_CLEANSE_UNIFY: &str = "LLM_REVIEW_MAX_TOKENS_CLEANSE_UNIFY";
    pub const LLM_REVIEW_MAX_TOKENS_MODEL: &str = "LLM_REVIEW_MAX_TOKENS_MODEL";
    pub const LLM_REVIEW_MAX_TOKENS_MODEL_UNIFY: &str = "LLM_REVIEW_MAX_TOKENS_MODEL_UNIFY";
    pub const LLM_REVIEW_BATCH_CONCURRENCY: &str = "LLM_REVIEW_BATCH_CONCURRENCY";

    // SQL-first
    pub const REACT_SQL_FIRST_MAX_OUTPUT_TOKENS: &str = "REACT_SQL_FIRST_MAX_OUTPUT_TOKENS";
    pub const REACT_SQL_FIRST_MAX_REPAIR_ATTEMPTS: &str = "REACT_SQL_FIRST_MAX_REPAIR_ATTEMPTS";

    // Repair reasoning
    pub const LLM_REPAIR_REASONING_EFFORT: &str = "LLM_REPAIR_REASONING_EFFORT";

    // Patch protocol
    #[cfg(test)]
    pub const REACT_PATCH_LOOP_MAX_OUTPUT_TOKENS: &str = "REACT_PATCH_LOOP_MAX_OUTPUT_TOKENS";

    // dbt policy
    pub const DBT_ALLOW_COMPILE_ONLY_COMPLETE: &str = "DBT_ALLOW_COMPILE_ONLY_COMPLETE";

    // dbt config (also read from YAML; env takes priority)
    pub const DBT_TARGET_SCHEMA: &str = "DBT_TARGET_SCHEMA";
    pub const DBT_SILVER_SUFFIX: &str = "DBT_SILVER_SUFFIX";
    pub const DBT_GOLD_SUFFIX: &str = "DBT_GOLD_SUFFIX";
    pub const DBT_PROFILES_DIR: &str = "DBT_PROFILES_DIR";
    pub const DBT_TARGET: &str = "DBT_TARGET";
    pub const DBT_RUNNER: &str = "DBT_RUNNER";
    pub const DBT_DOCKER_IMAGE: &str = "DBT_DOCKER_IMAGE";
    pub const DBT_DOCKER_PLATFORM: &str = "DBT_DOCKER_PLATFORM";
    pub const DBT_DOCKER_NETWORK: &str = "DBT_DOCKER_NETWORK";
    pub const DBT_DOCKER_MOUNT_AWS_DIR: &str = "DBT_DOCKER_MOUNT_AWS_DIR";
}

// ---------------------------------------------------------------------------
// Centralized reader functions (with defaults, clamping, etc.)
// ---------------------------------------------------------------------------

const MODEL_PHASE_STEP_OVERRIDES: &[(&str, usize)] = &[("gpt-5.4", 140)];

pub fn max_phase_steps_for_model(model: &str) -> usize {
    if let Some(v) = env_usize(env_keys::AGENT_MAX_PHASE_STEPS) {
        return v.max(MIN_PHASE_STEPS).min(MAX_PHASE_STEPS);
    }
    for &(prefix, steps) in MODEL_PHASE_STEP_OVERRIDES {
        if model.starts_with(prefix) {
            return steps.max(MIN_PHASE_STEPS).min(MAX_PHASE_STEPS);
        }
    }
    DEFAULT_MAX_PHASE_STEPS
}

pub fn max_replan_backtracks() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        env_usize(env_keys::AGENT_MAX_REPLAN_BACKTRACKS)
            .unwrap_or(DEFAULT_MAX_REPLAN_BACKTRACKS)
            .max(MIN_REPLAN_BACKTRACKS)
            .min(MAX_REPLAN_BACKTRACKS)
    })
}

pub fn max_publish_retries() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        env_usize(env_keys::AGENT_MAX_PUBLISH_RETRIES)
            .unwrap_or(DEFAULT_MAX_PUBLISH_RETRIES)
            .max(1)
            .min(12)
    })
}

pub fn plan_enrich_chunk_size() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        env_usize(env_keys::LLM_PLAN_ENRICH_CHUNK_SIZE)
            .unwrap_or(DEFAULT_PLAN_ENRICH_CHUNK_SIZE)
            .clamp(MIN_PLAN_ENRICH_CHUNK_SIZE, MAX_PLAN_ENRICH_CHUNK_SIZE)
    })
}

pub fn model_plan_min_score() -> i32 {
    static CACHE: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var(env_keys::LLM_MODEL_PLAN_MIN_SCORE)
            .ok()
            .and_then(|s| s.parse::<i32>().ok())
            .map(|p| p.clamp(MIN_MODEL_PLAN_SCORE, MAX_MODEL_PLAN_SCORE))
            .unwrap_or(DEFAULT_MODEL_PLAN_MIN_SCORE)
    })
}

pub fn headless_mode_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| env_bool_truthy(env_keys::REACT_HEADLESS).unwrap_or(false))
}

pub fn ask_reasoning_effort() -> react_core::llm::ReasoningEffort {
    parse_reasoning_effort_env(env_keys::LLM_ASK_REASONING_EFFORT)
        .unwrap_or(react_core::llm::ReasoningEffort::None)
}

pub fn ask_max_tokens() -> u32 {
    env_u32(env_keys::LLM_ASK_MAX_TOKENS)
        .unwrap_or(2_000)
        .clamp(256, 8_000)
}

pub fn catalog_bootstrap_timeout_secs() -> u64 {
    static CACHE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        env_u32(env_keys::DE_CATALOG_BOOTSTRAP_TIMEOUT_SECS)
            .unwrap_or(DEFAULT_CATALOG_BOOTSTRAP_TIMEOUT_SECS as u32)
            .max(30)
            .min(1800) as u64
    })
}

pub fn dbt_allow_compile_only_complete() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var(env_keys::DBT_ALLOW_COMPILE_ONLY_COMPLETE)
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

pub fn sql_first_max_output_tokens(default: u32) -> u32 {
    env_u32(env_keys::REACT_SQL_FIRST_MAX_OUTPUT_TOKENS)
        .unwrap_or(default)
        .max(800)
        .min(16_000)
}

pub fn sql_first_max_repair_attempts(default: usize) -> usize {
    env_usize(env_keys::REACT_SQL_FIRST_MAX_REPAIR_ATTEMPTS)
        .unwrap_or(default)
        .max(1)
        .min(8)
}

// ---------------------------------------------------------------------------
// Named defaults and bounds for agent-context / orchestration constants
// ---------------------------------------------------------------------------

pub const DEFAULT_TOP_K: usize = 30;

pub const ASK_MAX_STEPS: usize = 50;
pub const REVIEW_MAX_STEPS: usize = 40;
/// Fallback when source count is unknown at AgentCtx construction time.
pub const PLAN_DISCOVERY_MAX_STEPS: usize = 20;
const PLAN_DISCOVERY_STEPS_PER_SOURCE: usize = 3;
const PLAN_DISCOVERY_BASE_BUFFER: usize = 15;
const PLAN_DISCOVERY_MIN_STEPS: usize = 15;
const PLAN_DISCOVERY_MAX_STEPS_CAP: usize = 80;

/// Compute a proportional step budget for plan discovery.
///
/// Formula: `source_count * 3 + 15`, clamped to `[15, 80]`.
///
/// The base buffer (15) accounts for project-level reads that are
/// independent of source count: dbt_project.yml, manifest queries,
/// catalog files, schema.yml, vector search, models/ listing, etc.
/// The per-source factor (3) covers list + read SQL + sql_schema per
/// source.  The cap (80) prevents runaway loops while allowing large
/// projects enough room.
pub fn plan_discovery_steps_for_sources(source_count: usize) -> usize {
    let raw = source_count
        .saturating_mul(PLAN_DISCOVERY_STEPS_PER_SOURCE)
        .saturating_add(PLAN_DISCOVERY_BASE_BUFFER);
    raw.clamp(PLAN_DISCOVERY_MIN_STEPS, PLAN_DISCOVERY_MAX_STEPS_CAP)
}
pub const APPROVAL_PARSE_MAX_STEPS: usize = 6;
pub const AUTHOR_MAX_STEPS: usize = 10;

pub const DEFAULT_MAX_PHASE_STEPS: usize = 80;
pub const MIN_PHASE_STEPS: usize = 8;
pub const MAX_PHASE_STEPS: usize = 400;

pub const DEFAULT_MAX_REPLAN_BACKTRACKS: usize = 3;
pub const MIN_REPLAN_BACKTRACKS: usize = 2;
pub const MAX_REPLAN_BACKTRACKS: usize = 20;

pub const DEFAULT_MAX_PUBLISH_RETRIES: usize = 3;

pub const DEFAULT_PLAN_ENRICH_CHUNK_SIZE: usize = 3;
pub const MIN_PLAN_ENRICH_CHUNK_SIZE: usize = 1;
pub const MAX_PLAN_ENRICH_CHUNK_SIZE: usize = 3;

pub const DEFAULT_REVIEW_BATCH_CONCURRENCY: usize = 2;
pub const MAX_REVIEW_BATCH_CONCURRENCY: usize = 4;

pub const DEFAULT_MODEL_PLAN_MIN_SCORE: i32 = 70;
pub const MIN_MODEL_PLAN_SCORE: i32 = 0;
pub const MAX_MODEL_PLAN_SCORE: i32 = 100;

pub const DEFAULT_CATALOG_BOOTSTRAP_TIMEOUT_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Tool timeout tiers (seconds)
// ---------------------------------------------------------------------------

pub const TOOL_TIMEOUT_FAST_SECS: u64 = 120;
pub const TOOL_TIMEOUT_MEDIUM_SECS: u64 = 300;
pub const TOOL_TIMEOUT_SLOW_SECS: u64 = 600;
pub const TOOL_TIMEOUT_EXTRA_SLOW_SECS: u64 = 900;

// ---------------------------------------------------------------------------
// Project identity constants
// ---------------------------------------------------------------------------

pub const SUITE_PROJECT_NAME: &str = "data_engineer";
pub const DEFAULT_AGENT_NAME: &str = "model";
pub const UNKNOWN_AGENT: &str = "unknown";

// ---------------------------------------------------------------------------
// File-operation limit defaults used in deterministic invariant checks
// ---------------------------------------------------------------------------

pub const FILE_LIST_LIMIT: u64 = 500;
pub const FILE_GET_MAX_CHARS: u64 = 2000;
