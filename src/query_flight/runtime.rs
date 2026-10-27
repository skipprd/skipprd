//! One QueryRuntime: in-process datafusion-distributed Worker + distributed planner.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use datafusion::error::DataFusionError;
use datafusion::execution::{SessionState, SessionStateBuilder};
use datafusion::prelude::{SessionConfig, SessionContext};
use datafusion_distributed::grpc::{create_worker_client, BoxCloneSyncChannel};
use datafusion_distributed::{
    ChannelResolver, DistributedExt, SessionStateBuilderExt, Worker, WorkerChannel,
    WorkerQueryContext, WorkerResolver,
};
use skippr_lease::DurableError;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Channel;
use url::Url;

use crate::query_flight::codec::SkipprHostCodec;
use crate::sqlrt::udfs;

struct QueryRuntime {
    ctx: SessionContext,
    advertised_worker: SocketAddr,
    worker_bind: SocketAddr,
    stop: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

static RUNTIME: RwLock<Option<QueryRuntime>> = RwLock::new(None);

/// Local `skipprd query` is in-process: plaintext Worker on localhost.
/// Clustered skipprd fail-closes on cluster mTLS. Not an env knob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerTransport {
    LocalPlaintext,
    ClusterMtls,
}

impl WorkerTransport {
    fn tls(self) -> bool {
        matches!(self, Self::ClusterMtls)
    }

    fn scheme(self) -> &'static str {
        if self.tls() {
            "https"
        } else {
            "http"
        }
    }
}

#[derive(Clone)]
struct SkipprWorkerResolver {
    self_url: Url,
}

impl WorkerResolver for SkipprWorkerResolver {
    fn get_urls(&self) -> Result<Vec<Url>, DataFusionError> {
        let mut urls = vec![self.self_url.clone()];
        if let Some(gossip) = crate::cluster::gossip::gossip_directory() {
            for ad in gossip.known_ads_sync() {
                let Some(addr) = ad.worker else {
                    continue;
                };
                let scheme = self.self_url.scheme();
                if let Ok(url) = Url::parse(&format!("{scheme}://{addr}")) {
                    if url != self.self_url {
                        urls.push(url);
                    }
                }
            }
        }
        Ok(urls)
    }
}

struct SkipprChannelResolver {
    cache: Mutex<HashMap<String, BoxCloneSyncChannel>>,
    transport: WorkerTransport,
}

impl SkipprChannelResolver {
    fn new(transport: WorkerTransport) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            transport,
        }
    }
}

#[async_trait]
impl ChannelResolver for SkipprChannelResolver {
    async fn get_worker_client_for_url(
        &self,
        url: &Url,
    ) -> Result<Box<dyn WorkerChannel>, DataFusionError> {
        let key = url.to_string();
        if let Some(channel) = self
            .cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(&key)
            .cloned()
        {
            return Ok(create_worker_client(channel));
        }
        let mut endpoint = Channel::from_shared(key.clone())
            .map_err(|err| DataFusionError::External(Box::new(err)))?;
        if self.transport.tls() {
            let tls = crate::cluster::tls::tonic_client_tls()
                .map_err(|err| DataFusionError::External(Box::new(err)))?;
            endpoint = endpoint
                .tls_config(tls)
                .map_err(|err| DataFusionError::External(Box::new(err)))?;
        }
        let channel = endpoint
            .connect()
            .await
            .map_err(|err| DataFusionError::External(Box::new(err)))?;
        let boxed = BoxCloneSyncChannel::new(channel);
        self.cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(key, boxed.clone());
        Ok(create_worker_client(boxed))
    }
}

fn watch_shutdown(mut stop_rx: watch::Receiver<bool>) -> impl std::future::Future<Output = ()> {
    async move {
        loop {
            if stop_rx.changed().await.is_err() {
                break;
            }
            if *stop_rx.borrow() {
                break;
            }
        }
    }
}

fn worker_url(addr: SocketAddr, transport: WorkerTransport) -> Result<Url, DurableError> {
    Url::parse(&format!("{}://{addr}", transport.scheme()))
        .map_err(|err| DurableError::Io(err.to_string()))
}

async fn build_worker_session(ctx: WorkerQueryContext) -> Result<SessionState, DataFusionError> {
    let state = ctx
        .builder
        .with_default_features()
        .with_distributed_user_codec(SkipprHostCodec::default())
        .build();
    let session = SessionContext::from(state);
    udfs::register_observability_udfs(&session);
    Ok(session.state().clone())
}

fn coordinating_session_builder(
    resolver: impl WorkerResolver + 'static,
    worker: &Worker,
    self_url: Url,
    transport: WorkerTransport,
) -> SessionStateBuilder {
    SessionStateBuilder::new()
        .with_config(SessionConfig::new())
        .with_default_features()
        .with_distributed_planner()
        .with_distributed_user_codec(SkipprHostCodec::default())
        .with_distributed_worker_resolver(resolver)
        .with_distributed_channel_resolver(SkipprChannelResolver::new(transport))
        .with_distributed_local_worker_context(worker.to_local_worker_context(self_url))
}

pub fn query_context() -> datafusion::error::Result<SessionContext> {
    RUNTIME
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|rt| rt.ctx.clone()))
        .ok_or_else(|| DataFusionError::Execution("QueryRuntime is not started".into()))
}

/// Per-query catalog list. Planner, codec, and UDFs stay on QueryRuntime.
pub fn request_context() -> datafusion::error::Result<SessionContext> {
    use datafusion::catalog::memory::{
        MemoryCatalogProvider, MemoryCatalogProviderList, MemorySchemaProvider,
    };
    use datafusion::catalog::{CatalogProvider, CatalogProviderList};
    let base = query_context()?.state();
    let opts = base.config().options().catalog.clone();
    let schema = Arc::new(MemorySchemaProvider::new());
    let catalog = Arc::new(MemoryCatalogProvider::new());
    catalog.register_schema(&opts.default_schema, schema)?;
    let catalogs = Arc::new(MemoryCatalogProviderList::new());
    catalogs.register_catalog(opts.default_catalog.clone(), catalog);
    Ok(SessionContext::new_with_state(
        SessionStateBuilder::new_from_existing(base)
            .with_catalog_list(catalogs)
            .build(),
    ))
}

pub fn advertised_worker() -> Option<SocketAddr> {
    RUNTIME
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|rt| rt.advertised_worker))
}

pub fn worker_bind() -> Option<SocketAddr> {
    RUNTIME
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|rt| rt.worker_bind))
}

/// Bind an in-process df-dist Worker and install the coordinating SessionContext.
/// Local `skipprd query` uses plaintext. Clustered skipprd uses [`start_clustered`].
pub async fn start(advertised_ip: IpAddr) -> Result<(), DurableError> {
    bind_worker(advertised_ip, WorkerTransport::LocalPlaintext).await
}

/// Clustered QueryRuntime: Worker gRPC is cluster mTLS, fail-closed.
pub async fn start_clustered(advertised_ip: IpAddr) -> Result<(), DurableError> {
    bind_worker(advertised_ip, WorkerTransport::ClusterMtls).await
}

async fn bind_worker(
    advertised_ip: IpAddr,
    transport: WorkerTransport,
) -> Result<(), DurableError> {
    drain().await;
    let listener = TcpListener::bind("0.0.0.0:0")
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let worker_bind = listener
        .local_addr()
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let advertised_worker = SocketAddr::new(advertised_ip, worker_bind.port());
    let self_url = worker_url(advertised_worker, transport)?;
    let resolver = SkipprWorkerResolver {
        self_url: self_url.clone(),
    };
    let worker = Worker::from_session_builder(build_worker_session);
    let ctx = SessionContext::from(
        coordinating_session_builder(resolver, &worker, self_url, transport).build(),
    );
    udfs::register_observability_udfs(&ctx);
    let (stop, stop_rx) = watch::channel(false);
    let mut server = tonic::transport::Server::builder();
    if transport.tls() {
        let tls = crate::cluster::tls::tonic_server_tls()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        server = server
            .tls_config(tls)
            .map_err(|err| DurableError::Io(err.to_string()))?;
    }
    let worker_svc = worker.into_worker_server();
    let worker_task = tokio::spawn(async move {
        let _ = server
            .add_service(worker_svc)
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), watch_shutdown(stop_rx))
            .await;
    });
    *RUNTIME.write().unwrap_or_else(|err| err.into_inner()) = Some(QueryRuntime {
        ctx,
        advertised_worker,
        worker_bind,
        stop,
        tasks: vec![worker_task],
    });
    tracing::info!(worker = %advertised_worker, transport = ?transport, "QueryRuntime started");
    Ok(())
}

pub async fn ensure_started() -> Result<(), DurableError> {
    if query_context().is_ok() {
        return Ok(());
    }
    start(IpAddr::from(std::net::Ipv4Addr::LOCALHOST)).await
}

pub async fn drain() {
    let runtime = match RUNTIME.write() {
        Ok(mut guard) => guard.take(),
        Err(err) => err.into_inner().take(),
    };
    if let Some(runtime) = runtime {
        let _ = runtime.stop.send_replace(true);
        for task in runtime.tasks {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[tokio::test]
    async fn request_context_isolates_same_namespace_providers() {
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::datasource::MemTable;

        let _guard = TEST_LOCK.lock().await;
        drain().await;
        start("127.0.0.1".parse().unwrap()).await.unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let a = request_context().unwrap();
        let b = request_context().unwrap();
        let p1 = Arc::new(MemTable::try_new(schema.clone(), vec![vec![]]).unwrap());
        let p2 = Arc::new(MemTable::try_new(schema, vec![vec![]]).unwrap());
        let a_task = {
            let a = a.clone();
            let p1 = p1.clone();
            tokio::spawn(async move { a.register_table("ns", p1) })
        };
        let b_task = {
            let b = b.clone();
            let p2 = p2.clone();
            tokio::spawn(async move { b.register_table("ns", p2) })
        };
        a_task.await.unwrap().unwrap();
        b_task.await.unwrap().unwrap();
        a.sql("SELECT * FROM ns").await.unwrap();
        b.sql("SELECT * FROM ns").await.unwrap();
        assert!(
            query_context()
                .unwrap()
                .sql("SELECT * FROM ns")
                .await
                .is_err(),
            "shared QueryRuntime catalog must stay empty"
        );
        drain().await;
    }

    #[tokio::test]
    async fn query_context_fails_closed_without_runtime() {
        let _guard = TEST_LOCK.lock().await;
        drain().await;
        assert!(query_context().is_err());
        start("127.0.0.1".parse().unwrap()).await.unwrap();
        assert!(query_context().is_ok());
        drain().await;
        assert!(query_context().is_err());
        assert!(advertised_worker().is_none());
    }

    #[tokio::test]
    async fn start_binds_unspecified_worker_and_advertised_host() {
        let _guard = TEST_LOCK.lock().await;
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        start(ip).await.unwrap();
        let bind = worker_bind().expect("worker bind");
        assert!(bind.ip().is_unspecified());
        assert_ne!(bind.port(), 0);
        let advertised = advertised_worker().expect("advertised worker");
        assert_eq!(advertised.ip(), ip);
        assert_eq!(advertised.port(), bind.port());
        assert!(query_context().is_ok());
        drain().await;
        assert!(advertised_worker().is_none());
        assert!(query_context().is_err());
    }

    #[test]
    fn observability_udfs_register_on_query_runtime() {
        let src = include_str!("runtime.rs");
        assert!(src.contains("register_observability_udfs"));
        assert!(src.contains("with_distributed_planner"));
        assert!(src.contains("with_distributed_local_worker_context"));
        assert!(
            src.contains("coordinating_session_builder(resolver"),
            "start() must use coordinating_session_builder"
        );
        assert!(
            src.contains("with_distributed_channel_resolver"),
            "QueryRuntime must install SkipprChannelResolver"
        );
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            prod.contains("tonic_client_tls"),
            "worker gRPC clients must use cluster mTLS when TLS is required"
        );
        assert!(
            prod.contains("WorkerTransport::LocalPlaintext"),
            "local skipprd query must start QueryRuntime without cluster mTLS"
        );
        assert!(
            prod.contains("WorkerTransport::ClusterMtls"),
            "clustered QueryRuntime must fail-closed on cluster mTLS"
        );
        assert!(
            prod.contains("ensure_started") && prod.contains("LocalPlaintext"),
            "ensure_started (skipprd query) is local plaintext, not cluster TLS"
        );
        assert!(
            include_str!("../cluster/scheduler.rs").contains("start_clustered"),
            "run_clustered must start QueryRuntime with cluster mTLS"
        );
        assert!(!prod.contains("SessionContext::remote"));
        assert!(!prod.contains("elect_scheduler"));
        assert!(!prod.contains("ballista"));
    }

    #[tokio::test]
    async fn coordinating_select_one_uses_query_runtime() {
        let _guard = TEST_LOCK.lock().await;
        drain().await;
        start("127.0.0.1".parse().unwrap()).await.unwrap();
        let ctx = query_context().expect("runtime");
        let batches = ctx.sql("SELECT 1").await.unwrap().collect().await.unwrap();
        assert_eq!(batches[0].num_rows(), 1);
        drain().await;
    }

    #[tokio::test]
    async fn local_start_does_not_require_cluster_tls_env() {
        let _guard = TEST_LOCK.lock().await;
        drain().await;
        let cert = std::env::var("SKIPPR_CLUSTER_TLS_CERT").ok();
        let key = std::env::var("SKIPPR_CLUSTER_TLS_KEY").ok();
        let ca = std::env::var("SKIPPR_CLUSTER_TLS_CA").ok();
        std::env::remove_var("SKIPPR_CLUSTER_TLS_CERT");
        std::env::remove_var("SKIPPR_CLUSTER_TLS_KEY");
        std::env::remove_var("SKIPPR_CLUSTER_TLS_CA");
        let started = start("127.0.0.1".parse().unwrap()).await;
        let select_ok = if started.is_ok() {
            let ctx = query_context().expect("runtime");
            ctx.sql("SELECT 1").await.unwrap().collect().await.unwrap()[0].num_rows() == 1
        } else {
            false
        };
        drain().await;
        if let Some(value) = cert {
            std::env::set_var("SKIPPR_CLUSTER_TLS_CERT", value);
        }
        if let Some(value) = key {
            std::env::set_var("SKIPPR_CLUSTER_TLS_KEY", value);
        }
        if let Some(value) = ca {
            std::env::set_var("SKIPPR_CLUSTER_TLS_CA", value);
        }
        started.expect("local QueryRuntime must start without cluster TLS PEMs");
        assert!(select_ok);
    }

    struct TwoWorkerResolver(Vec<Url>);

    impl WorkerResolver for TwoWorkerResolver {
        fn get_urls(&self) -> Result<Vec<Url>, DataFusionError> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn two_worker_resolver_plans_network_boundary() {
        use datafusion::arrow::array::{Int64Array, StringArray};
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::arrow::record_batch::RecordBatch;
        use std::sync::Arc;

        let schema = Arc::new(Schema::new(vec![
            Field::new("n", DataType::Utf8, true),
            Field::new("v", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["a", "b", "a", "b"])),
                Arc::new(Int64Array::from(vec![1, 2, 3, 4])),
            ],
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parquet");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut writer = parquet::arrow::ArrowWriter::try_new(file, schema, None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
        }
        let urls = vec![
            Url::parse("http://127.0.0.1:9").unwrap(),
            Url::parse("http://127.0.0.1:10").unwrap(),
        ];
        let self_urls = SkipprWorkerResolver {
            self_url: urls[0].clone(),
        }
        .get_urls()
        .unwrap();
        assert_eq!(self_urls, vec![urls[0].clone()]);
        let worker = Worker::from_session_builder(build_worker_session);
        // Test-only estimator: tiny parquet must still fan out so the shared
        // coordinating planner inserts a network boundary. Production auto-tunes.
        let ctx = SessionContext::from(
            coordinating_session_builder(
                TwoWorkerResolver(urls.clone()),
                &worker,
                urls[0].clone(),
                WorkerTransport::LocalPlaintext,
            )
            .with_distributed_file_scan_config_bytes_per_partition(1)
            .unwrap()
            .build(),
        );
        ctx.register_parquet("t", path.to_str().unwrap(), Default::default())
            .await
            .unwrap();
        let plan = ctx
            .sql("SELECT n, count(*) FROM t GROUP BY n")
            .await
            .unwrap()
            .create_physical_plan()
            .await
            .unwrap();
        let display = datafusion_distributed::display_plan_ascii(plan.as_ref(), false);
        assert!(
            display.contains("NetworkCoalesceExec") || display.contains("NetworkShuffleExec"),
            "two-worker QueryRuntime plan must insert a network boundary, got:\n{display}"
        );
    }

    #[tokio::test]
    async fn skippr_channel_resolver_connects_to_local_worker() {
        let _guard = TEST_LOCK.lock().await;
        drain().await;
        start("127.0.0.1".parse().unwrap()).await.unwrap();
        let url = worker_url(
            advertised_worker().expect("advertised worker"),
            WorkerTransport::LocalPlaintext,
        )
        .unwrap();
        assert_eq!(url.scheme(), "http");
        SkipprChannelResolver::new(WorkerTransport::LocalPlaintext)
            .get_worker_client_for_url(&url)
            .await
            .expect("plaintext local worker channel");
        drain().await;
    }
}
