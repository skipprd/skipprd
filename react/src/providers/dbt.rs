use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{io::BufRead, process::Stdio};

use crate::adapters::storage::StorageAdapter;
use crate::providers::{Keyspace, RequestScope};

use react_core::providers::{DbtProvider, DbtValidateArgs, DbtValidateResult};
use crate::ws::terminal::{self, TerminalEvent};

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
    /// Runner mode: "host" (default) or "docker".
    pub mode: String,
    /// Docker image to use when mode=="docker" (pinned strongly recommended).
    pub docker_image: Option<String>,
    /// Optional docker platform (e.g. "linux/amd64").
    pub docker_platform: Option<String>,
    /// Optional docker network (e.g. "host" or a named network).
    pub docker_network: Option<String>,
    /// If true, mount ~/.aws into the container at /root/.aws (useful for AWS_PROFILE flows).
    pub docker_mount_aws_dir: bool,
}

fn encode_key_component(s: &str) -> String {
    // Same encoding strategy as Keyspace: keep a conservative safe set, percent-encode the rest.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        let safe = c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'; // allow dots
        if safe {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())
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
    // We redact values after '=' for any token passed to `-e`, and fully redact known AWS secrets.
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut next_is_env = false;
    for a in args.iter() {
        if next_is_env {
            next_is_env = false;
            if let Some((k, _v)) = a.split_once('=') {
                out.push(format!("{}=***", k));
            } else {
                out.push("***".to_string());
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
            let k_uc = k.to_ascii_uppercase();
            if k_uc.contains("AWS_SECRET_ACCESS_KEY")
                || k_uc.contains("AWS_SESSION_TOKEN")
                || k_uc.contains("AWS_ACCESS_KEY_ID")
            {
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
            let item = self.items_by_idx.entry(idx).or_insert_with(|| DbtItemState {
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
                    let remaining = self
                        .running_names_set
                        .len()
                        .saturating_sub(shown.len());
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
    } else if toks.get(0).map(|s| *s) == Some("relationships") {
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
    let started = std::time::Instant::now();
    tracing::info!(
        target: "dbt",
        phase = %label,
        runner = "host",
        cmd = %cmd,
        args = %args.join(" "),
        "starting"
    );
    let mut c = std::process::Command::new(cmd);
    c.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs.iter() {
        c.env(k, v);
    }

    let mut child = match c.spawn() {
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
                if let Some(detail) = p.consume_line(&line) {
                    if let Some(s) = terminal::sink() {
                        s.emit(TerminalEvent::DbtProgress {
                            phase: label.to_string(),
                            detail,
                        });
                    }
                }
            }
            if should_log_dbt_info_line(&line) {
                tracing::info!(target: "dbt", phase = %label, runner = "host", stream = "stdout", "{}", line);
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
        runner = "host",
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

fn run_cmd_docker(
    runner: &DbtRunnerConfig,
    project_dir: &Path,
    profiles_dir: Option<&Path>,
    dbt_args: &[&str],
    envs: &[(&str, String)],
) -> CmdOut {
    run_cmd_docker_labeled(runner, project_dir, profiles_dir, dbt_args, envs, "dbt")
}

fn run_cmd_docker_labeled(
    runner: &DbtRunnerConfig,
    project_dir: &Path,
    profiles_dir: Option<&Path>,
    dbt_args: &[&str],
    envs: &[(&str, String)],
    label: &str,
) -> CmdOut {
    let started = std::time::Instant::now();
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

    tracing::info!(
        target: "dbt",
        phase = %label,
        runner = "docker",
        cmd = "docker",
        args = %redact_docker_args_for_log(&args),
        "starting"
    );

    let mut c = std::process::Command::new("docker");
    c.args(&args)
        .current_dir(project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match c.spawn() {
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
    for (is_err, line) in rx {
        if is_err {
            err_buf.push_str(&line);
            err_buf.push('\n');
        } else {
            if should_log_dbt_info_line(&line) {
                tracing::info!(target: "dbt", phase = %label, runner = "docker", stream = "stdout", "{}", line);
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
        runner = "docker",
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
                self.storage.put_bytes(&key, &bytes, content_type).await?;
                uploaded += 1;
            }
        }
        Ok(uploaded)
    }
}

#[async_trait]
impl DbtProvider for DbtProjectProvider {
    async fn ensure_minimal_project(&self, scope: &RequestScope) -> Result<(), String> {
        let project_key = self.keyspace.dbt_project_key(scope);
        if self.storage.head_etag(&project_key).await?.is_some() {
            // Back-compat / self-heal: if an existing dbt_project.yml is invalid for dbt-core (e.g. has a top-level
            // `depends_on` key), rewrite it to a minimal valid project file. Otherwise, leave it intact.
            if let Ok(bytes) = self.storage.get_bytes(&project_key).await {
                let text = String::from_utf8_lossy(&bytes).to_string();
                if let Ok(yv) = serde_yaml::from_str::<serde_yaml::Value>(&text) {
                    if let Some(map) = yv.as_mapping() {
                        let has_depends_on = map
                            .keys()
                            .any(|k| k.as_str().map(|s| s == "depends_on").unwrap_or(false));
                        if !has_depends_on {
                            return Ok(());
                        }
                    } else {
                        // Non-mapping YAML: treat as invalid and rewrite.
                    }
                } else {
                    // Unparseable YAML: rewrite.
                }
            }
            // Fall through and rewrite below.
        }
        let name = format!("{}_project", scope.project_id.replace('/', "_"));
        let y = format!(
            "name: {name}\nversion: '1.0'\nprofile: '{project_id}'\nmodel-paths: ['models']\nseed-paths: ['seeds']\nmacro-paths: ['macros']\ntarget-path: 'target'\n\nmodels:\n  {name}:\n    # Suffix strategy: dbt materializes schemas as <DBT_TARGET_SCHEMA>_<suffix>.\n    # Default all models into GOLD by setting their custom schema name to the gold suffix.\n    +schema: \"{{{{ env_var('DBT_GOLD_SUFFIX', 'warehouse') }}}}\"\n    # Force staging models under models/staging into SILVER.\n    staging:\n      +schema: \"{{{{ env_var('DBT_SILVER_SUFFIX', 'silver') }}}}\"\n",
            name = name,
            project_id = scope.project_id
        );
        self.storage
            .put_bytes(&project_key, y.as_bytes(), "text/yaml")
            .await?;
        Ok(())
    }

    async fn write_model_sql(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        name: &str,
        sql: &str,
    ) -> Result<String, String> {
        let dir = encode_key_component(dataset_id);
        let key = format!(
            "{}models/{}/{}.sql",
            self.keyspace.dbt_prefix(scope),
            dir,
            name
        );
        self.storage
            .put_bytes(&key, sql.as_bytes(), "text/sql")
            .await?;
        Ok(key)
    }

    async fn write_metricflow_yaml(
        &self,
        scope: &RequestScope,
        dataset_id: &str,
        name: &str,
        yaml_text: &str,
    ) -> Result<String, String> {
        let dir = encode_key_component(dataset_id);
        let key = format!(
            "{}metrics/{}/{}.yaml",
            self.keyspace.dbt_prefix(scope),
            dir,
            name
        );
        self.storage
            .put_bytes(&key, yaml_text.as_bytes(), "text/yaml")
            .await?;
        Ok(key)
    }

    async fn validate_project(
        &self,
        scope: &RequestScope,
        args: &DbtValidateArgs,
    ) -> Result<DbtValidateResult, String> {
        let project_name = if args.project_name.is_empty() {
            "data_engineer".to_string()
        } else {
            args.project_name.clone()
        };
        let s3_prefix_base = {
            let pref = self.keyspace.dbt_prefix(scope);
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
        let keys = self.storage.list_prefix(&s3_prefix_base).await?;
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
            let bytes = self.storage.get_bytes(&key).await?;
            write_file(&dest, &bytes)?;
            file_count += 1;
        }

        // Ensure minimal project file if missing
        let proj = root.join("dbt_project.yml");
        if !proj.exists() {
            // Important: profile name must match the generated `profiles.yml` entry, which is scope.project_id.
            let y = format!(
                "name: {}\nversion: '1.0'\nprofile: '{}'\nmodel-paths: ['models']\ntarget-path: 'target'\n\nmodels:\n  {}:\n    # Suffix strategy: dbt materializes schemas as <DBT_TARGET_SCHEMA>_<suffix>.\n    +schema: \"{{{{ env_var('DBT_GOLD_SUFFIX', 'warehouse') }}}}\"\n    staging:\n      +schema: \"{{{{ env_var('DBT_SILVER_SUFFIX', 'silver') }}}}\"\n",
                project_name,
                scope.project_id
                ,
                project_name
            );
            write_file(&proj, y.as_bytes())?;
        }

        // Sanitize dbt_project.yml for dbt-core strict schema:
        // - Remove invalid top-level keys like `depends_on` (seen in some templates).
        // - Force `profile:` to match scope.project_id so it aligns with generated profiles.yml.
        // - Persist the sanitized version back to storage so future runs are clean.
        {
            let project_key = self.keyspace.dbt_project_key(scope);
            let raw = std::fs::read_to_string(&proj).unwrap_or_default();
            let mut changed = false;
            let mut v: serde_yaml::Value = match serde_yaml::from_str(&raw) {
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
            let map = v.as_mapping_mut().unwrap();
            // Remove invalid top-level depends_on
            let dep_key = serde_yaml::Value::String("depends_on".to_string());
            if map.remove(&dep_key).is_some() {
                changed = true;
            }
            // Force profile to scope.project_id
            let prof_key = serde_yaml::Value::String("profile".to_string());
            let desired_profile = serde_yaml::Value::String(scope.project_id.clone());
            if map.get(&prof_key) != Some(&desired_profile) {
                map.insert(prof_key, desired_profile);
                changed = true;
            }
            if changed {
                let new_text = serde_yaml::to_string(&v).unwrap_or_else(|_| raw.clone());
                let _ = write_file(&proj, new_text.as_bytes());
                let _ = self
                    .storage
                    .put_bytes(&project_key, new_text.as_bytes(), "text/yaml")
                    .await;
            }
        }

        let mut envs: Vec<(&str, String)> = Vec::new();
        if let Some(pd) = profiles_dir.as_ref() {
            envs.push(("DBT_PROFILES_DIR", pd.clone()));
        }

        let use_docker = self.runner.mode.to_lowercase() == "docker";
        let profiles_path = profiles_dir.as_ref().map(|s| Path::new(s));

        // Hard fail if dbt CLI itself is broken on this runner (do NOT attempt to repair).
        // This catches host-level Python/env issues early (e.g. import errors) before we try deps/parse/compile.
        let version_res = if use_docker {
            run_cmd_docker_labeled(
                &self.runner,
                &root,
                profiles_path,
                &["--version"],
                &envs,
                "version",
            )
        } else {
            run_cmd_labeled("dbt", &["--version"], &root, &envs, "version")
        };
        if !version_res.status_ok {
            return Err(format!(
                "dbt environment check failed (dbt --version). This is a host/runner issue; do not attempt auto-repair.\n\nstdout:\n{}\n\nstderr:\n{}",
                version_res.stdout.trim(),
                version_res.stderr.trim()
            ));
        }

        let deps_res = if use_docker {
            run_cmd_docker_labeled(&self.runner, &root, profiles_path, &["deps"], &envs, "deps")
        } else {
            run_cmd_labeled("dbt", &["deps"], &root, &envs, "deps")
        };
        let parse_res = if use_docker {
            run_cmd_docker_labeled(
                &self.runner,
                &root,
                profiles_path,
                &["parse"],
                &envs,
                "parse",
            )
        } else {
            run_cmd_labeled("dbt", &["parse"], &root, &envs, "parse")
        };
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
            if use_docker {
                run_cmd_docker_labeled(
                    &self.runner,
                    &root,
                    profiles_path,
                    &argv_refs,
                    &envs,
                    "compile",
                )
            } else {
                run_cmd_labeled("dbt", &argv_refs, &root, &envs, "compile")
            }
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
                if use_docker {
                    run_cmd_docker_labeled(
                        &self.runner,
                        &root,
                        profiles_path,
                        &argv_refs,
                        &envs,
                        "build",
                    )
                } else {
                    run_cmd_labeled("dbt", &argv_refs, &root, &envs, "build")
                }
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
                if use_docker {
                    run_cmd_docker_labeled(
                        &self.runner,
                        &root,
                        profiles_path,
                        &argv_refs,
                        &envs,
                        "run",
                    )
                } else {
                    run_cmd_labeled("dbt", &argv_refs, &root, &envs, "run")
                }
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

        Ok(DbtValidateResult {
            ok,
            deps_ok: deps_res.status_ok,
            parse_ok: parse_res.status_ok,
            compile_ok: compile_res.status_ok,
            run_ok: run_or_build_res.as_ref().map(|o| o.status_ok),
            uploaded_target_files: uploaded_files,
            errors: errs_vec,
            warnings: vec![],
            logs: serde_json::json!({
                "deps": { "code": deps_res.code, "stdout": deps_res.stdout, "stderr": deps_res.stderr },
                "parse": { "code": parse_res.code, "stdout": parse_res.stdout, "stderr": parse_res.stderr },
                "compile": { "code": compile_res.code, "stdout": compile_res.stdout, "stderr": compile_res.stderr },
                "run_or_build": run_or_build_res.as_ref().map(|r| serde_json::json!({ "code": r.code, "stdout": r.stdout, "stderr": r.stderr })),
                "fetched_files": file_count,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_args_require_image() {
        let runner = DbtRunnerConfig {
            mode: "docker".to_string(),
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
            mode: "docker".to_string(),
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
}
