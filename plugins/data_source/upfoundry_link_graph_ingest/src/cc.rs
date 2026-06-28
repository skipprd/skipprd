use serde::{Deserialize, Serialize};

use arrow::array::{
    Array, BinaryArray, Int32Array, Int64Array, LargeBinaryArray, LargeStringArray, StringArray,
    StringViewArray, UInt32Array, UInt64Array,
};
use arrow::record_batch::RecordBatch;
use aws_credential_types::provider::ProvideCredentials;
use aws_sdk_s3::Client as S3Client;
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use object_store::aws::AmazonS3Builder;
use object_store::ObjectStore;
use sha2::{Digest, Sha256};
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
    pub files_total: u32,
    pub next_file_index: u32,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CcIndexRequest<'a> {
    pub fixture_dir: Option<&'a str>,
    pub frontier_domains: &'a [String],
    pub max_urls: u32,
    pub include_subdomains: bool,
    pub ops_bucket: &'a str,
    pub ops_prefix: &'a str,
    pub cc_index_base_uri: &'a str,
    pub cc_urls_index_prefix: &'a str,
    pub cc_index_source: &'a str,
    pub cc_direct_index_enabled: bool,
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

pub(crate) fn domain_hash(domain: &str) -> String {
    let digest =
        Sha256::digest(format!("domain:{}", domain.trim().to_ascii_lowercase()).as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

fn urls_index_uri(bucket: &str, ops_prefix: &str, urls_index_prefix: &str, domain: &str) -> String {
    format!(
        "s3://{}/{}/{}/domain_hash={}/",
        bucket.trim(),
        ops_prefix.trim_matches('/'),
        urls_index_prefix.trim_matches('/'),
        domain_hash(domain)
    )
}

fn batch_state_key(ops_prefix: &str, domain: &str, crawl_id: &str) -> String {
    format!(
        "{}/state/cc_index_batch_state/domain_hash={}/crawl_id={}.json",
        ops_prefix.trim_matches('/'),
        domain_hash(domain),
        crawl_id
    )
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
    let col = batch.column(idx);
    if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
        if arr.is_null(row) {
            return None;
        }
        return Some(arr.value(row).to_string());
    }
    if let Some(arr) = col.as_any().downcast_ref::<LargeStringArray>() {
        if arr.is_null(row) {
            return None;
        }
        return Some(arr.value(row).to_string());
    }
    if let Some(arr) = col.as_any().downcast_ref::<StringViewArray>() {
        if arr.is_null(row) {
            return None;
        }
        return Some(arr.value(row).to_string());
    }
    if let Some(arr) = col.as_any().downcast_ref::<BinaryArray>() {
        if arr.is_null(row) {
            return None;
        }
        return std::str::from_utf8(arr.value(row)).ok().map(str::to_string);
    }
    if let Some(arr) = col.as_any().downcast_ref::<LargeBinaryArray>() {
        if arr.is_null(row) {
            return None;
        }
        return std::str::from_utf8(arr.value(row)).ok().map(str::to_string);
    }
    None
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

async fn register_s3_object_store(
    ctx: &SessionContext,
    s3_loc: &str,
) -> Result<(), std::io::Error> {
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
            .unwrap_or_else(|| "eu-west-1".to_string())
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
    let store = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_region(&region)
        .build()
        .map_err(|err: object_store::Error| std::io::Error::other(err.to_string()))?;
    let store_arc: Arc<dyn ObjectStore> = Arc::new(store);
    let endpoint = Url::parse(&format!("s3://{bucket}/"))
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    ctx.runtime_env()
        .register_object_store(&endpoint, store_arc);
    Ok(())
}

async fn s3_prefix_has_objects(s3_loc: &str) -> Result<bool, std::io::Error> {
    Ok(!list_parquet_files(s3_loc).await?.is_empty())
}

async fn read_direct_file_cursor(
    bucket: &str,
    ops_prefix: &str,
    domain: &str,
    crawl_id: &str,
) -> Result<u32, std::io::Error> {
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let client = S3Client::new(&conf);
    let resp = match client
        .get_object()
        .bucket(bucket)
        .key(batch_state_key(ops_prefix, domain, crawl_id))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(_) => return Ok(0),
    };
    let bytes = resp
        .body
        .collect()
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?
        .into_bytes();
    let value = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    Ok(value
        .get("next_file_index")
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0))
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

async fn query_local_urls_index_path(
    path: &str,
    frontier_domains: &[String],
    crawl_ids: &[String],
    limit: u32,
) -> Result<Vec<CcUrlRecord>, std::io::Error> {
    let mut files = list_parquet_files(path).await?;
    if files.is_empty() {
        for crawl_id in crawl_ids {
            let crawl_path = format!("{}crawl_id={}/", path.trim_end_matches('/'), crawl_id);
            files.extend(list_parquet_files(&crawl_path).await?);
        }
    }
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let ctx = SessionContext::new();
    for file in &files {
        register_s3_object_store(&ctx, file).await?;
    }
    let df = ctx
        .read_parquet(files.clone(), ParquetReadOptions::default())
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let batches = df
        .collect()
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let crawl_set = crawl_ids
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<std::collections::HashSet<_>>();
    let mut records = Vec::new();
    'rows: for batch in batches {
        for row in 0..batch.num_rows() {
            if records.len() as u32 >= limit {
                break 'rows;
            }
            let Some(frontier_domain) = string_value(&batch, "frontier_domain", row) else {
                continue;
            };
            if !frontier_domains
                .iter()
                .any(|domain| frontier_domain.eq_ignore_ascii_case(domain))
            {
                continue;
            }
            let Some(cc_crawl_id) = string_value(&batch, "cc_crawl_id", row) else {
                continue;
            };
            if !crawl_set.contains(&cc_crawl_id.to_ascii_lowercase()) {
                continue;
            }
            let fetch_status = i64_value(&batch, "fetch_status", row);
            if let Some(status) = fetch_status {
                if status != 200 {
                    continue;
                }
            }
            let content_mime_type = string_value(&batch, "content_mime_type", row)
                .unwrap_or_else(|| "text/html".into());
            if !content_mime_type
                .to_ascii_lowercase()
                .starts_with("text/html")
            {
                continue;
            }
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
                fetch_status: fetch_status.and_then(|value| {
                    if value >= 0 {
                        u32::try_from(value).ok()
                    } else {
                        None
                    }
                }),
                content_mime_type,
                fetch_time: string_value(&batch, "fetch_time", row).unwrap_or_default(),
            });
        }
    }
    Ok(records)
}

async fn list_parquet_files(path: &str) -> Result<Vec<String>, std::io::Error> {
    let url = Url::parse(path).map_err(|err| std::io::Error::other(err.to_string()))?;
    if url.scheme() != "s3" {
        return Ok(Vec::new());
    }
    let bucket = url
        .host_str()
        .ok_or_else(|| std::io::Error::other(format!("missing S3 bucket in {path}")))?;
    let prefix = url.path().trim_start_matches('/');
    let base_conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let region = if bucket == "commoncrawl" {
        "us-east-1".to_string()
    } else {
        base_conf
            .region()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "eu-west-1".to_string())
    };
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region))
        .load()
        .await;
    let client = S3Client::new(&conf);
    let mut token = None;
    let mut files = Vec::new();
    loop {
        let resp = client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .set_continuation_token(token)
            .send()
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        for obj in resp.contents() {
            let Some(key) = obj.key() else {
                continue;
            };
            if key.ends_with(".parquet") {
                files.push(format!("s3://{bucket}/{key}"));
            }
        }
        token = resp.next_continuation_token().map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    files.sort();
    if files.is_empty() {
        eprintln!("list_parquet_files: no parquet under s3://{bucket}/{prefix}");
    }
    Ok(files)
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

    if request.cc_index_source == "local_urls_index" {
        let mut stats = CcIndexLoadStats {
            mode: "local_urls_index".into(),
            crawl_ids: request.crawl_ids.clone(),
            ..Default::default()
        };
        let mut records = Vec::new();
        for domain in request.frontier_domains {
            if records.len() as u32 >= request.max_urls {
                break;
            }
            let base = urls_index_uri(
                request.ops_bucket,
                request.ops_prefix,
                request.cc_urls_index_prefix,
                domain,
            );
            let mut candidate_paths = vec![base.clone()];
            for crawl_id in &request.crawl_ids {
                candidate_paths.push(format!("{base}crawl_id={crawl_id}/"));
            }
            for path in candidate_paths {
                if records.len() as u32 >= request.max_urls {
                    break;
                }
                stats.index_paths.push(path.clone());
                stats.batches_attempted += 1;
                let remaining = request.max_urls.saturating_sub(records.len() as u32);
                match query_local_urls_index_path(
                    &path,
                    request.frontier_domains,
                    &request.crawl_ids,
                    remaining,
                )
                .await
                {
                    Ok(mut batch_records) if !batch_records.is_empty() => {
                        records.append(&mut batch_records);
                    }
                    Ok(_) => {}
                    Err(err) => {
                        stats.batches_failed += 1;
                        stats.errors.push(format!("{path}: {err}"));
                    }
                }
            }
            if records.is_empty() && stats.batches_attempted > 0 {
                stats.mode = "local_urls_index_missing".into();
            }
        }
        stats.rows_selected = records.len() as u32;
        return CcIndexLoadResult { records, stats };
    }

    if !request.cc_direct_index_enabled {
        return CcIndexLoadResult {
            records: Vec::new(),
            stats: CcIndexLoadStats {
                mode: "direct_index_disabled".into(),
                crawl_ids: request.crawl_ids,
                errors: vec!["direct Common Crawl index scans are disabled".into()],
                ..Default::default()
            },
        };
    }

    let mut stats = CcIndexLoadStats {
        mode: "direct_datafusion_file".into(),
        crawl_ids: request.crawl_ids.clone(),
        ..Default::default()
    };
    let mut records = Vec::new();
    for crawl_id in &request.crawl_ids {
        if records.len() as u32 >= request.max_urls {
            break;
        }
        let crawl_path = crawl_index_uri(request.cc_index_base_uri, crawl_id);
        match list_parquet_files(&crawl_path).await {
            Ok(files) => {
                stats.files_total = stats.files_total.saturating_add(files.len() as u32);
                let cursor_domain = request
                    .frontier_domains
                    .first()
                    .map(String::as_str)
                    .unwrap_or("frontier");
                let start_index = read_direct_file_cursor(
                    request.ops_bucket,
                    request.ops_prefix,
                    cursor_domain,
                    crawl_id,
                )
                .await
                .unwrap_or(0) as usize;
                let mut processed_index = start_index;
                for (idx, path) in files.into_iter().enumerate().skip(start_index).take(1) {
                    if records.len() as u32 >= request.max_urls {
                        break;
                    }
                    processed_index = idx + 1;
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
                stats.next_file_index = processed_index as u32;
            }
            Err(err) => {
                stats.batches_failed += 1;
                stats.errors.push(format!("{crawl_path}: {err}"));
            }
        }
    }

    stats.rows_selected = records.len() as u32;
    CcIndexLoadResult { records, stats }
}

#[cfg(test)]
mod tests {
    use super::{crawl_index_uri, domain_hash, urls_index_uri};

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

    #[test]
    fn urls_index_uri_uses_domain_hash_partition() {
        assert_eq!(domain_hash("Skippr.io"), domain_hash("skippr.io"),);
        let uri = urls_index_uri(
            "bucket",
            "link-graph-corpus/",
            "/cc/urls_index",
            "skippr.io",
        );
        assert!(uri.starts_with("s3://bucket/link-graph-corpus/cc/urls_index/domain_hash="));
        assert!(uri.ends_with('/'));
    }

    /// Live local URL index read — requires AWS creds on upfoundry-prod-datalake.
    #[tokio::test]
    #[ignore = "live ops datalake S3 query"]
    async fn query_local_urls_index_live_semrush() {
        use super::{load_url_candidates, CcIndexRequest};

        let result = load_url_candidates(CcIndexRequest {
            fixture_dir: None,
            frontier_domains: &["semrush.com".to_string()],
            max_urls: 10,
            include_subdomains: true,
            ops_bucket: "upfoundry-prod-datalake",
            ops_prefix: "link-graph-corpus",
            cc_index_base_uri: "s3://commoncrawl/cc-index/table/cc-main/warc",
            cc_urls_index_prefix: "cc/urls_index",
            cc_index_source: "local_urls_index",
            cc_direct_index_enabled: false,
            crawl_ids: vec!["CC-MAIN-2025-08".to_string()],
        })
        .await;

        eprintln!("stats={:?}", result.stats);
        assert!(
            result.stats.rows_selected > 0,
            "expected local semrush URL rows"
        );
        assert!(!result.records.is_empty());
    }

    /// Live CC index probe — requires AWS creds with s3:ListBucket/GetObject on commoncrawl.
    /// Run: AWS_PROFILE=skippr-prod cargo test -p skippr-plugin-data-source-upfoundry-link-graph-ingest query_cc_index_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "live Common Crawl S3 query"]
    async fn query_cc_index_live_skippr_io_single_crawl() {
        use super::{load_url_candidates, CcIndexRequest};
        use std::time::Instant;

        let started = Instant::now();
        let result = load_url_candidates(CcIndexRequest {
            fixture_dir: None,
            frontier_domains: &["skippr.io".to_string()],
            max_urls: 5,
            include_subdomains: true,
            ops_bucket: "unused",
            ops_prefix: "link-graph-corpus",
            cc_index_base_uri: "s3://commoncrawl/cc-index/table/cc-main/warc",
            cc_urls_index_prefix: "cc/urls_index",
            cc_index_source: "direct_datafusion_file",
            cc_direct_index_enabled: true,
            crawl_ids: vec!["CC-MAIN-2025-08".to_string()],
        })
        .await;

        eprintln!("elapsed_secs={}", started.elapsed().as_secs());
        eprintln!("stats={:?}", result.stats);
        for row in &result.records {
            eprintln!("url={}", row.url);
        }

        assert_eq!(
            result.stats.batches_failed, 0,
            "errors={:?}",
            result.stats.errors
        );
        assert!(
            result.stats.rows_selected > 0,
            "expected skippr.io URLs in CC-MAIN-2025-08"
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
