use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

use crate::public_config::SkipprDbtConfig;
use crate::skippr_bin;

const MAX_INVENTORY_FILES: usize = 80;
const MAX_LOG_FILES: usize = 5;
const MAX_EXCERPT_BYTES: u64 = 16 * 1024;
const MAX_EXCERPT_CHARS: usize = 8 * 1024;
const MAX_DEPTH: usize = 5;

/// Wrapper for values that must not appear in default report/debug serialization.
///
/// Operational paths must call `expose()` deliberately when they genuinely need
/// the raw value, for example to write a config file or launch a connector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sensitive<T>(T);

impl<T> Sensitive<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> Serialize for Sensitive<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("[REDACTED]")
    }
}

#[derive(Debug, Serialize)]
struct DiagnosticsBundle {
    schema_version: u32,
    generated_at: String,
    feedback_id: String,
    thread_id: String,
    platform: PlatformDiagnostics,
    config: ConfigDiagnostics,
    binaries: Vec<BinaryDiagnostics>,
    local_artifacts: Vec<FileInventoryEntry>,
    latest_logs: Vec<LogExcerpt>,
}

#[derive(Debug, Serialize)]
struct PlatformDiagnostics {
    os: String,
    arch: String,
    current_exe: Option<String>,
    cwd: String,
}

#[derive(Debug, Serialize)]
struct ConfigDiagnostics {
    path: String,
    summary: ConfigSummary,
    redacted_yaml: Option<String>,
    read_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ConfigSummary {
    project_present: bool,
    warehouse_kind: Option<&'static str>,
    source_kind: Option<&'static str>,
    dbt_configured: bool,
    schema_sink_configured: bool,
}

#[derive(Debug, Serialize)]
struct BinaryDiagnostics {
    name: String,
    expected_version: Option<String>,
    path_results: Vec<String>,
    managed_path: Option<String>,
    managed_exists: Option<bool>,
    lookup_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FileInventoryEntry {
    root: String,
    path: String,
    size_bytes: u64,
    modified_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LogExcerpt {
    path: String,
    size_bytes: u64,
    modified_at: Option<String>,
    excerpt: String,
    truncated: bool,
}

pub fn collect(
    cfg: &SkipprDbtConfig,
    config_path: &Path,
    thread_id: &str,
    feedback_id: &str,
) -> serde_json::Value {
    let project_root = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = dirs_next::home_dir();
    let normalizer = PathNormalizer::new(cwd.clone(), home.clone(), project_root.clone());

    let mut artifact_roots = vec![
        ("project .skippr", project_root.join(".skippr")),
        ("project .react", project_root.join(".react")),
    ];
    if let Some(home) = home.as_ref() {
        artifact_roots.push(("home .skippr", home.join(".skippr")));
    }

    let mut inventory = Vec::new();
    let mut log_candidates = Vec::new();
    for (label, root) in artifact_roots {
        collect_inventory(
            label,
            &root,
            &normalizer,
            &mut inventory,
            &mut log_candidates,
        );
    }
    inventory.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    inventory.truncate(MAX_INVENTORY_FILES);

    log_candidates.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    let latest_logs = log_candidates
        .into_iter()
        .take(MAX_LOG_FILES)
        .filter_map(|entry| read_log_excerpt(entry, &normalizer))
        .collect();

    let bundle = DiagnosticsBundle {
        schema_version: 1,
        generated_at: Utc::now().to_rfc3339(),
        feedback_id: feedback_id.to_string(),
        thread_id: thread_id.to_string(),
        platform: PlatformDiagnostics {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            current_exe: std::env::current_exe()
                .ok()
                .map(|p| normalizer.normalize_path(&p)),
            cwd: normalizer.normalize_path(&cwd),
        },
        config: config_diagnostics(cfg, config_path, &normalizer),
        binaries: binary_diagnostics(&normalizer),
        local_artifacts: inventory,
        latest_logs,
    };

    serde_json::to_value(bundle).unwrap_or_else(|e| {
        json!({
            "schema_version": 1,
            "generated_at": Utc::now().to_rfc3339(),
            "feedback_id": feedback_id,
            "thread_id": thread_id,
            "error": format!("failed to serialize diagnostics: {e}")
        })
    })
}

fn config_diagnostics(
    cfg: &SkipprDbtConfig,
    config_path: &Path,
    normalizer: &PathNormalizer,
) -> ConfigDiagnostics {
    let summary = ConfigSummary {
        project_present: !cfg.project.trim().is_empty(),
        warehouse_kind: cfg.warehouse_kind_str(),
        source_kind: cfg.source_kind_str(),
        dbt_configured: cfg.dbt.is_some(),
        schema_sink_configured: cfg.schema_sink.is_some(),
    };

    match fs::read_to_string(config_path) {
        Ok(raw) => {
            let redacted_yaml = match serde_yaml::from_str::<serde_yaml::Value>(&raw) {
                Ok(mut value) => {
                    redact_yaml_value(&mut value, None, normalizer);
                    serde_yaml::to_string(&value).ok()
                }
                Err(_) => Some(redact_freeform(&normalizer.normalize_text(&raw))),
            };
            ConfigDiagnostics {
                path: normalizer.normalize_path(config_path),
                summary,
                redacted_yaml,
                read_error: None,
            }
        }
        Err(e) => ConfigDiagnostics {
            path: normalizer.normalize_path(config_path),
            summary,
            redacted_yaml: None,
            read_error: Some(e.to_string()),
        },
    }
}

fn binary_diagnostics(normalizer: &PathNormalizer) -> Vec<BinaryDiagnostics> {
    let managed = skippr_bin::managed_binary_path().ok();
    let legacy_managed = dirs_next::home_dir().map(|home| {
        home.join(".skippr").join("bin").join(if cfg!(windows) {
            "skippr-el.exe"
        } else {
            "skippr-el"
        })
    });
    vec![
        BinaryDiagnostics {
            name: "skippr".to_string(),
            expected_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            path_results: lookup_binary("skippr", normalizer),
            managed_path: None,
            managed_exists: None,
            lookup_error: None,
        },
        BinaryDiagnostics {
            name: "skipprd".to_string(),
            expected_version: Some(skippr_bin::SKIPPR_VERSION.to_string()),
            path_results: lookup_binary("skipprd", normalizer),
            managed_path: managed.as_ref().map(|p| normalizer.normalize_path(p)),
            managed_exists: managed.as_ref().map(|p| p.is_file()),
            lookup_error: None,
        },
        BinaryDiagnostics {
            name: "skippr-el".to_string(),
            expected_version: None,
            path_results: lookup_binary("skippr-el", normalizer),
            managed_path: legacy_managed
                .as_ref()
                .map(|p| normalizer.normalize_path(p)),
            managed_exists: legacy_managed.as_ref().map(|p| p.is_file()),
            lookup_error: None,
        },
    ]
}

fn lookup_binary(name: &str, normalizer: &PathNormalizer) -> Vec<String> {
    let locator = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(locator).arg(name).output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| normalizer.normalize_text(line))
            .collect(),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = stderr.trim();
            if msg.is_empty() {
                Vec::new()
            } else {
                vec![redact_freeform(&normalizer.normalize_text(msg))]
            }
        }
        Err(e) => vec![format!("lookup failed: {e}")],
    }
}

fn collect_inventory(
    label: &str,
    root: &Path,
    normalizer: &PathNormalizer,
    inventory: &mut Vec<FileInventoryEntry>,
    log_candidates: &mut Vec<FileInventoryEntry>,
) {
    if !root.exists() {
        return;
    }
    collect_inventory_inner(label, root, root, normalizer, inventory, log_candidates, 0);
}

fn collect_inventory_inner(
    label: &str,
    root: &Path,
    dir: &Path,
    normalizer: &PathNormalizer,
    inventory: &mut Vec<FileInventoryEntry>,
    log_candidates: &mut Vec<FileInventoryEntry>,
    depth: usize,
) {
    if depth > MAX_DEPTH || inventory.len() >= MAX_INVENTORY_FILES * 2 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if should_never_collect(&path) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_inventory_inner(
                label,
                root,
                &path,
                normalizer,
                inventory,
                log_candidates,
                depth + 1,
            );
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let entry = FileInventoryEntry {
            root: label.to_string(),
            path: normalizer.normalize_path(&path),
            size_bytes: metadata.len(),
            modified_at: metadata.modified().ok().map(system_time_rfc3339),
        };
        if is_log_or_state_file(root, &path) {
            log_candidates.push(entry.clone());
        }
        inventory.push(entry);
    }
}

fn read_log_excerpt(entry: FileInventoryEntry, normalizer: &PathNormalizer) -> Option<LogExcerpt> {
    let path = normalizer.denormalize_path_hint(&entry.path)?;
    let mut file = fs::File::open(&path).ok()?;
    let start = entry.size_bytes.saturating_sub(MAX_EXCERPT_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return None;
    }
    let mut buf = Vec::new();
    if file.take(MAX_EXCERPT_BYTES).read_to_end(&mut buf).is_err() {
        return None;
    }
    let mut text = String::from_utf8_lossy(&buf).to_string();
    text = normalizer.normalize_text(&text);
    text = redact_freeform(&text);
    let truncated_by_chars = text.chars().count() > MAX_EXCERPT_CHARS;
    if truncated_by_chars {
        text = text.chars().take(MAX_EXCERPT_CHARS).collect::<String>();
        text.push_str("\n[TRUNCATED]");
    }
    Some(LogExcerpt {
        path: entry.path,
        size_bytes: entry.size_bytes,
        modified_at: entry.modified_at,
        excerpt: text,
        truncated: start > 0 || truncated_by_chars,
    })
}

fn should_never_collect(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name == "credentials.json"
        || name.ends_with(".p8")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.contains("private_key")
}

fn is_log_or_state_file(root: &Path, path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let path_lc = path.to_string_lossy().to_ascii_lowercase();
    let rel_lc = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_ascii_lowercase();
    name.ends_with(".log")
        || name.ends_with(".out")
        || name.ends_with(".err")
        || (name.ends_with(".txt") && path_lc.contains("log"))
        || (name.ends_with(".json")
            && (rel_lc.contains("/logs/")
                || rel_lc.contains("\\logs\\")
                || rel_lc.contains("/state/")
                || rel_lc.contains("\\state\\")
                || rel_lc.contains("/threads/")
                || rel_lc.contains("\\threads\\")))
}

fn redact_yaml_value(
    value: &mut serde_yaml::Value,
    key_hint: Option<&str>,
    normalizer: &PathNormalizer,
) {
    if key_hint.is_some_and(is_sensitive_key) {
        *value = serde_yaml::Value::String("[REDACTED]".to_string());
        return;
    }

    match value {
        serde_yaml::Value::Mapping(map) => {
            for (key, child) in map.iter_mut() {
                let key_hint = key.as_str();
                redact_yaml_value(child, key_hint, normalizer);
            }
        }
        serde_yaml::Value::Sequence(seq) => {
            for child in seq {
                redact_yaml_value(child, None, normalizer);
            }
        }
        serde_yaml::Value::String(s) => {
            *s = redact_freeform(&normalizer.normalize_text(s));
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("password")
        || key.contains("secret")
        || key.contains("token")
        || key.contains("api_key")
        || key.contains("apikey")
        || key.contains("private_key")
        || key.contains("connection_string")
        || key.contains("sas")
        || key.contains("credential")
        || key == "key"
        || key.ends_with("_key")
        || key == "user"
        || key == "username"
        || key == "account"
        || key.contains("authorization")
        || key.contains("cookie")
}

fn redact_freeform(input: &str) -> String {
    let mut out = redact_email_like(input);
    out = redact_key_value_segments(&out);
    out = redact_bearer_tokens(&out);
    out
}

fn redact_email_like(input: &str) -> String {
    input
        .split_whitespace()
        .map(|token| {
            let trimmed = token.trim_matches(|c: char| {
                matches!(c, ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']')
            });
            if trimmed.contains('@') && trimmed.rsplit_once('.').is_some() {
                token.replace(trimmed, "[REDACTED_EMAIL]")
            } else {
                token.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_key_value_segments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut token = String::new();
    for ch in input.chars() {
        if matches!(ch, ';' | '&' | '\n' | '\r') {
            out.push_str(&redact_assignment_token(&token));
            token.clear();
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    out.push_str(&redact_assignment_token(&token));
    out
}

fn redact_assignment_token(token: &str) -> String {
    let Some((key, value)) = token.split_once('=') else {
        return token.to_string();
    };
    if is_sensitive_key(key.trim()) || key.trim().eq_ignore_ascii_case("sig") {
        let suffix = value
            .find(char::is_whitespace)
            .map(|idx| &value[idx..])
            .unwrap_or_default();
        format!("{}=[REDACTED]{}", key, suffix)
    } else {
        token.to_string()
    }
}

fn redact_bearer_tokens(input: &str) -> String {
    let mut words = input.split_whitespace().peekable();
    let mut out = Vec::new();
    while let Some(word) = words.next() {
        out.push(word.to_string());
        if word.eq_ignore_ascii_case("bearer") {
            if words.next().is_some() {
                out.push("[REDACTED]".to_string());
            }
        }
    }
    out.join(" ")
}

fn system_time_rfc3339(time: SystemTime) -> String {
    let dt: DateTime<Utc> = time.into();
    dt.to_rfc3339()
}

struct PathNormalizer {
    cwd: PathBuf,
    home: Option<PathBuf>,
    project_root: PathBuf,
}

impl PathNormalizer {
    fn new(cwd: PathBuf, home: Option<PathBuf>, project_root: PathBuf) -> Self {
        Self {
            cwd,
            home,
            project_root,
        }
    }

    fn normalize_path(&self, path: &Path) -> String {
        self.normalize_text(&path.to_string_lossy())
    }

    fn normalize_text(&self, text: &str) -> String {
        let mut out = text.to_string();
        out = replace_path_prefix(out, &self.project_root, "$PROJECT_ROOT");
        out = replace_path_prefix(out, &self.cwd, "$CWD");
        if let Some(home) = self.home.as_ref() {
            out = replace_path_prefix(out, home, "$HOME");
        }
        out
    }

    fn denormalize_path_hint(&self, value: &str) -> Option<PathBuf> {
        let path = if let Some(rest) = value.strip_prefix("$PROJECT_ROOT") {
            self.project_root.join(rest.trim_start_matches(['/', '\\']))
        } else if let Some(rest) = value.strip_prefix("$CWD") {
            self.cwd.join(rest.trim_start_matches(['/', '\\']))
        } else if let (Some(home), Some(rest)) = (self.home.as_ref(), value.strip_prefix("$HOME")) {
            home.join(rest.trim_start_matches(['/', '\\']))
        } else {
            PathBuf::from(value)
        };
        Some(path)
    }
}

fn replace_path_prefix(mut text: String, prefix: &Path, replacement: &str) -> String {
    let prefix = prefix.to_string_lossy();
    if prefix.is_empty() || prefix == "." {
        return text;
    }
    text = text.replace(prefix.as_ref(), replacement);
    let alt = prefix.replace('\\', "/");
    if alt != prefix {
        text = text.replace(&alt, replacement);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_default_serialization_redacts_value() {
        let value = Sensitive::new("supersecret".to_string());
        assert_eq!(value.expose(), "supersecret");
        assert_eq!(serde_json::to_value(value).unwrap(), "[REDACTED]");
    }

    #[test]
    fn redacts_yaml_sensitive_fields_but_keeps_shape() {
        let dir = tempfile::tempdir().unwrap();
        let normalizer = PathNormalizer::new(dir.path().into(), None, dir.path().into());
        let mut value: serde_yaml::Value = serde_yaml::from_str(
            r#"
project: mssql-migration
warehouse:
  kind: snowflake
  user: ubaid@example.com
source:
  kind: mssql
  connection_string: server=tcp:localhost;password=SkipprPass123!
"#,
        )
        .unwrap();

        redact_yaml_value(&mut value, None, &normalizer);
        let yaml = serde_yaml::to_string(&value).unwrap();

        assert!(yaml.contains("project: mssql-migration"));
        assert!(yaml.contains("kind: snowflake"));
        assert!(yaml.contains("user: '[REDACTED]'") || yaml.contains("user: \"[REDACTED]\""));
        assert!(
            yaml.contains("connection_string: '[REDACTED]'")
                || yaml.contains("connection_string: \"[REDACTED]\"")
        );
        assert!(!yaml.contains("SkipprPass123"));
        assert!(!yaml.contains("ubaid@example.com"));
    }

    #[test]
    fn redacts_freeform_connection_strings_and_bearer_tokens() {
        let redacted = redact_freeform(
            "Authorization: Bearer abc.def.ghi; password=secret&sig=abc user@example.com",
        );

        assert!(redacted.contains("Bearer [REDACTED]"));
        assert!(redacted.contains("password=[REDACTED]"));
        assert!(redacted.contains("sig=[REDACTED]"));
        assert!(redacted.contains("[REDACTED_EMAIL]"));
        assert!(!redacted.contains("abc.def.ghi"));
        assert!(!redacted.contains("user@example.com"));
    }

    #[test]
    fn collect_includes_redacted_config_and_latest_logs() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("skippr.yaml");
        fs::write(
            &config,
            r#"
project: demo
warehouse:
  kind: snowflake
  password: topsecret
source:
  kind: mssql
  connection_string: server=tcp:localhost;password=SkipprPass123!
"#,
        )
        .unwrap();
        let logs = dir.path().join(".react/logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("react.log"),
            "failed with password=secret and email user@example.com",
        )
        .unwrap();
        let cfg = SkipprDbtConfig::load_from(&config).unwrap();

        let value = collect(&cfg, &config, "thread-1", "feedback-1");
        let serialized = serde_json::to_string(&value).unwrap();

        assert!(serialized.contains("redacted_yaml"));
        assert!(serialized.contains("latest_logs"));
        assert!(!serialized.contains("topsecret"));
        assert!(!serialized.contains("SkipprPass123"));
        assert!(!serialized.contains("user@example.com"));
    }
}
