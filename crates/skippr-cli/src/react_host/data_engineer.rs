use std::sync::Arc;

use react::runtime_settings::{getenv_nonempty, getenv_u64, getenv_usize};
use react_core::keyspace::Keyspace;
use react_core::resolved_config::StorageMode;
use react_core::suite::SuiteCtx;
use react_module_provider_athena::{AthenaProvider, AthenaSettings};
use react_module_provider_bigquery::{BigQueryProvider, BigQuerySettings};
use react_module_provider_catalog::DefaultCatalogProvider;
use react_module_provider_clickhouse::{ClickHouseProvider, ClickHouseSettings};
use react_module_provider_databricks::{DatabricksProvider, DatabricksSettings};
use react_module_provider_dbt::{DbtProjectProvider, DbtRunnerConfig, DbtRunnerMode};
use react_module_provider_motherduck::{MotherDuckProvider, MotherDuckSettings};
use react_module_provider_mssql::{MssqlProvider, MssqlSettings};
use react_module_provider_postgres::{PostgresProvider, PostgresSettings};
use react_module_provider_redshift::{RedshiftProvider, RedshiftSettings};
use react_module_provider_skippr::SkipprCliProvider;
use react_module_provider_snowflake::{SnowflakeProvider, SnowflakeSettings};
use react_module_provider_synapse::{SynapseProvider, SynapseSettings};
use react_suite_data_engineer::ctx_ext::{
    CatalogCap, DatasetsCap, DbtCap, ProvidersCfgCap, QueryCap, SkipprCap, WarehouseCap,
};
use react_suite_data_engineer::de_config::{self as de_cfg, WarehouseKind};

use super::vector::LanceVectorStore;

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn apply_aws_region_fallback(warehouse_extras: &serde_json::Value) {
    if let Some(region) = warehouse_extras
        .get("region")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let has_default = std::env::var("AWS_DEFAULT_REGION")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some();
        let has_region = std::env::var("AWS_REGION")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some();
        if !has_default {
            std::env::set_var("AWS_DEFAULT_REGION", region);
        }
        if !has_region {
            std::env::set_var("AWS_REGION", region);
        }
    }
}

fn resolve_athena_settings(providers: &de_cfg::ProvidersResolved) -> AthenaSettings {
    let extras = &providers.warehouse.extras;
    let region = getenv_nonempty("AWS_REGION")
        .or_else(|| getenv_nonempty("AWS_DEFAULT_REGION"))
        .or_else(|| {
            extras
                .get("region")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    let workgroup = getenv_nonempty("ATHENA_WORKGROUP").or_else(|| {
        extras
            .get("workgroup")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });
    let result_output_location = getenv_nonempty("ATHENA_RESULT_S3").or_else(|| {
        extras
            .get("result_s3")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });
    let default_catalog = getenv_nonempty("ATHENA_TARGET_CATALOG")
        .or_else(|| Some(providers.warehouse.container.clone()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "AwsDataCatalog".to_string());
    let source_schema = getenv_nonempty("ATHENA_SOURCE_SCHEMA")
        .or_else(|| Some(providers.warehouse.namespace.clone()).filter(|s| !s.is_empty()));
    let max_concurrency = getenv_usize("ATHENA_MAX_CONCURRENCY")
        .or_else(|| {
            extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(15);
    let discovery_cache_ttl_secs = getenv_u64("ATHENA_DISCOVERY_CACHE_TTL_SECS")
        .or_else(|| {
            extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(120);

    AthenaSettings {
        region,
        workgroup,
        result_output_location,
        default_catalog,
        source_schema,
        max_concurrency,
        discovery_cache_ttl_secs,
    }
}

fn resolve_snowflake_settings(providers: &de_cfg::ProvidersResolved) -> SnowflakeSettings {
    let extras = &providers.warehouse.extras;
    let warehouse = getenv_nonempty("SNOWFLAKE_WAREHOUSE").or_else(|| {
        extras
            .get("warehouse")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });
    let role = getenv_nonempty("SNOWFLAKE_ROLE").or_else(|| {
        extras
            .get("role")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });
    let max_concurrency = getenv_usize("SNOWFLAKE_MAX_CONCURRENCY")
        .or_else(|| {
            extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(15);
    let discovery_cache_ttl_secs = getenv_u64("SNOWFLAKE_DISCOVERY_CACHE_TTL_SECS")
        .or_else(|| {
            extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(120);

    let private_key_pem =
        getenv_nonempty("SNOWFLAKE_PRIVATE_KEY_PATH").and_then(
            |path| match std::fs::read_to_string(&path) {
                Ok(contents) => Some(contents),
                Err(e) => {
                    tracing::error!("failed to read SNOWFLAKE_PRIVATE_KEY_PATH={}: {}", path, e);
                    None
                }
            },
        );

    SnowflakeSettings {
        account: getenv_nonempty("SNOWFLAKE_ACCOUNT").or_else(|| {
            extras
                .get("account")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        user: getenv_nonempty("SNOWFLAKE_USER").or_else(|| {
            extras
                .get("user")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        password: getenv_nonempty("SNOWFLAKE_PASSWORD").or_else(|| {
            extras
                .get("password")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        private_key_pem: private_key_pem.or_else(|| {
            extras
                .get("private_key_path")
                .and_then(|v| v.as_str())
                .and_then(|path| std::fs::read_to_string(path).ok())
        }),
        database: nonempty(&providers.warehouse.container),
        schema: nonempty(&providers.warehouse.namespace),
        warehouse,
        role,
        max_concurrency,
        discovery_cache_ttl_secs,
    }
}

fn resolve_mssql_settings(providers: &de_cfg::ProvidersResolved) -> MssqlSettings {
    let extras = &providers.warehouse.extras;
    let max_concurrency = getenv_usize("MSSQL_MAX_CONCURRENCY")
        .or_else(|| {
            extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(15);
    let discovery_cache_ttl_secs = getenv_u64("MSSQL_DISCOVERY_CACHE_TTL_SECS")
        .or_else(|| {
            extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(120);

    MssqlSettings {
        host: getenv_nonempty("MSSQL_HOST"),
        port: getenv_nonempty("MSSQL_PORT").and_then(|v| v.parse::<u16>().ok()),
        user: getenv_nonempty("MSSQL_USER"),
        password: getenv_nonempty("MSSQL_PASSWORD"),
        database: nonempty(&providers.warehouse.container),
        schema: nonempty(&providers.warehouse.namespace),
        max_concurrency,
        discovery_cache_ttl_secs,
        trust_cert: false,
    }
}

fn resolve_databricks_settings(providers: &de_cfg::ProvidersResolved) -> DatabricksSettings {
    let extras = &providers.warehouse.extras;
    let max_concurrency = getenv_usize("DATABRICKS_MAX_CONCURRENCY")
        .or_else(|| {
            extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(15);
    let discovery_cache_ttl_secs = getenv_u64("DATABRICKS_DISCOVERY_CACHE_TTL_SECS")
        .or_else(|| {
            extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(120);

    DatabricksSettings {
        workspace_url: getenv_nonempty("DATABRICKS_HOST").or_else(|| {
            extras
                .get("workspace_url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        token: getenv_nonempty("DATABRICKS_TOKEN").or_else(|| {
            extras
                .get("token")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        warehouse_id: getenv_nonempty("DATABRICKS_WAREHOUSE_ID").or_else(|| {
            extras
                .get("warehouse_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        catalog: Some(providers.warehouse.container.clone()).filter(|s| !s.is_empty()),
        schema: Some(providers.warehouse.namespace.clone()).filter(|s| !s.is_empty()),
        max_concurrency,
        discovery_cache_ttl_secs,
    }
}

fn resolve_synapse_settings(providers: &de_cfg::ProvidersResolved) -> SynapseSettings {
    let extras = &providers.warehouse.extras;
    let max_concurrency = getenv_usize("SYNAPSE_MAX_CONCURRENCY")
        .or_else(|| {
            extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(15);
    let discovery_cache_ttl_secs = getenv_u64("SYNAPSE_DISCOVERY_CACHE_TTL_SECS")
        .or_else(|| {
            extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(120);

    let connection_string = getenv_nonempty("SYNAPSE_CONNECTION_STRING").or_else(|| {
        extras
            .get("connection_string")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });

    let (host, port, user, password, database) = if let Some(_cs) = &connection_string {
        (
            getenv_nonempty("SYNAPSE_HOST"),
            getenv_nonempty("SYNAPSE_PORT").and_then(|v| v.parse::<u16>().ok()),
            getenv_nonempty("SYNAPSE_USER"),
            getenv_nonempty("SYNAPSE_PASSWORD"),
            nonempty(&providers.warehouse.container),
        )
    } else {
        (
            getenv_nonempty("SYNAPSE_HOST"),
            getenv_nonempty("SYNAPSE_PORT").and_then(|v| v.parse::<u16>().ok()),
            getenv_nonempty("SYNAPSE_USER"),
            getenv_nonempty("SYNAPSE_PASSWORD"),
            nonempty(&providers.warehouse.container),
        )
    };

    SynapseSettings {
        host,
        port,
        user,
        password,
        database,
        schema: nonempty(&providers.warehouse.namespace),
        max_concurrency,
        discovery_cache_ttl_secs,
        trust_cert: false,
    }
}

fn resolve_redshift_settings(providers: &de_cfg::ProvidersResolved) -> RedshiftSettings {
    let extras = &providers.warehouse.extras;
    let max_concurrency = getenv_usize("REDSHIFT_MAX_CONCURRENCY")
        .or_else(|| {
            extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(15);
    let discovery_cache_ttl_secs = getenv_u64("REDSHIFT_DISCOVERY_CACHE_TTL_SECS")
        .or_else(|| {
            extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(120);

    RedshiftSettings {
        database: nonempty(&providers.warehouse.container),
        cluster_identifier: getenv_nonempty("REDSHIFT_CLUSTER_IDENTIFIER").or_else(|| {
            extras
                .get("cluster_identifier")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        workgroup_name: getenv_nonempty("REDSHIFT_WORKGROUP_NAME").or_else(|| {
            extras
                .get("workgroup_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        db_user: getenv_nonempty("REDSHIFT_DB_USER").or_else(|| {
            extras
                .get("db_user")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        schema: nonempty(&providers.warehouse.namespace),
        region: getenv_nonempty("AWS_REGION").or_else(|| {
            extras
                .get("region")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        max_concurrency,
        discovery_cache_ttl_secs,
    }
}

fn resolve_clickhouse_settings(providers: &de_cfg::ProvidersResolved) -> ClickHouseSettings {
    let extras = &providers.warehouse.extras;
    let max_concurrency = getenv_usize("CLICKHOUSE_MAX_CONCURRENCY")
        .or_else(|| {
            extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(15);
    let discovery_cache_ttl_secs = getenv_u64("CLICKHOUSE_DISCOVERY_CACHE_TTL_SECS")
        .or_else(|| {
            extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(120);

    ClickHouseSettings {
        url: getenv_nonempty("CLICKHOUSE_URL").or_else(|| {
            extras
                .get("url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        database: nonempty(&providers.warehouse.container),
        user: getenv_nonempty("CLICKHOUSE_USER").or_else(|| {
            extras
                .get("user")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        password: getenv_nonempty("CLICKHOUSE_PASSWORD").or_else(|| {
            extras
                .get("password")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        max_concurrency,
        discovery_cache_ttl_secs,
    }
}

fn resolve_motherduck_settings(providers: &de_cfg::ProvidersResolved) -> MotherDuckSettings {
    let extras = &providers.warehouse.extras;
    let max_concurrency = getenv_usize("MOTHERDUCK_MAX_CONCURRENCY")
        .or_else(|| {
            extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(4);

    let discovery_cache_ttl_secs = getenv_u64("MOTHERDUCK_DISCOVERY_CACHE_TTL_SECS")
        .or_else(|| {
            extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(120);

    MotherDuckSettings {
        motherduck_token: getenv_nonempty("MOTHERDUCK_TOKEN").or_else(|| {
            extras
                .get("motherduck_token")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }),
        database: nonempty(&providers.warehouse.container),
        schema: nonempty(&providers.warehouse.namespace),
        max_concurrency,
        discovery_cache_ttl_secs,
    }
}

/// Wire all data_engineer-specific providers onto the SuiteCtx.
///
/// This module is intentionally owned by `skippr`: it binds the generic
/// `react` runtime to the data-engineer suite's concrete provider graph.
pub(crate) async fn wire_providers(
    sctx: &mut SuiteCtx,
    keyspace: &Arc<dyn Keyspace>,
    lance_uri_prefix: &str,
    lance_storage_opts: Vec<(String, String)>,
) -> Result<(), String> {
    let cfg = sctx
        .resolved_config()
        .as_ref()
        .expect("resolved_config must be set before wire_providers");
    let providers = de_cfg::de_config_from_resolved(cfg).unwrap_or_default();
    let wh_kind = providers.warehouse.kind;
    let skippr_storage_mode =
        getenv_nonempty("SKIPPR_STORAGE_MODE").unwrap_or_else(|| match cfg.storage.mode {
            StorageMode::Local => "local".to_string(),
            StorageMode::S3 => "s3".to_string(),
        });
    let skippr_storage_bucket = if skippr_storage_mode.eq_ignore_ascii_case("s3") {
        getenv_nonempty("SKIPPR_S3_BUCKET").or_else(|| cfg.storage.bucket.clone())
    } else {
        None
    };
    let skippr_data_dir = {
        let fs_root = cfg.storage.path.as_deref().unwrap_or("./.skippr");
        std::path::PathBuf::from(fs_root)
            .join(cfg.scope.tenant.as_str())
            .join(cfg.scope.workspace.as_str())
            .join(cfg.scope.project_id.as_str())
            .join("skippr")
    };

    match wh_kind {
        WarehouseKind::Athena => {
            apply_aws_region_fallback(&providers.warehouse.extras);
            let athena =
                Arc::new(AthenaProvider::from_settings(resolve_athena_settings(&providers)).await);
            sctx.set_capability(Arc::new(WarehouseCap(athena.clone())));
            sctx.set_capability(Arc::new(QueryCap(athena.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(athena.clone())));
        }
        WarehouseKind::Postgres => {
            let dbname = nonempty(&providers.warehouse.container);
            let default_schema = nonempty(&providers.warehouse.namespace);
            let pg = Arc::new(PostgresProvider::from_settings(PostgresSettings {
                dbname,
                default_schema,
                ..Default::default()
            }));
            sctx.set_capability(Arc::new(WarehouseCap(pg.clone())));
            sctx.set_capability(Arc::new(QueryCap(pg.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(pg.clone())));
        }
        WarehouseKind::Bigquery => {
            let project = nonempty(&providers.warehouse.container);
            let dataset = nonempty(&providers.warehouse.namespace);
            let location = providers
                .warehouse
                .extras
                .get("location")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let max_conc = providers
                .warehouse
                .extras
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(15);
            let ttl_secs = providers
                .warehouse
                .extras
                .get("discovery_cache_ttl_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(120);
            let bq = Arc::new(
                BigQueryProvider::from_settings(BigQuerySettings {
                    project,
                    dataset,
                    location,
                    max_concurrency: max_conc,
                    discovery_cache_ttl_secs: ttl_secs,
                })
                .await
                .map_err(|e| format!("BigQuery provider init failed: {}", e))?,
            );
            sctx.set_capability(Arc::new(WarehouseCap(bq.clone())));
            sctx.set_capability(Arc::new(QueryCap(bq.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(bq.clone())));
        }
        WarehouseKind::Snowflake => {
            let sf = Arc::new(
                SnowflakeProvider::from_settings(resolve_snowflake_settings(&providers))
                    .await
                    .map_err(|e| format!("Snowflake provider init failed: {}", e))?,
            );
            sctx.set_capability(Arc::new(WarehouseCap(sf.clone())));
            sctx.set_capability(Arc::new(QueryCap(sf.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(sf.clone())));
        }
        WarehouseKind::Mssql => {
            let ms = Arc::new(
                MssqlProvider::from_settings(resolve_mssql_settings(&providers))
                    .await
                    .map_err(|e| format!("MSSQL provider init failed: {}", e))?,
            );
            sctx.set_capability(Arc::new(WarehouseCap(ms.clone())));
            sctx.set_capability(Arc::new(QueryCap(ms.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(ms.clone())));
        }
        WarehouseKind::Databricks => {
            let db = Arc::new(
                DatabricksProvider::from_settings(resolve_databricks_settings(&providers))
                    .await
                    .map_err(|e| format!("Databricks provider init failed: {}", e))?,
            );
            sctx.set_capability(Arc::new(WarehouseCap(db.clone())));
            sctx.set_capability(Arc::new(QueryCap(db.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(db.clone())));
        }
        WarehouseKind::Synapse => {
            let syn = Arc::new(
                SynapseProvider::from_settings(resolve_synapse_settings(&providers))
                    .await
                    .map_err(|e| format!("Synapse provider init failed: {}", e))?,
            );
            sctx.set_capability(Arc::new(WarehouseCap(syn.clone())));
            sctx.set_capability(Arc::new(QueryCap(syn.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(syn.clone())));
        }
        WarehouseKind::Redshift => {
            apply_aws_region_fallback(&providers.warehouse.extras);
            let rs = Arc::new(
                RedshiftProvider::from_settings(resolve_redshift_settings(&providers))
                    .await
                    .map_err(|e| format!("Redshift provider init failed: {}", e))?,
            );
            sctx.set_capability(Arc::new(WarehouseCap(rs.clone())));
            sctx.set_capability(Arc::new(QueryCap(rs.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(rs.clone())));
        }
        WarehouseKind::Clickhouse => {
            let ch = Arc::new(
                ClickHouseProvider::from_settings(resolve_clickhouse_settings(&providers))
                    .map_err(|e| format!("ClickHouse provider init failed: {}", e))?,
            );
            sctx.set_capability(Arc::new(WarehouseCap(ch.clone())));
            sctx.set_capability(Arc::new(QueryCap(ch.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(ch.clone())));
        }
        WarehouseKind::Motherduck => {
            let md = Arc::new(
                MotherDuckProvider::from_settings(resolve_motherduck_settings(&providers))
                    .map_err(|e| format!("MotherDuck provider init failed: {}", e))?,
            );
            sctx.set_capability(Arc::new(WarehouseCap(md.clone())));
            sctx.set_capability(Arc::new(QueryCap(md.clone())));
            sctx.set_capability(Arc::new(DatasetsCap(md.clone())));
        }
    }

    if providers.catalog.enabled {
        let cat = Arc::new(
            DefaultCatalogProvider::new(
                sctx.storage().clone(),
                keyspace.clone(),
                sctx.llm().clone(),
                30,
                6,
            )
            .with_thread_id_fn(Arc::new(react::llm::thread_ctx::current_thread_id)),
        );
        sctx.set_capability(Arc::new(CatalogCap(cat)));
    }

    if providers.vector.enabled {
        let lance = LanceVectorStore::new(lance_uri_prefix.to_string())
            .with_storage_options(lance_storage_opts);
        sctx.set_vector(Some(Arc::new(lance)));
    }

    if providers.dbt.enabled {
        let runner_mode =
            DbtRunnerMode::parse(&providers.dbt.runner).map_err(|e| format!("{}", e))?;
        let runner = DbtRunnerConfig {
            mode: runner_mode,
            docker_image: providers.dbt.docker_image.clone(),
            docker_platform: providers.dbt.docker_platform.clone(),
            docker_network: providers.dbt.docker_network.clone(),
            docker_mount_aws_dir: providers.dbt.docker_mount_aws_dir,
        };
        sctx.set_capability(Arc::new(DbtCap(Arc::new(DbtProjectProvider::new(
            sctx.storage().clone(),
            keyspace.clone(),
            runner,
        )))));
    }

    if providers.el.enabled {
        let skippr_provider = SkipprCliProvider::new(
            providers.el.clone(),
            providers.warehouse.clone(),
            skippr_data_dir,
            Some(skippr_storage_mode),
            skippr_storage_bucket,
        );
        sctx.set_capability(Arc::new(SkipprCap(Arc::new(skippr_provider))));
    }

    sctx.set_capability(Arc::new(ProvidersCfgCap(providers)));
    Ok(())
}
