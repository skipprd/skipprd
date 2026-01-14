//! Skippr library crate.
//!
//! This package builds both:
//! - a library (`skippr`) for reusable components (ReAct runtime, SQL runtime, adapters)
//! - a binary (`src/main.rs`) for ingestion/CLI
//!
//! The refactor in progress moves agent runtime under `react/` and SQL/DataFusion under `sqlrt/`.

// NOTE: This library is being split into clean boundaries (react + sqlrt) over time.
// For now, we expose the existing modules so the library can compile and the binary can
// progressively migrate off `mod ...` declarations.

pub mod globals;
pub use globals::*;

pub mod adapters;
pub mod arr;
pub mod benchmark;
pub mod buffer;
pub mod cli;
pub mod converters;
pub mod discover;
pub mod helpers;
pub mod ingest;
pub mod ingest_work;
pub mod internalfields;
pub mod llm;
pub mod metrics;
pub mod plugins;
pub mod serdes;
pub mod vector;
pub mod sqlrt;

