use std::sync::OnceLock;

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static LOGGING_INIT: OnceLock<()> = OnceLock::new();

/// Child-process stderr logger. Host skipprd logging stays in the engine crate.
pub fn init_logging(level_opt: Option<String>) {
    if LOGGING_INIT.set(()).is_err() {
        return;
    }
    let Some(level_opt) = level_opt else {
        return;
    };
    let lvl = parse_log_level(&level_opt).unwrap_or_else(|err| panic!("{err}"));
    let filter = EnvFilter::new(env_filter_directives(&lvl));
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false)
                .with_thread_ids(false)
                .with_level(true),
        )
        .with(filter)
        .init();
}

pub fn parse_log_level(raw: &str) -> Result<String, String> {
    let level = raw.to_ascii_lowercase();
    match level.as_str() {
        "trace" | "debug" | "info" | "warn" | "error" | "perf" => Ok(level),
        _ => Err(format!(
            "unknown log level {raw}; expected perf|trace|debug|info|warn|error"
        )),
    }
}

fn env_filter_directives(level: &str) -> String {
    let crate_filter = match level {
        "trace" => "skippr_runtime_sdk=trace",
        "debug" => "skippr_runtime_sdk=debug",
        "warn" => "skippr_runtime_sdk=warn",
        "error" => "skippr_runtime_sdk=error",
        "perf" => "skippr_runtime_sdk=info",
        "info" => "skippr_runtime_sdk=info",
        other => panic!("unparsed log level {other}; call parse_log_level first"),
    };
    format!("{crate_filter},aws_config=warn,hyper=warn,reqwest=warn,rustls=warn,h2=warn")
}
