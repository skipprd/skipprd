use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static mut CLI_LOGS_ENABLED: bool = false;

pub fn init_logging(level_opt: Option<String>) {
    let enabled = level_opt.is_some();
    unsafe { CLI_LOGS_ENABLED = enabled; }
    if !enabled {
        // Do not install a subscriber; tracing macros become no-ops
        return;
    }

    let fmt_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_level(true);

    // Default filter (when --log with no level): info
    // - Silence noisy deps (AWS SDK/Smithy, HTTP stacks, TLS) to warn+
    // You can override via RUST_LOG.
    let base_filter = "\
aws_config=warn,\
aws_credential_types=warn,\
aws_smithy_types=warn,\
aws_smithy_http=warn,\
aws_smithy_runtime=warn,\
aws_smithy_client=warn,\
aws_sig_auth=warn,\
hyper=warn,reqwest=warn,rustls=warn,h2=warn";

    // Determine our crate log level
    let lvl = level_opt.unwrap_or_else(|| "info".to_string()).to_lowercase();
    let crate_level = match lvl.as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "warn"  => "warn",
        "error" => "error",
        _ => "info",
    };
    let composed = format!("skippr={},{}", crate_level, base_filter);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(composed));

    // let filter = EnvFilter::new(default_filter);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}

pub fn cli_logs_enabled() -> bool {
    unsafe { CLI_LOGS_ENABLED }
}


