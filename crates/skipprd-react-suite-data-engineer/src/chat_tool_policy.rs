//! Static capability lists for `skippr chat` modes (ask / plan / agent).
//!
//! The suite’s runtime tool gating lives in [`crate::agent_modes`]; this module documents the
//! intended CLI/IDE contract for operators and future wiring.

/// Tools and capabilities available in **ask** and **plan** (read-only) chat modes.
pub const READ_CHAT_CAPABILITIES: &[&str] = &[
    "catalog_read",
    "config_read",
    "dbt_read",
    "docs_search_public",
    "doctor_read",
    "log_tail_read",
    "lineage_read",
];

/// Additional capabilities for **agent** mode (includes delegated model sub-agent runs).
pub const AGENT_CHAT_EXTRA_CAPABILITIES: &[&str] = &[
    "init",
    "connect",
    "doctor_invoke",
    "model_subagent",
    "discover",
    "sync_once",
    "read_only_sql",
    "chart_hint",
];
