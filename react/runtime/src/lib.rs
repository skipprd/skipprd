//! ReAct agent runtime (library).
//!
//! Runtime crate (CLI + WS server + concrete provider implementations).

pub mod adapters;
pub mod config;
pub mod helpers;
pub mod llm;
pub mod models;
pub mod providers;
pub mod run;
pub mod thread_logs;
pub mod ws;

// Concrete implementations and utilities used by runtime wiring.
pub mod discover;
pub mod embeddings;
pub mod util;
