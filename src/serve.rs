use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::cli::ServeArgs;
use crate::cluster::iceberg_lake::require_one_physical_catalog;
use crate::cluster::pipeline_view::QueryBackend;
use crate::cluster::IcebergCatalogSpec;
use crate::helpers::configuration::Config;
use crate::query_flight::service::{FlightAuth, FlightEngine, QueryFlightServer};
use skippr_iceberg_rest::RestState;
use tonic::transport::{Identity, ServerTlsConfig};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServePlan {
    pub spec: IcebergCatalogSpec,
    pub ingest_namespaces: BTreeSet<String>,
    pub pipeline_names: BTreeSet<String>,
}

pub fn plan_serve(config: &Config) -> Result<ServePlan, String> {
    let pipeline_names: BTreeSet<String> = config.pipelines.keys().cloned().collect();
    let mut lakes = Vec::new();
    for name in &pipeline_names {
        match QueryBackend::for_pipeline(config, name).map_err(|err| err.to_string())? {
            QueryBackend::Iceberg(lake) => {
                if pipeline_names.contains(&lake.ingest_namespace) {
                    return Err(format!(
                        "ingest namespace '{}' equals a pipeline name",
                        lake.ingest_namespace
                    ));
                }
                lakes.push(lake);
            }
            QueryBackend::WalOnly => {}
        }
    }
    let spec = require_one_physical_catalog(&lakes)?.clone();
    let ingest_namespaces = lakes
        .into_iter()
        .map(|lake| lake.ingest_namespace)
        .collect();
    Ok(ServePlan {
        spec,
        ingest_namespaces,
        pipeline_names,
    })
}

pub fn require_token(token_env: &str) -> Result<String, String> {
    let token = std::env::var(token_env)
        .map_err(|_| format!("{token_env} must be set to a non-empty bearer token"))?;
    if token.trim().is_empty() {
        return Err(format!(
            "{token_env} must be set to a non-empty bearer token"
        ));
    }
    Ok(token)
}

pub fn require_loopback_or_tls(addrs: &[SocketAddr], tls: bool) -> Result<(), String> {
    if tls {
        return Ok(());
    }
    for addr in addrs {
        if !addr.ip().is_loopback() {
            return Err(format!(
                "non-loopback bind {addr} requires --tls-cert and --tls-key"
            ));
        }
    }
    Ok(())
}

pub async fn run(config: Config, options: ServeArgs) -> Result<(), String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let token = require_token(&options.token_env)?;
    let tls = options.tls_cert.is_some() && options.tls_key.is_some();
    require_loopback_or_tls(&[options.rest_bind, options.flight_bind], tls)?;
    if options.tls_cert.is_some() != options.tls_key.is_some() {
        return Err("--tls-cert and --tls-key are required together".into());
    }
    let tls_material = match (options.tls_cert.as_ref(), options.tls_key.as_ref()) {
        (Some(cert), Some(key)) => Some((
            std::fs::read(cert).map_err(|err| err.to_string())?,
            std::fs::read(key).map_err(|err| err.to_string())?,
        )),
        _ => None,
    };
    let plan = plan_serve(&config)?;
    let catalog = crate::cluster::backend::open_iceberg_catalog(&plan.spec)
        .await
        .map_err(|err| err.to_string())?;
    let rest_listener = tokio::net::TcpListener::bind(options.rest_bind)
        .await
        .map_err(|err| err.to_string())?;
    let rest_addr = rest_listener.local_addr().map_err(|err| err.to_string())?;
    let rest_state = RestState {
        catalog,
        warehouse: plan.spec.warehouse().to_string(),
        ingest_namespaces: plan.ingest_namespaces,
        pipeline_names: plan.pipeline_names,
        token: Arc::from(token.as_str()),
        prefix: String::new(),
    };
    let rest_task = if let Some((cert, key)) = tls_material.clone() {
        let std_listener = rest_listener.into_std().map_err(|err| err.to_string())?;
        tokio::spawn(async move {
            if let Err(err) =
                skippr_iceberg_rest::serve_tls(std_listener, rest_state, cert, key).await
            {
                tracing::error!(error = %err, "Iceberg REST server stopped");
            }
        })
    } else {
        tokio::spawn(async move {
            if let Err(err) = skippr_iceberg_rest::serve(rest_listener, rest_state).await {
                tracing::error!(error = %err, "Iceberg REST server stopped");
            }
        })
    };
    let scope = crate::cluster::identity::TenantScope::new(
        config.get_tenant(),
        config.get_workspace_name(),
    )
    .map_err(|err| err.to_string())?;
    let flight_tls = tls_material
        .map(|(cert, key)| ServerTlsConfig::new().identity(Identity::from_pem(cert, key)));
    let flight = QueryFlightServer::start_with(
        options.flight_bind,
        FlightAuth::Bearer {
            token: Arc::from(token.as_str()),
            scope,
        },
        FlightEngine::Local,
        config,
        flight_tls,
    )
    .await
    .map_err(|err| err.to_string())?;
    let flight_addr = flight.bind_addr();
    if let Some(path) = options.ready_file {
        write_ready_file(&path, rest_addr, flight_addr, tls)?;
    }
    tracing::info!(rest = %rest_addr, flight = %flight_addr, "skipprd serve listening");
    tokio::signal::ctrl_c()
        .await
        .map_err(|err| err.to_string())?;
    flight.drain().await;
    rest_task.abort();
    Ok(())
}

fn write_ready_file(
    path: &PathBuf,
    rest: SocketAddr,
    flight: SocketAddr,
    tls: bool,
) -> Result<(), String> {
    let scheme = if tls { "https" } else { "http" };
    let flight_scheme = if tls { "grpc+tls" } else { "grpc" };
    let body = serde_json::json!({
        "rest": format!("{scheme}://{rest}"),
        "flight": format!("{flight_scheme}://{flight}"),
    });
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&body).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::configuration::Config;

    fn config_with_sink(plugin: &str, body: serde_json::Value) -> Config {
        let mut sink = serde_json::Map::new();
        sink.insert(plugin.to_string(), body);
        serde_json::from_value(serde_json::json!({
            "skippr": { "workspace": "ws", "tenant": "t" },
            "pipelines": {
                "p": {
                    "data_source": "data_sources.sample",
                    "data_sink": "data_sinks.out"
                }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            },
            "data_sinks": {
                "out": sink
            }
        }))
        .unwrap()
    }

    #[test]
    fn serve_plan_accepts_skipprlake() {
        let cfg = config_with_sink(
            "SkipprLake",
            serde_json::json!({
                "warehouse": "s3://wh/",
                "catalog_table": "cat",
                "table_namespace": "bronze"
            }),
        );
        let plan = plan_serve(&cfg).unwrap();
        assert_eq!(plan.spec.kind_name(), "SkipprLake");
        assert!(plan.ingest_namespaces.contains("bronze"));
        assert!(plan.pipeline_names.contains("p"));
    }

    #[test]
    fn serve_plan_accepts_athena_iceberg() {
        let cfg = config_with_sink(
            "AthenaIceberg",
            serde_json::json!({
                "warehouse": "s3://wh/",
                "glue_database_name": "db",
                "athena_workgroup_name": "wg",
                "athena_results_s3_bucket": "r"
            }),
        );
        let plan = plan_serve(&cfg).unwrap();
        assert_eq!(plan.spec.kind_name(), "AthenaIceberg");
        assert!(plan.ingest_namespaces.contains("db"));
    }

    #[test]
    fn serve_plan_accepts_duckdb() {
        let cfg = config_with_sink(
            "Duckdb",
            serde_json::json!({
                "warehouse": "file:///tmp/lake",
                "table_namespace": "bronze"
            }),
        );
        let plan = plan_serve(&cfg).unwrap();
        assert_eq!(plan.spec.kind_name(), "Duckdb");
        assert!(plan.ingest_namespaces.contains("bronze"));
    }

    #[test]
    fn serve_plan_rejects_wal_only_only() {
        let cfg = config_with_sink(
            "Athena",
            serde_json::json!({
                "s3_bucket": "b",
                "s3_prefix": "p",
                "athena_workgroup_name": "wg",
                "athena_results_s3_bucket": "r"
            }),
        );
        let err = plan_serve(&cfg).unwrap_err();
        assert!(err.contains("at least one Iceberg sink"), "{err}");
    }

    #[test]
    fn serve_plan_rejects_mixed_catalogs() {
        let skippr = config_with_sink(
            "SkipprLake",
            serde_json::json!({
                "warehouse": "s3://wh/",
                "catalog_table": "cat",
                "table_namespace": "bronze"
            }),
        );
        let mut glue_sink = serde_json::Map::new();
        glue_sink.insert(
            "AthenaIceberg".to_string(),
            serde_json::json!({
                "warehouse": "s3://wh/",
                "glue_database_name": "db",
                "athena_workgroup_name": "wg",
                "athena_results_s3_bucket": "r"
            }),
        );
        let mut cfg = skippr;
        cfg.pipelines.insert(
            "q".into(),
            serde_json::from_value(serde_json::json!({
                "data_source": "data_sources.sample",
                "data_sink": "data_sinks.glue"
            }))
            .unwrap(),
        );
        cfg.data_sinks.as_mut().unwrap().insert(
            "glue".into(),
            serde_json::from_value(serde_json::json!(glue_sink)).unwrap(),
        );
        let err = plan_serve(&cfg).unwrap_err();
        assert!(err.contains("one Iceberg catalog"), "{err}");
    }

    #[test]
    fn serve_plan_rejects_ingest_namespace_equal_pipeline() {
        let cfg = config_with_sink(
            "SkipprLake",
            serde_json::json!({
                "warehouse": "s3://wh/",
                "catalog_table": "cat",
                "table_namespace": "p"
            }),
        );
        let err = plan_serve(&cfg).unwrap_err();
        assert!(err.contains("equals a pipeline name"), "{err}");
    }

    #[test]
    fn non_loopback_without_tls_is_rejected() {
        let err = require_loopback_or_tls(&["8.8.8.8:8181".parse().unwrap()], false).unwrap_err();
        assert!(err.contains("requires --tls-cert"), "{err}");
    }

    #[test]
    fn loopback_without_tls_is_ok() {
        assert!(require_loopback_or_tls(&["127.0.0.1:8181".parse().unwrap()], false).is_ok());
    }

    #[test]
    fn ready_file_uses_grpc_scheme() {
        let dir = std::env::temp_dir().join(format!("skipprd-ready-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        write_ready_file(
            &dir,
            "127.0.0.1:8181".parse().unwrap(),
            "127.0.0.1:8815".parse().unwrap(),
            false,
        )
        .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&dir).unwrap()).unwrap();
        let _ = std::fs::remove_file(&dir);
        assert_eq!(body["rest"], "http://127.0.0.1:8181");
        assert_eq!(body["flight"], "grpc://127.0.0.1:8815");
    }
}
