//! ReAct agent runtime (library).
//!
//! Runtime crate (CLI + WS server + concrete provider implementations).

pub mod adapters;
pub mod config;
pub mod helpers;
pub mod llm;
pub mod models;
pub mod providers;
pub mod ws;

// Concrete implementations and utilities used by runtime wiring.
pub mod vector;
pub mod embeddings;
pub mod util;
pub mod discover;

