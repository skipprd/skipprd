use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static mut CLI_LOGS_ENABLED: bool = false;

const LOG_DIR_NAME: &str = "logs";
const MAX_LOG_FILES: usize = 10;

pub fn init_logging(level_opt: Option<String>) {
    let enabled = level_opt.is_some();
    unsafe {
        CLI_LOGS_ENABLED = enabled;
    }

    let base_filter = "\
aws_config=warn,\
aws_credential_types=warn,\
aws_smithy_types=warn,\
aws_smithy_http=warn,\
aws_smithy_runtime=warn,\
aws_smithy_client=warn,\
aws_sig_auth=warn,\
hyper=warn,reqwest=warn,rustls=warn,h2=warn";

    let lvl = level_opt
        .as_deref()
        .unwrap_or("info")
        .to_lowercase();
    let crate_level = match lvl.as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    };
    let composed = format!("skippr={},{}", crate_level, base_filter);

    let file_appender = build_file_appender();

    match (enabled, file_appender) {
        (true, Some(appender)) => {
            let stderr_filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&composed));
            let file_filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&composed));

            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_target(false)
                        .with_thread_ids(false)
                        .with_level(true)
                        .with_filter(stderr_filter),
                )
                .with(
                    fmt::layer()
                        .with_writer(appender)
                        .with_target(true)
                        .with_thread_ids(false)
                        .with_level(true)
                        .with_ansi(false)
                        .with_filter(file_filter),
                )
                .init();
        }
        (true, None) => {
            let filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&composed));

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
        (false, Some(appender)) => {
            let file_filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&composed));

            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .with_writer(appender)
                        .with_target(true)
                        .with_thread_ids(false)
                        .with_level(true)
                        .with_ansi(false)
                        .with_filter(file_filter),
                )
                .init();
        }
        (false, None) => {
            // No logging at all
        }
    }
}

fn build_file_appender() -> Option<tracing_appender::rolling::RollingFileAppender> {
    let log_dir = resolve_log_dir()?;
    if std::fs::create_dir_all(&log_dir).is_err() {
        eprintln!("skippr-el: could not create log directory {:?}", log_dir);
        return None;
    }

    purge_old_logs(&log_dir, MAX_LOG_FILES);

    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("skippr-el")
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(&log_dir)
        .ok()
}

fn resolve_log_dir() -> Option<std::path::PathBuf> {
    if let Ok(data_dir) = std::env::var("DATA_DIR") {
        if !data_dir.is_empty() {
            return Some(std::path::PathBuf::from(data_dir).join(LOG_DIR_NAME));
        }
    }
    let cwd = std::env::current_dir().ok()?;
    Some(cwd.join(LOG_DIR_NAME))
}

/// Remove old skippr-el log files beyond the retention limit.
/// `tracing-appender` handles rotation but not cleanup of files it no longer
/// manages (e.g. leftover from previous config). Belt-and-suspenders.
fn purge_old_logs(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut log_files: Vec<(std::path::PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("skippr-el") && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), modified))
        })
        .collect();

    if log_files.len() <= keep {
        return;
    }

    log_files.sort_by(|a, b| b.1.cmp(&a.1));

    for (path, _) in log_files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(&path);
    }
}

pub fn cli_logs_enabled() -> bool {
    unsafe { CLI_LOGS_ENABLED }
}
