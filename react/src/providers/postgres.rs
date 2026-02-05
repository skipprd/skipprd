use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Semaphore;

use react_core::providers::dataset_catalog_provider::{DatasetCatalogProvider, DatasetId};
use react_core::providers::warehouse::WarehouseNaming;
use react_core::providers::{QueryProvider, QueryResult, DEFAULT_WAREHOUSE_MAX_CONCURRENCY};

#[derive(Clone, Debug, Default)]
pub struct PostgresSettings {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub dbname: Option<String>,
    /// Default schema used when dataset ids are `<schema>.<table>` or `<table>`.
    pub default_schema: Option<String>,
    pub max_concurrency: usize,
}

#[derive(Clone)]
pub struct PostgresProvider {
    inner: Arc<Inner>,
}

struct Inner {
    settings: PostgresSettings,
    limiter: Arc<Semaphore>,
}

impl PostgresProvider {
    pub fn from_settings(settings: PostgresSettings) -> Self {
        let max = settings.max_concurrency.max(1).min(64);
        Self {
            inner: Arc::new(Inner {
                settings,
                limiter: Arc::new(Semaphore::new(max)),
            }),
        }
    }

    fn dbname(&self) -> Result<String, String> {
        self.inner
            .settings
            .dbname
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "postgres dbname is required (set providers.*.dbname or env PGDATABASE)".to_string()
            })
    }

    fn connect_cfg(&self) -> tokio_postgres::Config {
        let mut cfg = tokio_postgres::Config::new();
        if let Some(h) = self
            .inner
            .settings
            .host
            .as_ref()
            .filter(|s| !s.trim().is_empty())
        {
            cfg.host(h.trim());
        } else if let Ok(h) = std::env::var("PGHOST") {
            if !h.trim().is_empty() {
                cfg.host(h.trim());
            }
        }
        if let Some(p) = self.inner.settings.port {
            cfg.port(p);
        } else if let Some(p) = std::env::var("PGPORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
        {
            cfg.port(p);
        }
        if let Some(u) = self
            .inner
            .settings
            .user
            .as_ref()
            .filter(|s| !s.trim().is_empty())
        {
            cfg.user(u.trim());
        } else if let Ok(u) = std::env::var("PGUSER") {
            if !u.trim().is_empty() {
                cfg.user(u.trim());
            }
        }
        if let Some(pw) = self
            .inner
            .settings
            .password
            .as_ref()
            .filter(|s| !s.trim().is_empty())
        {
            cfg.password(pw.trim());
        } else if let Ok(pw) = std::env::var("PGPASSWORD") {
            if !pw.trim().is_empty() {
                cfg.password(pw.trim());
            }
        }
        if let Ok(db) = self.dbname() {
            cfg.dbname(&db);
        }
        cfg
    }

    async fn with_client<T>(
        &self,
        f: impl FnOnce(tokio_postgres::Client) -> futures::future::BoxFuture<'static, Result<T, String>>
            + Send,
    ) -> Result<T, String>
    where
        T: Send + 'static,
    {
        let _permit = self
            .inner
            .limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "postgres limiter closed".to_string())?;
        let cfg = self.connect_cfg();
        let (client, conn) = cfg
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        f(client).await
    }

    fn parse_dataset_id(&self, s: &str) -> Result<DatasetId, String> {
        let raw = s.trim().trim_matches('"').trim_matches('`');
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
                catalog: self.dbname()?,
                database: parts[0].to_string(),
                table: parts[1].to_string(),
            }),
            1 => {
                let schema = self
                    .inner
                    .settings
                    .default_schema
                    .clone()
                    .or_else(|| std::env::var("PGSCHEMA").ok())
                    .unwrap_or_else(|| "public".to_string());
                Ok(DatasetId {
                    catalog: self.dbname()?,
                    database: schema,
                    table: parts[0].to_string(),
                })
            }
            _ => Err(
                "dataset id must be <database>.<schema>.<table> (or <schema>.<table>)".to_string(),
            ),
        }
    }

    fn quote_ident_pg(ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

impl WarehouseNaming for PostgresProvider {
    fn kind(&self) -> &'static str {
        "postgres"
    }

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
        self.parse_dataset_id(dataset_fqn)
    }

    fn quote_ident(&self, ident: &str) -> String {
        Self::quote_ident_pg(ident)
    }

    fn quote_fqn(&self, id: &DatasetId) -> String {
        // Postgres does not use a 3-part object reference in SQL; `catalog` is connection-level.
        format!(
            "{}.{}",
            Self::quote_ident_pg(&id.database),
            Self::quote_ident_pg(&id.table)
        )
    }
}

#[async_trait]
impl QueryProvider for PostgresProvider {
    async fn query(&self, sql: &str) -> Result<QueryResult, String> {
        self.with_client(|c| {
            let sql = sql.to_string();
            Box::pin(async move {
                let rows = c.query(&sql, &[]).await.map_err(|e| e.to_string())?;
                let header: Vec<String> = rows
                    .get(0)
                    .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                    .unwrap_or_default();
                let mut out_rows: Vec<Vec<String>> = Vec::new();
                for r in rows.iter() {
                    let mut v: Vec<String> = Vec::new();
                    for (i, col) in r.columns().iter().enumerate() {
                        let _ = col; // reserved for future typing-specific coercions
                        let s: String = r
                            .try_get::<usize, String>(i)
                            .or_else(|_| r.try_get::<usize, &str>(i).map(|s| s.to_string()))
                            .unwrap_or_else(|_| {
                                // Best-effort: nulls and non-string types become empty string here.
                                // Suites treat QueryResult as display/debug only.
                                "".to_string()
                            });
                        v.push(s);
                    }
                    out_rows.push(v);
                }
                Ok(QueryResult {
                    header,
                    rows: out_rows,
                    meta: Some(serde_json::json!({"engine":"postgres"})),
                })
            })
        })
        .await
    }

    async fn schema(&self, dataset_fqn: &str) -> Result<Vec<(String, String)>, String> {
        let ds = self.parse_dataset_id(dataset_fqn)?;
        let schema = ds.database.clone();
        let table = ds.table.clone();
        self.with_client(|c| {
            Box::pin(async move {
                let sql = "select column_name, data_type from information_schema.columns where table_schema = $1 and table_name = $2 order by ordinal_position";
                let rows = c.query(sql, &[&schema, &table]).await.map_err(|e| e.to_string())?;
                let mut out: Vec<(String, String)> = Vec::new();
                for r in rows {
                    let n: String = r.try_get(0).unwrap_or_default();
                    let t: String = r.try_get(1).unwrap_or_default();
                    if !n.trim().is_empty() {
                        out.push((n, t));
                    }
                }
                Ok(out)
            })
        })
        .await
    }

    async fn sample(&self, dataset_fqn: &str, limit: usize) -> Result<Vec<Vec<String>>, String> {
        let ds = self.parse_dataset_id(dataset_fqn)?;
        let lim = limit.max(1).min(5000) as i64;
        let sql = format!("select * from {} limit {}", self.quote_fqn(&ds), lim);
        Ok(self.query(&sql).await?.rows)
    }

    fn max_concurrency(&self) -> usize {
        self.inner.settings.max_concurrency.max(1).min(64)
    }
}

#[async_trait]
impl DatasetCatalogProvider for PostgresProvider {
    async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
        let db = self.dbname()?;
        self.with_client(|c| {
            Box::pin(async move {
                let sql = "select table_schema, table_name from information_schema.tables where table_type='BASE TABLE' and table_schema not in ('pg_catalog','information_schema') order by table_schema, table_name";
                let rows = c.query(sql, &[]).await.map_err(|e| e.to_string())?;
                let mut out: Vec<DatasetId> = Vec::new();
                for r in rows {
                    let schema: String = r.try_get(0).unwrap_or_default();
                    let table: String = r.try_get(1).unwrap_or_default();
                    if !schema.trim().is_empty() && !table.trim().is_empty() {
                        out.push(DatasetId { catalog: db.clone(), database: schema, table });
                    }
                }
                Ok(out)
            })
        })
        .await
    }

    async fn get_dataset_schema(
        &self,
        dataset: &DatasetId,
    ) -> Result<Vec<(String, String)>, String> {
        self.schema(&dataset.fqn()).await
    }

    async fn get_dataset_stats(
        &self,
        _dataset: &DatasetId,
        _max_fields: usize,
    ) -> Result<
        (
            react_core::discover::stats::DatasetFieldStats,
            react_core::providers::catalog::types::DatasetStats,
        ),
        String,
    > {
        Err("postgres stats not implemented".to_string())
    }

    fn max_concurrency(&self) -> usize {
        self.inner.settings.max_concurrency.max(1).min(64)
    }
}

impl Default for PostgresProvider {
    fn default() -> Self {
        Self::from_settings(PostgresSettings {
            host: None,
            port: None,
            user: None,
            password: None,
            dbname: None,
            default_schema: None,
            max_concurrency: DEFAULT_WAREHOUSE_MAX_CONCURRENCY,
        })
    }
}
