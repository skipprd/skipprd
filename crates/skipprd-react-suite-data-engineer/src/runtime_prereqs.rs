use crate::providers::{DbtNamespaceShape, DbtTier, DbtTierNamespace, DbtTierRouting};
use react_core::agent::AgentCtx;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimePrerequisiteFailure {
    pub brief: String,
    pub log_excerpts: Option<String>,
    pub resources: Vec<String>,
}

impl RuntimePrerequisiteFailure {
    pub(crate) fn into_evidence(self) -> crate::evaluation::Evidence {
        crate::evaluation::Evidence::RuntimePrerequisite(
            crate::evaluation::RuntimePrerequisiteEvidence {
                brief: self.brief,
                log_excerpts: self.log_excerpts,
                resources: self.resources,
            },
        )
    }
}

pub(crate) fn failure_from_dbt_errors(
    brief: &str,
    errors: &[String],
) -> Option<RuntimePrerequisiteFailure> {
    let joined = errors.join("\n");
    let normalized = crate::failure_text::normalize_text(&format!("{brief}\n{joined}"));
    let namespace_issue = normalized.contains("database error while listing schemas")
        || normalized.contains("object does not exist, or operation cannot be performed")
        || normalized.contains("schema does not exist")
        || normalized.contains("database does not exist");
    if !namespace_issue {
        return None;
    }
    Some(RuntimePrerequisiteFailure {
        brief: format!(
            "dbt runtime prerequisites are not available for validation; deterministic namespace bootstrap did not satisfy the warehouse before dbt build.\n\n{brief}"
        ),
        log_excerpts: Some(joined),
        resources: Vec::new(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NamespaceStatement {
    resource: String,
    sql: String,
}

pub(crate) async fn ensure_dbt_runtime_prerequisites(
    ctx: &AgentCtx,
) -> Result<(), RuntimePrerequisiteFailure> {
    let cfg = crate::resolved_config_from_ctx(ctx).ok_or_else(|| RuntimePrerequisiteFailure {
        brief: "dbt runtime prerequisites could not be checked: resolved config is missing"
            .to_string(),
        log_excerpts: None,
        resources: Vec::new(),
    })?;
    let providers = crate::de_config::de_config_from_resolved(cfg).ok_or_else(|| {
        RuntimePrerequisiteFailure {
            brief:
                "dbt runtime prerequisites could not be checked: data-engineer config is invalid"
                    .to_string(),
            log_excerpts: None,
            resources: Vec::new(),
        }
    })?;
    let warehouse =
        crate::ctx_ext::actx_warehouse(ctx).ok_or_else(|| RuntimePrerequisiteFailure {
            brief: "dbt runtime prerequisites could not be checked: warehouse provider is missing"
                .to_string(),
            log_excerpts: None,
            resources: Vec::new(),
        })?;
    let routing = crate::dbt::profile::tier_routing(cfg, &providers);
    let statements = namespace_statements(
        providers.warehouse.kind,
        providers.warehouse.container.as_str(),
        &routing,
        |ident| warehouse.dbt_runtime_namespace_ident(ident),
    );
    let resources: Vec<String> = statements
        .iter()
        .map(|stmt| stmt.resource.clone())
        .collect();

    let mut failures = Vec::new();
    for statement in statements {
        match warehouse.query(&statement.sql).await {
            Ok(_) => {
                tracing::info!(
                    resource = %statement.resource,
                    "dbt runtime prerequisite ensured"
                );
            }
            Err(error) => {
                tracing::warn!(
                    resource = %statement.resource,
                    sql = %statement.sql,
                    error = %error,
                    "dbt runtime prerequisite ensure failed"
                );
                failures.push(format!(
                    "{}\nSQL: {}\nError: {}",
                    statement.resource, statement.sql, error
                ));
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(RuntimePrerequisiteFailure {
            brief: format!(
                "dbt runtime prerequisites could not be created or verified for {} resource(s)",
                failures.len()
            ),
            log_excerpts: Some(failures.join("\n\n")),
            resources,
        })
    }
}

fn namespace_statements(
    warehouse_kind: crate::de_config::WarehouseKind,
    warehouse_container: &str,
    routing: &DbtTierRouting,
    quote_ident: impl Fn(&str) -> String,
) -> Vec<NamespaceStatement> {
    let mut out = Vec::new();
    for tier in [DbtTier::Silver, DbtTier::Gold] {
        let ns = routing.namespace(tier);
        out.extend(namespace_statements_for_tier(
            warehouse_kind,
            warehouse_container,
            routing.shape,
            tier,
            ns,
            &quote_ident,
        ));
    }
    out
}

fn namespace_statements_for_tier(
    warehouse_kind: crate::de_config::WarehouseKind,
    warehouse_container: &str,
    shape: DbtNamespaceShape,
    tier: DbtTier,
    ns: &DbtTierNamespace,
    quote_ident: &impl Fn(&str) -> String,
) -> Vec<NamespaceStatement> {
    use crate::de_config::WarehouseKind;

    let tier_label = match tier {
        DbtTier::Silver => "silver",
        DbtTier::Gold => "gold",
    };
    let schema = ns.schema.trim();
    if schema.is_empty() {
        return Vec::new();
    }

    match shape {
        DbtNamespaceShape::DatabaseAndSchema => {
            let Some(database) = ns
                .database
                .as_deref()
                .map(str::trim)
                .filter(|db| !db.is_empty())
            else {
                return Vec::new();
            };
            vec![
                NamespaceStatement {
                    resource: format!("{tier_label} database {database}"),
                    sql: format!("create database if not exists {}", quote_ident(database)),
                },
                NamespaceStatement {
                    resource: format!("{tier_label} schema {database}.{schema}"),
                    sql: format!(
                        "create schema if not exists {}.{}",
                        quote_ident(database),
                        quote_ident(schema)
                    ),
                },
            ]
        }
        DbtNamespaceShape::CatalogAndSchema => match warehouse_kind {
            WarehouseKind::Athena => vec![NamespaceStatement {
                resource: format!("{tier_label} database {schema}"),
                sql: format!("create database if not exists {}", quote_ident(schema)),
            }],
            WarehouseKind::Bigquery | WarehouseKind::Databricks => {
                let container = warehouse_container.trim();
                let qualified = if container.is_empty() {
                    quote_ident(schema)
                } else {
                    format!("{}.{}", quote_ident(container), quote_ident(schema))
                };
                vec![NamespaceStatement {
                    resource: if container.is_empty() {
                        format!("{tier_label} schema {schema}")
                    } else {
                        format!("{tier_label} schema {container}.{schema}")
                    },
                    sql: format!("create schema if not exists {qualified}"),
                }]
            }
            _ => vec![NamespaceStatement {
                resource: format!("{tier_label} schema {schema}"),
                sql: format!("create schema if not exists {}", quote_ident(schema)),
            }],
        },
        DbtNamespaceShape::ConnectionDatabaseAndSchema => match warehouse_kind {
            WarehouseKind::Clickhouse | WarehouseKind::Motherduck => vec![NamespaceStatement {
                resource: format!("{tier_label} database {schema}"),
                sql: format!("create database if not exists {}", quote_ident(schema)),
            }],
            _ => vec![NamespaceStatement {
                resource: format!("{tier_label} schema {schema}"),
                sql: format!("create schema if not exists {}", quote_ident(schema)),
            }],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(ident: &str) -> String {
        format!("\"{}\"", ident)
    }

    fn snowflake_dbt_ident(ident: &str) -> String {
        ident.trim().to_ascii_uppercase()
    }

    #[test]
    fn snowflake_prereqs_create_tier_databases_and_schemas() {
        let routing = DbtTierRouting {
            base_schema: "bank".to_string(),
            silver: DbtTierNamespace {
                database: Some("bank_silver".to_string()),
                schema: "bank".to_string(),
            },
            gold: DbtTierNamespace {
                database: Some("bank_gold".to_string()),
                schema: "bank".to_string(),
            },
            shape: DbtNamespaceShape::DatabaseAndSchema,
            custom_schema_policy: crate::providers::DbtCustomSchemaPolicy::Exact,
        };

        let statements = namespace_statements(
            crate::de_config::WarehouseKind::Snowflake,
            "ANALYTICS",
            &routing,
            snowflake_dbt_ident,
        );
        let sql: Vec<String> = statements.into_iter().map(|stmt| stmt.sql).collect();
        assert_eq!(
            sql,
            vec![
                "create database if not exists BANK_SILVER",
                "create schema if not exists BANK_SILVER.BANK",
                "create database if not exists BANK_GOLD",
                "create schema if not exists BANK_GOLD.BANK",
            ]
        );
    }

    #[test]
    fn athena_prereqs_create_tier_databases() {
        let routing = DbtTierRouting {
            base_schema: "bank".to_string(),
            silver: DbtTierNamespace {
                database: None,
                schema: "bank_silver".to_string(),
            },
            gold: DbtTierNamespace {
                database: None,
                schema: "bank_gold".to_string(),
            },
            shape: DbtNamespaceShape::CatalogAndSchema,
            custom_schema_policy: crate::providers::DbtCustomSchemaPolicy::AdapterDefault,
        };

        let statements = namespace_statements(
            crate::de_config::WarehouseKind::Athena,
            "AwsDataCatalog",
            &routing,
            quoted,
        );
        let sql: Vec<String> = statements.into_iter().map(|stmt| stmt.sql).collect();
        assert_eq!(
            sql,
            vec![
                "create database if not exists \"bank_silver\"",
                "create database if not exists \"bank_gold\"",
            ]
        );
    }
}
