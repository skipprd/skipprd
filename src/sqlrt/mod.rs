//! SQL/DataFusion runtime (library).
//!
//! Implementation is being migrated from `src/sql/*` into this module.

pub mod doc_parser;
pub mod docs;
pub mod flight_sql_table;
pub mod iceberg_table;
pub mod metadata;
pub mod operators;
pub mod parser;
pub mod query;
pub mod registry;
pub mod schema_seed;
pub mod session;
pub mod stream_plan;
pub mod tables;
pub mod tui;
pub mod udfs;
pub mod wal_reader;
pub mod wal_table;
