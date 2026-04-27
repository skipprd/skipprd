use react_core::resolved_config::ReactResolvedConfig;

/// Returns a human-readable SQL dialect description based on the warehouse provider.
pub fn active_provider_dialect(cfg: &ReactResolvedConfig) -> String {
    use crate::de_config::WarehouseKind;
    let providers = crate::de_config::de_config_from_resolved(cfg);
    let kind = providers
        .as_ref()
        .map(|p| p.warehouse.kind.clone())
        .unwrap_or(WarehouseKind::Athena);
    match kind {
        WarehouseKind::Athena => "Amazon Athena (engine v3 / Trino SQL)".to_string(),
        WarehouseKind::Postgres => "PostgreSQL".to_string(),
        WarehouseKind::Mssql => "Microsoft SQL Server (T-SQL)".to_string(),
        WarehouseKind::Snowflake => "Snowflake SQL".to_string(),
        WarehouseKind::Bigquery => "Google BigQuery (Standard SQL)".to_string(),
        WarehouseKind::Databricks => "Databricks SQL".to_string(),
        WarehouseKind::Synapse => "Azure Synapse Analytics (T-SQL)".to_string(),
        WarehouseKind::Redshift => "Amazon Redshift SQL".to_string(),
        WarehouseKind::Clickhouse => "ClickHouse SQL".to_string(),
        WarehouseKind::Motherduck => "MotherDuck SQL".to_string(),
    }
}
