//! SQL/DataFusion runtime (library).
//!
//! Implementation is being migrated from `src/sql/*` into this module.

#[cfg(test)]
mod alter_table_matrix;
pub mod cdc_reconcile;
pub mod doc_parser;
pub mod docs;
pub mod dst;
pub mod iceberg_catalog;
pub mod iceberg_lake;
pub mod iceberg_table;
pub mod metadata;
pub mod operators;
pub mod parser;
pub mod query;
pub mod read_boundary;
pub mod registry;
pub mod schema_seed;
pub mod session;
pub mod stream_plan;
pub mod tables;
pub mod tui;
pub mod udfs;
pub mod wal_slice;
pub mod wal_table;
