use std::sync::OnceLock;

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static mut CLI_LOGS_ENABLED: bool = false;
static LOGGING_INIT: OnceLock<()> = OnceLock::new();

const LOG_DIR_NAME: &str = "logs";
const MAX_LOG_FILES: usize = 10;

pub fn init_logging(level_opt: Option<String>) {
    let enabled = level_opt.is_some();
    unsafe {
        CLI_LOGS_ENABLED = enabled;
    }
    if LOGGING_INIT.set(()).is_err() {
        return;
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

    let lvl = parse_log_level(level_opt.as_deref().unwrap_or("info"))
        .unwrap_or_else(|err| panic!("{err}"));
    let composed = env_filter_directives(&lvl, base_filter);

    let file_appender = build_file_appender();

    match (enabled, file_appender) {
        (true, Some(appender)) => {
            let stderr_filter = EnvFilter::new(&composed);
            let file_filter = EnvFilter::new(&composed);

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
            let filter = EnvFilter::new(&composed);

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
            let file_filter = EnvFilter::new(&composed);

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
        eprintln!("skipprd: could not create log directory {:?}", log_dir);
        return None;
    }

    purge_old_logs(&log_dir, MAX_LOG_FILES);

    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("skipprd")
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

/// Remove old skipprd log files beyond the retention limit.
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
                .map(|n| n.starts_with("skipprd") && n.ends_with(".log"))
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

pub fn parse_log_level(raw: &str) -> Result<String, String> {
    let level = raw.to_ascii_lowercase();
    match level.as_str() {
        "trace" | "debug" | "info" | "warn" | "error" | "perf" => Ok(level),
        _ => Err(format!(
            "unknown log level {raw}; expected perf|trace|debug|info|warn|error"
        )),
    }
}

pub fn env_filter_directives(level: &str, base_filter: &str) -> String {
    let crate_filter = match level {
        "trace" => "skipprd=trace,skippr=trace",
        "debug" => "skipprd=debug,skippr=debug",
        "warn" => "skipprd=warn,skippr=warn",
        "error" => "skipprd=error,skippr=error",
        "perf" => "skipprd::perf=info,skipprd=warn,skippr=warn",
        "info" => "skipprd=info,skippr=info",
        other => panic!("unparsed log level {other}; call parse_log_level first"),
    };
    format!("{crate_filter},{base_filter}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "aws=warn,hyper=warn";

    #[test]
    fn perf_level_enables_skipprd_perf_and_warns_the_rest() {
        let filter = env_filter_directives("perf", BASE);
        assert!(filter.contains("skipprd::perf=info"), "{filter}");
        assert!(filter.contains("skipprd=warn"), "{filter}");
        assert!(filter.contains("skippr=warn"), "{filter}");
        assert!(
            !filter.contains("skipprd=debug") && !filter.contains("skipprd=info,"),
            "{filter}"
        );
        EnvFilter::try_new(&filter).expect("perf filter should parse");
    }

    #[test]
    fn unknown_level_is_rejected() {
        assert!(parse_log_level("verbose").is_err());
        assert_eq!(parse_log_level("perf").unwrap(), "perf");
        assert_eq!(parse_log_level("INFO").unwrap(), "info");
    }

    #[test]
    fn explicit_log_level_is_filter_authority() {
        let src = include_str!("logging.rs");
        let impl_src = src.split("mod tests").next().expect("logging tests module");
        assert!(
            !impl_src.contains("try_from_default_env"),
            "--log composed filter is SoT; RUST_LOG must not override"
        );
        assert!(impl_src.contains("EnvFilter::new(&composed)"));
    }
}
