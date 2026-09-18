use clap::Parser;
use std::path::PathBuf;

use crate::helpers::wal_storage::{ElStorageMode, OffsetStoreKind, WalStorage};

mod connect_generated;
pub mod metadata;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Enable logging. Optional level: debug|info|warn|error. Using --log defaults to 'info'.
    #[arg(long, global = true, num_args=0..=1, default_missing_value="info", value_name="LEVEL")]
    pub log: Option<String>,
    /// Path to skippr.yml / skippr.yaml (also SKIPPR_CONFIG_FILE env)
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// WAL backend: disk, s3, or clustered (also WAL_STORAGE env)
    #[arg(long, global = true, value_enum)]
    pub wal_storage: Option<WalStorage>,
    /// Dedicated S3 bucket for WAL segments (also SKIPPR_WAL_S3_BUCKET env)
    #[arg(long, global = true)]
    pub wal_s3_bucket: Option<String>,
    /// Offset store: sled (default), dynamodb, or cloud-tables (also SKIPPR_OFFSET_STORE env)
    #[arg(long, global = true, value_enum)]
    pub offset_store: Option<OffsetStoreKind>,
    /// DynamoDB table for offsets when offset_store=dynamodb
    #[arg(long, global = true)]
    pub offset_dynamodb_table: Option<String>,
    /// skippr.workspace
    #[arg(long, global = true)]
    pub workspace: Option<String>,
    /// skippr.skipprd_el_storage_mode
    #[arg(long, global = true, value_enum)]
    pub storage_mode: Option<ElStorageMode>,
    /// skippr.skippr_s3_bucket
    #[arg(long, global = true)]
    pub skippr_s3_bucket: Option<String>,
    /// skippr.tenant
    #[arg(long, global = true)]
    pub tenant: Option<String>,
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
    /// Show or apply persisted pipeline metadata.
    Metadata {
        #[command(subcommand)]
        action: metadata::MetadataAction,
    },
    Doctor(DoctorOptions),
    Df(DfOptions),
    Connect(connect_generated::ConnectArgs),
}

#[derive(Parser, Clone, PartialEq)]
pub struct SyncOptions {
    /// The pipeline to use
    #[arg(short, long)]
    pub pipeline: Option<String>,
    /// Output mode: "progress" (interactive), "json" (structured JSON lines), or "text" (plain text)
    #[arg(long, default_value = "progress")]
    pub output: String,
    /// Run a single sync pass and exit (instead of continuous daemon mode)
    #[arg(long, default_value_t = false)]
    pub once: bool,
}

#[derive(Parser, Clone, PartialEq)]
pub struct DisocverOptions {
    /// The pipeline to use
    #[arg(short, long)]
    pub pipeline: Option<String>,
    /// Output mode: "progress" (interactive), "json" (structured JSON lines), or "text" (plain text)
    #[arg(long, default_value = "progress")]
    pub output: String,
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

#[derive(Parser, Clone, PartialEq)]
pub struct DoctorOptions {
    /// Output mode: text or json
    #[arg(long, default_value = "text")]
    pub output: String,
}

#[derive(Parser, Clone, PartialEq)]
pub struct DfOptions {
    /// Pipeline to read (also `-p`)
    #[arg(short, long)]
    pub pipeline: Option<String>,
    /// Namespace, or pipeline.namespace
    #[arg(long)]
    pub namespace: Option<String>,
    /// Print CSV-like rows instead of a table
    #[arg(long, default_value_t = false)]
    pub plain: bool,
}
