use std::fs;
use std::path::{Path, PathBuf};

fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                walk_rs_files(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
}

#[test]
fn react_must_not_depend_on_sqlrt() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<PathBuf> = Vec::new();
    walk_rs_files(&root, &mut files);
    files.sort();

    let mut offenders: Vec<String> = Vec::new();
    for p in files {
        let Ok(s) = fs::read_to_string(&p) else { continue };
        if s.contains("crate::sqlrt::") || s.contains("::sqlrt::") {
            offenders.push(p.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "Found sqlrt usage under src/react (must remain ingest-only):\n{}",
        offenders.join("\n")
    );
}

