use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tiberius::{AuthMethod, Client, Config, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use react_core::discover::stats::FieldStats;
use react_suite_data_engineer::providers::warehouse_utils;
use react_suite_data_engineer::providers::{
    DatasetCatalogProvider, DatasetFieldStats, DatasetId, DatasetStats, QueryProvider, QueryResult,
    WarehouseNaming,
};

const DEFAULT_MAX_CONCURRENCY: usize = 15;
const MAX_CONCURRENCY_CAP: usize = 20;
const DEFAULT_DISCOVERY_CACHE_TTL_SECS: u64 = 120;

#[derive(Clone, Debug, Default)]
pub struct SynapseSettings {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub max_concurrency: usize,
    pub discovery_cache_ttl_secs: u64,
    pub trust_cert: bool,
}

#[derive(Clone)]
pub struct SynapseProvider {
    inner: Arc<Inner>,
}

struct Inner {
    tiberius_config: Config,
    database: String,
    schema: String,
    max_concurrency: usize,
    limiter: Arc<Semaphore>,
    cache_ttl: Duration,
    cache: RwLock<Cache>,
}

#[derive(Default)]
struct Cache {
    schemas: Option<(Instant, Vec<String>)>,
    tables_by_schema: HashMap<String, (Instant, Vec<String>)>,
    columns_by_fqn: HashMap<String, (Instant, Vec<(String, String)>)>,
}

impl SynapseProvider {
    pub async fn from_settings(settings: SynapseSettings) -> Result<Self, String> {
        let host = settings
            .host
            .or_else(|| getenv_nonempty("SYNAPSE_HOST"))
            .ok_or("Synapse host is required (SYNAPSE_HOST)")?;
        let port = settings
            .port
            .or_else(|| getenv_nonempty("SYNAPSE_PORT").and_then(|v| v.parse::<u16>().ok()))
            .unwrap_or(1433);
        let user = settings
            .user
            .or_else(|| getenv_nonempty("SYNAPSE_USER"))
            .ok_or("Synapse user is required (SYNAPSE_USER)")?;
        let password = settings
            .password
            .or_else(|| getenv_nonempty("SYNAPSE_PASSWORD"))
            .ok_or("Synapse password is required (SYNAPSE_PASSWORD)")?;
        let database = settings
            .database
            .filter(|s| !s.trim().is_empty())
            .or_else(|| getenv_nonempty("SYNAPSE_DATABASE"))
            .ok_or("Synapse database is required (SYNAPSE_DATABASE)")?;
        let schema = settings
            .schema
            .filter(|s| !s.trim().is_empty())
            .or_else(|| getenv_nonempty("SYNAPSE_SCHEMA"))
            .unwrap_or_else(|| "dbo".to_string());

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

        let mut config = Config::new();
        config.host(&host);
        config.port(port);
        config.authentication(AuthMethod::sql_server(&user, &password));
        config.database(&database);

        let trust = settings.trust_cert
            || getenv_nonempty("SYNAPSE_TRUST_CERT")
                .map(|v| {
                    let vv = v.trim().to_lowercase();
                    vv == "1" || vv == "true" || vv == "yes"
                })
                .unwrap_or(false);
        if trust {
            config.trust_cert();
        }

        Ok(Self {
            inner: Arc::new(Inner {
                tiberius_config: config,
                database,
                schema,
                max_concurrency,
                limiter: Arc::new(Semaphore::new(max_concurrency)),
                cache_ttl: Duration::from_secs(ttl_secs),
                cache: RwLock::new(Cache::default()),
            }),
        })
    }

    async fn connect(&self) -> Result<Client<tokio_util::compat::Compat<TcpStream>>, String> {
        let config = self.inner.tiberius_config.clone();
        let addr = config.get_addr().to_string();
        let tcp = TcpStream::connect(&addr)
            .await
            .map_err(|e| format!("synapse: TCP connect to {} failed: {}", addr, e))?;
        tcp.set_nodelay(true)
            .map_err(|e| format!("synapse: set_nodelay failed: {}", e))?;
        let client = Client::connect(config, tcp.compat_write())
            .await
            .map_err(|e| format!("synapse: TDS connect failed: {}", e))?;
        Ok(client)
    }

    async fn run_query(&self, sql: &str) -> Result<QueryResult, String> {
        let _permit = self
            .inner
            .limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "synapse: query limiter closed".to_string())?;

        let mut client = self.connect().await?;
        tracing::info!(target: "synapse", sql_len = sql.len(), "query_started");

        let stream = client
            .query(sql, &[])
            .await
            .map_err(|e| format!("synapse: query failed: {}", e))?;

        let mut header: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();

        let result_set: Vec<QueryItem> = futures::TryStreamExt::try_collect(stream)
            .await
            .map_err(|e| format!("synapse: result stream failed: {}", e))?;

        for item in result_set {
            match item {
                QueryItem::Metadata(meta) => {
                    if header.is_empty() {
                        header = meta
                            .columns()
                            .iter()
                            .map(|c| c.name().to_string())
                            .collect();
                    }
                }
                QueryItem::Row(row) => {
                    let mut out_row: Vec<String> = Vec::new();
                    for i in 0..row.len() {
                        let val: Option<&str> = row.try_get(i).ok().flatten();
                        out_row.push(val.unwrap_or("").to_string());
                    }
                    rows.push(out_row);
                }
            }
        }

        tracing::info!(
            target: "synapse",
            rows = rows.len(),
            cols = header.len(),
            "query_succeeded"
        );

        Ok(QueryResult {
            header,
            rows,
            meta: Some(serde_json::json!({"engine": "synapse"})),
        })
    }

    fn quote_ident_syn(ident: &str) -> String {
        format!("[{}]", ident.replace(']', "]]"))
    }

    fn quote_table(database: &str, schema: &str, table: &str) -> String {
        format!(
            "{}.{}.{}",
            Self::quote_ident_syn(database),
            Self::quote_ident_syn(schema),
            Self::quote_ident_syn(table)
        )
    }

    async fn cached_schemas(&self) -> Result<Vec<String>, String> {
        let default = &self.inner.schema;
        if !default.trim().is_empty() && default != "dbo" {
            return Ok(vec![default.clone()]);
        }
        {
            let cache = self.inner.cache.read().await;
            if let Some((ts, v)) = cache.schemas.as_ref() {
                if ts.elapsed() < self.inner.cache_ttl {
                    return Ok(v.clone());
                }
            }
        }

        let sql = format!(
            "SELECT SCHEMA_NAME \
             FROM {db}.INFORMATION_SCHEMA.SCHEMATA \
             WHERE SCHEMA_NAME NOT IN ('INFORMATION_SCHEMA', 'sys', 'guest') \
             ORDER BY SCHEMA_NAME",
            db = Self::quote_ident_syn(&self.inner.database),
        );
        let qr = self.run_query(&sql).await?;
        let mut out: Vec<String> = qr
            .rows
            .iter()
            .filter_map(|r| r.first().cloned().filter(|s| !s.trim().is_empty()))
            .collect();
        out.sort();
        out.dedup();

        let mut cache = self.inner.cache.write().await;
        cache.schemas = Some((Instant::now(), out.clone()));
        Ok(out)
    }

    async fn cached_tables(&self, schema: &str) -> Result<Vec<String>, String> {
        {
            let cache = self.inner.cache.read().await;
            if let Some((ts, v)) = cache.tables_by_schema.get(schema) {
                if ts.elapsed() < self.inner.cache_ttl {
                    return Ok(v.clone());
                }
            }
        }

        let sql = format!(
            "SELECT TABLE_NAME \
             FROM {db}.INFORMATION_SCHEMA.TABLES \
             WHERE TABLE_SCHEMA = '{schema}' \
             AND TABLE_TYPE IN ('BASE TABLE', 'VIEW') \
             ORDER BY TABLE_NAME",
            db = Self::quote_ident_syn(&self.inner.database),
            schema = schema.replace('\'', "''"),
        );
        let qr = self.run_query(&sql).await?;
        let mut out: Vec<String> = qr
            .rows
            .iter()
            .filter_map(|r| r.first().cloned().filter(|s| !s.trim().is_empty()))
            .collect();
        out.sort();
        out.dedup();

        let mut cache = self.inner.cache.write().await;
        cache
            .tables_by_schema
            .insert(schema.to_string(), (Instant::now(), out.clone()));
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
            "SELECT COLUMN_NAME, DATA_TYPE \
             FROM {db}.INFORMATION_SCHEMA.COLUMNS \
             WHERE TABLE_SCHEMA = '{schema}' AND TABLE_NAME = '{table}' \
             ORDER BY ORDINAL_POSITION",
            db = Self::quote_ident_syn(&dataset.catalog),
            schema = dataset.database.replace('\'', "''"),
            table = dataset.table.replace('\'', "''"),
        );
        let qr = self.run_query(&sql).await?;
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

impl WarehouseNaming for SynapseProvider {
    fn kind(&self) -> react_suite_data_engineer::de_config::WarehouseKind {
        react_suite_data_engineer::de_config::WarehouseKind::Synapse
    }

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
        warehouse_utils::parse_fqn_common(
            dataset_fqn,
            &self.inner.database,
            Some(&self.inner.schema),
        )
    }

    fn quote_ident(&self, ident: &str) -> String {
        Self::quote_ident_syn(ident)
    }

    fn quote_fqn(&self, id: &DatasetId) -> String {
        Self::quote_table(&id.catalog, &id.database, &id.table)
    }

    fn sql_prompt_rules(&self) -> Vec<&'static str> {
        vec![
            "Synapse T-SQL: Use TOP N instead of LIMIT N. Place TOP immediately after SELECT.",
            "Synapse T-SQL: Use square bracket quoting for identifiers: [schema].[table].[column].",
            "Synapse T-SQL: Use GETDATE() instead of NOW() or CURRENT_TIMESTAMP.",
            "Synapse T-SQL: TRUNCATE TABLE may not be available in serverless SQL pools.",
            "Synapse T-SQL: Use CTAS (CREATE TABLE AS SELECT) for large data movements.",
            "Synapse T-SQL: Dedicated pools require DISTRIBUTION hints (HASH, ROUND_ROBIN, REPLICATE).",
            "Synapse T-SQL: Use TRY_CAST(expr AS type) for safe conversions.",
        ]
    }

    fn sql_remediation_rules(&self) -> Vec<&'static str> {
        vec![
            "Synapse rule: LIMIT is not supported. Replace LIMIT N with TOP N after SELECT.",
            "Synapse rule: The || operator is logical OR, not string concatenation. Use + or CONCAT().",
            "Synapse rule: NOW() is not valid. Use GETDATE() or SYSDATETIME().",
            "Synapse rule: Boolean type does not exist. Use BIT (0/1) instead of TRUE/FALSE.",
            "Synapse rule: Some DDL (CREATE INDEX, ALTER TABLE) differs between dedicated and serverless pools.",
        ]
    }
}

#[async_trait]
impl QueryProvider for SynapseProvider {
    async fn query(&self, sql: &str) -> Result<QueryResult, String> {
        self.run_query(sql).await
    }

    async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
        let ds = self.parse_dataset_fqn(dataset_fqn)?;
        self.cached_columns(&ds).await
    }

    async fn sample(&self, dataset_fqn: &str, limit: usize) -> Result<Vec<Vec<String>>, String> {
        let ds = self.parse_dataset_fqn(dataset_fqn)?;
        let lim = limit.max(1).min(5000);
        let tbl = Self::quote_table(&ds.catalog, &ds.database, &ds.table);
        let sql = format!("SELECT TOP {} * FROM {}", lim, tbl);
        let qr = self.query(&sql).await?;
        Ok(qr.rows)
    }

    fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }
}

#[async_trait]
impl DatasetCatalogProvider for SynapseProvider {
    async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
        let schemas = self.cached_schemas().await?;
        let mut out: Vec<DatasetId> = Vec::new();
        for schema in schemas {
            let tables = self
                .cached_tables(&schema)
                .await
                .map_err(|e| format!("synapse: discovery failed for schema '{}': {}", schema, e))?;
            for t in tables {
                out.push(DatasetId {
                    catalog: self.inner.database.clone(),
                    database: schema.clone(),
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
        self.cached_columns(dataset).await
    }

    async fn get_dataset_stats(
        &self,
        dataset: &DatasetId,
        max_fields: usize,
    ) -> Result<(DatasetFieldStats, DatasetStats), String> {
        let cols = self.cached_columns(dataset).await?;
        let max_fields = max_fields.max(1).min(500);
        let tbl = Self::quote_table(&dataset.catalog, &dataset.database, &dataset.table);

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
            let expr = Self::quote_ident_syn(&name);
            let ty_lc = ty.trim().to_lowercase();
            let is_complex = ty_lc == "xml" || ty_lc == "geography" || ty_lc == "geometry";
            let is_timestamp = ty_lc.contains("datetime") || ty_lc == "smalldatetime";
            let is_date = ty_lc == "date";

            let min_expr = if is_timestamp || is_date {
                format!("CAST(DATEDIFF(SECOND, '19700101', {}) AS FLOAT)", expr)
            } else {
                format!("TRY_CAST({} AS FLOAT)", expr)
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
                        "synapse stats: dataset='{}' field='{}' type='{}' failed: {}",
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
