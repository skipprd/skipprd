use std::path::PathBuf;

use regex::Regex;
use walkdir::WalkDir;

#[test]
fn runtime_sink_and_schema_plugins_do_not_reintroduce_host_globals() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !root.join("plugins/skippr-plugin-runtime-link").exists(),
        "legacy runtime-link crate should be deleted"
    );
    let monitored_dirs = ["plugins/data_sink", "plugins/schema_sink"];
    let banned_patterns = [
        ("Config::", Regex::new(r"\bConfig::").unwrap()),
        ("METADATA", Regex::new(r"\bMETADATA\b").unwrap()),
        ("PIPELINE_NAME", Regex::new(r"\bPIPELINE_NAME\b").unwrap()),
        ("std::env::var(", Regex::new(r"std::env::var\(").unwrap()),
    ];

    let mut violations = Vec::new();
    for relative_dir in monitored_dirs {
        for entry in WalkDir::new(root.join(relative_dir))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "rs"))
        {
            let path = entry.path();
            let contents = std::fs::read_to_string(path)
                .unwrap_or_else(|err| panic!("failed to read {}: {}", path.display(), err));
            for (name, pattern) in &banned_patterns {
                if pattern.is_match(&contents) {
                    violations.push(format!("{} matched {}", path.display(), name));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "runtime sink/schema files must not use host-owned globals:\n{}",
        violations.join("\n")
    );
}

#[test]
fn legacy_runtime_plugin_authoring_artifacts_are_deleted() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !root.join("runtime_plugins/manifests").exists(),
        "legacy runtime plugin manifest templates should be deleted"
    );
}

#[test]
fn runtime_sink_and_schema_plugins_use_sdk_entrypoints() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let entrypoint_bans = [
        (
            "RuntimeSessionHello",
            Regex::new(r"\bRuntimeSessionHello\b").unwrap(),
        ),
        (
            "SKIPPR_RUNTIME_CONTROL_ADDR_ENV",
            Regex::new(r"\bSKIPPR_RUNTIME_CONTROL_ADDR_ENV\b").unwrap(),
        ),
        (
            "SKIPPR_RUNTIME_DATA_ADDR_ENV",
            Regex::new(r"\bSKIPPR_RUNTIME_DATA_ADDR_ENV\b").unwrap(),
        ),
        (
            "HostFrame::RunSink",
            Regex::new(r"\bHostFrame::RunSink\b").unwrap(),
        ),
        (
            "HostFrame::RunSchema",
            Regex::new(r"\bHostFrame::RunSchema\b").unwrap(),
        ),
    ];
    let mut violations = Vec::new();

    for (relative_dir, helper_call) in [
        ("plugins/data_sink", "run_runtime_data_sink_plugin("),
        ("plugins/schema_sink", "run_runtime_schema_sink_plugin("),
    ] {
        for entry in WalkDir::new(root.join(relative_dir))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| entry.file_name() == "main.rs")
        {
            let path = entry.path();
            let contents = std::fs::read_to_string(path)
                .unwrap_or_else(|err| panic!("failed to read {}: {}", path.display(), err));

            if !contents.contains("sink_runtime_entry") || !contents.contains(helper_call) {
                violations.push(format!(
                    "{} must use skippr_runtime_sdk::sink_runtime_entry::{}",
                    path.display(),
                    helper_call.trim_end_matches('(')
                ));
            }

            for (name, pattern) in &entrypoint_bans {
                if pattern.is_match(&contents) {
                    violations.push(format!("{} matched {}", path.display(), name));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "runtime sink/schema plugins must stay on SDK entrypoints:\n{}",
        violations.join("\n")
    );
}
