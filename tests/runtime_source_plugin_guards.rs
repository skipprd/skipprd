use std::path::PathBuf;

use regex::Regex;

#[test]
fn runtime_source_plugins_stay_on_sdk_protocol_boundary() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let data_source_root = root.join("plugins").join("data_source");
    let banned_patterns = [
        (
            "runtime-link import",
            Regex::new(r"\bskippr_plugin_runtime_link\b").unwrap(),
        ),
        (
            "shared append source path include",
            Regex::new(r"append_source_runtime\.rs").unwrap(),
        ),
        (
            "manual runtime session handshake",
            Regex::new(r"\bRuntimeSessionHello\b").unwrap(),
        ),
        (
            "manual runtime control env wiring",
            Regex::new(r"\bSKIPPR_RUNTIME_CONTROL_ADDR_ENV\b").unwrap(),
        ),
        (
            "manual runtime data env wiring",
            Regex::new(r"\bSKIPPR_RUNTIME_DATA_ADDR_ENV\b").unwrap(),
        ),
        (
            "manual RunSource dispatch",
            Regex::new(r"\bHostFrame::RunSource\b").unwrap(),
        ),
        (
            "plugin ingest_file",
            Regex::new(r"\.ingest_file\s*\(").unwrap(),
        ),
        (
            "runtime ingest relay",
            Regex::new(r"\bRuntimeIngestRelay\b").unwrap(),
        ),
        (
            "relay raw ingest",
            Regex::new(r"\brelay_raw_ingest_tasks\b").unwrap(),
        ),
        (
            "plugin-owned ingest worker",
            Regex::new(r"\bingest:\s*Ingest\b").unwrap(),
        ),
    ];

    let mut source_files = Vec::new();
    for plugin_dir in std::fs::read_dir(&data_source_root)
        .unwrap_or_else(|err| panic!("failed to read {}: {}", data_source_root.display(), err))
    {
        let plugin_dir = plugin_dir.unwrap_or_else(|err| panic!("failed to read dir entry: {err}"));
        let src_dir = plugin_dir.path().join("src");
        if !src_dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&src_dir)
            .unwrap_or_else(|err| panic!("failed to read {}: {}", src_dir.display(), err))
        {
            let entry = entry.unwrap_or_else(|err| panic!("failed to read dir entry: {err}"));
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                source_files.push(path);
            }
        }
    }

    let mut violations = Vec::new();
    for path in &source_files {
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {}", path.display(), err));
        for (name, pattern) in &banned_patterns {
            if pattern.is_match(&contents) {
                violations.push(format!("{} matched {}", path.display(), name));
            }
        }
    }

    for path in source_files
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("main.rs"))
    {
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {}", path.display(), err));
        if !contents.contains("run_append_data_source_main(") {
            violations.push(format!(
                "{} must use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main",
                path.display()
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "runtime source plugin files must stay on the SDK/runtime boundary:\n{}",
        violations.join("\n")
    );
}
