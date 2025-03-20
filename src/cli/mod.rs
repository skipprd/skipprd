use std::string::ToString;
use std::sync::Arc;
use clap::Parser;
use once_cell::sync::Lazy;
use crate::helpers::timed_rwlock::TimedRwLock;

pub static CLI_MODE: Lazy<TimedRwLock<Mode>> = Lazy::new(|| TimedRwLock::new("cli_mode".to_string(), Mode::Sync(SyncOptions {
    pipeline: None,
})));

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) mode: Mode,
}

#[derive(Parser, Clone, PartialEq)]
pub enum Mode {
    Discover(DisocverOptions),
    Sync(SyncOptions),
    Query(QueryOptions),
    Schema(SchemaOptions),
    SqlHelp(SqlHelpOptions),
    // ... other modes
}

#[derive(Parser, Clone, PartialEq)]
pub struct SyncOptions {
    /// The pipeline to use
    #[arg(short, long)]
    pub(crate) pipeline: Option<String>,
}

#[derive(Parser, Clone, PartialEq)]
pub struct DisocverOptions {
    /// The pipeline to use
    #[arg(short, long)]
    pub(crate) pipeline: Option<String>,
}

#[derive(Parser, Clone, PartialEq)]
pub struct QueryOptions {
    /// The SQL query to run
    #[arg(short, long)]
    pub(crate) sql: String,
}

#[derive(Parser, Clone, PartialEq)]
pub struct SchemaOptions {
    /// The schema to use
    #[arg(short, long)]
    pub(crate) pipeline: String,
}

#[derive(Parser, Clone, PartialEq)]
pub struct SqlHelpOptions {
    /// The SQL command to get help for. If not provided, shows all commands.
    #[arg(short, long)]
    pub(crate) command: Option<String>,
    
    /// Generate documentation and save to file
    #[arg(short, long)]
    pub(crate) output: Option<String>,
    
    /// Format for documentation output (md, html, json)
    #[arg(short, long, default_value = "md")]
    pub(crate) format: Option<String>,
}
