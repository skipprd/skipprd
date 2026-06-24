use serde::{Deserialize, Serialize};

use arrow::array::{Array, Int32Array, Int64Array, StringArray, UInt32Array, UInt64Array};
use arrow::record_batch::RecordBatch;
use aws_credential_types::provider::ProvideCredentials;
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use object_store::aws::AmazonS3Builder;
use object_store::ObjectStore;
use std::sync::Arc;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CcUrlRecord {
    pub url: String,
    pub warc_filename: String,
    pub warc_record_offset: i64,
    pub warc_record_length: i64,
    #[serde(default)]
    pub fetch_status: Option<u32>,
    #[serde(default)]
    pub content_mime_type: String,
    pub fetch_time: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CcIndexLoadStats {
    pub mode: String,
    pub crawl_ids: Vec<String>,
    pub index_paths: Vec<String>,
    pub rows_selected: u32,
    pub batches_attempted: u32,
    pub batches_failed: u32,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CcIndexRequest<'a> {
    pub fixture_dir: Option<&'a str>,
    pub frontier_domains: &'a [String],
    pub max_urls: u32,
    pub include_subdomains: bool,
    pub cc_index_base_uri: &'a str,
    pub crawl_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CcIndexLoadResult {
    pub records: Vec<CcUrlRecord>,
    pub stats: CcIndexLoadStats,
}

fn load_fixture_url_candidates(
    fixture_dir: Option<&str>,
    frontier_domains: &[String],
    max_urls: u32,
) -> Vec<CcUrlRecord> {
    if let Some(dir) = fixture_dir {
        let path = std::path::Path::new(dir).join("cc_urls.jsonl");
        if let Ok(content) = std::fs::read_to_string(path) {
            let mut rows = Vec::new();
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(row) = serde_json::from_str::<CcUrlRecord>(line) {
                    rows.push(row);
                }
            }
            return rows.into_iter().take(max_urls as usize).collect();
        }
    }
    let mut out = Vec::new();
    for domain in frontier_domains {
        out.push(CcUrlRecord {
            url: format!("https://{domain}/"),
            warc_filename: "fixtures/sample.warc.gz".into(),
            warc_record_offset: 0,
            warc_record_length: 0,
            fetch_status: Some(200),
            content_mime_type: "text/html".into(),
            fetch_time: "2025-01-01T00:00:00Z".into(),
        });
        if out.len() as u32 >= max_urls {
            break;
        }
    }
    out
}

fn crawl_index_uri(base_uri: &str, crawl_id: &str) -> String {
    let base = base_uri.trim_end_matches('/');
    if base.contains("subset=") {
        return format!("{base}/");
    }
    let crawl_root = if base.contains("{crawl_id}") {
        base.replace("{crawl_id}", crawl_id)
    } else if base.ends_with(crawl_id) || base.ends_with(&format!("crawl={crawl_id}")) {
        base.to_string()
    } else {
        format!("{base}/crawl={crawl_id}")
    };
    format!("{}/subset=warc/", crawl_root.trim_end_matches('/'))
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn domain_filter(frontier_domains: &[String], include_subdomains: bool) -> String {
    let domains = frontier_domains
        .iter()
        .map(|d| d.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let exact = domains
        .iter()
        .map(|d| sql_string(d))
        .collect::<Vec<_>>()
        .join(",");
    let mut clauses = vec![
        format!("lower(url_host_registered_domain) IN ({exact})"),
        format!("lower(url_host_name) IN ({exact})"),
    ];
    if include_subdomains {
        clauses.extend(domains.iter().map(|d| {
            format!(
                "lower(url_host_name) LIKE {}",
                sql_string(&format!("%.{}", d))
            )
        }));
    }
    format!("({})", clauses.join(" OR "))
}

fn string_value(batch: &RecordBatch, name: &str, row: usize) -> Option<String> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx).as_any().downcast_ref::<StringArray>()?;
    if arr.is_null(row) {
        return None;
    }
    Some(arr.value(row).to_string())
}

fn i64_value(batch: &RecordBatch, name: &str, row: usize) -> Option<i64> {
    let idx = batch.schema().index_of(name).ok()?;
    let col = batch.column(idx).as_any();
    if let Some(arr) = col.downcast_ref::<Int64Array>() {
        return Some(arr.value(row));
    }
    if let Some(arr) = col.downcast_ref::<Int32Array>() {
        return Some(i64::from(arr.value(row)));
    }
    if let Some(arr) = col.downcast_ref::<UInt64Array>() {
        return i64::try_from(arr.value(row)).ok();
    }
    if let Some(arr) = col.downcast_ref::<UInt32Array>() {
        return Some(i64::from(arr.value(row)));
    }
    None
}

async fn register_s3_object_store(ctx: &SessionContext, s3_loc: &str) -> Result<(), std::io::Error> {
    let url = Url::parse(s3_loc).map_err(|err| std::io::Error::other(err.to_string()))?;
    if url.scheme() != "s3" {
        return Ok(());
    }
    let bucket = url
        .host_str()
        .ok_or_else(|| std::io::Error::other(format!("missing S3 bucket in {s3_loc}")))?;
    std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    // Common Crawl lives in us-east-1; pipeline Lambdas run in eu-west-1.
    let region = if bucket == "commoncrawl" {
        "us-east-1".to_string()
    } else {
        conf.region()
            .map(|r| r.to_string())
            .unwrap_or_else(|| "us-east-1".to_string())
    };
    std::env::set_var("AWS_REGION", &region);
    std::env::set_var("AWS_DEFAULT_REGION", &region);
    if let Some(provider) = conf.credentials_provider() {
        let creds = provider
            .provide_credentials()
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        std::env::set_var("AWS_ACCESS_KEY_ID", creds.access_key_id());
        std::env::set_var("AWS_SECRET_ACCESS_KEY", creds.secret_access_key());
        if let Some(token) = creds.session_token() {
            std::env::set_var("AWS_SESSION_TOKEN", token);
        }
    }
    let store = if bucket == "commoncrawl" {
        // Public dataset; anonymous reads avoid IAM ListBucket on the shared bucket.
        AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_region(&region)
            .with_skip_signature(true)
            .build()
            .map_err(|err: object_store::Error| std::io::Error::other(err.to_string()))?
    } else {
        AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .with_region(&region)
            .build()
            .map_err(|err: object_store::Error| std::io::Error::other(err.to_string()))?
    };
    let store_arc: Arc<dyn ObjectStore> = Arc::new(store);
    let endpoint = Url::parse(&format!("s3://{bucket}/"))
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    ctx.runtime_env()
        .register_object_store(&endpoint, store_arc);
    Ok(())
}

async fn query_index_path(
    path: &str,
    frontier_domains: &[String],
    include_subdomains: bool,
    limit: u32,
) -> Result<Vec<CcUrlRecord>, std::io::Error> {
    let ctx = SessionContext::new();
    if path.starts_with("s3://") {
        register_s3_object_store(&ctx, path).await?;
    }
    ctx.register_parquet("cc_index", path, ParquetReadOptions::default())
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;

    let filter = domain_filter(frontier_domains, include_subdomains);
    let sql = format!(
        r#"
        SELECT
          url,
          warc_filename,
          CAST(warc_record_offset AS BIGINT) AS warc_record_offset,
          CAST(warc_record_length AS BIGINT) AS warc_record_length,
          CAST(fetch_status AS BIGINT) AS fetch_status,
          CAST(content_mime_type AS VARCHAR) AS content_mime_type,
          CAST(fetch_time AS VARCHAR) AS fetch_time
        FROM cc_index
        WHERE {filter}
          AND (fetch_status = 200 OR fetch_status IS NULL)
          AND (content_mime_type LIKE 'text/html%' OR content_mime_type IS NULL)
        LIMIT {limit}
        "#
    );
    let df = ctx
        .sql(&sql)
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let batches = df
        .collect()
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let mut records = Vec::new();
    for batch in batches {
        for row in 0..batch.num_rows() {
            let Some(url) = string_value(&batch, "url", row) else {
                continue;
            };
            let Some(warc_filename) = string_value(&batch, "warc_filename", row) else {
                continue;
            };
            records.push(CcUrlRecord {
                url,
                warc_filename,
                warc_record_offset: i64_value(&batch, "warc_record_offset", row).unwrap_or(0),
                warc_record_length: i64_value(&batch, "warc_record_length", row).unwrap_or(0),
                fetch_status: i64_value(&batch, "fetch_status", row).and_then(|value| {
                    if value >= 0 {
                        u32::try_from(value).ok()
                    } else {
                        None
                    }
                }),
                content_mime_type: string_value(&batch, "content_mime_type", row)
                    .unwrap_or_else(|| "text/html".into()),
                fetch_time: string_value(&batch, "fetch_time", row).unwrap_or_default(),
            });
        }
    }
    Ok(records)
}

/// Load URL candidates from fixtures for tests, or from Common Crawl URL Index Parquet in production.
pub async fn load_url_candidates(request: CcIndexRequest<'_>) -> CcIndexLoadResult {
    if request.fixture_dir.is_some() {
        let records = load_fixture_url_candidates(
            request.fixture_dir,
            request.frontier_domains,
            request.max_urls,
        );
        return CcIndexLoadResult {
            stats: CcIndexLoadStats {
                mode: "fixture".into(),
                crawl_ids: request.crawl_ids,
                rows_selected: records.len() as u32,
                ..Default::default()
            },
            records,
        };
    }

    let mut stats = CcIndexLoadStats {
        mode: "datafusion_parquet".into(),
        crawl_ids: request.crawl_ids.clone(),
        ..Default::default()
    };
    let mut records = Vec::new();
    for crawl_id in &request.crawl_ids {
        if records.len() as u32 >= request.max_urls {
            break;
        }
        let path = crawl_index_uri(request.cc_index_base_uri, crawl_id);
        stats.index_paths.push(path.clone());
        stats.batches_attempted += 1;
        let remaining = request.max_urls.saturating_sub(records.len() as u32);
        match query_index_path(
            &path,
            request.frontier_domains,
            request.include_subdomains,
            remaining,
        )
        .await
        {
            Ok(mut batch_records) => records.append(&mut batch_records),
            Err(err) => {
                stats.batches_failed += 1;
                stats.errors.push(format!("{path}: {err}"));
            }
        }
    }

    stats.rows_selected = records.len() as u32;
    CcIndexLoadResult { records, stats }
}

#[cfg(test)]
mod tests {
    use super::crawl_index_uri;

    #[test]
    fn crawl_index_uri_appends_subset_warc_partition() {
        let base = "s3://commoncrawl/cc-index/table/cc-main/warc";
        assert_eq!(
            crawl_index_uri(base, "CC-MAIN-2025-08"),
            "s3://commoncrawl/cc-index/table/cc-main/warc/crawl=CC-MAIN-2025-08/subset=warc/"
        );
    }

    #[test]
    fn crawl_index_uri_honors_crawl_id_placeholder() {
        let base = "s3://commoncrawl/cc-index/table/cc-main/warc/crawl={crawl_id}";
        assert_eq!(
            crawl_index_uri(base, "CC-MAIN-2024-51"),
            "s3://commoncrawl/cc-index/table/cc-main/warc/crawl=CC-MAIN-2024-51/subset=warc/"
        );
    }

    #[test]
    fn crawl_index_uri_preserves_explicit_subset() {
        let base = "s3://commoncrawl/cc-index/table/cc-main/warc/crawl=CC-MAIN-2025-08/subset=warc";
        assert_eq!(
            crawl_index_uri(base, "CC-MAIN-2025-08"),
            "s3://commoncrawl/cc-index/table/cc-main/warc/crawl=CC-MAIN-2025-08/subset=warc/"
        );
    }
}

pub fn domain_matches(url: &str, frontier_domains: &[String], include_subdomains: bool) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    frontier_domains.iter().any(|d| {
        let d = d.to_ascii_lowercase();
        host == d || (include_subdomains && host.ends_with(&format!(".{d}")))
    })
}
