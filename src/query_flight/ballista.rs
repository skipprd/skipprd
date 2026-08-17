use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ballista::extension::SessionContextExt;
use ballista_core::extension::{EndpointOverrideFn, SessionConfigExt};
use ballista_core::serde::protobuf::{
    scheduler_grpc_client::SchedulerGrpcClient, scheduler_grpc_server::SchedulerGrpcServer,
    ExecutorRegistration,
};
use ballista_core::serde::scheduler::{
    ExecutorOperatingSystemSpecification, ExecutorSpecification,
};
use ballista_core::serde::BallistaCodec;
use ballista_core::utils::{create_grpc_server, GrpcServerConfig};
use ballista_executor::execution_loop;
use ballista_executor::executor::Executor;
use ballista_executor::flight_service::BallistaFlightService;
use ballista_executor::metrics::LoggingMetricsCollector;
use ballista_scheduler::cluster::BallistaCluster;
use ballista_scheduler::config::SchedulerConfig;
use ballista_scheduler::metrics::default_metrics_collector;
use ballista_scheduler::scheduler_server::SchedulerServer;
use datafusion::execution::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use datafusion_proto::protobuf::{LogicalPlanNode, PhysicalPlanNode};
use skippr_lease::{DurableError, NodeId, CONTROL_FRAME_MAX_BYTES};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_stream::wrappers::TcpListenerStream;
use uuid::Uuid;

use crate::cluster::gossip::elect_scheduler_live;
use crate::query_flight::codec::{SkipprHostCodec, SkipprLogicalCodec};

type PlanCodec = BallistaCodec<LogicalPlanNode, PhysicalPlanNode>;

struct QueryRuntime {
    ctx: SessionContext,
    advertised_scheduler: SocketAddr,
    elected: SocketAddr,
    local_node: Option<NodeId>,
    executor_host: String,
    scheduler_bind: SocketAddr,
    executor: Option<Arc<Executor>>,
    executor_codec: Option<PlanCodec>,
    session_state: datafusion::execution::SessionState,
    poll_task: Option<tokio::task::JoinHandle<()>>,
    stop: watch::Sender<bool>,
    _work_dir: Option<tempfile::TempDir>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

static RUNTIME: RwLock<Option<QueryRuntime>> = RwLock::new(None);

pub fn query_context() -> datafusion::error::Result<SessionContext> {
    RUNTIME
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|rt| rt.ctx.clone()))
        .ok_or_else(|| {
            datafusion::error::DataFusionError::Execution(
                "Ballista query runtime is not started".into(),
            )
        })
}

pub fn advertised_scheduler() -> Option<SocketAddr> {
    RUNTIME
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|rt| rt.advertised_scheduler))
}

pub fn executor_host() -> Option<String> {
    RUNTIME
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|rt| rt.executor_host.clone()))
}

pub fn scheduler_bind() -> Option<SocketAddr> {
    RUNTIME
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|rt| rt.scheduler_bind))
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

fn grpc_url(addr: SocketAddr) -> String {
    format!("https://{addr}")
}

fn cluster_endpoint_override() -> Result<EndpointOverrideFn, DurableError> {
    Ok(std::sync::Arc::new(
        |endpoint: tonic::transport::Endpoint| {
            let uri = endpoint.uri().clone();
            let host = uri.host().unwrap_or("127.0.0.1");
            let port = uri.port_u16().unwrap_or(443);
            let https = format!("https://{host}:{port}");
            let tls = crate::cluster::tls::tonic_client_tls()
                .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> { err.into() })?;
            tonic::transport::Endpoint::from_shared(https)
                .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> { err.into() })?
                .tls_config(tls)
                .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> { err.into() })
        },
    ))
}

fn tls_grpc_server() -> Result<tonic::transport::Server, DurableError> {
    create_grpc_server(&GrpcServerConfig::default())
        .tls_config(
            crate::cluster::tls::tonic_server_tls()
                .map_err(|err| DurableError::Io(err.to_string()))?,
        )
        .map_err(|err| DurableError::Io(err.to_string()))
}

async fn connect_scheduler(
    url: String,
) -> Result<SchedulerGrpcClient<tonic::transport::Channel>, DurableError> {
    let tls =
        crate::cluster::tls::tonic_client_tls().map_err(|err| DurableError::Io(err.to_string()))?;
    let channel = tonic::transport::Endpoint::from_shared(url)
        .map_err(|err| DurableError::Io(err.to_string()))?
        .tls_config(tls)
        .map_err(|err| DurableError::Io(err.to_string()))?
        .connect()
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    Ok(SchedulerGrpcClient::new(channel))
}

fn df_url(addr: SocketAddr) -> String {
    format!("df://{addr}")
}

async fn scheduler_reachable(addr: SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(200), TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

fn should_reconnect(winner: SocketAddr, elected: SocketAddr, elected_live: bool) -> bool {
    winner != elected || !elected_live
}

async fn live_winner(local_node: NodeId, local_sched: SocketAddr) -> SocketAddr {
    let ads = match crate::cluster::gossip::gossip_directory() {
        Some(gossip) => gossip.known_ads().await,
        None => Vec::new(),
    };
    let mut live = std::collections::HashSet::from([local_sched]);
    for ad in &ads {
        if let Some(addr) = ad.scheduler {
            if addr != local_sched && scheduler_reachable(addr).await {
                live.insert(addr);
            }
        }
    }
    elect_scheduler_live(local_node, local_sched, &ads, |addr| live.contains(&addr))
}

/// Reconnect the session if the elected scheduler is dead or a lower `NodeId` stole.
pub async fn ensure_elected_live() -> Result<(), DurableError> {
    let (local_node, local_sched, elected) = {
        let guard = RUNTIME
            .read()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let Some(rt) = guard.as_ref() else {
            return Ok(());
        };
        let Some(local_node) = rt.local_node else {
            return Ok(());
        };
        (local_node, rt.advertised_scheduler, rt.elected)
    };
    let winner = live_winner(local_node, local_sched).await;
    let elected_live = scheduler_reachable(elected).await;
    if should_reconnect(winner, elected, elected_live) {
        reconnect(winner).await?;
    }
    Ok(())
}

/// Bind a local scheduler+executor on `0.0.0.0` and connect to self as the elected scheduler.
pub async fn start(advertised_ip: IpAddr) -> Result<(), DurableError> {
    drain().await;
    let _ = tls_grpc_server()?;
    let _ =
        crate::cluster::tls::tonic_client_tls().map_err(|err| DurableError::Io(err.to_string()))?;
    let codec = Arc::new(SkipprHostCodec::default());
    let endpoint_override = cluster_endpoint_override()?;
    let config = SessionConfig::new_with_ballista()
        .with_ballista_logical_extension_codec(Arc::new(SkipprLogicalCodec::default()))
        .with_ballista_physical_extension_codec(codec.clone())
        .with_ballista_grpc_client_max_message_size(CONTROL_FRAME_MAX_BYTES)
        .with_ballista_use_tls(true)
        .with_ballista_override_create_grpc_client_endpoint(endpoint_override.clone());
    let state = SessionStateBuilder::new()
        .with_config(config.clone())
        .with_default_features()
        .build();

    let logical = state.config().ballista_logical_extension_codec();
    let physical = state.config().ballista_physical_extension_codec();
    let scheduler_codec: PlanCodec = BallistaCodec::new(logical.clone(), physical.clone());
    let executor_codec: PlanCodec = BallistaCodec::new(logical, physical);
    let session_config = state.config().clone();
    let session_state = state.clone();
    let session_builder = Arc::new(move |_: SessionConfig| Ok(session_state.clone()));
    let config_producer = Arc::new(move || session_config.clone());
    let scheduler_listener = TcpListener::bind("0.0.0.0:0")
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let scheduler_bind = scheduler_listener
        .local_addr()
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let advertised_scheduler = SocketAddr::new(advertised_ip, scheduler_bind.port());
    let cluster = BallistaCluster::new_memory(
        &advertised_scheduler.to_string(),
        session_builder,
        config_producer,
    );
    let metrics_collector =
        default_metrics_collector().map_err(|err| DurableError::Io(err.to_string()))?;
    let mut scheduler_server: SchedulerServer<LogicalPlanNode, PhysicalPlanNode> =
        SchedulerServer::new(
            advertised_scheduler.to_string(),
            cluster,
            scheduler_codec,
            Arc::new(
                SchedulerConfig::default()
                    .with_scheduler_policy(ballista_core::config::TaskSchedulingPolicy::PullStaged)
                    .with_use_tls(true)
                    .with_override_create_grpc_client_endpoint(endpoint_override),
            ),
            metrics_collector,
        );
    scheduler_server
        .init()
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let grpc_config = config.clone();
    let scheduler_svc = SchedulerGrpcServer::new(scheduler_server)
        .max_decoding_message_size(grpc_config.ballista_grpc_client_max_message_size())
        .max_encoding_message_size(grpc_config.ballista_grpc_client_max_message_size());
    let (stop, stop_rx) = watch::channel(false);
    let scheduler_stop = stop_rx.clone();
    let scheduler_task = tokio::spawn(async move {
        let Ok(mut server) = tls_grpc_server() else {
            return;
        };
        let _ = server
            .add_service(scheduler_svc)
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(scheduler_listener),
                watch_shutdown(scheduler_stop),
            )
            .await;
    });

    let scheduler_url = grpc_url(advertised_scheduler);
    let deadline = Instant::now() + Duration::from_secs(5);
    let scheduler = loop {
        match connect_scheduler(scheduler_url.clone()).await {
            Ok(client) => break client,
            Err(err) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = err;
            }
            Err(err) => {
                return Err(DurableError::Io(format!(
                    "Ballista scheduler did not become ready: {err}"
                )));
            }
        }
    };

    let listener = TcpListener::bind("0.0.0.0:0")
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let executor_bind = listener
        .local_addr()
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let concurrent_tasks = config.ballista_standalone_parallelism();
    let executor_host = advertised_ip.to_string();
    let executor_meta = ExecutorRegistration {
        id: Uuid::new_v4().to_string(),
        host: Some(executor_host.clone()),
        port: executor_bind.port() as u32,
        grpc_port: executor_bind.port() as u32,
        specification: Some(
            ExecutorSpecification::default()
                .with_task_slots(concurrent_tasks as u32)
                .into(),
        ),
        os_info: Some(ExecutorOperatingSystemSpecification::default().into()),
    };
    let runtime = state.runtime_env().clone();
    let config_for_exec = config.clone();
    let config_producer: ballista_core::ConfigProducer = Arc::new(move || config_for_exec.clone());
    let runtime_producer: ballista_core::RuntimeProducer = Arc::new(move |_| Ok(runtime.clone()));
    let work = tempfile::TempDir::new().map_err(|err| DurableError::Io(err.to_string()))?;
    let work_dir = work
        .path()
        .to_str()
        .ok_or_else(|| DurableError::Io("ballista work_dir is not utf-8".into()))?
        .to_string();
    let function_registry = ballista_core::registry::BallistaFunctionRegistry::from(&state);
    let executor = Arc::new(Executor::with_default_execution_engine(
        executor_meta,
        &work_dir,
        runtime_producer,
        config_producer,
        Arc::new(function_registry),
        Arc::new(LoggingMetricsCollector::default()),
        concurrent_tasks,
    ));
    let service = BallistaFlightService::new(work_dir);
    let server = arrow_flight::flight_service_server::FlightServiceServer::new(service)
        .max_decoding_message_size(CONTROL_FRAME_MAX_BYTES)
        .max_encoding_message_size(CONTROL_FRAME_MAX_BYTES);
    let executor_stop = stop_rx.clone();
    let executor_task = tokio::spawn(async move {
        let Ok(mut grpc) = tls_grpc_server() else {
            return;
        };
        let _ = grpc
            .add_service(server)
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                watch_shutdown(executor_stop),
            )
            .await;
    });
    let poll_executor = executor.clone();
    let poll_codec = executor_codec.clone();
    let poll_task = tokio::spawn(async move {
        let _ = execution_loop::poll_loop(scheduler, poll_executor, poll_codec).await;
    });

    let ctx = SessionContext::remote_with_state(&df_url(advertised_scheduler), state.clone())
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    *RUNTIME
        .write()
        .map_err(|err| DurableError::Io(err.to_string()))? = Some(QueryRuntime {
        ctx,
        advertised_scheduler,
        elected: advertised_scheduler,
        local_node: None,
        executor_host,
        scheduler_bind,
        executor: Some(executor),
        executor_codec: Some(executor_codec),
        session_state: state,
        poll_task: Some(poll_task),
        stop,
        _work_dir: Some(work),
        tasks: vec![scheduler_task, executor_task],
    });
    tracing::info!(
        elected_scheduler = %advertised_scheduler,
        scheduler_bind = %scheduler_bind,
        "clustered Ballista connected"
    );
    Ok(())
}

pub fn spawn_election_watch(local_node: NodeId) {
    if let Ok(mut guard) = RUNTIME.write() {
        if let Some(rt) = guard.as_mut() {
            rt.local_node = Some(local_node);
        }
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let snapshot = match RUNTIME.read() {
                Ok(guard) => guard
                    .as_ref()
                    .map(|rt| (rt.advertised_scheduler, rt.elected, rt.stop.subscribe())),
                Err(_) => None,
            };
            let Some((local_sched, elected, stop_rx)) = snapshot else {
                break;
            };
            if *stop_rx.borrow() {
                break;
            }
            let winner = live_winner(local_node, local_sched).await;
            let elected_live = scheduler_reachable(elected).await;
            if should_reconnect(winner, elected, elected_live) {
                if let Err(err) = reconnect(winner).await {
                    tracing::warn!(
                        error = %err,
                        elected_scheduler = %winner,
                        "Ballista scheduler reconnect failed"
                    );
                }
            }
        }
    });
}

async fn reconnect(elected: SocketAddr) -> Result<(), DurableError> {
    let (executor, codec, state, already) = {
        let guard = RUNTIME
            .read()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let rt = guard
            .as_ref()
            .ok_or_else(|| DurableError::Io("Ballista runtime is not started".into()))?;
        let executor = rt
            .executor
            .clone()
            .ok_or_else(|| DurableError::Io("Ballista executor is not started".into()))?;
        let codec = rt
            .executor_codec
            .clone()
            .ok_or_else(|| DurableError::Io("Ballista executor codec is not started".into()))?;
        (
            executor,
            codec,
            rt.session_state.clone(),
            rt.elected == elected,
        )
    };
    if already && scheduler_reachable(elected).await {
        return Ok(());
    }
    if !scheduler_reachable(elected).await {
        return Err(DurableError::Io(format!(
            "Ballista scheduler is unreachable: {elected}"
        )));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let client = loop {
        match connect_scheduler(grpc_url(elected)).await {
            Ok(client) => break client,
            Err(err) if Instant::now() < deadline && scheduler_reachable(elected).await => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = err;
            }
            Err(err) => {
                return Err(DurableError::Io(format!(
                    "Ballista scheduler reconnect failed: {err}"
                )));
            }
        }
    };
    let ctx = SessionContext::remote_with_state(&df_url(elected), state)
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let poll_task = tokio::spawn(async move {
        let _ = execution_loop::poll_loop(client, executor, codec).await;
    });
    {
        let mut guard = RUNTIME
            .write()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let rt = guard
            .as_mut()
            .ok_or_else(|| DurableError::Io("Ballista runtime is not started".into()))?;
        if let Some(previous) = rt.poll_task.take() {
            previous.abort();
        }
        rt.poll_task = Some(poll_task);
        rt.ctx = ctx;
        rt.elected = elected;
    }
    tracing::info!(elected_scheduler = %elected, "clustered Ballista connected");
    Ok(())
}

pub async fn drain() {
    let runtime = match RUNTIME.write() {
        Ok(mut guard) => guard.take(),
        Err(err) => err.into_inner().take(),
    };
    if let Some(runtime) = runtime {
        let _ = runtime.stop.send_replace(true);
        if let Some(poll) = runtime.poll_task {
            poll.abort();
        }
        for task in runtime.tasks {
            task.abort();
        }
    }
}

pub fn install_query_context(ctx: SessionContext) {
    let (stop, _) = watch::channel(false);
    let previous = match RUNTIME.write() {
        Ok(mut guard) => guard.take(),
        Err(err) => err.into_inner().take(),
    };
    if let Some(previous) = previous {
        let _ = previous.stop.send_replace(true);
        if let Some(poll) = previous.poll_task {
            poll.abort();
        }
        for task in previous.tasks {
            task.abort();
        }
    }
    let dummy: SocketAddr = "127.0.0.1:0".parse().unwrap();
    *RUNTIME.write().unwrap_or_else(|err| err.into_inner()) = Some(QueryRuntime {
        ctx: ctx.clone(),
        advertised_scheduler: dummy,
        elected: dummy,
        local_node: None,
        executor_host: "127.0.0.1".into(),
        scheduler_bind: dummy,
        executor: None,
        executor_codec: None,
        session_state: ctx.state(),
        poll_task: None,
        stop,
        _work_dir: None,
        tasks: Vec::new(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[tokio::test]
    async fn query_context_fails_closed_without_runtime() {
        let _guard = TEST_LOCK.lock().await;
        drain().await;
        assert!(query_context().is_err());
        install_query_context(SessionContext::new());
        assert!(query_context().is_ok());
        drain().await;
        assert!(query_context().is_err());
        assert!(advertised_scheduler().is_none());
    }

    #[tokio::test]
    async fn start_binds_unspecified_scheduler_and_advertised_executor_host() {
        let _guard = TEST_LOCK.lock().await;
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        start(ip).await.unwrap();
        let bind = scheduler_bind().expect("scheduler bind");
        assert!(bind.ip().is_unspecified());
        assert_ne!(bind.port(), 0);
        let advertised = advertised_scheduler().expect("advertised scheduler");
        assert_eq!(advertised.ip(), ip);
        assert_eq!(advertised.port(), bind.port());
        assert_eq!(executor_host().as_deref(), Some("127.0.0.1"));
        assert!(query_context().is_ok());
        drain().await;
        assert!(advertised_scheduler().is_none());
        assert!(query_context().is_err());
    }

    #[test]
    fn reconnects_when_elected_scheduler_is_dead() {
        let local: SocketAddr = "127.0.0.1:5".parse().unwrap();
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(should_reconnect(local, dead, false));
        assert!(!should_reconnect(local, local, true));
        assert!(should_reconnect(local, local, false));
    }
}
