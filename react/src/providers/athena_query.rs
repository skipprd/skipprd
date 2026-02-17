use async_trait::async_trait;
use aws_sdk_athena::types::{QueryExecutionState, ResultConfiguration};
use aws_sdk_athena::Client as AthenaClient;
use aws_sdk_glue::Client as GlueClient;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::sync::Semaphore;

use crate::providers::dataset_catalog_provider::{DatasetCatalogProvider, DatasetId};
use crate::providers::{QueryProvider, QueryResult};
use react_core::providers::warehouse::WarehouseNaming;

const DEFAULT_ATHENA_MAX_CONCURRENCY: usize = 15;
const ATHENA_MAX_CONCURRENCY_CAP: usize = 20;
fn clamp_athena_concurrency(n: usize) -> usize {
    n.max(1).min(ATHENA_MAX_CONCURRENCY_CAP)
}

#[derive(Clone)]
pub struct AthenaQueryProvider {
    inner: Arc<Inner>,
}

/// Explicit Athena configuration (preferred over reading env directly).
#[derive(Clone, Debug, Default)]
pub struct AthenaSettings {
    pub workgroup: Option<String>,
    pub result_output_location: Option<String>,
    pub default_catalog: String,
    /// Default schema/database for source discovery + unqualified queries.
    pub source_schema: Option<String>,
    /// Max number of in-flight Athena queries to allow (soft-limited by Athena/workgroup).
    pub max_concurrency: usize,
    pub discovery_cache_ttl_secs: u64,
}

struct Inner {
    athena: AthenaClient,
    glue: GlueClient,
    workgroup: Option<String>,
    result_output_location: Option<String>,
    default_catalog: String,
    source_schema: Option<String>,
    max_concurrency: usize,
    limiter: Arc<Semaphore>,
    cache_ttl: Duration,
    cache: RwLock<Cache>,
}

#[derive(Default)]
struct Cache {
    databases: Option<(Instant, Vec<String>)>,
    tables_by_db: HashMap<String, (Instant, Vec<String>)>,
    schema_by_fqn: HashMap<String, (Instant, Vec<(String, String)>)>,
}

impl AthenaQueryProvider {
    /// Create from explicit settings using the standard AWS credential chain.
    pub async fn from_settings(settings: AthenaSettings) -> Self {
        let ttl_secs = settings.discovery_cache_ttl_secs.max(5).min(3600);
        let max_concurrency = clamp_athena_concurrency(if settings.max_concurrency == 0 {
            DEFAULT_ATHENA_MAX_CONCURRENCY
        } else {
            settings.max_concurrency
        });
        let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let athena = AthenaClient::new(&aws_cfg);
        let glue = GlueClient::new(&aws_cfg);

        Self {
            inner: Arc::new(Inner {
                athena,
                glue,
                workgroup: settings.workgroup,
                result_output_location: settings.result_output_location,
                default_catalog: settings.default_catalog,
                source_schema: settings.source_schema,
                max_concurrency,
                limiter: Arc::new(Semaphore::new(max_concurrency)),
                cache_ttl: Duration::from_secs(ttl_secs),
                cache: RwLock::new(Cache::default()),
            }),
        }
    }

    /// Create from environment variables using the standard AWS credential chain.
    ///
    /// Supported env vars (with backward-compatible aliases):
    /// - `ATHENA_WORKGROUP` (alias: `DATA_OUTPUT_ATHENA_WORKGROUP_NAME`)
    /// - `ATHENA_RESULT_S3` (alias: `DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET` + optional prefix)
    /// - `ATHENA_SOURCE_SCHEMA` (alias: `ATHENA_SOURCE_DATABASE`)
    /// - `ATHENA_TARGET_CATALOG` (alias: `ATHENA_CATALOG`, default: `AwsDataCatalog`)
    /// - `ATHENA_MAX_CONCURRENCY` (default: 15, cap: 20)
    /// - `ATHENA_DISCOVERY_CACHE_TTL_SECS` (default: 120)
    pub async fn from_env() -> Self {
        let workgroup = getenv_nonempty("ATHENA_WORKGROUP")
            .or_else(|| getenv_nonempty("DATA_OUTPUT_ATHENA_WORKGROUP_NAME"));
        let source_schema = getenv_nonempty("ATHENA_SOURCE_SCHEMA")
            .or_else(|| getenv_nonempty("ATHENA_SOURCE_DATABASE"));
        let default_catalog = getenv_nonempty("ATHENA_TARGET_CATALOG")
            .unwrap_or_else(|| getenv("ATHENA_CATALOG", "AwsDataCatalog"));

        // Prefer a single s3://... output location if provided; else leave None and rely on WG config.
        let result_output_location = getenv_nonempty("ATHENA_RESULT_S3").or_else(|| {
            // Legacy: only bucket provided; user can still set ATHENA_RESULT_S3 for full control
            let b = getenv_nonempty("DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET")?;
            Some(format!("s3://{}/", b.trim_end_matches('/')))
        });

        let ttl_secs: u64 = getenv("ATHENA_DISCOVERY_CACHE_TTL_SECS", "120")
            .parse::<u64>()
            .unwrap_or(120);
        let max_concurrency: usize = std::env::var("ATHENA_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_ATHENA_MAX_CONCURRENCY);

        Self::from_settings(AthenaSettings {
            workgroup,
            result_output_location,
            default_catalog,
            source_schema,
            max_concurrency,
            discovery_cache_ttl_secs: ttl_secs,
        })
        .await
    }

    fn parse_dataset_id(&self, s: &str) -> Result<DatasetId, String> {
        let raw = s.trim();
        if raw.is_empty() {
            return Err("dataset id is empty".to_string());
        }

        // Allow quoted identifiers in input, but keep parsing simple.
        let cleaned = raw.trim_matches('"').trim_matches('`');
        let parts: Vec<&str> = cleaned.split('.').collect();
        match parts.len() {
            3 => Ok(DatasetId {
                catalog: parts[0].to_string(),
                database: parts[1].to_string(),
                table: parts[2].to_string(),
            }),
            2 => Ok(DatasetId {
                catalog: self.inner.default_catalog.clone(),
                database: parts[0].to_string(),
                table: parts[1].to_string(),
            }),
            1 => {
                let db = self.inner.source_schema.clone().ok_or_else(|| {
                    "dataset id missing database; set ATHENA_SOURCE_SCHEMA or use <db>.<table>"
                        .to_string()
                })?;
                Ok(DatasetId {
                    catalog: self.inner.default_catalog.clone(),
                    database: db,
                    table: parts[0].to_string(),
                })
            }
            _ => Err("dataset id must be <catalog>.<db>.<table> (or <db>.<table>)".to_string()),
        }
    }

    fn quote_ident(ident: &str) -> String {
        // Athena uses double quotes for identifiers; escape quotes by doubling.
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    fn quote_table(db: &str, table: &str) -> String {
        format!("{}.{}", Self::quote_ident(db), Self::quote_ident(table))
    }

    async fn start_query(&self, sql: &str, database: Option<&str>) -> Result<String, String> {
        let mut req = self
            .inner
            .athena
            .start_query_execution()
            .query_string(sql.to_string());
        if let Some(wg) = self.inner.workgroup.as_ref() {
            req = req.work_group(wg);
        }
        // Use explicit database if provided, else fall back to env default.
        if let Some(db) = database.or(self.inner.source_schema.as_deref()) {
            req = req.query_execution_context(
                aws_sdk_athena::types::QueryExecutionContext::builder()
                    .database(db)
                    .build(),
            );
        }
        // If output location specified, set it; otherwise rely on workgroup.
        if let Some(loc) = self.inner.result_output_location.as_ref() {
            req = req
                .result_configuration(ResultConfiguration::builder().output_location(loc).build());
        }
        let out = req.send().await.map_err(|e| e.to_string())?;
        out.query_execution_id()
            .map(|s| s.to_string())
            .ok_or_else(|| "missing query_execution_id".to_string())
    }

    async fn wait_query_succeeded(&self, qid: &str) -> Result<(Duration, Option<i64>), String> {
        let start = Instant::now();
        let mut sleep_ms: u64 = 200;
        loop {
            let out = self
                .inner
                .athena
                .get_query_execution()
                .query_execution_id(qid)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let qe = out
                .query_execution()
                .ok_or_else(|| "missing query_execution".to_string())?;
            let status = qe
                .status()
                .ok_or_else(|| "missing query_execution.status".to_string())?;
            match status.state() {
                Some(QueryExecutionState::Succeeded) => {
                    let bytes = qe.statistics().and_then(|s| s.data_scanned_in_bytes());
                    return Ok((start.elapsed(), bytes));
                }
                Some(QueryExecutionState::Failed) | Some(QueryExecutionState::Cancelled) => {
                    let reason = status.state_change_reason().unwrap_or("query failed");
                    return Err(reason.to_string());
                }
                _ => {
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                    sleep_ms = (sleep_ms * 2).min(1500);
                    continue;
                }
            }
        }
    }

    async fn fetch_all_rows(
        &self,
        qid: &str,
    ) -> Result<(Vec<String>, Vec<Vec<String>>, usize), String> {
        let mut next_token: Option<String> = None;
        let mut header: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut is_first_page = true;
        let mut pages: usize = 0;
        loop {
            let mut req = self
                .inner
                .athena
                .get_query_results()
                .query_execution_id(qid);
            if let Some(tok) = next_token.as_ref() {
                req = req.next_token(tok);
            }
            let out = req.send().await.map_err(|e| e.to_string())?;
            pages += 1;
            if let Some(rs) = out.result_set() {
                if header.is_empty() {
                    if let Some(md) = rs.result_set_metadata() {
                        let cols = md.column_info();
                        header = cols.iter().map(|c| c.name().to_string()).collect();
                    }
                }
                let rws = rs.rows();
                for (idx, r) in rws.iter().enumerate() {
                    // Athena includes a header row as the first row of the first page.
                    if is_first_page && idx == 0 {
                        continue;
                    }
                    let mut out_row: Vec<String> = Vec::new();
                    for d in r.data() {
                        let s = d.var_char_value().unwrap_or("").to_string();
                        out_row.push(s);
                    }
                    rows.push(out_row);
                }
            }
            next_token = out.next_token().map(|s| s.to_string());
            if next_token.is_none() {
                break;
            }
            is_first_page = false;
        }
        Ok((header, rows, pages))
    }

    async fn cached_databases(&self) -> Result<Vec<String>, String> {
        // If configured with a single source database, scope discovery to it.
        if let Some(db) = self
            .inner
            .source_schema
            .as_ref()
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(vec![db.trim().to_string()]);
        }
        {
            let cache = self.inner.cache.read().await;
            if let Some((ts, v)) = cache.databases.as_ref() {
                if ts.elapsed() < self.inner.cache_ttl {
                    return Ok(v.clone());
                }
            }
        }
        let mut out: Vec<String> = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let mut req = self.inner.glue.get_databases().max_results(100);
            if let Some(tok) = next_token.as_ref() {
                req = req.next_token(tok);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            for d in resp.database_list() {
                out.push(d.name().to_string());
            }
            next_token = resp.next_token().map(|s| s.to_string());
            if next_token.is_none() {
                break;
            }
        }
        out.sort();
        out.dedup();
        let mut cache = self.inner.cache.write().await;
        cache.databases = Some((Instant::now(), out.clone()));
        Ok(out)
    }

    async fn cached_tables(&self, database: &str) -> Result<Vec<String>, String> {
        {
            let cache = self.inner.cache.read().await;
            if let Some((ts, v)) = cache.tables_by_db.get(database) {
                if ts.elapsed() < self.inner.cache_ttl {
                    return Ok(v.clone());
                }
            }
        }
        let mut out: Vec<String> = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let mut req = self
                .inner
                .glue
                .get_tables()
                .database_name(database)
                .max_results(100);
            if let Some(tok) = next_token.as_ref() {
                req = req.next_token(tok);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            for t in resp.table_list() {
                out.push(t.name().to_string());
            }
            next_token = resp.next_token().map(|s| s.to_string());
            if next_token.is_none() {
                break;
            }
        }
        out.sort();
        out.dedup();
        let mut cache = self.inner.cache.write().await;
        cache
            .tables_by_db
            .insert(database.to_string(), (Instant::now(), out.clone()));
        Ok(out)
    }

    async fn cached_schema(&self, dataset: &DatasetId) -> Result<Vec<(String, String)>, String> {
        let fqn = dataset.fqn();
        {
            let cache = self.inner.cache.read().await;
            if let Some((ts, cols)) = cache.schema_by_fqn.get(&fqn) {
                if ts.elapsed() < self.inner.cache_ttl {
                    return Ok(cols.clone());
                }
            }
        }
        let resp = self
            .inner
            .glue
            .get_table()
            .database_name(&dataset.database)
            .name(&dataset.table)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let table = resp
            .table()
            .ok_or_else(|| "Glue GetTable returned no table".to_string())?;
        let sd = table
            .storage_descriptor()
            .ok_or_else(|| "Glue table missing storage_descriptor".to_string())?;

        let mut cols: Vec<(String, String)> = Vec::new();
        for col in sd.columns() {
            let name = col.name().to_string();
            let ty = col.r#type().unwrap_or("").to_string();
            if !name.is_empty() {
                cols.push((name, ty));
            }
        }
        // Include partition keys as well (Athena exposes them as columns in queries).
        for col in table.partition_keys() {
            let name = col.name().to_string();
            let ty = col.r#type().unwrap_or("").to_string();
            if !name.is_empty() && !cols.iter().any(|(n, _)| n == &name) {
                cols.push((name, ty));
            }
        }
        let mut cache = self.inner.cache.write().await;
        cache
            .schema_by_fqn
            .insert(fqn, (Instant::now(), cols.clone()));
        Ok(cols)
    }
}

impl WarehouseNaming for AthenaQueryProvider {
    fn kind(&self) -> &'static str {
        "athena"
    }

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
        let raw = dataset_fqn.trim().trim_matches('"').trim_matches('`');
        if raw.is_empty() {
            return Err("dataset id is empty".to_string());
        }
        let parts: Vec<&str> = raw.split('.').collect();
        match parts.len() {
            3 => Ok(DatasetId {
                catalog: parts[0].to_string(),
                database: parts[1].to_string(),
                table: parts[2].to_string(),
            }),
            2 => Ok(DatasetId {
                catalog: self.inner.default_catalog.clone(),
                database: parts[0].to_string(),
                table: parts[1].to_string(),
            }),
            1 => {
                let db = self
                    .inner
                    .source_schema
                    .clone()
                    .ok_or_else(|| "athena dataset id must be <catalog>.<database>.<table> (or configure source_schema)".to_string())?;
                Ok(DatasetId {
                    catalog: self.inner.default_catalog.clone(),
                    database: db,
                    table: parts[0].to_string(),
                })
            }
            _ => Err(
                "athena dataset id must be <catalog>.<database>.<table> (or <database>.<table>)"
                    .to_string(),
            ),
        }
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    fn sql_prompt_rules(&self) -> Vec<&'static str> {
        vec![
            "If Provider is athena (Trino SQL), DO NOT use initcap() (it is not registered). Avoid title-casing strings.",
            "If Provider is athena (Trino SQL), never reference a SELECT-list alias inside another expression in the same SELECT list. If one derived field depends on another, split into CTE/subquery + outer SELECT.",
        ]
    }

    fn sql_remediation_rules(&self) -> Vec<&'static str> {
        vec![
            "Trino/Athena rule: you cannot reference a SELECT-list alias in another expression in the same SELECT list. If one derived field depends on another, compute base fields in a CTE/subquery and use an outer SELECT.",
        ]
    }

    fn unsupported_sql_reason(&self, sql: &str) -> Option<String> {
        let s = sql.to_ascii_lowercase();
        if s.contains("initcap(") {
            return Some(
                "initcap() is not supported on Athena/Trino; remove it (use trim/lower/upper, or leave casing unchanged)."
                    .to_string(),
            );
        }
        if has_obvious_same_select_alias_reuse(sql) {
            return Some("Athena/Trino cannot reference a SELECT-list alias inside another expression in the same SELECT list; move the dependent expression to an outer SELECT/CTE.".to_string());
        }
        None
    }
}

fn has_obvious_same_select_alias_reuse(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    let Some(select_pos) = lower.find("select") else {
        return false;
    };
    let from_search_start = select_pos + "select".len();
    let Some(from_rel) = lower[from_search_start..].find(" from ") else {
        return false;
    };
    let select_end = from_search_start + from_rel;
    let select_list = &lower[from_search_start..select_end];
    let mut idx = 0usize;
    while let Some(as_rel) = select_list[idx..].find(" as ") {
        let as_pos = idx + as_rel;
        let alias_start = as_pos + 4;
        let Some((alias_name, consumed)) = parse_alias_token(&select_list[alias_start..]) else {
            idx = alias_start;
            continue;
        };
        if alias_name.is_empty() {
            idx = alias_start + consumed;
            continue;
        }
        let rest = &select_list[alias_start + consumed..];
        if contains_identifier_reference(rest, &alias_name) {
            return true;
        }
        idx = alias_start + consumed;
    }
    false
}

fn parse_alias_token(s: &str) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let q = bytes[i];
    if q == b'`' || q == b'"' {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != q {
            j += 1;
        }
        if j >= bytes.len() {
            return None;
        }
        return Some((s[i + 1..j].to_string(), j + 1));
    }
    let start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((s[start..i].to_string(), i))
}

fn contains_identifier_reference(haystack: &str, ident: &str) -> bool {
    if ident.trim().is_empty() {
        return false;
    }
    if haystack.contains(&format!("`{}`", ident)) || haystack.contains(&format!("\"{}\"", ident)) {
        return true;
    }
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(ident) {
        let pos = start + rel;
        let end = pos + ident.len();
        let prev = if pos == 0 {
            None
        } else {
            haystack.as_bytes().get(pos - 1).copied()
        };
        let next = haystack.as_bytes().get(end).copied();
        let prev_is_ident = prev
            .map(|b| b.is_ascii_alphanumeric() || b == b'_')
            .unwrap_or(false);
        let next_is_ident = next
            .map(|b| b.is_ascii_alphanumeric() || b == b'_')
            .unwrap_or(false);
        if !prev_is_ident && !next_is_ident {
            return true;
        }
        start = end;
    }
    false
}

#[async_trait]
impl QueryProvider for AthenaQueryProvider {
    async fn query(&self, sql: &str) -> Result<QueryResult, String> {
        // Global throttle: cap in-flight Athena queries to avoid soft-limit failures and reduce timeouts.
        let _permit = self
            .inner
            .limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "athena query limiter closed".to_string())?;

        let kind = if sql.contains("COUNT(1) AS __cnt") {
            "row_count"
        } else if sql.contains(" AS __distinct") || sql.contains(" AS __nulls") {
            "field_stats"
        } else if sql.contains("SELECT *") && sql.contains(" LIMIT ") {
            "sample"
        } else {
            "query"
        };
        let qid = self.start_query(sql, None).await?;
        tracing::info!(target: "athena", qid = %qid, kind = %kind, "query_started");
        let (elapsed, bytes_scanned) = self.wait_query_succeeded(&qid).await?;
        let (header, rows, pages) = self.fetch_all_rows(&qid).await?;
        tracing::info!(
            target: "athena",
            qid = %qid,
            kind = %kind,
            bytes_scanned = bytes_scanned,
            pages = pages,
            rows = rows.len(),
            "query_succeeded"
        );
        let meta = serde_json::json!({
            "engine": "athena",
            "query_execution_id": qid,
            "elapsed_ms": elapsed.as_millis() as u64,
            "bytes_scanned": bytes_scanned,
            "pages": pages
        });
        Ok(QueryResult {
            header,
            rows,
            meta: Some(meta),
        })
    }

    async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
        let ds = self.parse_dataset_id(dataset_fqn)?;
        self.cached_schema(&ds).await
    }

    async fn sample(&self, dataset_fqn: &str, limit: usize) -> Result<Vec<Vec<String>>, String> {
        let ds = self.parse_dataset_id(dataset_fqn)?;
        let lim = limit.max(1).min(5000);
        let sql = format!(
            "SELECT * FROM {} LIMIT {}",
            Self::quote_table(&ds.database, &ds.table),
            lim
        );
        let qr = self.query(&sql).await?;
        Ok(qr.rows)
    }

    fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }
}

#[async_trait]
impl DatasetCatalogProvider for AthenaQueryProvider {
    async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
        let dbs = self.cached_databases().await?;
        let mut out: Vec<DatasetId> = Vec::new();
        for db in dbs {
            let tables = self.cached_tables(&db).await.unwrap_or_default();
            for t in tables {
                out.push(DatasetId {
                    catalog: self.inner.default_catalog.clone(),
                    database: db.clone(),
                    table: t,
                });
            }
        }
        Ok(out)
    }

    async fn get_dataset_schema(
        &self,
        dataset: &DatasetId,
    ) -> Result<Vec<(String, String)>, String> {
        self.cached_schema(dataset).await
    }

    async fn get_dataset_stats(
        &self,
        dataset: &DatasetId,
        max_fields: usize,
    ) -> Result<
        (
            crate::discover::stats::DatasetFieldStats,
            crate::providers::catalog::types::DatasetStats,
        ),
        String,
    > {
        // Minimal baseline: row count + per-field nulls/distinct/min/max where feasible.
        // This is intentionally conservative; callers can choose to skip stats for very wide tables.
        let cols = self.cached_schema(dataset).await?;
        let max_fields = max_fields.max(1).min(500);
        let progress_every: usize = std::env::var("CATALOG_STATS_PROGRESS_EVERY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(10)
            .max(1)
            .min(250);
        tracing::info!(
            target: "catalog_stats",
            dataset = %dataset.fqn(),
            raw_columns = cols.len(),
            max_fields = max_fields,
            progress_every = progress_every,
            "dataset_stats_start"
        );

        let tbl = Self::quote_table(&dataset.database, &dataset.table);
        // Row count
        let count_sql = format!("SELECT COUNT(1) AS __cnt FROM {}", tbl);
        let qr_cnt = self.query(&count_sql).await?;
        let total_rows: u64 = qr_cnt
            .rows
            .get(0)
            .and_then(|r| r.get(0))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let mut ns_stats = crate::discover::stats::DatasetFieldStats::new(&dataset.fqn());
        // Expand nested struct/row columns into leaf dot-path fields (bounded by max_fields).
        let mut expanded: Vec<(String, String, Option<String>, bool)> = Vec::new(); // (path, type, expr, is_complex)
        for (name, ty) in cols.iter() {
            let leafs = crate::providers::type_parse::flatten_athena_type(name, ty);
            for lf in leafs {
                // Prefer a stable expression for top-level identifiers by quoting the root segment.
                let expr = if let Some(e) = lf.expr.as_ref() {
                    // If it's a nested deref (a.b.c), quote only the root.
                    if let Some((root, rest)) = e.split_once('.') {
                        Some(format!(
                            "{}{}",
                            Self::quote_ident(root),
                            format!(".{}", rest)
                        ))
                    } else {
                        Some(Self::quote_ident(e))
                    }
                } else {
                    None
                };
                expanded.push((lf.path, lf.data_type, expr, lf.is_complex));
            }
            if expanded.len() >= max_fields {
                break;
            }
        }
        expanded.truncate(max_fields);
        tracing::info!(
            target: "catalog_stats",
            dataset = %dataset.fqn(),
            expanded_fields = expanded.len(),
            "expanded_fields_ready"
        );

        let mut attempted: usize = 0;
        let mut executed: usize = 0;
        let mut succeeded: usize = 0;
        let mut failed: usize = 0;

        for (path, ty, expr_opt, is_complex) in expanded.into_iter() {
            attempted += 1;
            if attempted == 1 || attempted % progress_every == 0 {
                tracing::info!(
                    target: "catalog_stats",
                    dataset = %dataset.fqn(),
                    done = attempted,
                    last_field = %path,
                    "field_stats_progress"
                );
            }
            let Some(expr) = expr_opt else {
                let mut fs = crate::discover::stats::FieldStats::default();
                fs.total = total_rows;
                fs.finalize();
                ns_stats.fields.insert(path, fs);
                continue;
            };

            let ty_lc = ty.to_lowercase();
            let is_timestamp = ty_lc.starts_with("timestamp");
            let is_date = ty_lc.starts_with("date");

            let sql = if is_complex {
                format!(
                    "SELECT \
                        COUNT(1) AS __rows, \
                        SUM(CASE WHEN {c} IS NULL THEN 1 ELSE 0 END) AS __nulls \
                     FROM {}",
                    tbl,
                    c = expr,
                )
            } else {
                format!(
                    "SELECT \
                        COUNT(1) AS __rows, \
                        SUM(CASE WHEN {c} IS NULL THEN 1 ELSE 0 END) AS __nulls, \
                        COUNT(DISTINCT {c}) AS __distinct, \
                        MIN({min_expr}) AS __min_num, \
                        MAX({max_expr}) AS __max_num \
                     FROM {}",
                    tbl,
                    c = expr,
                    // Athena/Presto can't cast TIMESTAMP -> DOUBLE directly; use epoch seconds instead.
                    min_expr = if is_timestamp {
                        format!("to_unixtime({})", expr)
                    } else if is_date {
                        format!("to_unixtime(CAST({} AS TIMESTAMP))", expr)
                    } else {
                        format!("TRY_CAST({} AS DOUBLE)", expr)
                    },
                    max_expr = if is_timestamp {
                        format!("to_unixtime({})", expr)
                    } else if is_date {
                        format!("to_unixtime(CAST({} AS TIMESTAMP))", expr)
                    } else {
                        format!("TRY_CAST({} AS DOUBLE)", expr)
                    },
                )
            };

            let qr = match self.query(&sql).await {
                Ok(qr) => qr,
                Err(e) => {
                    tracing::warn!(
                        "athena stats: dataset='{}' field='{}' type='{}' failed: {}",
                        dataset.fqn(),
                        path,
                        ty,
                        e
                    );
                    executed += 1;
                    failed += 1;
                    let mut fs = crate::discover::stats::FieldStats::default();
                    fs.total = total_rows;
                    fs.finalize();
                    ns_stats.fields.insert(path, fs);
                    continue;
                }
            };
            executed += 1;
            succeeded += 1;
            let row = qr.rows.get(0).cloned().unwrap_or_default();
            let mut fs = crate::discover::stats::FieldStats::default();
            fs.total = total_rows;
            fs.nulls = row.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            if !is_complex {
                fs.approx_distinct = row.get(2).and_then(|s| s.parse::<u64>().ok());
                fs.min_numeric = row.get(3).and_then(|s| s.parse::<f64>().ok());
                fs.max_numeric = row.get(4).and_then(|s| s.parse::<f64>().ok());
            }
            fs.finalize();
            ns_stats.fields.insert(path, fs);
        }

        let mut ds = crate::providers::catalog::types::DatasetStats::default();
        ds.approx_total_rows = total_rows;
        tracing::info!(
            target: "catalog_stats",
            dataset = %dataset.fqn(),
            approx_total_rows = total_rows,
            attempted_fields = attempted,
            executed_queries = executed,
            succeeded_queries = succeeded,
            failed_queries = failed,
            "dataset_stats_done"
        );
        Ok((ns_stats, ds))
    }

    fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }
}

fn getenv(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn getenv_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .and_then(|v| if v.trim().is_empty() { None } else { Some(v) })
}
