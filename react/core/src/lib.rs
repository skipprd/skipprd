//! ReAct runner core (usecase-agnostic).
//!
//! This crate intentionally contains only:
//! - The ReAct loop (`agent`)
//! - Tool interface/registry (`tools`)
//! - Thread transcript persistence (`session`)
//! - Minimal capability traits/types (LLM + storage + scope/keyspace + optional providers)

pub mod agent;
pub mod control_flow;
pub mod discover;
pub mod error_context;
pub mod helpers;
pub mod keyspace;
pub mod llm;
pub mod llm_observability;
pub mod providers;
pub mod resolved_config;
pub mod schema_registry;
pub mod scope;
pub mod session;
pub mod storage;
pub mod tools;
