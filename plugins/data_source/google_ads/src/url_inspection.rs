use chrono::NaiveDate;
use skippr_plugin_shared_api_source::RetryableHttpClient;

use crate::gsc_api::{inspect_url, normalize_site_url};

pub const MAX_URL_INSPECTIONS_PER_RUN: usize = 20;

pub async fn inspection_rows_for_date(
    http: &RetryableHttpClient,
    auth_header: &str,
    site_url: &str,
    urls: &[String],
    partition_date: NaiveDate,
) -> Result<Vec<serde_json::Value>, std::io::Error> {
    let site = normalize_site_url(site_url);
    let date_key = partition_date.format("%Y-%m-%d").to_string();
    let capped = urls.iter().take(MAX_URL_INSPECTIONS_PER_RUN);
    let mut rows = Vec::new();
    for inspection_url in capped {
        let body = inspect_url(http, auth_header, &site, inspection_url).await?;
        let mut record = serde_json::Map::new();
        record.insert("site_url".into(), serde_json::Value::String(site.clone()));
        record.insert("date".into(), serde_json::Value::String(date_key.clone()));
        record.insert(
            "inspection_url".into(),
            serde_json::Value::String(inspection_url.clone()),
        );
        if let Some(result) = body.get("inspectionResult") {
            record.insert("inspection_result".into(), result.clone());
        } else {
            record.insert("inspection_result".into(), body);
        }
        rows.push(serde_json::Value::Object(record));
    }
    Ok(rows)
}
