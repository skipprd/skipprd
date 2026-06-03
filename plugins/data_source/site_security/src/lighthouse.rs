use arrow::array::{Array, Float64Array, StringArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::{json, Value};
use std::fs::File;
use std::path::Path;

/// Maps site_quality Lighthouse audit IDs to site_security issue codes.
const LH_AUDIT_TO_ISSUE: &[(&str, &str)] = &[
    ("is-on-https", "LH_IS_ON_HTTPS"),
    ("redirects-http", "LH_REDIRECTS_HTTP"),
    ("uses-http2", "LH_USES_HTTP2"),
    ("geolocation-on-start", "LH_GEOLOCATION_ON_START"),
    ("notification-on-start", "LH_NOTIFICATION_ON_START"),
    ("password-inputs-can-be-pasted", "LH_PASSWORD_PASTE_ALLOWED"),
    ("image-aspect-ratio", "LH_IMAGE_ASPECT_RATIO"),
    ("doctype", "LH_DOCTYPE"),
    ("charset", "LH_CHARSET"),
    ("js-libraries", "LH_JS_LIBRARIES"),
];

fn find_lighthouse_table_root(data_dir: &Path) -> Option<std::path::PathBuf> {
    let datalake = data_dir.join("datalake");
    let entries = std::fs::read_dir(&datalake).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let table = path.join("site_quality_lighthouse_audit");
            if table.is_dir() {
                return Some(table);
            }
        }
    }
    None
}

fn path_matches_run_date(path: &Path, run_date: &str) -> bool {
    let raw = path.to_string_lossy();
    if raw.contains(run_date) {
        return true;
    }
    let mut parts = run_date.split('-');
    let (Some(year), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let month_num = month.trim_start_matches('0');
    let day_num = day.trim_start_matches('0');
    raw.contains(&format!("year={year}"))
        && (raw.contains(&format!("month={month}")) || raw.contains(&format!("month={month_num}")))
        && (raw.contains(&format!("day={day}")) || raw.contains(&format!("day={day_num}")))
}

fn collect_parquet_files(dir: &Path, run_date: &str, out: &mut Vec<std::path::PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_parquet_files(&path, run_date, out);
        } else if path.extension().is_some_and(|e| e == "parquet")
            && path_matches_run_date(&path, run_date)
        {
            out.push(path);
        }
    }
}

fn parquet_rows(path: &Path, site: &str) -> Vec<Value> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let builder = match ParquetRecordBatchReaderBuilder::try_new(file) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let mut rows = Vec::new();
    let schema = builder.schema().clone();
    let reader = match builder.build() {
        Ok(r) => r,
        Err(_) => return rows,
    };
    for batch in reader {
        let batch = match batch {
            Ok(b) => b,
            Err(_) => continue,
        };
        let n = batch.num_rows();
        for i in 0..n {
            let mut obj = serde_json::Map::new();
            for (col_idx, field) in schema.fields().iter().enumerate() {
                let col = batch.column(col_idx);
                let name = field.name();
                if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                    if arr.is_valid(i) {
                        obj.insert(name.clone(), Value::String(arr.value(i).to_string()));
                    }
                } else if let Some(arr) = col.as_any().downcast_ref::<Float64Array>() {
                    if arr.is_valid(i) {
                        obj.insert(name.clone(), json!(arr.value(i)));
                    }
                }
            }
            let row = Value::Object(obj);
            if row.get("site").and_then(|s| s.as_str()) == Some(site) {
                rows.push(row);
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn matches_run_date_partition_path() {
        let path = PathBuf::from(
            "/tmp/datalake/site/site_quality_lighthouse_audit/year=2026/month=6/day=3/a.parquet",
        );
        assert!(path_matches_run_date(&path, "2026-06-03"));
    }

    #[test]
    fn rejects_other_run_date_partition_path() {
        let path = PathBuf::from(
            "/tmp/datalake/site/site_quality_lighthouse_audit/year=2026/month=6/day=2/a.parquet",
        );
        assert!(!path_matches_run_date(&path, "2026-06-03"));
    }
}

/// Load site_quality lighthouse_audit rows from local datalake parquet (prior site-quality run).
pub fn load_lighthouse_audits(data_dir: &Path, site: &str, run_date: &str) -> Vec<Value> {
    let Some(table_root) = find_lighthouse_table_root(data_dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    collect_parquet_files(&table_root, run_date, &mut files);
    let mut rows = Vec::new();
    for path in files {
        rows.extend(parquet_rows(&path, site));
    }
    rows
}

pub fn checks_from_lighthouse(
    site: &str,
    page_url: &str,
    run_date: &str,
    audits: &[Value],
) -> Vec<Value> {
    use crate::issue::check_row;
    let mut by_id: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for row in audits {
        if row.get("page_url").and_then(|p| p.as_str()) != Some(page_url) {
            continue;
        }
        let Some(id) = row.get("audit_id").and_then(|a| a.as_str()) else {
            continue;
        };
        let score = row.get("score").and_then(|s| s.as_f64()).unwrap_or(1.0);
        by_id.insert(id.to_string(), score);
    }

    let mut rows = Vec::new();
    for (audit_id, issue_code) in LH_AUDIT_TO_ISSUE {
        let score = by_id.get(*audit_id).copied();
        let (status, severity, message) = match score {
            None => (
                "pass",
                "info",
                format!("Lighthouse audit {audit_id} not failing (reused from site-quality)"),
            ),
            Some(s) if s >= 0.9 => (
                "pass",
                "info",
                format!("Lighthouse {audit_id} score {s:.2}"),
            ),
            Some(s) if s >= 0.5 => (
                "warn",
                "warning",
                format!("Lighthouse {audit_id} score {s:.2}"),
            ),
            Some(s) => (
                "fail",
                "warning",
                format!("Lighthouse {audit_id} score {s:.2}"),
            ),
        };
        rows.push(check_row(
            site,
            page_url,
            run_date,
            issue_code,
            "lighthouse",
            status,
            severity,
            &message,
        ));
    }
    rows
}
