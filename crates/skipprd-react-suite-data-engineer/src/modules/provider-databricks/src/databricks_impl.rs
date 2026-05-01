use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};

use react_core::discover::stats::FieldStats;
use react_suite_data_engineer::providers::warehouse_utils;
use react_suite_data_engineer::providers::{
    DatasetCatalogProvider, DatasetFieldStats, DatasetId, DatasetStats, QueryProvider, QueryResult,
    WarehouseNaming,
};

const DEFAULT_MAX_CONCURRENCY: usize = 15;
const MAX_CONCURRENCY_CAP: usize = 20;
const DEFAULT_DISCOVERY_CACHE_TTL_SECS: u64 = 120;
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_POLL_DURATION: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, Default)]
pub struct DatabricksSettings {
    pub workspace_url: Option<String>,
    pub token: Option<String>,
    pub warehouse_id: Option<String>,
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub max_concurrency: usize,
    pub discovery_cache_ttl_secs: u64,
}

#[derive(Clone)]
pub struct DatabricksProvider {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    base_url: String,
    token: String,
    warehouse_id: String,
    catalog: String,
    schema: String,
    max_concurrency: usize,
    limiter: Arc<Semaphore>,
    cache_ttl: Duration,
    cache: RwLock<Cache>,
}

#[derive(Default)]
struct Cache {
    tables: Option<(Instant, Vec<DatasetId>)>,
    columns_by_fqn: HashMap<String, (Instant, Vec<(String, String)>)>,
}

impl DatabricksProvider {
    pub async fn from_settings(settings: DatabricksSettings) -> Result<Self, String> {
        let workspace_url = settings
            .workspace_url
            .or_else(|| getenv_nonempty("DATABRICKS_HOST"))
            .ok_or("Databricks workspace URL is required (DATABRICKS_HOST)")?;
        let base_url = workspace_url.trim_end_matches('/').to_string();

        let token = settings
            .token
            .or_else(|| getenv_nonempty("DATABRICKS_TOKEN"))
            .ok_or("Databricks token is required (DATABRICKS_TOKEN)")?;

        let warehouse_id = settings
            .warehouse_id
            .or_else(|| getenv_nonempty("DATABRICKS_WAREHOUSE_ID"))
            .ok_or("Databricks warehouse_id is required (DATABRICKS_WAREHOUSE_ID)")?;

        let catalog = settings
            .catalog
            .or_else(|| getenv_nonempty("DATABRICKS_CATALOG"))
            .unwrap_or_else(|| "main".to_string());

        let schema = settings
            .schema
            .or_else(|| getenv_nonempty("DATABRICKS_SCHEMA"))
            .unwrap_or_else(|| "default".to_string());

        let max_concurrency = warehouse_utils::clamp_concurrency(
            if settings.max_concurrency == 0 {
                DEFAULT_MAX_CONCURRENCY
            } else {
                settings.max_concurrency
            },
            MAX_CONCURRENCY_CAP,
        );
        let ttl_secs =
            warehouse_utils::clamp_cache_ttl_secs(if settings.discovery_cache_ttl_secs == 0 {
                DEFAULT_DISCOVERY_CACHE_TTL_SECS
            } else {
                settings.discovery_cache_ttl_secs
            });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| format!("databricks: failed to build HTTP client: {}", e))?;

        Ok(Self {
            inner: Arc::new(Inner {
                client,
                base_url,
                token,
                warehouse_id,
                catalog,
                schema,
                max_concurrency,
                limiter: Arc::new(Semaphore::new(max_concurrency)),
                cache_ttl: Duration::from_secs(ttl_secs),
                cache: RwLock::new(Cache::default()),
            }),
        })
    }

    async fn execute_sql(&self, sql: &str) -> Result<QueryResult, String> {
        let _permit = self
            .inner
            .limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "databricks: query limiter closed".to_string())?;

        tracing::info!(target: "databricks", sql_len = sql.len(), "query_started");

        let url = format!("{}/api/2.0/sql/statements", self.inner.base_url);
        let body = serde_json::json!({
            "warehouse_id": self.inner.warehouse_id,
            "statement": sql,
            "wait_timeout": "50s",
            "disposition": "INLINE",
            "format": "JSON_ARRAY",
        });

        let resp = self
            .inner
            .client
            .post(&url)
            .bearer_auth(&self.inner.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("databricks: request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("databricks: HTTP {}: {}", status, text));
        }

        let mut result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("databricks: failed to parse response: {}", e))?;

        let status_state = result
            .pointer("/status/state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if status_state == "FAILED" {
            let msg = result
                .pointer("/status/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("databricks: query failed: {}", msg));
        }

        if status_state == "PENDING" || status_state == "RUNNING" {
            let stmt_id = result
                .get("statement_id")
                .and_then(|v| v.as_str())
                .ok_or("databricks: no statement_id in pending response")?
                .to_string();
            result = self.poll_statement(&stmt_id).await?;
        }

        self.parse_result(&result)
    }

    async fn poll_statement(&self, statement_id: &str) -> Result<serde_json::Value, String> {
        let url = format!(
            "{}/api/2.0/sql/statements/{}",
            self.inner.base_url, statement_id
        );
        let start = Instant::now();

        loop {
            if start.elapsed() > MAX_POLL_DURATION {
                return Err(format!(
                    "databricks: statement {} timed out after {:?}",
                    statement_id, MAX_POLL_DURATION
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;

            let resp = self
                .inner
                .client
                .get(&url)
                .bearer_auth(&self.inner.token)
                .send()
                .await
                .map_err(|e| format!("databricks: poll failed: {}", e))?;

            let result: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("databricks: poll parse failed: {}", e))?;

            let state = result
                .pointer("/status/state")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match state {
                "SUCCEEDED" => return Ok(result),
                "FAILED" | "CANCELED" | "CLOSED" => {
                    let msg = result
                        .pointer("/status/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(format!(
                        "databricks: query {}: {}",
                        state.to_lowercase(),
                        msg
                    ));
                }
                _ => continue,
            }
        }
    }

    fn parse_result(&self, result: &serde_json::Value) -> Result<QueryResult, String> {
        let header: Vec<String> = result
            .pointer("/manifest/schema/columns")
            .and_then(|v| v.as_array())
            .map(|cols| {
                cols.iter()
                    .filter_map(|c| {
                        c.get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();

        let rows: Vec<Vec<String>> = result
            .pointer("/result/data_array")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|row| {
                        row.as_array()
                            .map(|cells| {
                                cells
                                    .iter()
                                    .map(|c| match c {
                                        serde_json::Value::String(s) => s.clone(),
                                        serde_json::Value::Null => String::new(),
                                        other => other.to_string(),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();

        tracing::info!(
            target: "databricks",
            rows = rows.len(),
            cols = header.len(),
            "query_succeeded"
        );

        Ok(QueryResult {
            header,
            rows,
            meta: Some(serde_json::json!({"engine": "databricks"})),
        })
    }

    fn quote_ident_db(ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    async fn cached_tables(&self) -> Result<Vec<DatasetId>, String> {
        {
            let cache = self.inner.cache.read().await;
            if let Some((ts, v)) = cache.tables.as_ref() {
                if ts.elapsed() < self.inner.cache_ttl {
                    return Ok(v.clone());
                }
            }
        }

        let sql = format!(
            "SELECT table_catalog, table_schema, table_name \
             FROM {cat}.information_schema.tables \
             WHERE table_schema = '{schema}' \
             AND table_type IN ('BASE TABLE', 'MANAGED', 'EXTERNAL') \
             ORDER BY table_schema, table_name",
            cat = Self::quote_ident_db(&self.inner.catalog),
            schema = self.inner.schema.replace('\'', "''"),
        );
        let qr = self.execute_sql(&sql).await?;
        let mut out: Vec<DatasetId> = Vec::new();
        for row in &qr.rows {
            if row.len() >= 3
                && !row[0].trim().is_empty()
                && !row[1].trim().is_empty()
                && !row[2].trim().is_empty()
            {
                out.push(DatasetId {
                    catalog: row[0].clone(),
                    database: row[1].clone(),
                    table: row[2].clone(),
                });
            }
        }

        let mut cache = self.inner.cache.write().await;
        cache.tables = Some((Instant::now(), out.clone()));
        Ok(out)
    }

    async fn cached_columns(&self, dataset: &DatasetId) -> Result<Vec<(String, String)>, String> {
        let fqn = dataset.fqn();
        {
            let cache = self.inner.cache.read().await;
            if let Some((ts, cols)) = cache.columns_by_fqn.get(&fqn) {
                if ts.elapsed() < self.inner.cache_ttl {
                    return Ok(cols.clone());
                }
            }
        }

        let sql = format!(
            "SELECT column_name, data_type \
             FROM {cat}.information_schema.columns \
             WHERE table_catalog = '{cat_val}' \
             AND table_schema = '{schema}' \
             AND table_name = '{table}' \
             ORDER BY ordinal_position",
            cat = Self::quote_ident_db(&dataset.catalog),
            cat_val = dataset.catalog.replace('\'', "''"),
            schema = dataset.database.replace('\'', "''"),
            table = dataset.table.replace('\'', "''"),
        );
        let qr = self.execute_sql(&sql).await?;
        let cols: Vec<(String, String)> = qr
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() >= 2 && !r[0].trim().is_empty() {
                    Some((r[0].clone(), r[1].clone()))
                } else {
                    None
                }
            })
            .collect();

        let mut cache = self.inner.cache.write().await;
        cache
            .columns_by_fqn
            .insert(fqn, (Instant::now(), cols.clone()));
        Ok(cols)
    }
}

impl WarehouseNaming for DatabricksProvider {
    fn kind(&self) -> react_suite_data_engineer::de_config::WarehouseKind {
        react_suite_data_engineer::de_config::WarehouseKind::Databricks
    }

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
        warehouse_utils::parse_fqn_common(
            dataset_fqn,
            &self.inner.catalog,
            Some(&self.inner.schema),
        )
    }

    fn quote_ident(&self, ident: &str) -> String {
        Self::quote_ident_db(ident)
    }

    fn sql_prompt_rules(&self) -> Vec<&'static str> {
        vec![
            "Databricks SQL: Use backtick quoting for identifiers: `catalog`.`schema`.`table`.",
            "Databricks SQL: Unity Catalog uses 3-part names: catalog.schema.table.",
            "Databricks SQL: Use TRY_CAST(expr AS type) for safe type conversions.",
            "Databricks SQL: Use LIMIT N for row limiting.",
            "Databricks SQL: Date functions include DATE_FORMAT(), UNIX_TIMESTAMP(), FROM_UNIXTIME().",
            "Databricks SQL: String concatenation uses CONCAT() or || operator.",
        ]
    }

    fn sql_remediation_rules(&self) -> Vec<&'static str> {
        vec![
            "Databricks rule: Use backticks (`) not double quotes for identifier quoting.",
            "Databricks rule: SAFE_CAST is not supported. Use TRY_CAST(expr AS type) instead.",
            "Databricks rule: TOP N is not supported. Use LIMIT N at end of query.",
            "Databricks rule: GETDATE()/SYSDATE not supported. Use CURRENT_TIMESTAMP() or NOW().",
        ]
    }
}

#[async_trait]
impl QueryProvider for DatabricksProvider {
    async fn query(&self, sql: &str) -> Result<QueryResult, String> {
        self.execute_sql(sql).await
    }

    async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
        let ds = self.parse_dataset_fqn(dataset_fqn)?;
        self.cached_columns(&ds).await
    }

    async fn sample(&self, dataset_fqn: &str, limit: usize) -> Result<Vec<Vec<String>>, String> {
        let ds = self.parse_dataset_fqn(dataset_fqn)?;
        let lim = limit.max(1).min(5000);
        let sql = format!("SELECT * FROM {} LIMIT {}", self.quote_fqn(&ds), lim);
        let qr = self.query(&sql).await?;
        Ok(qr.rows)
    }

    fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }
}

#[async_trait]
impl DatasetCatalogProvider for DatabricksProvider {
    async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
        self.cached_tables().await
    }

    async fn get_dataset_schema(
        &self,
        dataset: &DatasetId,
    ) -> Result<Vec<(String, String)>, String> {
        self.cached_columns(dataset).await
    }

    async fn get_dataset_stats(
        &self,
        dataset: &DatasetId,
        max_fields: usize,
    ) -> Result<(DatasetFieldStats, DatasetStats), String> {
        let cols = self.cached_columns(dataset).await?;
        let max_fields = max_fields.max(1).min(500);
        let tbl = self.quote_fqn(dataset);

        let count_sql = format!("SELECT COUNT(1) AS __cnt FROM {}", tbl);
        let qr_cnt = self.query(&count_sql).await?;
        let total_rows: u64 = qr_cnt
            .rows
            .first()
            .and_then(|r| r.first())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let mut ns_stats = DatasetFieldStats::new(&dataset.fqn());

        for (name, ty) in cols.into_iter().take(max_fields) {
            let expr = Self::quote_ident_db(&name);
            let ty_lc = ty.trim().to_lowercase();
            let is_complex = ty_lc.starts_with("struct")
                || ty_lc.starts_with("array")
                || ty_lc.starts_with("map");
            let is_timestamp = ty_lc.contains("timestamp");
            let is_date = ty_lc == "date";

            let min_expr = if is_timestamp || is_date {
                format!("UNIX_TIMESTAMP({})", expr)
            } else {
                format!("TRY_CAST({} AS DOUBLE)", expr)
            };
            let max_expr = min_expr.clone();

            let sql = if is_complex {
                format!(
                    "SELECT \
                        COUNT(1) AS __rows, \
                        SUM(CASE WHEN {c} IS NULL THEN 1 ELSE 0 END) AS __nulls, \
                        NULL AS __distinct, \
                        NULL AS __min_num, \
                        NULL AS __max_num \
                     FROM {tbl}",
                    c = expr,
                    tbl = tbl,
                )
            } else {
                format!(
                    "SELECT \
                        COUNT(1) AS __rows, \
                        SUM(CASE WHEN {c} IS NULL THEN 1 ELSE 0 END) AS __nulls, \
                        COUNT(DISTINCT {c}) AS __distinct, \
                        MIN({min_e}) AS __min_num, \
                        MAX({max_e}) AS __max_num \
                     FROM {tbl}",
                    c = expr,
                    min_e = min_expr,
                    max_e = max_expr,
                    tbl = tbl,
                )
            };

            let qr = match self.query(&sql).await {
                Ok(qr) => qr,
                Err(e) => {
                    tracing::warn!(
                        "databricks stats: dataset='{}' field='{}' type='{}' failed: {}",
                        dataset.fqn(),
                        name,
                        ty,
                        e
                    );
                    let mut fs = FieldStats::default();
                    fs.total = total_rows;
                    fs.finalize();
                    ns_stats.fields.insert(name, fs);
                    continue;
                }
            };

            let row = qr.rows.first().cloned().unwrap_or_default();
            let mut fs = FieldStats::default();
            fs.total = total_rows;
            fs.nulls = row.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            if !is_complex {
                fs.approx_distinct = row.get(2).and_then(|s| s.parse::<u64>().ok());
                if fs.approx_distinct.is_some() {
                    ns_stats.exact_distinct_fields.insert(name.clone());
                }
                fs.min_numeric = row.get(3).and_then(|s| s.parse::<f64>().ok());
                fs.max_numeric = row.get(4).and_then(|s| s.parse::<f64>().ok());
            }
            fs.finalize();
            ns_stats.fields.insert(name, fs);
        }

        let mut ds_stats = DatasetStats::default();
        ds_stats.approx_total_rows = total_rows;
        Ok((ns_stats, ds_stats))
    }

    fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }
}

fn getenv_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .and_then(|v| if v.trim().is_empty() { None } else { Some(v) })
}
