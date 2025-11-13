use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn init_logging(enabled: bool) {
    if !enabled {
        // Do not install a subscriber; tracing macros become no-ops
        return;
    }

    let fmt_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_level(true);

    // Default filter:
    // - Our crate at info when --log is enabled
    // - Silence noisy deps (AWS SDK/Smithy, HTTP stacks, TLS) to warn+
    // You can override via RUST_LOG.
    let default_filter = "info,\
aws_config=warn,\
aws_credential_types=warn,\
aws_smithy_types=warn,\
aws_smithy_http=warn,\
aws_smithy_runtime=warn,\
aws_smithy_client=warn,\
aws_sig_auth=warn,\
hyper=warn,reqwest=warn,rustls=warn,h2=warn";

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_filter));

    // let filter = EnvFilter::new(default_filter);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}


