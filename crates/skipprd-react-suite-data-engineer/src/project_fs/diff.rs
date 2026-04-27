struct DiffResult {
    lines: Vec<String>,
    added: usize,
    removed: usize,
}

fn diff_lines_inner(old: &str, new: &str) -> DiffResult {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut lines: Vec<String> = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() {
            if old_lines[i] == new_lines[j] {
                lines.push(format!(" {}", old_lines[i]));
                i += 1;
                j += 1;
            } else {
                lines.push(format!("- {}", old_lines[i]));
                lines.push(format!("+ {}", new_lines[j]));
                removed += 1;
                added += 1;
                i += 1;
                j += 1;
            }
        } else if i < old_lines.len() {
            lines.push(format!("- {}", old_lines[i]));
            removed += 1;
            i += 1;
        } else {
            lines.push(format!("+ {}", new_lines[j]));
            added += 1;
            j += 1;
        }
    }
    DiffResult {
        lines,
        added,
        removed,
    }
}

pub fn compute_unified_diff(old: &str, new: &str) -> String {
    let d = diff_lines_inner(old, new);
    let mut out = vec!["--- original".to_string(), "+++ modified".to_string()];
    out.extend(d.lines);
    out.join("\n")
}

pub fn diff_stats(old: &str, new: &str) -> (usize, usize) {
    let d = diff_lines_inner(old, new);
    (d.added, d.removed)
}
