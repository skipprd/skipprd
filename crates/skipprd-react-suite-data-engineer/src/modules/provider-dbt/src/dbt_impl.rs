// TODO(item-78): This file (~1500 lines) covers DBT project management, command
// execution, result parsing, progress tracking, and Docker runner support. Consider
// decomposing into:
//   - dbt_runner.rs (host + docker command execution)
//   - dbt_parsing.rs (manifest/result JSON parsing, failure classification)
//   - dbt_progress.rs (progress state machine)
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{io::BufRead, process::Stdio};

use crate::adapters::storage::StorageAdapter;
use crate::providers::{Keyspace, RequestScope};

use react_suite_data_engineer::providers::{
    DbtProvider, DbtValidateArgs, DbtValidateResult, DBT_GOLD_DATABASE_ENV, DBT_GOLD_SCHEMA_ENV,
    DBT_SILVER_DATABASE_ENV, DBT_SILVER_SCHEMA_ENV,
};

#[derive(Clone)]
pub struct DbtProjectProvider {
    pub storage: Arc<dyn StorageAdapter>,
    pub keyspace: Arc<dyn Keyspace>,
    /// How DBT commands should be executed (host vs docker).
    pub runner: DbtRunnerConfig,
}

impl DbtProjectProvider {
    pub fn new(
        storage: Arc<dyn StorageAdapter>,
        keyspace: Arc<dyn Keyspace>,
        runner: DbtRunnerConfig,
    ) -> Self {
        Self {
            storage,
            keyspace,
            runner,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DbtRunnerConfig {
    /// Runner mode: host (default) or docker.
    pub mode: DbtRunnerMode,
    /// Docker image to use when mode=="docker" (pinned strongly recommended).
    pub docker_image: Option<String>,
    /// Optional docker platform (e.g. "linux/amd64").
    pub docker_platform: Option<String>,
    /// Optional docker network (e.g. "host" or a named network).
    pub docker_network: Option<String>,
    /// If true, mount ~/.aws into the container at /root/.aws (useful for AWS_PROFILE flows).
    pub docker_mount_aws_dir: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DbtRunnerMode {
    #[default]
    Host,
    Docker,
}

impl DbtRunnerMode {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "host" => Ok(Self::Host),
            "docker" => Ok(Self::Docker),
            other => Err(format!(
                "invalid dbt runner mode '{other}'. expected 'host' or 'docker'"
            )),
        }
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())
}

#[derive(Clone, Debug)]
struct SanitizedProjectYaml {
    text: String,
    changed: bool,
    /// One entry per top-level key the sanitizer removed from `dbt_project.yml`. Callers must
    /// persist these into the active plan's stripped-artifact buffer so the next author/repair
    /// turn can surface them. Empty when nothing was stripped.
    stripped: Vec<react_suite_data_engineer::StrippedArtifact>,
}

fn canonical_dbt_project_name(_scope: &RequestScope) -> &'static str {
    "data_engineer"
}

fn resolve_dbt_project_name(scope: &RequestScope, requested: &str) -> Result<String, String> {
    let canonical = canonical_dbt_project_name(scope);
    let requested = requested.trim();
    if requested.is_empty() || requested == canonical {
        return Ok(canonical.to_string());
    }
    Err(format!(
        "dbt project_name is system-managed for the data_engineer suite: expected '{canonical}', got '{requested}'"
    ))
}

/// Canonical template for the **shared** `dbt_project.yml`.
///
/// The system governs the project's structural identity (name, profile, model-paths,
/// schema-suffix strategy). The LLM may extend the file with per-folder `models:` configuration
/// for things like `+materialized`, but every top-level key listed in
/// [`react_suite_data_engineer::file_ownership`]'s `dbt_project.yml` policy is stripped by
/// [`sanitize_dbt_project_yaml`] on every save and reported back as a `StrippedArtifact`.
fn render_dbt_project_yaml(project_name: &str, profile_name: &str) -> String {
    format!(
        "name: {name}\nversion: '1.0'\nprofile: '{profile_name}'\nmodel-paths: ['models']\nseed-paths: ['seeds']\nmacro-paths: ['macros']\ntarget-path: 'target'\n\non-run-start:\n  # Snowflake needs the target databases to exist before dbt creates schemas inside them.\n  - \"{{% if target.type == 'snowflake' %}}create database if not exists {{{{ env_var('{silver_db_env}', target.database) }}}}{{% else %}}select 1{{% endif %}}\"\n  - \"{{% if target.type == 'snowflake' %}}create database if not exists {{{{ env_var('{gold_db_env}', target.database) }}}}{{% else %}}select 1{{% endif %}}\"\n\nmodels:\n  {name}:\n    # Default gold models into the configured gold tier namespace.\n    +database: \"{{{{ env_var('{gold_db_env}', target.database) }}}}\"\n    +schema: \"{{{{ env_var('{gold_schema_env}', target.schema) }}}}\"\n    # Force staging models under models/staging into the configured silver tier namespace.\n    staging:\n      +database: \"{{{{ env_var('{silver_db_env}', target.database) }}}}\"\n      +schema: \"{{{{ env_var('{silver_schema_env}', target.schema) }}}}\"\n",
        name = project_name,
        profile_name = profile_name,
        silver_db_env = DBT_SILVER_DATABASE_ENV,
        silver_schema_env = DBT_SILVER_SCHEMA_ENV,
        gold_db_env = DBT_GOLD_DATABASE_ENV,
        gold_schema_env = DBT_GOLD_SCHEMA_ENV
    )
}

/// Truncate a serialized YAML value to keep prompt size bounded while still conveying the intent
/// of the stripped content. Long values are cut at a byte boundary with a trailing marker.
fn truncate_value_summary(yaml: &str) -> String {
    const MAX_CHARS: usize = 400;
    let trimmed = yaml.trim_start_matches("---\n").trim_end().to_string();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed;
    }
    let mut out: String = trimmed.chars().take(MAX_CHARS).collect();
    out.push_str("\n... (truncated)");
    out
}

/// Strip-and-notify, not strip-silently.
///
/// Every removed top-level key that the LLM AUTHORED produces a
/// [`react_suite_data_engineer::StrippedArtifact`] so the next author/repair turn can re-author
/// the intent in the right place. The list of keys to strip and the relocation hints come from
/// [`react_suite_data_engineer::file_ownership`] — the single source of truth for file
/// ownership across the suite.
///
/// The canonical template (the one [`render_dbt_project_yaml`] writes on bootstrap) supplies
/// system-managed values for structural keys like `name`, `profile`, `model-paths`. When the
/// existing file's value for such a key matches the canonical value, we treat it as
/// system-rendered and silently keep it. When it diverges, we emit a strip artifact and restore
/// the canonical value. For pure-strip keys (canonical template has no value), any LLM-authored
/// presence emits an artifact.
fn sanitize_dbt_project_yaml(
    raw: &str,
    project_name: &str,
    desired_profile: &str,
) -> SanitizedProjectYaml {
    use react_suite_data_engineer::file_ownership;
    let mut changed = false;
    let mut stripped: Vec<react_suite_data_engineer::StrippedArtifact> = Vec::new();

    let canonical_text = render_dbt_project_yaml(project_name, desired_profile);
    let canonical_map: serde_yaml::Mapping =
        match serde_yaml::from_str::<serde_yaml::Value>(&canonical_text) {
            Ok(serde_yaml::Value::Mapping(m)) => m,
            _ => serde_yaml::Mapping::new(),
        };

    let mut v: serde_yaml::Value = match serde_yaml::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            changed = true;
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
        }
    };
    if !matches!(v, serde_yaml::Value::Mapping(_)) {
        v = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        changed = true;
    }
    let map = v.as_mapping_mut().expect("mapping enforced above");

    let strip_keys = file_ownership::shared_skippr_top_level_keys("dbt_project.yml");
    for key in strip_keys {
        let key_value = serde_yaml::Value::String((*key).to_string());
        let canonical_value = canonical_map.get(&key_value).cloned();
        let Some(existing_value) = map.remove(&key_value) else {
            // Key absent from existing. If the canonical template provides one, install it
            // silently (matches the prior "force canonical profile" behaviour).
            if let Some(cv) = canonical_value {
                map.insert(key_value, cv);
                changed = true;
            }
            continue;
        };
        let matches_canonical = canonical_value.as_ref() == Some(&existing_value);
        if matches_canonical {
            // System-rendered key still matches the canonical template value: keep it, no
            // artifact emitted.
            map.insert(key_value, existing_value);
            continue;
        }
        // Divergent (system-rendered key changed by the LLM) or pure-strip (no canonical):
        // emit a strip artifact and install canonical if one exists.
        changed = true;
        let value_summary = serde_yaml::to_string(&existing_value)
            .map(|s| truncate_value_summary(&s))
            .unwrap_or_default();
        let reason = match file_ownership::classify_rel_path("dbt_project.yml") {
            file_ownership::Ownership::Shared { reason, .. } => reason.to_string(),
            _ => String::new(),
        };
        let hint = file_ownership::relocation_hint("dbt_project.yml", key).map(|s| s.to_string());
        stripped.push(react_suite_data_engineer::StrippedArtifact {
            file: "dbt_project.yml".to_string(),
            key_path: vec![(*key).to_string()],
            value_summary,
            reason,
            relocation_hint: hint,
        });
        if let Some(cv) = canonical_value {
            map.insert(key_value, cv);
        }
    }

    let text = serde_yaml::to_string(&v).unwrap_or_else(|_| raw.to_string());
    if text != raw {
        changed = true;
    }
    SanitizedProjectYaml {
        text,
        changed,
        stripped,
    }
}

#[derive(Clone, Debug)]
struct CmdOut {
    status_ok: bool,
    code: i32,
    stdout: String,
    stderr: String,
}

fn redact_docker_args_for_log(args: &[String]) -> String {
    // IMPORTANT: docker args can include `-e KEY=VALUE` where VALUE may be a secret.
    // Redact only known secret env keys so non-sensitive operational values stay observable.
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut next_is_env = false;
    let is_secret_key = |k: &str| {
        let k_uc = k.to_ascii_uppercase();
        k_uc.contains("AWS_SECRET_ACCESS_KEY")
            || k_uc.contains("AWS_SESSION_TOKEN")
            || k_uc.contains("AWS_ACCESS_KEY_ID")
            || k_uc.contains("LLM_API_KEY")
            || k_uc.contains("OPENAI_API_KEY")
            || k_uc.contains("ANTHROPIC_API_KEY")
            || k_uc.contains("GOOGLE_API_KEY")
            || k_uc.contains("AZURE_OPENAI_API_KEY")
            || k_uc.contains("API_KEY")
            || k_uc.contains("TOKEN")
            || k_uc.contains("SECRET")
    };
    for a in args.iter() {
        if next_is_env {
            next_is_env = false;
            if let Some((k, _v)) = a.split_once('=') {
                if is_secret_key(k) {
                    out.push(format!("{}=***", k));
                } else {
                    out.push(a.clone());
                }
            } else {
                out.push(a.clone());
            }
            continue;
        }
        if a == "-e" || a == "--env" {
            out.push(a.clone());
            next_is_env = true;
            continue;
        }
        // Also redact inline KEY=VALUE tokens for common AWS secrets if present.
        if let Some((k, _v)) = a.split_once('=') {
            if is_secret_key(k) {
                out.push(format!("{}=***", k));
                continue;
            }
        }
        out.push(a.clone());
    }
    out.join(" ")
}

fn strip_ansi(s: &str) -> String {
    // Very small ANSI stripper for dbt CLI output (primarily color codes).
    // Removes ESC [ ... m sequences.
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(ch) = it.next() {
        if ch == '\u{1b}' && matches!(it.peek(), Some('[')) {
            // consume '['
            let _ = it.next();
            // skip until 'm' (or end)
            while let Some(c2) = it.next() {
                if c2 == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn should_log_dbt_info_line(line: &str) -> bool {
    // Keep INFO logs high-signal only. We still buffer full stdout/stderr for error handling.
    let clean = strip_ansi(line);
    let t = clean.trim();
    t.contains("Done. PASS=")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DbtProgressMode {
    Build,
    Compile,
}

impl Default for DbtProgressMode {
    fn default() -> Self {
        Self::Build
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DbtItemStatus {
    Start,
    Ok,
    Pass,
    Error,
    Fail,
    Skip,
    Warn,
    NoOp,
}

impl Default for DbtItemStatus {
    fn default() -> Self {
        Self::Start
    }
}

impl DbtItemStatus {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            DbtItemStatus::Ok
                | DbtItemStatus::Pass
                | DbtItemStatus::Error
                | DbtItemStatus::Fail
                | DbtItemStatus::Skip
                | DbtItemStatus::Warn
                | DbtItemStatus::NoOp
        )
    }
}

#[derive(Clone, Debug, Default)]
struct DbtItemState {
    status: DbtItemStatus,
    name: Option<String>,
    kind: Option<String>, // e.g. "model" | "test"
}

#[derive(Default)]
struct DbtProgressState {
    mode: DbtProgressMode,
    total: Option<usize>,
    items_by_idx: std::collections::HashMap<usize, DbtItemState>,
    // Build: running items captured from START lines.
    running_names_set: std::collections::HashSet<String>,
    running_names_recent: std::collections::VecDeque<String>,
    // Early init signals (useful before any START lines).
    found_models: Option<usize>,
    found_tests: Option<usize>,
    concurrency_threads: Option<usize>,
    dbt_version: Option<String>,
    // Compile: derived progress.
    compiled_total: Option<usize>,
    compiled_count: usize,
    last_compiled_node: Option<String>,
    in_compile_sql_dump: bool,
    // Build: authoritative final summary (if present).
    summary_pass: Option<usize>,
    summary_ok: Option<usize>,
    summary_error: Option<usize>,
    summary_skip: Option<usize>,
    summary_total: Option<usize>,
    // Debounce
    last_emit: Option<String>,
}

impl DbtProgressState {
    fn new(mode: DbtProgressMode) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }

    fn consume_line(&mut self, raw: &str) -> Option<String> {
        let s = strip_ansi(raw);
        let line = s.trim();
        if line.is_empty() {
            return None;
        }
        let content = strip_leading_hms_prefix(line);

        // Common early signals.
        if let Some(pos) = content.find("Running with dbt=") {
            let v = &content[pos + "Running with dbt=".len()..];
            let ver = v.split_whitespace().next().unwrap_or("").trim();
            if !ver.is_empty() {
                self.dbt_version = Some(ver.to_string());
            }
        }
        if content.contains("Found ") && content.contains(" models") {
            // Example: "Found 6 models, 9 data tests, 3 sources, 461 macros"
            // Very conservative parse: only the first two integers (models, tests).
            let toks: Vec<&str> = content.split_whitespace().collect();
            if toks.len() >= 2 {
                // Find token after "Found"
                if let Some(found_idx) = toks.iter().position(|t| *t == "Found") {
                    if let Some(v) = toks.get(found_idx + 1) {
                        if let Ok(n) = v.parse::<usize>() {
                            self.found_models = Some(n);
                            if self.mode == DbtProgressMode::Compile {
                                self.compiled_total = Some(n);
                            }
                        }
                    }
                } else if let Ok(n) = toks[1].parse::<usize>() {
                    self.found_models = Some(n);
                    if self.mode == DbtProgressMode::Compile {
                        self.compiled_total = Some(n);
                    }
                }
            }
            if let Some(pos) = toks.iter().position(|t| *t == "tests," || *t == "tests") {
                if pos >= 1 {
                    if let Ok(n) = toks[pos - 1].parse::<usize>() {
                        self.found_tests = Some(n);
                    }
                }
            }
        }
        if content.contains("Concurrency:") && content.contains("threads") {
            // Example: "Concurrency: 15 threads (target='athena')"
            let toks: Vec<&str> = content.split_whitespace().collect();
            if toks.len() >= 2 {
                if let Some(conc_idx) = toks.iter().position(|t| t.starts_with("Concurrency:")) {
                    if let Some(v) = toks.get(conc_idx + 1) {
                        if let Ok(n) = v.parse::<usize>() {
                            self.concurrency_threads = Some(n);
                        }
                    }
                }
            }
        }

        // Compile-specific headers.
        if self.mode == DbtProgressMode::Compile {
            if let Some(pos) = content.find("Compiled node '") {
                let rest = &content[pos + "Compiled node '".len()..];
                if let Some((name, _tail)) = rest.split_once("'") {
                    let nm = name.trim();
                    if !nm.is_empty() {
                        // Only increment once per node (best-effort).
                        if self.last_compiled_node.as_deref() != Some(nm) {
                            self.compiled_count = self.compiled_count.saturating_add(1);
                        }
                        self.last_compiled_node = Some(nm.to_string());
                    }
                    self.in_compile_sql_dump = true;
                }
            } else if line.starts_with("finished") || line.starts_with("starting") {
                self.in_compile_sql_dump = false;
            } else if self.in_compile_sql_dump {
                // Ignore noisy compiled SQL dump lines.
                return None;
            }
        }

        // Build/compile shared: i-of-n lines.
        if let Some((idx, total, status, rest_after_status)) = parse_i_of_n_status(content) {
            if total > 0 {
                self.total = Some(self.total.unwrap_or(0).max(total));
            }
            let st = parse_status(status);
            let item = self
                .items_by_idx
                .entry(idx)
                .or_insert_with(|| DbtItemState {
                    status: st,
                    ..Default::default()
                });
            item.status = st;

            // Try to extract a compact name for START/running display.
            if item.name.is_none() {
                if let Some((kind, nm)) = extract_item_kind_and_name(rest_after_status) {
                    item.kind = Some(kind);
                    item.name = Some(nm.clone());
                }
            }

            // Maintain running set.
            if st == DbtItemStatus::Start {
                if let Some(ref nm) = item.name {
                    if self.running_names_set.insert(nm.clone()) {
                        self.running_names_recent.push_back(nm.clone());
                    } else {
                        // Refresh recency: move to back.
                        self.running_names_recent.retain(|x| x != nm);
                        self.running_names_recent.push_back(nm.clone());
                    }
                }
            } else if st.is_terminal() {
                if let Some(ref nm) = item.name {
                    self.running_names_set.remove(nm);
                    self.running_names_recent.retain(|x| x != nm);
                }
            }
        }

        // Build: final summary line.
        if self.mode == DbtProgressMode::Build && content.starts_with("Done.") {
            // Example: "Done. PASS=15 WARN=0 ERROR=2 SKIP=0 NO-OP=0 TOTAL=17"
            for tok in content.split_whitespace() {
                if let Some((k, v)) = tok.split_once('=') {
                    let v = v.trim().trim_end_matches(',');
                    let n = v.parse::<usize>().ok();
                    match k {
                        "PASS" => self.summary_pass = n,
                        "OK" => self.summary_ok = n,
                        "ERROR" => self.summary_error = n,
                        "SKIP" => self.summary_skip = n,
                        "TOTAL" => self.summary_total = n,
                        _ => {}
                    }
                }
            }
            if let Some(t) = self.summary_total {
                self.total = Some(t);
            }
        }

        let detail = self.render_detail();
        if self.last_emit.as_deref() == Some(&detail) {
            return None;
        }
        self.last_emit = Some(detail.clone());
        Some(detail)
    }

    fn render_detail(&self) -> String {
        match self.mode {
            DbtProgressMode::Build => {
                let total = self.total;
                let (done, pass, ok, err, skip) = if self.summary_total.is_some() {
                    let p = self.summary_pass.unwrap_or(0);
                    let o = self.summary_ok.unwrap_or(0);
                    let e = self.summary_error.unwrap_or(0);
                    let sk = self.summary_skip.unwrap_or(0);
                    let dn = p + o + e + sk;
                    (dn, p, o, e, sk)
                } else {
                    let mut p = 0usize;
                    let mut o = 0usize;
                    let mut e = 0usize;
                    let mut sk = 0usize;
                    let mut dn = 0usize;
                    for it in self.items_by_idx.values() {
                        match it.status {
                            DbtItemStatus::Pass => {
                                p += 1;
                                dn += 1;
                            }
                            DbtItemStatus::Ok => {
                                o += 1;
                                dn += 1;
                            }
                            DbtItemStatus::Error | DbtItemStatus::Fail => {
                                e += 1;
                                dn += 1;
                            }
                            DbtItemStatus::Skip => {
                                sk += 1;
                                dn += 1;
                            }
                            DbtItemStatus::Warn | DbtItemStatus::NoOp => {
                                dn += 1;
                            }
                            DbtItemStatus::Start => {}
                        }
                    }
                    (dn, p, o, e, sk)
                };

                let mut base = if let Some(t) = total {
                    format!("({done}/{t} (PASS={pass} OK={ok} ERROR={err} SKIP={skip}))")
                } else {
                    let mut bits: Vec<String> = Vec::new();
                    if let Some(v) = self.dbt_version.as_deref() {
                        bits.push(format!("dbt={v}"));
                    }
                    if let Some(n) = self.concurrency_threads {
                        bits.push(format!("threads={n}"));
                    }
                    if let Some(n) = self.found_models {
                        bits.push(format!("models={n}"));
                    }
                    if let Some(n) = self.found_tests {
                        bits.push(format!("tests={n}"));
                    }
                    if bits.is_empty() {
                        "(init)".to_string()
                    } else {
                        format!("(init: {})", bits.join(" "))
                    }
                };

                if !self.running_names_recent.is_empty() {
                    let shown: Vec<String> = self
                        .running_names_recent
                        .iter()
                        .rev()
                        .take(2)
                        .cloned()
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    base.push_str(" · running: ");
                    base.push_str(&shown.join(", "));
                    let remaining = self.running_names_set.len().saturating_sub(shown.len());
                    if remaining > 0 {
                        base.push_str(&format!(" +{remaining}"));
                    }
                }
                base
            }
            DbtProgressMode::Compile => {
                let total = self.compiled_total.or(self.found_models);
                let mut s = if let Some(t) = total {
                    format!("({}/{})", self.compiled_count.min(t), t)
                } else {
                    format!("({})", self.compiled_count)
                };
                if let Some(ref last) = self.last_compiled_node {
                    s.push_str(&format!(" · last: {}", last));
                }
                s
            }
        }
    }
}

fn strip_leading_hms_prefix(s: &str) -> &str {
    // dbt default logs often prefix lines with "HH:MM:SS" and spacing.
    // Example: "08:10:08  Found 6 models, ..."
    let mut it = s.splitn(2, ' ');
    let first = it.next().unwrap_or("").trim();
    if first.len() == 8
        && first.as_bytes().get(2) == Some(&b':')
        && first.as_bytes().get(5) == Some(&b':')
        && first
            .chars()
            .filter(|c| *c != ':')
            .all(|c| c.is_ascii_digit())
    {
        return it.next().unwrap_or("").trim_start();
    }
    s
}

fn parse_i_of_n_status(line: &str) -> Option<(usize, usize, &str, &str)> {
    // Find "... <i> of <n> <status> <rest> ..."
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.len() < 5 {
        return None;
    }
    for i in 1..toks.len().saturating_sub(2) {
        if toks[i] != "of" {
            continue;
        }
        let left = toks.get(i.wrapping_sub(1)).copied().unwrap_or("");
        let right = toks.get(i + 1).copied().unwrap_or("");
        let status = toks.get(i + 2).copied().unwrap_or("");
        if let (Ok(idx), Ok(total)) = (left.parse::<usize>(), right.parse::<usize>()) {
            // rest after status: slice original line by locating status token occurrence.
            let mut parts = line.splitn(2, status);
            let _ = parts.next();
            let rest = parts.next().unwrap_or("").trim();
            return Some((idx, total, status, rest));
        }
    }
    None
}

fn parse_status(s: &str) -> DbtItemStatus {
    match s.trim().to_ascii_uppercase().as_str() {
        "START" => DbtItemStatus::Start,
        "OK" => DbtItemStatus::Ok,
        "PASS" => DbtItemStatus::Pass,
        "ERROR" => DbtItemStatus::Error,
        "FAIL" => DbtItemStatus::Fail,
        "SKIP" => DbtItemStatus::Skip,
        "WARN" => DbtItemStatus::Warn,
        "NO-OP" | "NOOP" => DbtItemStatus::NoOp,
        _ => DbtItemStatus::Start,
    }
}

fn extract_item_kind_and_name(rest: &str) -> Option<(String, String)> {
    // Examples:
    // - "sql view model de_picnic_dev_example_gold2.dim_customers .......... [RUN]"
    // - "test not_null_dim_customers_customer_id ........................... [RUN]"
    // - "creating sql view model de_picnic...fct_order_items [ERROR in 70.87s]"
    let r = rest.trim();
    // Cut off bracket status suffix.
    let r = r.split("[").next().unwrap_or(r).trim();
    let toks: Vec<&str> = r.split_whitespace().collect();
    if toks.is_empty() {
        return None;
    }
    let mut kind = None;
    let mut name = None;
    if let Some(pos) = toks.iter().position(|t| *t == "test") {
        kind = Some("test".to_string());
        name = toks.get(pos + 1).map(|s| s.to_string());
    } else if toks.iter().any(|t| *t == "model") {
        kind = Some("model".to_string());
        // pick the last token that contains a dot (relation fqn) and take suffix after last dot.
        for t in toks.iter().rev() {
            if t.contains('.') {
                let nm = t.split('.').last().unwrap_or(t).to_string();
                name = Some(nm);
                break;
            }
        }
    } else if toks.first().map(|s| *s) == Some("relationships") {
        kind = Some("test".to_string());
        name = Some(toks[0].to_string());
    }
    let nm = name?.trim_matches('.').trim().to_string();
    if nm.is_empty() {
        return None;
    }
    Some((kind.unwrap_or_else(|| "item".to_string()), nm))
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn feed(mode: DbtProgressMode, lines: &[&str]) -> Vec<String> {
        let mut st = DbtProgressState::new(mode);
        let mut out = Vec::new();
        for l in lines {
            if let Some(d) = st.consume_line(l) {
                out.push(d);
            }
        }
        out
    }

    #[test]
    fn build_progress_emits_on_start_and_completion_and_tracks_running_names() {
        let lines = [
            "08:10:08  Found 6 models, 9 data tests, 3 sources, 461 macros",
            "08:10:08  Concurrency: 15 threads (target='athena')",
            "08:10:13  1 of 6 START sql view model de_x.stg_test_raw_raw_customers  [RUN]",
            "08:10:19  1 of 6 OK created sql view model de_x.stg_test_raw_raw_customers  [OK -1 in 6.25s]",
            "08:10:24  3 of 6 START test not_null_dim_customers_customer_id  [RUN]",
            "08:10:28  3 of 6 PASS not_null_dim_customers_customer_id  [PASS in 3.96s]",
        ];
        let out = feed(DbtProgressMode::Build, &lines);
        // Should have init, then start, then ok/pass updates.
        assert!(out.iter().any(|s| s.contains("(init:")));
        assert!(out.iter().any(|s| s.contains("running:")));
        assert!(out.iter().any(|s| s.contains("PASS=") && s.contains("OK=")));
        assert!(out.last().unwrap().contains("PASS=1"));
    }

    #[test]
    fn build_progress_parses_out_of_order_and_done_summary() {
        let lines = [
            "08:10:24  6 of 6 START test unique_x  [RUN]",
            "08:10:28  6 of 6 PASS unique_x  [PASS in 3.90s]",
            "08:10:28  3 of 6 PASS not_null_x  [PASS in 3.96s]",
            "08:10:28  4 of 6 PASS rel_x  [PASS in 3.97s]",
            "08:12:01  5 of 6 ERROR rel_y  [ERROR in 96.09s]",
            "08:12:01  Done. PASS=5 WARN=0 ERROR=1 SKIP=0 NO-OP=0 TOTAL=6",
        ];
        let out = feed(DbtProgressMode::Build, &lines);
        let last = out.last().unwrap();
        assert!(last.contains("6/6"));
        assert!(last.contains("PASS=5"));
        assert!(last.contains("ERROR=1"));
    }

    #[test]
    fn compile_progress_counts_compiled_nodes_and_ignores_sql_dump_lines() {
        let lines = [
            "08:10:03  Found 6 models, 11 data tests, 3 sources, 461 macros",
            "Compiled node 'dim_customers' is:",
            "with customers as (",
            "    select * from foo",
            "Compiled node 'dim_orders' is:",
            "select 1",
        ];
        let out = feed(DbtProgressMode::Compile, &lines);
        // Should include 1/6 then 2/6, and ignore dump lines.
        assert!(out.iter().any(|s| s.starts_with("(1/6)")));
        assert!(out.iter().any(|s| s.starts_with("(2/6)")));
        assert!(!out.iter().any(|s| s.contains("select * from foo")));
        assert!(out.last().unwrap().contains("last: dim_orders"));
    }
}

fn run_cmd_labeled(
    cmd: &str,
    args: &[&str],
    cwd: &Path,
    envs: &[(&str, String)],
    label: &str,
) -> CmdOut {
    let mut c = std::process::Command::new(cmd);
    c.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs.iter() {
        c.env(k, v);
    }
    run_spawned_cmd_labeled(c, "host", cmd, args.join(" "), label)
}

fn resolve_host_dbt_command() -> String {
    if let Ok(path) = std::env::var("DBT_BIN") {
        let path = path.trim();
        if !path.is_empty() {
            return path.to_string();
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(".venv").join("bin").join("dbt");
        if candidate.is_file() {
            return candidate.display().to_string();
        }
    }
    "dbt".to_string()
}

fn run_dbt_host_labeled(
    dbt_args: &[&str],
    cwd: &Path,
    envs: &[(&str, String)],
    label: &str,
) -> CmdOut {
    let cmd = resolve_host_dbt_command();
    run_cmd_labeled(&cmd, dbt_args, cwd, envs, label)
}

fn run_spawned_cmd_labeled(
    mut cmd: std::process::Command,
    runner_label: &'static str,
    cmd_for_log: &str,
    args_for_log: String,
    label: &str,
) -> CmdOut {
    let started = std::time::Instant::now();
    tracing::info!(
        target: "dbt",
        phase = %label,
        runner = %runner_label,
        cmd = %cmd_for_log,
        args = %args_for_log,
        "starting"
    );

    let mut child = match cmd.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            return CmdOut {
                status_ok: false,
                code: -1,
                stdout: String::new(),
                stderr: format!("spawn error: {}", e),
            };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (tx, rx) = std::sync::mpsc::channel::<(bool, String)>(); // (is_stderr, line)
    if let Some(out) = stdout {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let br = std::io::BufReader::new(out);
            for line in br.lines().flatten() {
                let _ = tx.send((false, line));
            }
        });
    }
    if let Some(err) = stderr {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let br = std::io::BufReader::new(err);
            for line in br.lines().flatten() {
                let _ = tx.send((true, line));
            }
        });
    }
    drop(tx);

    let mut out_buf = String::new();
    let mut err_buf = String::new();
    let mut prog = if label == "build" {
        Some(DbtProgressState::new(DbtProgressMode::Build))
    } else if label == "compile" {
        Some(DbtProgressState::new(DbtProgressMode::Compile))
    } else {
        None
    };
    for (is_err, line) in rx {
        if is_err {
            err_buf.push_str(&line);
            err_buf.push('\n');
        } else {
            if let Some(p) = prog.as_mut() {
                let _ = p.consume_line(&line);
            }
            if should_log_dbt_info_line(&line) {
                tracing::info!(
                    target: "dbt",
                    phase = %label,
                    runner = %runner_label,
                    stream = "stdout",
                    "{}",
                    line
                );
            }
            out_buf.push_str(&line);
            out_buf.push('\n');
        }
    }

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            return CmdOut {
                status_ok: false,
                code: -1,
                stdout: out_buf,
                stderr: format!("wait error: {}", e),
            };
        }
    };
    let code = status.code().unwrap_or(-1);
    let ok = status.success();
    tracing::info!(
        target: "dbt",
        phase = %label,
        runner = %runner_label,
        exit_code = code,
        ok = ok,
        duration_ms = started.elapsed().as_millis() as u64,
        "finished"
    );
    CmdOut {
        status_ok: ok,
        code,
        stdout: out_buf,
        stderr: err_buf,
    }
}

fn build_docker_run_args(
    runner: &DbtRunnerConfig,
    project_dir: &Path,
    profiles_dir: Option<&Path>,
    dbt_args: &[&str],
    envs: &[(&str, String)],
) -> Result<Vec<String>, String> {
    let image = runner
        .docker_image
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "dbt runner is docker but providers.dbt.docker_image is not set".to_string()
        })?;

    let proj = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let proj_s = proj.to_string_lossy().to_string();

    let mut args: Vec<String> = Vec::new();
    args.push("run".to_string());
    args.push("--rm".to_string());
    if let Some(p) = runner
        .docker_platform
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        args.push("--platform".to_string());
        args.push(p.trim().to_string());
    }
    if let Some(n) = runner
        .docker_network
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        args.push("--network".to_string());
        args.push(n.trim().to_string());
    }

    // Mount project at /project.
    args.push("-v".to_string());
    args.push(format!("{}:/project", proj_s));
    args.push("-w".to_string());
    args.push("/project".to_string());

    // Mount profiles at /profiles and force DBT_PROFILES_DIR to that path.
    if let Some(pd) = profiles_dir {
        let pd = pd.canonicalize().unwrap_or_else(|_| pd.to_path_buf());
        args.push("-v".to_string());
        args.push(format!("{}:/profiles", pd.to_string_lossy()));
        args.push("-e".to_string());
        args.push("DBT_PROFILES_DIR=/profiles".to_string());
    }

    // Pass through envs requested by caller.
    for (k, v) in envs.iter() {
        args.push("-e".to_string());
        args.push(format!("{}={}", k, v));
    }

    // Pass through common AWS env vars if present. This avoids baking secrets into files.
    for k in [
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_SDK_LOAD_CONFIG",
    ] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                args.push("-e".to_string());
                args.push(format!("{}={}", k, v));
            }
        }
    }

    // Pass through Snowflake env vars for the dbt-snowflake adapter.
    for k in [
        "SNOWFLAKE_ACCOUNT",
        "SNOWFLAKE_USER",
        "SNOWFLAKE_PASSWORD",
        "SNOWFLAKE_WAREHOUSE",
        "SNOWFLAKE_DATABASE",
        "SNOWFLAKE_ROLE",
        "SNOWFLAKE_SCHEMA",
    ] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                args.push("-e".to_string());
                args.push(format!("{}={}", k, v));
            }
        }
    }

    // Key-pair auth: mount the private key file and remap the env var to the
    // container-internal path so dbt's `private_key_path` resolves correctly.
    if let Ok(host_key_path) = std::env::var("SNOWFLAKE_PRIVATE_KEY_PATH") {
        let host_key_path = host_key_path.trim().to_string();
        if !host_key_path.is_empty() {
            let container_key_path = "/run/secrets/snowflake_key.p8";
            args.push("-v".to_string());
            args.push(format!("{}:{}:ro", host_key_path, container_key_path));
            args.push("-e".to_string());
            args.push(format!("SNOWFLAKE_PRIVATE_KEY_PATH={}", container_key_path));
        }
    }

    // Pass through MSSQL env vars for the dbt-sqlserver adapter.
    for k in [
        "MSSQL_HOST",
        "MSSQL_USER",
        "MSSQL_PASSWORD",
        "MSSQL_DATABASE",
        "MSSQL_PORT",
    ] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                args.push("-e".to_string());
                args.push(format!("{}={}", k, v));
            }
        }
    }

    // Pass through Postgres env vars for the dbt-postgres adapter.
    for k in [
        "POSTGRES_HOST",
        "POSTGRES_USER",
        "POSTGRES_PASSWORD",
        "POSTGRES_PORT",
        "POSTGRES_DATABASE",
    ] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                args.push("-e".to_string());
                args.push(format!("{}={}", k, v));
            }
        }
    }

    // BigQuery: pass through project/location env vars and mount the service
    // account credentials file when present.
    for k in ["BIGQUERY_PROJECT", "BIGQUERY_LOCATION"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                args.push("-e".to_string());
                args.push(format!("{}={}", k, v));
            }
        }
    }
    if let Ok(host_cred_path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        let host_cred_path = host_cred_path.trim().to_string();
        if !host_cred_path.is_empty() {
            let container_cred_path = "/run/secrets/gcp_credentials.json";
            args.push("-v".to_string());
            args.push(format!("{}:{}:ro", host_cred_path, container_cred_path));
            args.push("-e".to_string());
            args.push(format!(
                "GOOGLE_APPLICATION_CREDENTIALS={}",
                container_cred_path
            ));
        }
    }

    // Optional: mount ~/.aws for profile-based auth chains.
    if runner.docker_mount_aws_dir {
        if let Ok(home) = std::env::var("HOME") {
            let aws_dir = Path::new(&home).join(".aws");
            if aws_dir.exists() {
                args.push("-v".to_string());
                args.push(format!("{}:/root/.aws:ro", aws_dir.to_string_lossy()));
            }
        }
    }

    // Image + command.
    args.push("--entrypoint".to_string());
    args.push("dbt".to_string());
    args.push(image);
    for a in dbt_args.iter() {
        args.push(a.to_string());
    }
    Ok(args)
}

fn run_cmd_docker_labeled(
    runner: &DbtRunnerConfig,
    project_dir: &Path,
    profiles_dir: Option<&Path>,
    dbt_args: &[&str],
    envs: &[(&str, String)],
    label: &str,
) -> CmdOut {
    let args = match build_docker_run_args(runner, project_dir, profiles_dir, dbt_args, envs) {
        Ok(v) => v,
        Err(e) => {
            return CmdOut {
                status_ok: false,
                code: -1,
                stdout: String::new(),
                stderr: e,
            };
        }
    };
    let mut c = std::process::Command::new("docker");
    c.args(&args)
        .current_dir(project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_spawned_cmd_labeled(
        c,
        "docker",
        "docker",
        redact_docker_args_for_log(&args),
        label,
    )
}

fn run_cmd_for_runner_labeled(
    runner: &DbtRunnerConfig,
    project_dir: &Path,
    profiles_dir: Option<&Path>,
    dbt_args: &[&str],
    envs: &[(&str, String)],
    label: &str,
) -> CmdOut {
    match runner.mode {
        DbtRunnerMode::Host => run_dbt_host_labeled(dbt_args, project_dir, envs, label),
        DbtRunnerMode::Docker => {
            run_cmd_docker_labeled(runner, project_dir, profiles_dir, dbt_args, envs, label)
        }
    }
}

fn combine_errors(a: &CmdOut, b: &CmdOut) -> Vec<String> {
    let mut v = Vec::new();
    if !a.status_ok {
        if !a.stderr.trim().is_empty() {
            v.push(a.stderr.trim().to_string());
        }
        if !a.stdout.trim().is_empty() {
            v.push(a.stdout.trim().to_string());
        }
    }
    if !b.status_ok {
        if !b.stderr.trim().is_empty() {
            v.push(b.stderr.trim().to_string());
        }
        if !b.stdout.trim().is_empty() {
            v.push(b.stdout.trim().to_string());
        }
    }
    v
}

impl DbtProjectProvider {
    async fn ensure_storage_project_yaml(
        &self,
        scope: &RequestScope,
        project_name: &str,
    ) -> Result<Vec<react_suite_data_engineer::StrippedArtifact>, String> {
        let project_key = self.keyspace.scoped_key(scope, &["dbt", "dbt_project.yml"]);
        let existing = if self
            .storage
            .head_etag(&project_key)
            .await
            .map_err(|e| e.to_string())?
            .is_some()
        {
            Some(
                self.storage
                    .get_bytes(&project_key)
                    .await
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };

        let rendered = render_dbt_project_yaml(project_name, scope.project_id.as_str());
        let existing_text = existing
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).to_string());
        let base = existing_text.as_deref().unwrap_or(&rendered);
        let sanitized = sanitize_dbt_project_yaml(base, project_name, scope.project_id.as_str());
        if existing.is_none() || sanitized.changed {
            self.storage
                .put_bytes(&project_key, sanitized.text.as_bytes(), "text/yaml")
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(sanitized.stripped)
    }

    async fn ensure_local_project_yaml(
        &self,
        scope: &RequestScope,
        project_name: &str,
        proj_path: &Path,
    ) -> Result<Vec<react_suite_data_engineer::StrippedArtifact>, String> {
        if !proj_path.exists() {
            let rendered = render_dbt_project_yaml(project_name, scope.project_id.as_str());
            write_file(proj_path, rendered.as_bytes())?;
        }
        let raw = std::fs::read_to_string(proj_path).unwrap_or_default();
        let sanitized = sanitize_dbt_project_yaml(&raw, project_name, scope.project_id.as_str());
        if sanitized.changed {
            write_file(proj_path, sanitized.text.as_bytes())?;
            // Best-effort persistence back to storage for future runs.
            let project_key = self.keyspace.scoped_key(scope, &["dbt", "dbt_project.yml"]);
            let _ = self
                .storage
                .put_bytes(&project_key, sanitized.text.as_bytes(), "text/yaml")
                .await;
        }
        Ok(sanitized.stripped)
    }

    async fn upload_dir_to_storage(&self, local_dir: &Path, prefix: &str) -> Result<usize, String> {
        if !local_dir.is_dir() {
            return Ok(0);
        }
        let mut stack: Vec<PathBuf> = vec![local_dir.to_path_buf()];
        let mut uploaded: usize = 0;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path.strip_prefix(local_dir).map_err(|e| e.to_string())?;
                let mut key = String::from(prefix);
                let rel_s = rel.to_string_lossy().replace('\\', "/");
                key.push_str(&rel_s);
                let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                let content_type = match path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase()
                    .as_str()
                {
                    "sql" => "text/sql",
                    "json" => "application/json",
                    "yml" | "yaml" => "text/yaml",
                    "txt" => "text/plain",
                    _ => "application/octet-stream",
                };
                self.storage
                    .put_bytes(&key, &bytes, content_type)
                    .await
                    .map_err(|e| e.to_string())?;
                uploaded += 1;
            }
        }
        Ok(uploaded)
    }
}

#[async_trait]
impl DbtProvider for DbtProjectProvider {
    async fn ensure_minimal_project(
        &self,
        scope: &RequestScope,
    ) -> Result<Vec<react_suite_data_engineer::StrippedArtifact>, String> {
        let name = canonical_dbt_project_name(scope);
        self.ensure_storage_project_yaml(scope, &name).await
    }

    async fn write_model_sql(
        &self,
        scope: &RequestScope,
        rel_path: &str,
        sql: &str,
    ) -> Result<String, String> {
        let rel = rel_path.trim_start_matches('/');
        if !rel.starts_with("models/") || !rel.ends_with(".sql") || rel.contains("..") {
            return Err("model rel_path must be a safe models/*.sql path".to_string());
        }
        let key = format!("{}{}", self.keyspace.scoped_prefix(scope, &["dbt"]), rel);
        self.storage
            .put_bytes(&key, sql.as_bytes(), "text/sql")
            .await
            .map_err(|e| e.to_string())?;
        Ok(key)
    }

    async fn write_metricflow_yaml(
        &self,
        scope: &RequestScope,
        rel_path: &str,
        yaml_text: &str,
    ) -> Result<String, String> {
        let rel = rel_path.trim_start_matches('/');
        if !rel.starts_with("metrics/") || !rel.ends_with(".yaml") || rel.contains("..") {
            return Err("metric rel_path must be a safe metrics/*.yaml path".to_string());
        }
        let key = format!("{}{}", self.keyspace.scoped_prefix(scope, &["dbt"]), rel);
        self.storage
            .put_bytes(&key, yaml_text.as_bytes(), "text/yaml")
            .await
            .map_err(|e| e.to_string())?;
        Ok(key)
    }

    async fn validate_project(
        &self,
        scope: &RequestScope,
        args: &DbtValidateArgs,
    ) -> Result<DbtValidateResult, String> {
        let project_name = resolve_dbt_project_name(scope, &args.project_name)?;
        let s3_prefix_base = {
            let pref = self.keyspace.scoped_prefix(scope, &["dbt"]);
            if pref.ends_with('/') {
                pref
            } else {
                format!("{}/", pref)
            }
        };

        let profiles_dir = args
            .profiles_dir
            .clone()
            .or_else(|| std::env::var("DBT_PROFILES_DIR").ok());
        let target = if args.target.is_empty() {
            return Err("dbt validate requires a non-empty target (e.g. 'athena'); no default target exists".to_string());
        } else {
            args.target.clone()
        };

        let run = args.run;
        let build = args.build;
        let select_terms = args.select.as_ref().filter(|v| !v.is_empty());
        let exclude_terms = args.exclude.as_ref().filter(|v| !v.is_empty());

        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let root = tmp.path().join(project_name.clone());
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;

        // Populate temp project from storage prefix
        let keys = self
            .storage
            .list_prefix(&s3_prefix_base)
            .await
            .map_err(|e| e.to_string())?;
        let mut file_count = 0usize;
        for key in keys {
            if key.ends_with('/') {
                continue;
            }
            let rel = key.strip_prefix(&s3_prefix_base).unwrap_or(&key);
            let dest = root.join(rel);
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let bytes = self
                .storage
                .get_bytes(&key)
                .await
                .map_err(|e| e.to_string())?;
            write_file(&dest, &bytes)?;
            file_count += 1;
        }

        let proj = root.join("dbt_project.yml");
        let stripped = self
            .ensure_local_project_yaml(scope, &project_name, &proj)
            .await?;

        let mut envs: Vec<(&str, String)> = Vec::new();
        if let Some(pd) = profiles_dir.as_ref() {
            envs.push(("DBT_PROFILES_DIR", pd.clone()));
        }
        if let Some(tier_routing) = args.tier_routing.as_ref() {
            for (key, value) in tier_routing.env_vars() {
                if !value.trim().is_empty() {
                    envs.push((key, value));
                }
            }
        }

        let profiles_path = profiles_dir.as_ref().map(|s| Path::new(s));

        // Hard fail if dbt CLI itself is broken on this runner (do NOT attempt to repair).
        // This catches host-level Python/env issues early (e.g. import errors) before we try deps/parse/compile.
        let version_res = run_cmd_for_runner_labeled(
            &self.runner,
            &root,
            profiles_path,
            &["--version"],
            &envs,
            "version",
        );
        if !version_res.status_ok {
            return Err(format!(
                "dbt environment check failed (dbt --version). This is a host/runner issue; do not attempt auto-repair.\n\nstdout:\n{}\n\nstderr:\n{}",
                version_res.stdout.trim(),
                version_res.stderr.trim()
            ));
        }

        let deps_res = run_cmd_for_runner_labeled(
            &self.runner,
            &root,
            profiles_path,
            &["deps"],
            &envs,
            "deps",
        );
        let parse_res = run_cmd_for_runner_labeled(
            &self.runner,
            &root,
            profiles_path,
            &["parse"],
            &envs,
            "parse",
        );
        let compile_res = {
            let mut argv: Vec<String> = vec![
                "compile".to_string(),
                "--target".to_string(),
                target.clone(),
            ];
            if let Some(sel) = select_terms {
                for t in sel.iter() {
                    argv.push("--select".to_string());
                    argv.push(t.clone());
                }
            }
            if let Some(ex) = exclude_terms {
                for t in ex.iter() {
                    argv.push("--exclude".to_string());
                    argv.push(t.clone());
                }
            }
            let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            run_cmd_for_runner_labeled(
                &self.runner,
                &root,
                profiles_path,
                &argv_refs,
                &envs,
                "compile",
            )
        };
        let run_or_build_res = if build {
            Some({
                let mut argv: Vec<String> =
                    vec!["build".to_string(), "--target".to_string(), target.clone()];
                if let Some(sel) = select_terms {
                    for t in sel.iter() {
                        argv.push("--select".to_string());
                        argv.push(t.clone());
                    }
                }
                if let Some(ex) = exclude_terms {
                    for t in ex.iter() {
                        argv.push("--exclude".to_string());
                        argv.push(t.clone());
                    }
                }
                let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
                run_cmd_for_runner_labeled(
                    &self.runner,
                    &root,
                    profiles_path,
                    &argv_refs,
                    &envs,
                    "build",
                )
            })
        } else if run {
            Some({
                let mut argv: Vec<String> =
                    vec!["run".to_string(), "--target".to_string(), target.clone()];
                if let Some(sel) = select_terms {
                    for t in sel.iter() {
                        argv.push("--select".to_string());
                        argv.push(t.clone());
                    }
                }
                if let Some(ex) = exclude_terms {
                    for t in ex.iter() {
                        argv.push("--exclude".to_string());
                        argv.push(t.clone());
                    }
                }
                let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
                run_cmd_for_runner_labeled(
                    &self.runner,
                    &root,
                    profiles_path,
                    &argv_refs,
                    &envs,
                    "run",
                )
            })
        } else {
            None
        };

        let ok = deps_res.status_ok
            && parse_res.status_ok
            && compile_res.status_ok
            && run_or_build_res
                .as_ref()
                .map(|o| o.status_ok)
                .unwrap_or(true);

        let mut uploaded_files: usize = 0;
        if compile_res.status_ok
            || run_or_build_res
                .as_ref()
                .map(|r| r.status_ok)
                .unwrap_or(false)
        {
            let local_target = root.join("target");
            if local_target.exists() {
                let target_prefix = format!("{}target/", s3_prefix_base);
                uploaded_files = self
                    .upload_dir_to_storage(&local_target, &target_prefix)
                    .await
                    .unwrap_or(0);
            }
        }

        let mut errs_vec: Vec<String> = Vec::new();
        for e in combine_errors(&deps_res, &parse_res) {
            errs_vec.push(e);
        }
        let empty = CmdOut {
            status_ok: true,
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        for e in combine_errors(&compile_res, run_or_build_res.as_ref().unwrap_or(&empty)) {
            errs_vec.push(e);
        }
        let failure_class =
            react_suite_data_engineer::failure_text::classify_dbt_failure(&errs_vec);

        Ok(DbtValidateResult {
            ok,
            deps_ok: deps_res.status_ok,
            parse_ok: parse_res.status_ok,
            compile_ok: compile_res.status_ok,
            run_ok: run_or_build_res.as_ref().map(|o| o.status_ok),
            uploaded_target_files: uploaded_files,
            failure_class,
            errors: errs_vec,
            warnings: vec![],
            logs: serde_json::json!({
                "deps": { "code": deps_res.code, "stdout": deps_res.stdout, "stderr": deps_res.stderr },
                "parse": { "code": parse_res.code, "stdout": parse_res.stdout, "stderr": parse_res.stderr },
                "compile": { "code": compile_res.code, "stdout": compile_res.stdout, "stderr": compile_res.stderr },
                "run_or_build": run_or_build_res.as_ref().map(|r| serde_json::json!({ "code": r.code, "stdout": r.stdout, "stderr": r.stderr })),
                "fetched_files": file_count,
            }),
            stripped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_dbt_project_yaml_has_expected_paths_and_suffixes() {
        let y = render_dbt_project_yaml("data_engineer", "project_a");
        assert!(y.contains("name: data_engineer"));
        assert!(y.contains("profile: 'project_a'"));
        assert!(y.contains("seed-paths: ['seeds']"));
        assert!(y.contains("macro-paths: ['macros']"));
        assert!(y.contains("DBT_GOLD_DATABASE"));
        assert!(y.contains("DBT_GOLD_SCHEMA"));
        assert!(y.contains("DBT_SILVER_DATABASE"));
        assert!(y.contains("DBT_SILVER_SCHEMA"));
    }

    #[test]
    fn sanitize_dbt_project_yaml_strips_depends_on_and_forces_profile() {
        let raw = "name: demo\ndepends_on: []\nprofile: wrong\n";
        let out = sanitize_dbt_project_yaml(raw, "demo", "correct_profile");
        assert!(out.changed);
        assert!(!out.text.contains("depends_on"));
        assert!(out.text.contains("profile: correct_profile"));
        let keys: Vec<&str> = out
            .stripped
            .iter()
            .map(|a| a.key_path[0].as_str())
            .collect();
        assert!(keys.contains(&"depends_on"));
        assert!(keys.contains(&"profile"));
    }

    #[test]
    fn sanitize_dbt_project_yaml_emits_on_run_start_strip_with_hint() {
        let raw =
            "name: demo\nprofile: demo\non-run-start:\n  - '{{ validate_athena_work_group() }}'\n";
        let out = sanitize_dbt_project_yaml(raw, "demo", "demo");
        assert!(out.changed);
        assert!(!out.text.contains("validate_athena_work_group"));
        assert!(out.text.contains("create database if not exists"));
        let artifact = out
            .stripped
            .iter()
            .find(|a| a.key_path == vec!["on-run-start".to_string()])
            .expect("on-run-start strip artifact");
        assert!(artifact
            .value_summary
            .contains("validate_athena_work_group"));
        assert!(artifact
            .relocation_hint
            .as_ref()
            .expect("hint")
            .contains("pre_hook"));
    }

    #[test]
    fn sanitize_dbt_project_yaml_emits_no_artifact_when_canonical_values_match() {
        let raw = render_dbt_project_yaml("demo", "demo");
        let out = sanitize_dbt_project_yaml(&raw, "demo", "demo");
        assert!(
            out.stripped.is_empty(),
            "sanitizing a freshly rendered template must not emit strip artifacts: {:?}",
            out.stripped
        );
    }

    #[test]
    fn resolve_dbt_project_name_rejects_mismatched_override() {
        let scope = RequestScope::parse(
            "76504ed9-9d6b-415e-812d-fd74cfc93244",
            "dev",
            "mssql_snowflake_e2e_cargo_build_25738299666_1",
        )
        .expect("valid scope");

        assert_eq!(
            resolve_dbt_project_name(&scope, "").expect("default"),
            "data_engineer"
        );
        let err = resolve_dbt_project_name(
            &scope,
            "mssql_snowflake_e2e_cargo_build_25738299666_1_project",
        )
        .unwrap_err();
        assert!(err.contains("system-managed"));
    }

    #[test]
    fn sanitize_dbt_project_yaml_strips_top_level_vars_with_per_model_hint() {
        let raw = "name: demo\nprofile: demo\nvars:\n  some_global: 1\n  another: 'x'\n";
        let out = sanitize_dbt_project_yaml(raw, "demo", "demo");
        assert!(out.changed);
        let artifact = out
            .stripped
            .iter()
            .find(|a| a.key_path == vec!["vars".to_string()])
            .expect("vars strip artifact");
        assert!(artifact
            .relocation_hint
            .as_ref()
            .expect("hint")
            .to_lowercase()
            .contains("per-model"));
    }

    #[test]
    fn docker_args_require_image() {
        let runner = DbtRunnerConfig {
            mode: DbtRunnerMode::Docker,
            docker_image: None,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let err = build_docker_run_args(&runner, tmp.path(), None, &["deps"], &[]).unwrap_err();
        assert!(err.to_lowercase().contains("docker_image"));
    }

    #[test]
    fn docker_args_include_project_mount_and_workdir() {
        let runner = DbtRunnerConfig {
            mode: DbtRunnerMode::Docker,
            docker_image: Some("ghcr.io/dbt-labs/dbt-athena:1.8.3".to_string()),
            docker_platform: Some("linux/amd64".to_string()),
            docker_network: Some("host".to_string()),
            docker_mount_aws_dir: false,
        };
        let tmp = tempfile::tempdir().unwrap();
        let args = build_docker_run_args(&runner, tmp.path(), None, &["deps"], &[]).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("--rm"));
        assert!(joined.contains("--platform linux/amd64"));
        assert!(joined.contains("--network host"));
        assert!(joined.contains(":/project"));
        assert!(joined.contains("-w /project"));
        assert!(joined.contains("ghcr.io/dbt-labs/dbt-athena:1.8.3"));
    }

    #[test]
    fn host_dbt_command_prefers_explicit_env() {
        std::env::set_var("DBT_BIN", "/tmp/custom-dbt");
        assert_eq!(resolve_host_dbt_command(), "/tmp/custom-dbt");
        std::env::remove_var("DBT_BIN");
    }

    #[test]
    fn redact_docker_args_only_redacts_secret_env_keys() {
        let args = vec![
            "docker".to_string(),
            "run".to_string(),
            "-e".to_string(),
            "ATHENA_WORKGROUP=picnic".to_string(),
            "-e".to_string(),
            "AWS_SECRET_ACCESS_KEY=supersecret".to_string(),
        ];
        let redacted = redact_docker_args_for_log(&args);
        assert!(redacted.contains("ATHENA_WORKGROUP=picnic"));
        assert!(redacted.contains("AWS_SECRET_ACCESS_KEY=***"));
        assert!(!redacted.contains("supersecret"));
    }
}
