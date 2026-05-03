use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};

use react_core::discover::stats::FieldStats;
use react_suite_data_engineer::providers::warehouse_utils;
use react_suite_data_engineer::providers::{
    finalize_provider_field_stats, parse_provider_u64, DatasetCatalogProvider, DatasetFieldStats,
    DatasetId, DatasetStats, ProviderEvidenceCapabilities, QueryProvider, QueryResult,
    WarehouseNaming,
};

const MOTHERDUCK_API_URL: &str = "https://api.motherduck.com/v1/sql";
const DEFAULT_MAX_CONCURRENCY: usize = 4;
const MAX_CONCURRENCY_CAP: usize = 8;
const DEFAULT_DISCOVERY_CACHE_TTL_SECS: u64 = 120;

#[derive(Clone, Debug, Default)]
pub struct MotherDuckSettings {
    pub motherduck_token: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub max_concurrency: usize,
    pub discovery_cache_ttl_secs: u64,
}

#[derive(Clone)]
pub struct MotherDuckProvider {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    token: String,
    database: String,
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

#[derive(serde::Deserialize)]
struct MdSqlResponse {
    #[serde(default)]
    columns: Vec<MdColumn>,
    #[serde(default)]
    data: Vec<Vec<serde_json::Value>>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(serde::Deserialize)]
struct MdColumn {
    name: String,
}

impl MotherDuckProvider {
    pub fn from_settings(settings: MotherDuckSettings) -> Result<Self, String> {
        let token = settings
            .motherduck_token
            .or_else(|| getenv_nonempty("MOTHERDUCK_TOKEN"))
            .ok_or_else(|| "motherduck: MOTHERDUCK_TOKEN is required".to_string())?;

        let database = settings
            .database
            .or_else(|| getenv_nonempty("MOTHERDUCK_DATABASE"))
            .unwrap_or_else(|| "my_db".to_string());

        let schema = settings
            .schema
            .filter(|s| !s.trim().is_empty())
            .or_else(|| getenv_nonempty("MOTHERDUCK_SCHEMA"))
            .unwrap_or_else(|| "main".to_string());

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
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| format!("motherduck: failed to build HTTP client: {}", e))?;

        Ok(Self {
            inner: Arc::new(Inner {
                client,
                token,
                database,
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
            .map_err(|_| "motherduck: query limiter closed".to_string())?;

        tracing::info!(target: "motherduck", sql_len = sql.len(), "query_started");

        let body = serde_json::json!({
            "sql": sql,
            "database": self.inner.database,
        });

        let resp = self
            .inner
            .client
            .post(MOTHERDUCK_API_URL)
            .bearer_auth(&self.inner.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("motherduck: request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("motherduck: HTTP {}: {}", status, text));
        }

        let md_resp: MdSqlResponse = resp
            .json()
            .await
            .map_err(|e| format!("motherduck: failed to parse response: {}", e))?;

        if let Some(err) = md_resp.error {
            return Err(format!("motherduck: query error: {}", err));
        }

        let header: Vec<String> = md_resp.columns.iter().map(|c| c.name.clone()).collect();
        let rows: Vec<Vec<String>> = md_resp
            .data
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| match v {
                        serde_json::Value::Null => String::new(),
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        other => other.to_string(),
                    })
                    .collect()
            })
            .collect();

        tracing::info!(
            target: "motherduck",
            rows = rows.len(),
            cols = header.len(),
            "query_succeeded"
        );

        Ok(QueryResult {
            header,
            rows,
            meta: Some(serde_json::json!({"engine": "motherduck"})),
        })
    }

    fn quote_ident_md(ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
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
             FROM information_schema.tables \
             WHERE table_schema NOT IN ('information_schema', 'pg_catalog') \
             AND table_schema = '{schema}' \
             ORDER BY table_schema, table_name",
            schema = self.inner.schema.replace('\'', "''"),
        );
        let qr = self.execute_sql(&sql).await?;
        let mut out: Vec<DatasetId> = Vec::new();
        for row in &qr.rows {
            if row.len() >= 3 && !row[2].trim().is_empty() {
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
             FROM information_schema.columns \
             WHERE table_schema = '{schema}' AND table_name = '{table}' \
             ORDER BY ordinal_position",
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

impl WarehouseNaming for MotherDuckProvider {
    fn kind(&self) -> react_suite_data_engineer::de_config::WarehouseKind {
        react_suite_data_engineer::de_config::WarehouseKind::Motherduck
    }

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
        warehouse_utils::parse_fqn_common(
            dataset_fqn,
            &self.inner.database,
            Some(&self.inner.schema),
        )
    }

    fn quote_ident(&self, ident: &str) -> String {
        Self::quote_ident_md(ident)
    }

    fn quote_fqn(&self, id: &DatasetId) -> String {
        format!(
            "{}.{}",
            Self::quote_ident_md(&id.database),
            Self::quote_ident_md(&id.table)
        )
    }

    fn sql_prompt_rules(&self) -> Vec<&'static str> {
        vec![
            "MotherDuck SQL: DuckDB-compatible dialect via MotherDuck cloud.",
            "MotherDuck SQL: Use double-quote quoting for identifiers.",
            "MotherDuck SQL: LIMIT N for row limiting.",
            "MotherDuck SQL: Supports list, struct, map, and union types natively.",
            "MotherDuck SQL: Use EPOCH(col) or EXTRACT(EPOCH FROM col) for Unix timestamps.",
            "MotherDuck SQL: TRY_CAST(expr AS type) for safe conversions.",
            "MotherDuck SQL: information_schema.tables and information_schema.columns available.",
        ]
    }

    fn sql_remediation_rules(&self) -> Vec<&'static str> {
        vec![
            "MotherDuck rule: No TOP N syntax. Use LIMIT N.",
            "MotherDuck rule: GETDATE() and SYSDATE are not supported. Use CURRENT_TIMESTAMP.",
            "MotherDuck rule: NVL() is not supported. Use COALESCE().",
        ]
    }
}

#[async_trait]
impl QueryProvider for MotherDuckProvider {
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
impl DatasetCatalogProvider for MotherDuckProvider {
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
            .and_then(|s| parse_provider_u64(s))
            .unwrap_or(0);

        let mut ns_stats = DatasetFieldStats::new(&dataset.fqn());

        for (name, ty) in cols.into_iter().take(max_fields) {
            let expr = Self::quote_ident_md(&name);
            let ty_lc = ty.trim().to_lowercase();
            let is_complex = ty_lc.starts_with("struct")
                || ty_lc.starts_with("list")
                || ty_lc.starts_with("map")
                || ty_lc.starts_with("union");
            let is_timestamp = ty_lc.contains("timestamp");
            let is_date = ty_lc == "date";

            let min_expr = if is_timestamp || is_date {
                format!("EXTRACT(EPOCH FROM {})", expr)
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
                        "motherduck stats: dataset='{}' field='{}' type='{}' failed: {}",
                        dataset.fqn(),
                        name,
                        ty,
                        e
                    );
                    continue;
                }
            };

            let row = qr.rows.first().cloned().unwrap_or_default();
            let mut fs = FieldStats::default();
            fs.total = total_rows;
            fs.nulls = row.get(1).and_then(|s| parse_provider_u64(s)).unwrap_or(0);
            if !is_complex {
                fs.approx_distinct = row.get(2).and_then(|s| parse_provider_u64(s));
                if fs.approx_distinct.is_some() {
                    ns_stats.exact_distinct_fields.insert(name.clone());
                }
                fs.min_numeric = row.get(3).and_then(|s| s.parse::<f64>().ok());
                fs.max_numeric = row.get(4).and_then(|s| s.parse::<f64>().ok());
            }
            finalize_provider_field_stats(&mut fs);
            ns_stats.fields.insert(name, fs);
        }

        let mut ds_stats = DatasetStats::default();
        ds_stats.approx_total_rows = total_rows;
        Ok((ns_stats, ds_stats))
    }

    fn evidence_capabilities(&self) -> ProviderEvidenceCapabilities {
        ProviderEvidenceCapabilities::sql_warehouse_without_relationships()
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
