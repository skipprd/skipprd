use async_trait::async_trait;
use snowflake_connector_rs::{
    SnowflakeAuthMethod, SnowflakeClient, SnowflakeClientConfig, SnowflakeSession,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, Semaphore};

use react_core::discover::stats::FieldStats;
use react_suite_data_engineer::providers::warehouse_utils;
use react_suite_data_engineer::providers::{
    finalize_provider_field_stats, parse_provider_u64, DatasetCatalogProvider, DatasetFieldStats,
    DatasetId, DatasetStats, ProviderEvidenceCapabilities, QueryProvider, QueryResult,
    WarehouseNaming,
};

const DEFAULT_SNOWFLAKE_MAX_CONCURRENCY: usize = 15;
const SNOWFLAKE_MAX_CONCURRENCY_CAP: usize = 20;
const DEFAULT_SNOWFLAKE_DISCOVERY_CACHE_TTL_SECS: u64 = 120;

#[derive(Clone, Debug, Default)]
pub struct SnowflakeSettings {
    pub account: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    /// PEM-encoded private key for key-pair auth (bypasses MFA).
    pub private_key_pem: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub warehouse: Option<String>,
    pub role: Option<String>,
    pub max_concurrency: usize,
    pub discovery_cache_ttl_secs: u64,
}

#[derive(Clone)]
pub struct SnowflakeProvider {
    inner: Arc<Inner>,
}

struct Inner {
    session: Mutex<Option<SnowflakeSession>>,
    client: SnowflakeClient,
    database: String,
    schema: Option<String>,
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

impl SnowflakeProvider {
    pub async fn from_settings(settings: SnowflakeSettings) -> Result<Self, String> {
        let account = settings
            .account
            .or_else(|| getenv_nonempty("SNOWFLAKE_ACCOUNT"))
            .ok_or_else(|| {
                "Snowflake account is required (providers.warehouse.account or SNOWFLAKE_ACCOUNT)"
                    .to_string()
            })?;
        let user = settings
            .user
            .or_else(|| getenv_nonempty("SNOWFLAKE_USER"))
            .ok_or_else(|| {
                "Snowflake user is required (providers.warehouse.user or SNOWFLAKE_USER)"
                    .to_string()
            })?;
        let private_key_pem = settings.private_key_pem;
        let password = settings
            .password
            .or_else(|| getenv_nonempty("SNOWFLAKE_PASSWORD"));
        let database = settings
            .database
            .filter(|s| !s.trim().is_empty())
            .or_else(|| getenv_nonempty("SNOWFLAKE_DATABASE"))
            .ok_or_else(|| {
                "Snowflake database is required (providers.warehouse.database or SNOWFLAKE_DATABASE)"
                    .to_string()
            })?;
        let schema = settings
            .schema
            .filter(|s| !s.trim().is_empty())
            .or_else(|| getenv_nonempty("SNOWFLAKE_SCHEMA"));
        let warehouse = settings
            .warehouse
            .or_else(|| getenv_nonempty("SNOWFLAKE_WAREHOUSE"));
        let role = settings.role.or_else(|| getenv_nonempty("SNOWFLAKE_ROLE"));

        let max_concurrency = warehouse_utils::clamp_concurrency(
            if settings.max_concurrency == 0 {
                DEFAULT_SNOWFLAKE_MAX_CONCURRENCY
            } else {
                settings.max_concurrency
            },
            SNOWFLAKE_MAX_CONCURRENCY_CAP,
        );
        let ttl_secs =
            warehouse_utils::clamp_cache_ttl_secs(if settings.discovery_cache_ttl_secs == 0 {
                DEFAULT_SNOWFLAKE_DISCOVERY_CACHE_TTL_SECS
            } else {
                settings.discovery_cache_ttl_secs
            });

        let auth_method = if let Some(pem) = private_key_pem {
            tracing::info!(target: "snowflake", "using key-pair authentication");
            SnowflakeAuthMethod::KeyPairUnencrypted { pem }
        } else if let Some(pw) = password {
            SnowflakeAuthMethod::Password(pw)
        } else {
            return Err(
                "Snowflake auth requires either SNOWFLAKE_PRIVATE_KEY_PATH (key-pair) or \
                 SNOWFLAKE_PASSWORD (password). Key-pair auth is recommended when MFA is \
                 enabled on the account."
                    .to_string(),
            );
        };

        let client = SnowflakeClient::new(
            &user,
            auth_method,
            SnowflakeClientConfig {
                account,
                role,
                warehouse,
                database: Some(database.clone()),
                schema: schema.clone(),
                timeout: Some(Duration::from_secs(600)),
            },
        )
        .map_err(|e| format!("Snowflake client init failed: {}", e))?;

        Ok(Self {
            inner: Arc::new(Inner {
                session: Mutex::new(None),
                client,
                database,
                schema,
                max_concurrency,
                limiter: Arc::new(Semaphore::new(max_concurrency)),
                cache_ttl: Duration::from_secs(ttl_secs),
                cache: RwLock::new(Cache::default()),
            }),
        })
    }

    async fn ensure_session(&self) -> Result<(), String> {
        let mut guard = self.inner.session.lock().await;
        if guard.is_none() {
            let session = self
                .inner
                .client
                .create_session()
                .await
                .map_err(|e| format!("Snowflake session creation failed: {}", e))?;
            *guard = Some(session);
        }
        Ok(())
    }

    async fn run_query(&self, sql: &str) -> Result<QueryResult, String> {
        let _permit = self
            .inner
            .limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "snowflake query limiter closed".to_string())?;

        self.ensure_session().await?;
        tracing::info!(target: "snowflake", sql_len = sql.len(), "query_started");

        let first_attempt = {
            let guard = self.inner.session.lock().await;
            let session = guard.as_ref().expect("session ensured above");
            session
                .query(sql)
                .await
                .map_err(|e| format!("Snowflake query failed: {}", e))
        };
        let rows = match first_attempt {
            Ok(rows) => rows,
            Err(err) if Self::is_session_expired_error(&err) => {
                tracing::warn!(
                    target: "snowflake",
                    error = %err,
                    "snowflake session expired; recreating session and retrying query once"
                );
                {
                    let mut guard = self.inner.session.lock().await;
                    *guard = None;
                }
                self.ensure_session().await?;
                let guard = self.inner.session.lock().await;
                let session = guard.as_ref().expect("session recreated above");
                session
                    .query(sql)
                    .await
                    .map_err(|e| format!("Snowflake query failed after session refresh: {}", e))?
            }
            Err(err) => return Err(err),
        };

        let header: Vec<String> = if let Some(first) = rows.first() {
            first
                .column_names()
                .into_iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            Vec::new()
        };

        let out_rows: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                header
                    .iter()
                    .enumerate()
                    .map(|(idx, _)| row.at::<String>(idx).unwrap_or_default())
                    .collect()
            })
            .collect();

        tracing::info!(
            target: "snowflake",
            rows = out_rows.len(),
            cols = header.len(),
            "query_succeeded"
        );

        Ok(QueryResult {
            header,
            rows: out_rows,
            meta: Some(serde_json::json!({"engine": "snowflake"})),
        })
    }

    fn is_session_expired_error(msg: &str) -> bool {
        msg.to_ascii_lowercase().contains("session expired")
    }

    fn quote_ident_sf(ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    fn quote_table(database: &str, schema: &str, table: &str) -> String {
        format!(
            "{}.{}.{}",
            Self::quote_ident_sf(database),
            Self::quote_ident_sf(schema),
            Self::quote_ident_sf(table)
        )
    }

    fn dbt_model_relation(database: &str, schema: &str, table: &str) -> String {
        format!("{}.{}.{}", database, schema, table)
    }

    fn dbt_model_lookup_id(id: &DatasetId) -> DatasetId {
        DatasetId {
            catalog: id.catalog.to_ascii_uppercase(),
            database: id.database.to_ascii_uppercase(),
            table: id.table.to_ascii_uppercase(),
        }
    }

    async fn cached_schemas(&self) -> Result<Vec<String>, String> {
        if let Some(schema) = self.inner.schema.as_ref().filter(|s| !s.trim().is_empty()) {
            return Ok(vec![schema.trim().to_string()]);
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
            "SELECT SCHEMA_NAME FROM {}.INFORMATION_SCHEMA.SCHEMATA \
             WHERE SCHEMA_NAME NOT IN ('INFORMATION_SCHEMA') \
             ORDER BY SCHEMA_NAME",
            Self::quote_ident_sf(&self.inner.database)
        );
        let qr = self.run_query(&sql).await?;
        let mut out: Vec<String> = qr
            .rows
            .iter()
            .filter_map(|r| r.first().cloned().filter(|s| !s.trim().is_empty()))
            .filter(|name| !is_skippr_internal_table(name))
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
            "SELECT TABLE_NAME FROM {db}.INFORMATION_SCHEMA.TABLES \
             WHERE TABLE_SCHEMA = '{schema}' \
             AND TABLE_TYPE IN ('BASE TABLE', 'VIEW') \
             ORDER BY TABLE_NAME",
            db = Self::quote_ident_sf(&self.inner.database),
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
            db = Self::quote_ident_sf(&dataset.catalog),
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

impl WarehouseNaming for SnowflakeProvider {
    fn kind(&self) -> react_suite_data_engineer::de_config::WarehouseKind {
        react_suite_data_engineer::de_config::WarehouseKind::Snowflake
    }

    fn parse_dataset_fqn(&self, dataset_fqn: &str) -> Result<DatasetId, String> {
        warehouse_utils::parse_fqn_common(
            dataset_fqn,
            &self.inner.database,
            self.inner.schema.as_deref(),
        )
    }

    fn quote_ident(&self, ident: &str) -> String {
        Self::quote_ident_sf(ident)
    }

    fn quote_fqn(&self, id: &DatasetId) -> String {
        Self::quote_table(&id.catalog, &id.database, &id.table)
    }

    fn format_dbt_model_relation_fqn(&self, id: &DatasetId) -> String {
        Self::dbt_model_relation(&id.catalog, &id.database, &id.table)
    }

    fn dbt_model_relation_lookup_id(&self, id: &DatasetId) -> DatasetId {
        Self::dbt_model_lookup_id(id)
    }

    fn sql_prompt_rules(&self) -> Vec<&'static str> {
        vec![
            "Snowflake SQL: Use VARIANT, OBJECT, and ARRAY types for semi-structured data. Access nested fields with colon notation (e.g. col:field::STRING).",
            "Snowflake SQL: Use FLATTEN() to unnest arrays or objects. LATERAL FLATTEN is the standard pattern.",
            "Snowflake SQL: Use QUALIFY with window functions instead of wrapping in a subquery (e.g. QUALIFY ROW_NUMBER() OVER (...) = 1).",
            "Snowflake SQL: Identifiers are case-insensitive unless double-quoted. Avoid double-quoting unless necessary.",
            "Snowflake SQL: Use TRY_CAST() for safe type conversions (not SAFE_CAST).",
            "Snowflake SQL: Use DATE_TRUNC('part', expr) not DATE_TRUNC(part, expr) -- the part must be a string literal.",
        ]
    }

    fn sql_remediation_rules(&self) -> Vec<&'static str> {
        vec![
            "Snowflake rule: SAFE_CAST is not a valid function. Use TRY_CAST(...) instead.",
            "Snowflake rule: try_to_timestamp() is TRY_TO_TIMESTAMP() in Snowflake. Verify the function name and argument order.",
            "Snowflake rule: String concatenation uses || operator. The + operator is for numeric addition only.",
            "Snowflake rule: LIMIT is supported (not TOP). Use LIMIT N at the end of the query.",
        ]
    }
}

#[async_trait]
impl QueryProvider for SnowflakeProvider {
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
        let sql = format!("SELECT * FROM {} LIMIT {}", tbl, lim);
        let qr = self.query(&sql).await?;
        Ok(qr.rows)
    }

    fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }
}

#[async_trait]
impl DatasetCatalogProvider for SnowflakeProvider {
    async fn list_datasets(&self) -> Result<Vec<DatasetId>, String> {
        let schemas = self.cached_schemas().await?;
        let mut out: Vec<DatasetId> = Vec::new();
        for schema in schemas {
            let tables = self.cached_tables(&schema).await.map_err(|e| {
                format!("snowflake discovery failed for schema '{}': {}", schema, e)
            })?;
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
            .and_then(|s| parse_provider_u64(s))
            .unwrap_or(0);

        let mut ns_stats = DatasetFieldStats::new(&dataset.fqn());

        for (name, ty) in cols.into_iter().take(max_fields) {
            let expr = format!("\"{}\"", name.replace('"', "\"\""));
            let ty_lc = ty.trim().to_lowercase();
            let is_complex = ty_lc.starts_with("variant")
                || ty_lc.starts_with("object")
                || ty_lc.starts_with("array");
            let is_binary = ty_lc.starts_with("binary") || ty_lc.starts_with("varbinary");
            let is_timestamp = ty_lc.contains("timestamp");
            let is_date = ty_lc == "date";
            let is_numeric = ty_lc.starts_with("number")
                || ty_lc.starts_with("decimal")
                || ty_lc.starts_with("numeric")
                || matches!(
                    ty_lc.as_str(),
                    "int"
                        | "integer"
                        | "bigint"
                        | "smallint"
                        | "tinyint"
                        | "byteint"
                        | "float"
                        | "float4"
                        | "float8"
                        | "double"
                        | "double precision"
                        | "real"
                );

            let min_expr = if is_timestamp {
                format!("EXTRACT(EPOCH FROM {})", expr)
            } else if is_date {
                format!("EXTRACT(EPOCH FROM {}::TIMESTAMP)", expr)
            } else if is_numeric {
                expr.clone()
            } else if is_binary {
                "NULL".to_string()
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
                        "snowflake stats: dataset='{}' field='{}' type='{}' failed: {}",
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

fn is_skippr_internal_table(table: &str) -> bool {
    table.trim().to_ascii_lowercase().starts_with("_skippr_")
}

#[cfg(test)]
mod tests {
    use super::{is_skippr_internal_table, DatasetId, SnowflakeProvider};

    #[test]
    fn skippr_internal_tables_are_excluded_from_discovery() {
        assert!(is_skippr_internal_table("_skippr_tombstones_orders"));
        assert!(is_skippr_internal_table("_SKIPPR_TOMBSTONES_ORDERS"));
        assert!(!is_skippr_internal_table("TRIP_START"));
        assert!(!is_skippr_internal_table("orders"));
    }

    #[test]
    fn dbt_model_relation_uses_unquoted_manifest_style() {
        let id = DatasetId {
            catalog: "ANALYTICS".to_string(),
            database: "target_schema_silver".to_string(),
            table: "stg_example".to_string(),
        };

        assert_eq!(
            SnowflakeProvider::dbt_model_relation(&id.catalog, &id.database, &id.table),
            "ANALYTICS.target_schema_silver.stg_example"
        );
        assert_eq!(
            SnowflakeProvider::quote_table(&id.catalog, &id.database, &id.table),
            "\"ANALYTICS\".\"target_schema_silver\".\"stg_example\""
        );
    }

    #[test]
    fn dbt_model_lookup_id_matches_snowflake_unquoted_identifier_fold() {
        let lookup = SnowflakeProvider::dbt_model_lookup_id(&DatasetId {
            catalog: "analytics".to_string(),
            database: "example_silver".to_string(),
            table: "stg_orders".to_string(),
        });

        assert_eq!(lookup.catalog, "ANALYTICS");
        assert_eq!(lookup.database, "EXAMPLE_SILVER");
        assert_eq!(lookup.table, "STG_ORDERS");
    }

    #[test]
    fn detects_session_expired_errors_case_insensitively() {
        assert!(SnowflakeProvider::is_session_expired_error(
            "Snowflake query failed: session expired"
        ));
        assert!(SnowflakeProvider::is_session_expired_error(
            "SNOWFLAKE QUERY FAILED: SESSION EXPIRED"
        ));
        assert!(!SnowflakeProvider::is_session_expired_error(
            "Snowflake query failed: syntax error"
        ));
    }
}
