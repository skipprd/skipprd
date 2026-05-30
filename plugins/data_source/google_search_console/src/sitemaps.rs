use chrono::NaiveDate;
use skippr_plugin_shared_api_source::RetryableHttpClient;

use crate::gsc_api::{list_sitemaps, normalize_site_url};

pub async fn sitemap_rows_for_date(
    http: &RetryableHttpClient,
    auth_header: &str,
    site_url: &str,
    partition_date: NaiveDate,
) -> Result<Vec<serde_json::Value>, std::io::Error> {
    let site = normalize_site_url(site_url);
    let body = list_sitemaps(http, auth_header, &site).await?;
    let date_key = partition_date.format("%Y-%m-%d").to_string();
    let sitemap_entries = body
        .get("sitemap")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    for entry in sitemap_entries {
        let path = entry
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mut record = serde_json::Map::new();
        record.insert("site_url".into(), serde_json::Value::String(site.clone()));
        record.insert("date".into(), serde_json::Value::String(date_key.clone()));
        record.insert("path".into(), serde_json::Value::String(path));
        for field in [
            "type",
            "lastSubmitted",
            "lastDownloaded",
            "isPending",
            "isSitemapsIndex",
        ] {
            if let Some(value) = entry.get(field) {
                record.insert(field.into(), value.clone());
            }
        }
        if let Some(contents) = entry.get("contents").and_then(|v| v.as_array()) {
            let warnings: u64 = contents
                .iter()
                .filter_map(|c| c.get("warning").and_then(|v| v.as_u64()))
                .sum();
            let errors: u64 = contents
                .iter()
                .filter_map(|c| c.get("error").and_then(|v| v.as_u64()))
                .sum();
            record.insert("warnings".into(), serde_json::json!(warnings));
            record.insert("errors".into(), serde_json::json!(errors));
        }
        rows.push(serde_json::Value::Object(record));
    }
    Ok(rows)
}
