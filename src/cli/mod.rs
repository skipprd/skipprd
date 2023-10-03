use clap::Parser;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) mode: Mode,
}

#[derive(Parser)]
pub enum Mode {
    Discover,
    Sync(SyncOptions),
    Query(QueryOptions),
    Schema(SchemaOptions),
    // ... other modes
}

#[derive(Parser)]
pub struct SyncOptions {
    /// The pipeline to use
    #[arg(short, long)]
    pub(crate) pipeline: Option<String>,
}

#[derive(Parser)]
pub struct QueryOptions {
    /// The SQL query to run
    #[arg(short, long)]
    pub(crate) query: String,
}

#[derive(Parser)]
pub struct SchemaOptions {
    /// The schema to use
    #[arg(short, long)]
    pub(crate) schema: String,
}
