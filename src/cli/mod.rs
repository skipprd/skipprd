use clap::{Parser, ValueEnum};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// What mode to run the program in
    #[arg(value_enum)]
    pub(crate) mode: Mode,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub(crate) enum Mode {
    Discover,
    Sync,
}

// #[derive(Subcommand)]
// pub enum SyncCommand {
//     Sync {
//         // input_plugin: Option<String>,
//         // input_serde: Option<String>,
//     }
// }
//
// #[derive(Subcommand)]
// pub enum DiscoverCommand {
//     Discover {
//         // input_plugin: Option<String>,
//         // input_serde: Option<String>,
//     }
// }
