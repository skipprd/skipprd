use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::info;

use skippr_runtime_sdk::helpers::configuration::Config;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceNamespaceContract, SourceOnceContract,
};
use skippr_runtime_sdk::source_compat::{submit_arrow_ipc_batches, SourceSyncContext};
use skippr_runtime_sdk::RUNNING;

use crate::arrow_batch::{batches_from_signal, deadletter_batch, ipc_batch, NS_SPANS};
use crate::config::OtlpConfig;
use crate::contracts::namespace_contracts;
use crate::decode::DecodedSignal;
use crate::grpc::{serve_otlp_grpc, OtlpGrpcService};
use crate::http::{otlp_http_router, HttpState};

pub struct DataSourceOtlpPlugin {
    config: OtlpConfig,
}

impl DataSourceOtlpPlugin {
    pub fn with_runtime_config(config: OtlpConfig) -> Self {
        Self { config }
    }

    fn inject_fields() -> BTreeMap<String, String> {
        Config::get_transform_inject_fields()
            .into_iter()
            .filter_map(|(k, v)| match v {
                Value::String(s) => Some((k, s)),
                Value::Number(n) => Some((k, n.to_string())),
                Value::Bool(b) => Some((k, b.to_string())),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl DataSource for DataSourceOtlpPlugin {
    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        namespace_contracts(&self.config)
    }

    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::HostIdleBounded)
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        for contract in self.source_namespace_contracts() {
            contract
                .validate()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
        }

        let http_addr = self
            .config
            .listen_address_http
            .parse::<std::net::SocketAddr>()
            .or_else(|_| {
                format!("{}:0", self.config.listen_address_http).parse::<std::net::SocketAddr>()
            })
            .unwrap_or_else(|_| "0.0.0.0:4318".parse().unwrap());
        let grpc_addr = self
            .config
            .listen_address_grpc
            .parse::<std::net::SocketAddr>()
            .unwrap_or_else(|_| "0.0.0.0:4317".parse().unwrap());

        let (tx, mut rx) = mpsc::channel::<DecodedSignal>(32);
        let inject = Arc::new(Self::inject_fields());
        let cfg = Arc::new(self.config.clone());

        let http_state = HttpState {
            tx: tx.clone(),
            config: Arc::clone(&cfg),
            inject: Arc::clone(&inject),
        };
        let listener = TcpListener::bind(http_addr)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to bind HTTP {http_addr}: {e}")))?;
        info!("Otlp HTTP listening on {http_addr}");
        let http_handle = tokio::spawn(async move {
            axum::serve(listener, otlp_http_router(http_state))
                .await
                .ok();
        });

        let grpc_svc = OtlpGrpcService::new(tx, Arc::clone(&cfg), inject);
        let grpc_handle = tokio::spawn(async move {
            let _ = serve_otlp_grpc(grpc_addr, grpc_svc).await;
        });
        info!("Otlp gRPC listening on {grpc_addr}");

        let mut counter: u64 = 0;
        while RUNNING.read().load(Ordering::SeqCst) {
            match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Some(signal)) => {
                    counter = counter.saturating_add(1);
                    match batches_from_signal(&signal, counter) {
                        Ok(batches) => {
                            submit_arrow_ipc_batches(ctx.as_ref(), batches)?;
                        }
                        Err(err) => {
                            let ns = format!("_dl_{}", Config::get_pipeline_name());
                            let batch = deadletter_batch(
                                NS_SPANS,
                                &err.to_string(),
                                "arrow_builder",
                                "otlp",
                                &counter.to_string(),
                                counter as i64,
                            )
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                            let ipc = ipc_batch(&ns, &counter.to_string(), batch, counter)
                                .map_err(|e| std::io::Error::other(e.to_string()))?;
                            submit_arrow_ipc_batches(ctx.as_ref(), vec![ipc])?;
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        http_handle.abort();
        grpc_handle.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OtlpSignal;
    use crate::decode::{decode_traces, traces_fixture_request};
    use prost::Message;
    use skippr_runtime_sdk::sdk::encode_record_batches;
    use std::collections::BTreeMap;

    #[test]
    fn traces_export_emits_three_namespaces() {
        let cfg: OtlpConfig =
            serde_json::from_value(serde_json::json!({"signals":["traces"]})).unwrap();
        let bytes = traces_fixture_request().encode_to_vec();
        let decoded = decode_traces(&bytes, &cfg, &BTreeMap::new()).unwrap();
        let batches = batches_from_signal(&DecodedSignal::Traces(decoded), 1).unwrap();
        let names: Vec<_> = batches.iter().map(|b| b.namespace.as_str()).collect();
        assert_eq!(names, ["spans", "span_events", "span_links"]);
    }

    #[test]
    fn logs_only_contracts_skip_spans() {
        let cfg: OtlpConfig =
            serde_json::from_value(serde_json::json!({"signals":["logs"]})).unwrap();
        let plugin = DataSourceOtlpPlugin::with_runtime_config(cfg);
        let names: Vec<_> = plugin
            .source_namespace_contracts()
            .into_iter()
            .map(|c| c.namespace)
            .collect();
        assert_eq!(names, ["log_records"]);
        assert!(!names.iter().any(|n| n == "spans"));
    }

    #[test]
    fn deadletter_has_failure_code() {
        let batch = deadletter_batch("spans", "boom", "arrow_builder", "otlp", "1", 1).unwrap();
        let idx = batch.schema().index_of("failure_code").unwrap();
        let col = batch
            .column(idx)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        assert_eq!(col.value(0), "arrow_builder");
        let _ = encode_record_batches(&[batch]).unwrap();
    }

    #[test]
    fn signals_exhaustive_match_compiles() {
        let cfg: OtlpConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        for signal in [OtlpSignal::Traces, OtlpSignal::Logs, OtlpSignal::Metrics] {
            assert!(cfg.accepts(signal));
        }
    }
}
