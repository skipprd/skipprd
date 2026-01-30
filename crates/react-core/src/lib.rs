//! ReAct runner core (usecase-agnostic).
//!
//! This crate intentionally contains only:
//! - The ReAct loop (`agent`)
//! - Tool interface/registry (`tools`)
//! - Thread transcript persistence (`session`)
//! - Minimal capability traits/types (LLM + storage + scope/keyspace + optional providers)

pub mod tools;
pub mod llm;
pub mod storage;
pub mod scope;
pub mod keyspace;
pub mod providers;
pub mod discover;
pub mod helpers;
pub mod session;
pub mod agent;
pub mod llm_observability;
pub mod error_context;

