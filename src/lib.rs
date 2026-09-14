//! Skippr library crate.
//!
//! This package builds both:
//! - a library (`skippr`) for reusable components (SQL runtime, adapters)
//! - a binary (`src/main.rs`) for ingestion/CLI

#![recursion_limit = "256"]

// NOTE: This library is being split into cleaner module boundaries over time.
// For now, we expose the existing modules so the library can compile and the binary can
// progressively migrate off `mod ...` declarations.

pub mod globals;
pub use globals::*;

pub mod adapters;
pub mod arr;
pub mod benchmark;
pub mod buffer;
pub mod catalog_budget;
pub mod catalog_coordinator;
pub mod catalog_outbox;
pub mod cli;
pub mod cluster;
pub mod converters;
pub mod discover;
pub mod engine;
pub mod helpers;
pub mod ingest;
pub mod ingest_work;
pub mod internalfields;
pub mod lineage;
pub mod metrics;
pub mod pipeline_backend;
pub mod plugins;
pub mod query_flight;
pub mod runtime_plugins;
mod schema_coordinator;
pub mod serdes;
pub mod sink_apply_identity;
pub mod sqlrt;
pub mod store;
pub mod table_formats;
