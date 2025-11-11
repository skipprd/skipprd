pub mod parser;
pub mod query;
pub mod tui;
pub mod operators;
pub mod docs;
pub mod doc_parser;
pub mod registry;
pub mod metadata;
pub mod tables;

// Re-export key components
pub use docs::{SqlStatementDoc};
pub use doc_parser::SqlDocParser;