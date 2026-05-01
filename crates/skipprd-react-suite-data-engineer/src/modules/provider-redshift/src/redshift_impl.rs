use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};

use aws_sdk_redshiftdata::types::StatusString;

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
pub struct RedshiftSettings {
    pub database: Option<String>,
    pub cluster_identifier: Option<String>,
    pub workgroup_name: Option<String>,
    pub db_user: Option<String>,
    pub schema: Option<String>,
    pub region: Option<String>,
    pub max_concurrency: usize,
    pub discovery_cache_ttl_secs: u64,
}

#[derive(Clone)]
pub struct RedshiftProvider {
    inner: Arc<Inner>,
}

struct Inner {
    client: aws_sdk_redshiftdata::Client,
    database: String,
    cluster_identifier: Option<String>,
    workgroup_name: Option<String>,
    db_user: Option<String>,
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

impl RedshiftProvider {
    pub async fn from_settings(settings: RedshiftSettings) -> Result<Self, String> {
        let database = settings
            .database
            .filter(|s| !s.trim().is_empty())
            .or_else(|| getenv_nonempty("REDSHIFT_DATABASE"))
            .ok_or("Redshift database is required (REDSHIFT_DATABASE)")?;

        let cluster_identifier = settings
            .cluster_identifier
            .or_else(|| getenv_nonempty("REDSHIFT_CLUSTER_IDENTIFIER"));
        let workgroup_name = settings
            .workgroup_name
            .or_else(|| getenv_nonempty("REDSHIFT_WORKGROUP_NAME"));

        if cluster_identifier.is_none() && workgroup_name.is_none() {
            return Err(
                "Redshift requires either REDSHIFT_CLUSTER_IDENTIFIER or REDSHIFT_WORKGROUP_NAME"
                    .to_string(),
            );
        }

        let db_user = settings
            .db_user
            .or_else(|| getenv_nonempty("REDSHIFT_DB_USER"));

        let schema = settings
            .schema
            .filter(|s| !s.trim().is_empty())
            .or_else(|| getenv_nonempty("REDSHIFT_SCHEMA"))
            .unwrap_or_else(|| "public".to_string());

        if let Some(region) = &settings.region {
            if !region.trim().is_empty() {
                if std::env::var("AWS_DEFAULT_REGION")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .is_none()
                {
                    std::env::set_var("AWS_DEFAULT_REGION", region.trim());
                }
                if std::env::var("AWS_REGION")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .is_none()
                {
                    std::env::set_var("AWS_REGION", region.trim());
                }
            }
        }

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

        let aws_cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = aws_sdk_redshiftdata::Client::new(&aws_cfg);

        Ok(Self {
            inner: Arc::new(Inner {
                client,
                database,
                cluster_identifier,
                workgroup_name,
                db_user,
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
            .map_err(|_| "redshift: query limiter closed".to_string())?;

        tracing::info!(target: "redshift", sql_len = sql.len(), "query_started");

        let mut req = self
            .inner
            .client
            .execute_statement()
            .database(&self.inner.database)
            .sql(sql);

        if let Some(cluster) = &self.inner.cluster_identifier {
            req = req.cluster_identifier(cluster);
            if let Some(user) = &self.inner.db_user {
                req = req.db_user(user);
            }
        } else if let Some(wg) = &self.inner.workgroup_name {
            req = req.workgroup_name(wg);
        }

        let exec_output = req
            .send()
            .await
            .map_err(|e| format!("redshift: execute_statement failed: {}", e))?;

        let stmt_id = exec_output
            .id()
            .ok_or("redshift: no statement id returned")?
            .to_string();

        self.wait_for_statement(&stmt_id).await?;
        self.fetch_results(&stmt_id).await
    }

    async fn wait_for_statement(&self, statement_id: &str) -> Result<(), String> {
        let start = Instant::now();
        loop {
            if start.elapsed() > MAX_POLL_DURATION {
                return Err(format!(
                    "redshift: statement {} timed out after {:?}",
                    statement_id, MAX_POLL_DURATION
                ));
            }

            let desc = self
                .inner
                .client
                .describe_statement()
                .id(statement_id)
                .send()
                .await
                .map_err(|e| format!("redshift: describe_statement failed: {}", e))?;

            match desc.status() {
                Some(StatusString::Finished) => return Ok(()),
                Some(StatusString::Failed) | Some(StatusString::Aborted) => {
                    let err = desc.error().unwrap_or("unknown error");
                    return Err(format!("redshift: query failed: {}", err));
                }
                _ => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    async fn fetch_results(&self, statement_id: &str) -> Result<QueryResult, String> {
        let mut header: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut next_token: Option<String> = None;

        loop {
            let mut req = self.inner.client.get_statement_result().id(statement_id);

            if let Some(token) = &next_token {
                req = req.next_token(token);
            }

            let result = req
                .send()
                .await
                .map_err(|e| format!("redshift: get_statement_result failed: {}", e))?;

            if header.is_empty() {
                let col_meta = result.column_metadata();
                if !col_meta.is_empty() {
                    header = col_meta
                        .iter()
                        .map(|c| c.name().unwrap_or("").to_string())
                        .collect();
                }
            }

            let records = result.records();
            for record in records {
                let row: Vec<String> = record
                    .iter()
                    .map(|field| {
                        if field.as_is_null().is_ok() {
                            String::new()
                        } else if let Ok(s) = field.as_string_value() {
                            s.to_string()
                        } else if let Ok(l) = field.as_long_value() {
                            l.to_string()
                        } else if let Ok(d) = field.as_double_value() {
                            d.to_string()
                        } else if let Ok(b) = field.as_boolean_value() {
                            b.to_string()
                        } else if let Ok(blob) = field.as_blob_value() {
                            format!("<blob {} bytes>", blob.as_ref().len())
                        } else {
                            String::new()
                        }
                    })
                    .collect();
                rows.push(row);
            }

            next_token = result.next_token().map(|s| s.to_string());
            if next_token.is_none() {
                break;
            }
        }

        tracing::info!(
            target: "redshift",
            rows = rows.len(),
            cols = header.len(),
            "query_succeeded"
        );

        Ok(QueryResult {
            header,
            rows,
            meta: Some(serde_json::json!({"engine": "redshift"})),
        })
    }

    fn quote_ident_rs(ident: &str) -> String {
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
            "SELECT table_schema, table_name \
             FROM information_schema.tables \
             WHERE table_schema = '{schema}' \
             AND table_type IN ('BASE TABLE', 'VIEW') \
             ORDER BY table_schema, table_name",
            schema = self.inner.schema.replace('\'', "''"),
        );
        let qr = self.execute_sql(&sql).await?;
        let mut out: Vec<DatasetId> = Vec::new();
        for row in &qr.rows {
            if row.len() >= 2 && !row[0].trim().is_empty() && !row[1].trim().is_empty() {
                out.push(DatasetId {
                    catalog: self.inner.database.clone(),
                    database: row[0].clone(),
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

impl WarehouseNaming for RedshiftProvider {
    fn kind(&self) -> react_suite_data_engineer::de_config::WarehouseKind {
        react_suite_data_engineer::de_config::WarehouseKind::Redshift
    }

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
        warehouse_utils::parse_fqn_common(
            dataset_fqn,
            &self.inner.database,
            Some(&self.inner.schema),
        )
    }

    fn quote_ident(&self, ident: &str) -> String {
        Self::quote_ident_rs(ident)
    }

    fn quote_fqn(&self, id: &DatasetId) -> String {
        format!(
            "{}.{}",
            Self::quote_ident_rs(&id.database),
            Self::quote_ident_rs(&id.table)
        )
    }

    fn sql_prompt_rules(&self) -> Vec<&'static str> {
        vec![
            "Redshift SQL: PostgreSQL-compatible with extensions (DISTKEY, SORTKEY, ENCODE).",
            "Redshift SQL: Use double-quote quoting for identifiers: \"schema\".\"table\".",
            "Redshift SQL: Use LIMIT N for row limiting.",
            "Redshift SQL: Use GETDATE() or SYSDATE for current timestamp.",
            "Redshift SQL: EXTRACT(EPOCH FROM col) for Unix timestamps.",
            "Redshift SQL: Use NVL(expr, default) or COALESCE(expr, default) for null handling.",
            "Redshift SQL: No support for LATERAL joins in some query forms.",
        ]
    }

    fn sql_remediation_rules(&self) -> Vec<&'static str> {
        vec![
            "Redshift rule: SAFE_CAST is not supported. Use CAST(expr AS type) with error handling.",
            "Redshift rule: ARRAY and MAP types are not natively supported in standard Redshift.",
            "Redshift rule: Use NVL() or COALESCE() instead of IFNULL().",
            "Redshift rule: NOW() returns the transaction start time. Use GETDATE() for current time.",
        ]
    }
}

#[async_trait]
impl QueryProvider for RedshiftProvider {
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
impl DatasetCatalogProvider for RedshiftProvider {
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
            let expr = Self::quote_ident_rs(&name);
            let ty_lc = ty.trim().to_lowercase();
            let is_complex = ty_lc == "super";
            let is_timestamp = ty_lc.contains("timestamp");
            let is_date = ty_lc == "date";

            let min_expr = if is_timestamp || is_date {
                format!("EXTRACT(EPOCH FROM {})", expr)
            } else {
                format!("CAST({} AS FLOAT)", expr)
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
                        "redshift stats: dataset='{}' field='{}' type='{}' failed: {}",
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
