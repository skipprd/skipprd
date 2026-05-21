use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};

use react_core::discover::stats::FieldStats;
use react_suite_data_engineer::providers::warehouse_utils;
use react_suite_data_engineer::providers::{
    finalize_provider_field_stats, parse_provider_u64, DatasetCatalogProvider, DatasetFieldStats,
    DatasetId, DatasetStats, ProviderEvidenceCapabilities, QueryHistoryCapability,
    QueryHistoryProviderError, QueryHistoryRequest, QueryHistoryResult, QueryProvider, QueryResult,
    WarehouseNaming, WarehouseQueryHistoryProvider,
};

const DEFAULT_MAX_CONCURRENCY: usize = 15;
const MAX_CONCURRENCY_CAP: usize = 20;
const DEFAULT_DISCOVERY_CACHE_TTL_SECS: u64 = 120;

#[derive(Clone, Debug, Default)]
pub struct ClickHouseSettings {
    pub url: Option<String>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub max_concurrency: usize,
    pub discovery_cache_ttl_secs: u64,
}

#[derive(Clone)]
pub struct ClickHouseProvider {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    base_url: String,
    database: String,
    user: String,
    password: String,
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

impl ClickHouseProvider {
    pub fn from_settings(settings: ClickHouseSettings) -> Result<Self, String> {
        let base_url = settings
            .url
            .or_else(|| getenv_nonempty("CLICKHOUSE_URL"))
            .unwrap_or_else(|| "http://localhost:8123".to_string());
        let base_url = base_url.trim_end_matches('/').to_string();

        let database = settings
            .database
            .or_else(|| getenv_nonempty("CLICKHOUSE_DATABASE"))
            .unwrap_or_else(|| "default".to_string());

        let user = settings
            .user
            .or_else(|| getenv_nonempty("CLICKHOUSE_USER"))
            .unwrap_or_else(|| "default".to_string());

        let password = settings
            .password
            .or_else(|| getenv_nonempty("CLICKHOUSE_PASSWORD"))
            .unwrap_or_default();

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
            .map_err(|e| format!("clickhouse: failed to build HTTP client: {}", e))?;

        Ok(Self {
            inner: Arc::new(Inner {
                client,
                base_url,
                database,
                user,
                password,
                max_concurrency,
                limiter: Arc::new(Semaphore::new(max_concurrency)),
                cache_ttl: Duration::from_secs(ttl_secs),
                cache: RwLock::new(Cache::default()),
            }),
        })
    }

    /// Execute a SQL query via the ClickHouse HTTP interface.
    /// Appends `FORMAT TabSeparatedWithNames` to get header + rows as TSV.
    async fn execute_sql(&self, sql: &str) -> Result<QueryResult, String> {
        let _permit = self
            .inner
            .limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "clickhouse: query limiter closed".to_string())?;

        tracing::info!(target: "clickhouse", sql_len = sql.len(), "query_started");

        let full_sql = format!(
            "{} FORMAT TabSeparatedWithNames",
            sql.trim().trim_end_matches(';')
        );

        let mut req = self
            .inner
            .client
            .post(&self.inner.base_url)
            .header("X-ClickHouse-Database", &self.inner.database)
            .header("X-ClickHouse-User", &self.inner.user)
            .body(full_sql);

        if !self.inner.password.is_empty() {
            req = req.header("X-ClickHouse-Key", &self.inner.password);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("clickhouse: request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("clickhouse: HTTP {}: {}", status, text));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| format!("clickhouse: failed to read response: {}", e))?;

        let mut lines = body.lines();
        let header: Vec<String> = lines
            .next()
            .map(|h| h.split('\t').map(|s| s.to_string()).collect())
            .unwrap_or_default();

        let rows: Vec<Vec<String>> = lines
            .filter(|l| !l.is_empty())
            .map(|l| l.split('\t').map(|s| s.to_string()).collect())
            .collect();

        tracing::info!(
            target: "clickhouse",
            rows = rows.len(),
            cols = header.len(),
            "query_succeeded"
        );

        Ok(QueryResult {
            header,
            rows,
            meta: Some(serde_json::json!({"engine": "clickhouse"})),
        })
    }

    fn quote_ident_ch(ident: &str) -> String {
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
            "SELECT database, name \
             FROM system.tables \
             WHERE database = '{db}' \
             AND engine NOT IN ('View', 'MaterializedView', 'LiveView') \
             ORDER BY name",
            db = self.inner.database.replace('\'', "\\'"),
        );
        let qr = self.execute_sql(&sql).await?;
        let mut out: Vec<DatasetId> = Vec::new();
        for row in &qr.rows {
            if row.len() >= 2 && !row[0].trim().is_empty() && !row[1].trim().is_empty() {
                out.push(DatasetId {
                    catalog: row[0].clone(),
                    database: "default".to_string(),
                    table: row[1].clone(),
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

        let db = &dataset.catalog;
        let tbl = &dataset.table;
        let sql = format!(
            "SELECT name, type \
             FROM system.columns \
             WHERE database = '{db}' AND table = '{tbl}' \
             ORDER BY position",
            db = db.replace('\'', "\\'"),
            tbl = tbl.replace('\'', "\\'"),
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

impl WarehouseNaming for ClickHouseProvider {
    fn kind(&self) -> react_suite_data_engineer::de_config::WarehouseKind {
        react_suite_data_engineer::de_config::WarehouseKind::Clickhouse
    }

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
        let trimmed = dataset_fqn.trim();
        let parts: Vec<&str> = trimmed.split('.').collect();
        match parts.len() {
            1 => Ok(DatasetId {
                catalog: self.inner.database.clone(),
                database: "default".to_string(),
                table: parts[0].trim_matches('`').to_string(),
            }),
            2 => Ok(DatasetId {
                catalog: parts[0].trim_matches('`').to_string(),
                database: "default".to_string(),
                table: parts[1].trim_matches('`').to_string(),
            }),
            3 => Ok(DatasetId {
                catalog: parts[0].trim_matches('`').to_string(),
                database: parts[1].trim_matches('`').to_string(),
                table: parts[2].trim_matches('`').to_string(),
            }),
            _ => Err("clickhouse: dataset id must be <table>, <database>.<table>, or <database>.<schema>.<table>".to_string()),
        }
    }

    fn quote_ident(&self, ident: &str) -> String {
        Self::quote_ident_ch(ident)
    }

    fn quote_fqn(&self, id: &DatasetId) -> String {
        format!(
            "{}.{}",
            Self::quote_ident_ch(&id.catalog),
            Self::quote_ident_ch(&id.table)
        )
    }

    fn sql_prompt_rules(&self) -> Vec<&'static str> {
        vec![
            "ClickHouse SQL: Use backtick quoting for identifiers: `database`.`table`.",
            "ClickHouse SQL: ClickHouse is columnar; no transactions, no UPDATE/DELETE on MergeTree (use ALTER TABLE ... DELETE/UPDATE).",
            "ClickHouse SQL: Use toFloat64() or toFloat64OrNull() for numeric casts.",
            "ClickHouse SQL: Use toUnixTimestamp() for epoch conversion.",
            "ClickHouse SQL: Use count() instead of COUNT(1). countDistinct(col) for distinct counts.",
            "ClickHouse SQL: Use LIMIT N for row limiting.",
            "ClickHouse SQL: Discovery uses system.tables and system.columns, not information_schema.",
            "ClickHouse SQL: String functions: concat(), substring(), lower(), upper(), length().",
        ]
    }

    fn sql_remediation_rules(&self) -> Vec<&'static str> {
        vec![
            "ClickHouse rule: CAST(x AS FLOAT) is not valid. Use toFloat64(x) or toFloat64OrNull(x).",
            "ClickHouse rule: UPDATE/DELETE syntax differs. Use ALTER TABLE ... DELETE WHERE / UPDATE ... WHERE.",
            "ClickHouse rule: EXTRACT(EPOCH FROM ...) is not supported. Use toUnixTimestamp(col).",
            "ClickHouse rule: GROUP BY must include all non-aggregated columns (no implicit grouping).",
            "ClickHouse rule: No BOOLEAN type. Use UInt8 with 0/1.",
        ]
    }
}

#[async_trait]
impl QueryProvider for ClickHouseProvider {
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
impl DatasetCatalogProvider for ClickHouseProvider {
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

        let count_sql = format!("SELECT count() AS __cnt FROM {}", tbl);
        let qr_cnt = self.query(&count_sql).await?;
        let total_rows: u64 = qr_cnt
            .rows
            .first()
            .and_then(|r| r.first())
            .and_then(|s| parse_provider_u64(s))
            .unwrap_or(0);

        let mut ns_stats = DatasetFieldStats::new(&dataset.fqn());

        for (name, ty) in cols.into_iter().take(max_fields) {
            let expr = Self::quote_ident_ch(&name);
            let ty_lc = ty.trim().to_lowercase();
            let is_complex = ty_lc.starts_with("array")
                || ty_lc.starts_with("map")
                || ty_lc.starts_with("tuple")
                || ty_lc.starts_with("nested");
            let is_date = ty_lc.starts_with("date") || ty_lc.starts_with("datetime");

            let min_expr = if is_date {
                format!("toUnixTimestamp({})", expr)
            } else {
                format!("toFloat64OrNull(toString({}))", expr)
            };
            let max_expr = min_expr.clone();

            let sql = if is_complex {
                format!(
                    "SELECT \
                        count() AS __rows, \
                        countIf(isNull({c})) AS __nulls, \
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
                        count() AS __rows, \
                        countIf(isNull({c})) AS __nulls, \
                        uniq({c}) AS __distinct, \
                        min({min_e}) AS __min_num, \
                        max({max_e}) AS __max_num \
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
                        "clickhouse stats: dataset='{}' field='{}' type='{}' failed: {}",
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

#[async_trait]
impl WarehouseQueryHistoryProvider for ClickHouseProvider {
    fn query_history_capability(&self) -> QueryHistoryCapability {
        QueryHistoryCapability::Supported
    }

    async fn list_query_history(
        &self,
        request: &QueryHistoryRequest,
    ) -> Result<QueryHistoryResult, QueryHistoryProviderError> {
        let limit = request.bounded_limit(100, 500);
        let mut predicates = vec!["type = 'QueryFinish'".to_string()];
        if !request.include_non_select {
            predicates.push("lower(trim(query)) like 'select%'".to_string());
        }
        if let Some(since) = request
            .since
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            predicates.push(format!(
                "event_time >= parseDateTimeBestEffort({})",
                react_suite_data_engineer::providers::sql_literal(since)
            ));
        }
        let sql = format!(
            "select query_id, query as query_text, user as user_name, client_name as client_application, event_time as start_time, query_duration_ms, type as execution_status, exception as error_message \
             from system.query_log where {} order by event_time desc limit {}",
            predicates.join(" and "),
            limit
        );
        let result = self.execute_sql(&sql).await.map_err(|raw| {
            if react_suite_data_engineer::providers::lower_ascii_contains(&raw, "query_log") {
                QueryHistoryProviderError::requires_configuration(
                    "ClickHouse query history requires system.query_log to be enabled",
                    Some(raw),
                )
            } else {
                QueryHistoryProviderError::provider(
                    "ClickHouse query history lookup failed",
                    Some(raw),
                )
            }
        })?;
        Ok(react_suite_data_engineer::providers::supported_result(
            react_suite_data_engineer::de_config::WarehouseKind::Clickhouse,
            react_suite_data_engineer::providers::records_from_query_result(
                react_suite_data_engineer::de_config::WarehouseKind::Clickhouse,
                result,
            ),
        ))
    }
}

fn getenv_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .and_then(|v| if v.trim().is_empty() { None } else { Some(v) })
}
