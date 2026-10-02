use std::fs;
use std::path::{Path, PathBuf};

/// `skipprd query` must not know any warehouse sink except SkipprLake.
/// Banned identifiers are assembled with concat! so this file does not trip itself.
#[test]
fn sqlrt_and_query_flight_know_no_other_sinks() {
    let banned: &[&str] = &[
        concat!("Ath", "ena"),
        concat!("Gl", "ue"),
        concat!("Hi", "ve"),
        concat!("Duck", "db"),
        concat!("Snow", "flake"),
        concat!("athena", "_admin"),
        concat!("build_s3", "_df"),
        concat!("manifest_key", "_for"),
        concat!("Iceberg", "CatalogConfig"),
        concat!("view.", "iceberg"),
        concat!("sink", "_plugin"),
        concat!("iceberg", "_only"),
        concat!("iceberg_sink", "_catalog"),
        concat!("run_iceberg", "_only"),
        "query_engine",
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for dir in ["sqlrt", "query_flight"] {
        for entry in walk(&root.join(dir)) {
            let text = fs::read_to_string(&entry).unwrap();
            let lower = text.to_ascii_lowercase();
            for word in banned {
                assert!(
                    !lower.contains(&word.to_ascii_lowercase()),
                    "{} mentions banned identifier `{word}`; skipprd query is SkipprLake-only",
                    entry.display()
                );
            }
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}
