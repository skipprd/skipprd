use clap::{Parser, ValueEnum};

// support commands `skippr query 'SELECT * FROM table'`

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// What mode to run the program in
    #[arg(value_enum)]
    pub(crate) mode: Mode,

    /// The SQL query to run
    #[arg(requires_if("mode", "query"))]
    pub(crate) query: Option<String>,

    #[arg(requires_if("mode", "schema"))]
    pub(crate) schema: Option<String>,

    // #[arg(short, long, requires_if("mode", "sync"))]
    // pub(crate) display_metrics: bool,


}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub(crate) enum Mode {
    Discover,
    Sync,
    Query,
    Schema,
    // Dump,
    // Head,
    // Tail,
    // Diff,

}

