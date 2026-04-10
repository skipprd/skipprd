//! Shared data source and data sink implementations for Skippr runtime plugin binaries.
//!
//! Connectors are written against `crate::helpers`, `crate::ingest_work`, etc. as if they lived in
//! the `skippr` crate; this crate re-exports those module paths from the real `skippr` dependency.

pub use skippr::{
    buffer, converters, discover, helpers, ingest, ingest_work, metrics, plugins, serdes,
};
pub use skippr::{METADATA, RUNNING};

pub mod runtime_plugin_data_sources;

#[cfg(feature = "runtime-sink-link")]
pub mod runtime_sink_link;
