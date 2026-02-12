//! BigQuery warehouse provider for bronze/raw discovery and query execution.
//!
//! Uses a single dataset (config namespace) for table discovery. Supports
//! BigQuery public datasets (e.g. bigquery-public-data.new_york_mv_collisions).
//!
//! Auth: Set GOOGLE_APPLICATION_CREDENTIALS to a service account JSON path.

use async_trait::async_trait;
use gcp_bigquery_client::Client;
use gcp_bigquery_client::model::query_request::QueryRequest;
use gcp_bigquery_client::model::table_cell::TableCell;
use gcp_bigquery_client::table::ListOptions;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::sync::Semaphore;

use crate::providers::dataset_catalog_provider::{DatasetCatalogProvider, DatasetId};
use crate::providers::{QueryProvider, QueryResult};
use react_core::discover::stats::{DatasetFieldStats, FieldStats};
use react_core::providers::warehouse::WarehouseNaming;

const DEFAULT_BIGQUERY_MAX_CONCURRENCY: usize = 15;
const BIGQUERY_MAX_CONCURRENCY_CAP: usize = 20;
const DEFAULT_BIGQUERY_DISCOVERY_CACHE_TTL_SECS: u64 = 120;

fn clamp_bigquery_concurrency(n: usize) -> usize {
    n.max(1).min(BIGQUERY_MAX_CONCURRENCY_CAP)
}

fn clamp_cache_ttl_secs(n: u64) -> u64 {
    n.max(5).min(3600)
}

fn cell_to_string(cell: &TableCell) -> String {
    cell.value
        .as_ref()
        .map(|v: &serde_json::Value| match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

#[derive(Clone, Debug, Default)]
pub struct BigQuerySettings {
    pub project: Option<String>,
    pub dataset: Option<String>,
    pub location: Option<String>,
    pub max_concurrency: usize,
    pub discovery_cache_ttl_secs: u64,
}

#[derive(Clone)]
pub struct BigQueryProvider {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    project: String,
    dataset: String,
    location: Option<String>,
    max_concurrency: usize,
    limiter: Arc<Semaphore>,
    cache_ttl: Duration,
    cache: RwLock<Cache>,
}

#[derive(Default)]
struct Cache {
    tables: Option<(Instant, Vec<String>)>,
    schema_by_fqn: HashMap<String, (Instant, Vec<(String, String)>)>,
}

impl BigQueryProvider {
    /// Create from explicit settings.
    /// Uses GOOGLE_APPLICATION_CREDENTIALS (service account JSON path).
    pub async fn from_settings(settings: BigQuerySettings) -> Result<Self, String> {
        let project = settings
            .project
            .as_ref()
            .and_then(|s| {
                let t = s.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            })
            .ok_or_else(|| {
                "BigQuery project is required (providers.warehouse.project or BIGQUERY_PROJECT)"
                    .to_string()
            })?;
        let dataset = settings
            .dataset
            .as_ref()
            .and_then(|s| {
                let t = s.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            })
            .ok_or_else(|| {
                "BigQuery dataset is required (providers.warehouse.dataset or namespace)"
                    .to_string()
            })?;

        let cred_path = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
            .map_err(|_| {
                "BigQuery requires GOOGLE_APPLICATION_CREDENTIALS (path to service account JSON)"
                    .to_string()
            })?;
        let client = Client::from_service_account_key_file(&cred_path)
            .await
            .map_err(|e| format!("BigQuery client init failed: {}", e))?;

        let max = clamp_bigquery_concurrency(if settings.max_concurrency == 0 {
            DEFAULT_BIGQUERY_MAX_CONCURRENCY
        } else {
            settings.max_concurrency
        });
        let ttl_secs = clamp_cache_ttl_secs(if settings.discovery_cache_ttl_secs == 0 {
            DEFAULT_BIGQUERY_DISCOVERY_CACHE_TTL_SECS
        } else {
            settings.discovery_cache_ttl_secs
        });

        Ok(Self {
            inner: Arc::new(Inner {
                client,
                project: project.clone(),
                dataset: dataset.clone(),
                location: settings.location.clone(),
                max_concurrency: max,
                limiter: Arc::new(Semaphore::new(max)),
                cache_ttl: Duration::from_secs(ttl_secs),
                cache: RwLock::new(Cache::default()),
            }),
        })
    }

    fn quote_ident(ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn quote_table(project: &str, dataset: &str, table: &str) -> String {
        format!(
            "{}.{}.{}",
            Self::quote_ident(project),
            Self::quote_ident(dataset),
            Self::quote_ident(table)
        )
    }

    async fn cached_tables(&self) -> Result<Vec<String>, String> {
        {
            let cache = self.inner.cache.read().await;
            if let Some((ts, v)) = cache.tables.as_ref() {
                if ts.elapsed() < self.inner.cache_ttl {
                    return Ok(v.clone());
                }
            }
        }

        let mut out: Vec<String> = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut opts = ListOptions::default().max_results(1000);
            if let Some(tok) = page_token.as_ref() {
                opts = opts.page_token(tok.clone());
            }
            let resp = self
                .inner
                .client
                .table()
                .list(&self.inner.project, &self.inner.dataset, opts)
                .await
                .map_err(|e| format!("BigQuery list tables failed: {}", e))?;

            if let Some(tables) = resp.tables {
                for t in tables {
                    let tr = &t.table_reference;
                    let table_id = tr.table_id.clone();
                    let ttype = t.r#type.as_deref().unwrap_or("");
                    if !table_id.is_empty() && (ttype.is_empty() || ttype == "TABLE") {
                        out.push(table_id);
                    }
                }
            }

            page_token = resp.next_page_token.clone();
            if page_token.is_none() {
                break;
            }
        }

        out.sort();
        out.dedup();
        let mut cache = self.inner.cache.write().await;
        cache.tables = Some((Instant::now(), out.clone()));
        Ok(out)
    }

    async fn cached_schema(&self, ds: &DatasetId) -> Result<Vec<(String, String)>, String> {
        let fqn = ds.fqn();
        {
            let cache = self.inner.cache.read().await;
            if let Some((ts, cols)) = cache.schema_by_fqn.get(&fqn) {
                if ts.elapsed() < self.inner.cache_ttl {
                    return Ok(cols.clone());
                }
            }
        }

        let table = ds.table.replace('\\', "\\\\").replace('\'', "''");
        // BigQuery requires project.dataset.INFORMATION_SCHEMA as one backtick-quoted identifier
        let schema_path = format!(
            "{}.{}.INFORMATION_SCHEMA.COLUMNS",
            ds.catalog.replace('`', "``"),
            ds.database.replace('`', "``")
        );
        let sql = format!(
            "SELECT column_name, data_type FROM `{}` WHERE table_name = '{}' ORDER BY ordinal_position",
            schema_path, table
        );
        let qr = self.run_query(&sql).await?;
        let mut cols: Vec<(String, String)> = Vec::new();
        for row in qr.rows {
            if row.len() >= 2 && !row[0].trim().is_empty() {
                cols.push((row[0].clone(), row[1].clone()));
            }
        }

        let mut cache = self.inner.cache.write().await;
        cache
            .schema_by_fqn
            .insert(fqn, (Instant::now(), cols.clone()));
        Ok(cols)
    }

    async fn run_query(&self, sql: &str) -> Result<QueryResult, String> {
        let _permit = self
            .inner
            .limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "BigQuery limiter closed".to_string())?;

        let mut request = QueryRequest::default();
        request.query = sql.to_string();
        request.use_legacy_sql = false;
        request.location = self.inner.location.clone();

        let resp = self
            .inner
            .client
            .job()
            .query(&self.inner.project, request)
            .await
            .map_err(|e| format!("BigQuery query failed: {}", e))?;

        if let Some(errors) = &resp.errors {
            if !errors.is_empty() {
                let msg = errors
                    .iter()
                    .map(|e| e.message.as_deref().unwrap_or("unknown"))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(format!("BigQuery error: {}", msg));
            }
        }

        let header: Vec<String> = resp
            .schema
            .as_ref()
            .and_then(|s| s.fields.as_ref())
            .map(|f| f.iter().map(|x| x.name.to_string()).collect())
            .unwrap_or_default();

        let rows: Vec<Vec<String>> = resp
            .rows
            .as_ref()
            .map(|r| {
                r.iter()
                    .map(|row| {
                        row.columns
                            .as_ref()
                            .map(|c| c.iter().map(cell_to_string).collect())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();

        let meta = serde_json::json!({
            "engine": "bigquery",
            "cache_hit": resp.cache_hit,
            "job_complete": resp.job_complete,
            "total_bytes_processed": resp.total_bytes_processed,
            "total_rows": resp.total_rows,
            "job_reference": resp.job_reference,
        });
        Ok(QueryResult {
            header,
            rows,
            meta: Some(meta),
        })
    }
}

impl WarehouseNaming for BigQueryProvider {
    fn kind(&self) -> &'static str {
        "bigquery"
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
                catalog: self.inner.project.clone(),
                database: parts[0].to_string(),
                table: parts[1].to_string(),
            }),
            1 => Ok(DatasetId {
                catalog: self.inner.project.clone(),
                database: self.inner.dataset.clone(),
                table: parts[0].to_string(),
            }),
            _ => Err("dataset id must be <project>.<dataset>.<table> (or <dataset>.<table> or <table>)".to_string()),
        }
    }

    fn quote_ident(&self, ident: &str) -> String {
        Self::quote_ident(ident)
    }

    fn quote_fqn(&self, id: &DatasetId) -> String {
        Self::quote_table(&id.catalog, &id.database, &id.table)
    }
}

#[async_trait]
impl QueryProvider for BigQueryProvider {
    async fn query(&self, sql: &str) -> Result<QueryResult, String> {
        self.run_query(sql).await
    }

    async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
        let ds = self.parse_dataset_fqn(dataset_fqn)?;
        self.cached_schema(&ds).await
    }

    async fn sample(&self, dataset_fqn: &str, limit: usize) -> Result<Vec<Vec<String>>, String> {
        let ds = self.parse_dataset_fqn(dataset_fqn)?;
        let lim = limit.max(1).min(5000);
        let tbl = Self::quote_table(&ds.catalog, &ds.database, &ds.table);
        let sql = format!("SELECT * FROM {} LIMIT {}", tbl, lim);
        let qr = self.query(&sql).await?;
        Ok(qr.rows)
    }

    fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }
}

#[async_trait]
impl DatasetCatalogProvider for BigQueryProvider {
    async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
        let tables = self.cached_tables().await?;
        Ok(tables
            .into_iter()
            .map(|t| DatasetId {
                catalog: self.inner.project.clone(),
                database: self.inner.dataset.clone(),
                table: t,
            })
            .collect())
    }

    async fn get_dataset_schema(
        &self,
        dataset: &DatasetId,
    ) -> Result<Vec<(String, String)>, String> {
        self.schema(&dataset.fqn()).await
    }

    async fn get_dataset_stats(
        &self,
        dataset: &DatasetId,
        max_fields: usize,
    ) -> Result<
        (
            react_core::discover::stats::DatasetFieldStats,
            react_core::providers::catalog::types::DatasetStats,
        ),
        String,
    > {
        let cols = self.cached_schema(dataset).await?;
        let max_fields = max_fields.max(1).min(500);

        let tbl = Self::quote_table(&dataset.catalog, &dataset.database, &dataset.table);

        // Row count
        let count_sql = format!("SELECT COUNT(1) AS __cnt FROM {}", tbl);
        let qr_cnt = self.query(&count_sql).await?;
        let total_rows: u64 = qr_cnt
            .rows
            .get(0)
            .and_then(|r| r.get(0))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let mut ns_stats = DatasetFieldStats::new(&dataset.fqn());
        let mut attempted: usize = 0;

        for (name, ty) in cols.into_iter().take(max_fields) {
            attempted += 1;
            let expr = Self::quote_ident(&name);
            let ty_lc = ty.trim().to_lowercase();
            let is_complex = ty_lc.starts_with("array<") || ty_lc.starts_with("struct<");
            let is_timestamp = ty_lc == "timestamp";
            let is_date = ty_lc == "date";
            let is_datetime = ty_lc == "datetime";

            let min_expr = if is_timestamp {
                format!("CAST(UNIX_SECONDS({}) AS FLOAT64)", expr)
            } else if is_date || is_datetime {
                // DATE/DATETIME -> TIMESTAMP -> epoch seconds
                format!("CAST(UNIX_SECONDS(TIMESTAMP({})) AS FLOAT64)", expr)
            } else {
                format!("SAFE_CAST({} AS FLOAT64)", expr)
            };
            let max_expr = min_expr.clone();

            // Keep a stable 5-column result shape for parsing.
            let sql = if is_complex {
                format!(
                    "SELECT \
                        COUNT(1) AS __rows, \
                        SUM(IF({c} IS NULL, 1, 0)) AS __nulls, \
                        CAST(NULL AS INT64) AS __distinct, \
                        CAST(NULL AS FLOAT64) AS __min_num, \
                        CAST(NULL AS FLOAT64) AS __max_num \
                     FROM {tbl}",
                    c = expr,
                    tbl = tbl
                )
            } else {
                format!(
                    "SELECT \
                        COUNT(1) AS __rows, \
                        SUM(IF({c} IS NULL, 1, 0)) AS __nulls, \
                        COUNT(DISTINCT {c}) AS __distinct, \
                        MIN({min_e}) AS __min_num, \
                        MAX({max_e}) AS __max_num \
                     FROM {tbl}",
                    c = expr,
                    min_e = min_expr,
                    max_e = max_expr,
                    tbl = tbl
                )
            };

            let qr = match self.query(&sql).await {
                Ok(qr) => qr,
                Err(_) => {
                    let mut fs = FieldStats::default();
                    fs.total = total_rows;
                    fs.finalize();
                    ns_stats.fields.insert(name, fs);
                    continue;
                }
            };

            let row = qr.rows.get(0).cloned().unwrap_or_default();
            let mut fs = FieldStats::default();
            fs.total = total_rows;
            fs.nulls = row.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            if !is_complex {
                fs.approx_distinct = row.get(2).and_then(|s| s.parse::<u64>().ok());
                fs.min_numeric = row.get(3).and_then(|s| s.parse::<f64>().ok());
                fs.max_numeric = row.get(4).and_then(|s| s.parse::<f64>().ok());
            }
            fs.finalize();
            ns_stats.fields.insert(name, fs);
        }

        let mut ds_stats = react_core::providers::catalog::types::DatasetStats::default();
        ds_stats.approx_total_rows = total_rows;
        let _ = attempted;
        Ok((ns_stats, ds_stats))
    }

    fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }
}
