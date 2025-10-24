use std::string::ToString;
use clap::Parser;
use once_cell::sync::Lazy;
use crate::helpers::timed_rwlock::TimedRwLock;

pub static CLI_MODE: Lazy<TimedRwLock<Mode>> = Lazy::new(|| TimedRwLock::new("cli_mode".to_string(), Mode::Sync(SyncOptions {
    pipeline: None,
})));

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Enable diagnostic logging output
    #[arg(long, global = true, default_value_t = false)]
    pub log: bool,
    #[command(subcommand)]
    pub mode: Mode,
}

#[derive(Parser, Clone, PartialEq)]
pub enum Mode {
    Discover(DisocverOptions),
    Sync(SyncOptions),
    Query(QueryOptions),
    Schema(SchemaOptions),
    SqlHelp(SqlHelpOptions),
    Benchmark(BenchmarkOptions),
    Llm(LlmOptions),
}

#[derive(Parser, Clone, PartialEq)]
pub struct SyncOptions {
    /// The pipeline to use
    #[arg(short, long)]
    pub pipeline: Option<String>,
}

#[derive(Parser, Clone, PartialEq)]
pub struct DisocverOptions {
    /// The pipeline to use
    #[arg(short, long)]
    pub pipeline: Option<String>,
}

#[derive(Parser, Clone, PartialEq)]
pub struct QueryOptions {
    /// The SQL query to run
    #[arg(short, long)]
    pub sql: Option<String>,
    /// Watch interval in seconds for live SELECT (optional)
    #[arg(long)]
    pub watch: Option<u64>,
    /// Print plain results to stdout instead of TUI (non-interactive)
    #[arg(long, default_value_t = false)]
    pub plain: bool,
}

#[derive(Parser, Clone, PartialEq)]
pub struct SchemaOptions {
    /// The schema to use
    #[arg(short, long)]
    pub pipeline: String,
}

#[derive(Parser, Clone, PartialEq)]
pub struct SqlHelpOptions {
    /// The SQL command to get help for. If not provided, shows all commands.
    #[arg(short, long)]
    pub command: Option<String>,
    
    /// Generate documentation and save to file
    #[arg(short, long)]
    pub output: Option<String>,
    
    /// Format for documentation output (md, html, json)
    #[arg(short, long, default_value = "md")]
    pub format: Option<String>,
}

#[derive(Parser, Clone, PartialEq, Default)]
pub struct BenchmarkOptions {
    /// Number of files to generate for benchmark
    #[arg(short = 'f', long)]
    pub num_files: usize,
    
    /// Number of records per file
    #[arg(short = 'r', long)]
    pub records_per_file: usize,
    
    /// Average record size in bytes
    #[arg(short = 's', long)]
    pub record_size: usize,
    
    /// Benchmark name
    #[arg(short, long, default_value = "baseline")]
    pub name: String,
    
    /// Description of what's being benchmarked (e.g., specific optimization)
    #[arg(short = 'd', long)]
    pub description: Option<String>,
}

#[derive(Parser, Clone, PartialEq, Default)]
pub struct LlmOptions {
    /// Chat prompt to send (uses configured chat model)
    #[arg(long)]
    pub chat: Option<String>,
    /// Embedding inputs (repeat flag to add multiple)
    #[arg(long)]
    pub embed: Vec<String>,
    /// Ask a question about ingested data (uses semantic & catalog)
    #[arg(long)]
    pub ask: Option<String>,
    /// Namespace to scope the question (defaults to pipeline name)
    #[arg(long)]
    pub namespace: Option<String>,
    /// Top-K rows or docs to consider
    #[arg(long, default_value_t = 5)]
    pub top_k: usize,
}
