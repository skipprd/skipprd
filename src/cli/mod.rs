use std::sync::Arc;
use clap::Parser;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use crate::helpers::timed_rwlock::TimedRwLock;

pub static CLI_MODE: Lazy<RwLock<Mode>> = Lazy::new(|| RwLock::new(Mode::Sync(SyncOptions {
    pipeline: None,
})));

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) mode: Mode,
}

#[derive(Parser, Clone)]
pub enum Mode {
    Discover(DisocverOptions),
    Sync(SyncOptions),
    Query(QueryOptions),
    Schema(SchemaOptions),
    // ... other modes
}

#[derive(Parser, Clone)]
pub struct SyncOptions {
    /// The pipeline to use
    #[arg(short, long)]
    pub(crate) pipeline: Option<String>,
}

#[derive(Parser, Clone)]
pub struct DisocverOptions {
    /// The pipeline to use
    #[arg(short, long)]
    pub(crate) pipeline: Option<String>,
}

#[derive(Parser, Clone)]
pub struct QueryOptions {
    /// The SQL query to run
    #[arg(short, long)]
    pub(crate) sql: String,
}

#[derive(Parser, Clone)]
pub struct SchemaOptions {
    /// The schema to use
    #[arg(short, long)]
    pub(crate) pipeline: String,
}
