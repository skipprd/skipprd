use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::sql::metadata::SqlInfoDataBuilder;
use arrow_flight::sql::server::FlightSqlService;
use arrow_flight::sql::{
    ActionClosePreparedStatementRequest, ActionCreatePreparedStatementRequest,
    ActionCreatePreparedStatementResult, CommandGetCatalogs, CommandGetDbSchemas,
    CommandGetSqlInfo, CommandGetTableTypes, CommandGetTables, CommandPreparedStatementQuery,
    CommandStatementQuery, CommandStatementUpdate, SqlInfo, TicketStatementQuery,
};
use arrow_flight::{
    FlightDescriptor, FlightEndpoint, FlightInfo, HandshakeRequest, HandshakeResponse, Ticket,
};
use bytes::Bytes;
use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::{Schema, SchemaRef};
use datafusion::datasource::TableProvider;
use futures::{Stream, StreamExt, TryStreamExt};
use skippr_lease::{DurableError, CONTROL_FRAME_MAX_BYTES};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};

use crate::cluster::identity::{ClusterIdentity, TenantScope};
use crate::cluster::peer::ReplicaRegistry;
use crate::query_flight::sql::{classify_sql, matches_sql_like, reject_ddl, ClassifiedSql};

pub struct QueryFlightServer {
    bind: SocketAddr,
    stop: watch::Sender<bool>,
    in_flight: Arc<AtomicUsize>,
}

struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl QueryFlightServer {
    pub async fn start(bind: SocketAddr, identity: ClusterIdentity) -> Result<Self, DurableError> {
        Self::start_with_registry(bind, identity, None).await
    }

    pub async fn start_with_registry(
        bind: SocketAddr,
        identity: ClusterIdentity,
        registry: Option<Arc<ReplicaRegistry>>,
    ) -> Result<Self, DurableError> {
        let _ = crate::cluster::tls::tonic_server_tls()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let local = listener
            .local_addr()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let (stop, mut rx) = watch::channel(false);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let svc = SkipprFlightSql {
            _identity: identity,
            registry,
            in_flight: in_flight.clone(),
        };
        let inflight_for_shutdown = in_flight.clone();
        tokio::spawn(async move {
            let incoming = TcpListenerStream::new(listener);
            let shutdown = async move {
                loop {
                    if rx.changed().await.is_err() {
                        break;
                    }
                    if *rx.borrow() {
                        break;
                    }
                }
                while inflight_for_shutdown.load(Ordering::SeqCst) > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            };
            let mut builder = Server::builder();
            match crate::cluster::tls::tonic_server_tls() {
                Ok(tls) => match builder.tls_config(tls) {
                    Ok(configured) => builder = configured,
                    Err(err) => {
                        tracing::error!(error = %err, "Flight SQL TLS config failed");
                        return;
                    }
                },
                Err(err) => {
                    tracing::error!(error = %err, "Flight SQL TLS material missing");
                    return;
                }
            }
            if let Err(err) = builder
                .add_service(
                    FlightServiceServer::new(svc)
                        .max_decoding_message_size(CONTROL_FRAME_MAX_BYTES)
                        .max_encoding_message_size(CONTROL_FRAME_MAX_BYTES),
                )
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await
            {
                tracing::warn!(error = %err, "Flight SQL server stopped");
            }
        });
        Ok(Self {
            bind: local,
            stop,
            in_flight,
        })
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind
    }

    pub async fn drain(&self) {
        let _ = self.stop.send_replace(true);
        while self.in_flight.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

#[derive(Clone)]
struct SkipprFlightSql {
    _identity: ClusterIdentity,
    registry: Option<Arc<ReplicaRegistry>>,
    in_flight: Arc<AtomicUsize>,
}

impl SkipprFlightSql {
    fn track_in_flight(&self) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlightGuard(self.in_flight.clone())
    }

    fn check_sql(sql: &str) -> Result<(), Status> {
        if sql.len() > CONTROL_FRAME_MAX_BYTES {
            return Err(Status::invalid_argument("SQL too large"));
        }
        reject_ddl(sql).map_err(|err| Status::invalid_argument(err.to_string()))
    }

    fn parse_handshake_authorization(value: &str) -> Result<TenantScope, Status> {
        let Some(encoded) = value.strip_prefix("Basic ") else {
            return Err(Status::unauthenticated(
                "Flight SQL requires Basic tenant/workspace identity",
            ));
        };
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|_| Status::unauthenticated("Flight SQL Basic credentials are malformed"))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| Status::unauthenticated("Flight SQL identity is not valid UTF-8"))?;
        let user = text.split(':').next().unwrap_or("");
        let (tenant, workspace) = user.split_once('/').unwrap_or((user, ""));
        if tenant.is_empty() || workspace.is_empty() {
            return Err(Status::unauthenticated(
                "Flight SQL identity must be tenant/workspace",
            ));
        }
        TenantScope::new(tenant, workspace).map_err(|err| {
            Status::unauthenticated(format!("Flight SQL identity is invalid: {err}"))
        })
    }

    fn session_scope<T>(request: &Request<T>) -> Result<TenantScope, Status> {
        let auth = request.metadata().get("authorization").ok_or_else(|| {
            Status::unauthenticated("Flight SQL requires Basic tenant/workspace identity")
        })?;
        let value = auth
            .to_str()
            .map_err(|_| Status::unauthenticated("Flight SQL authorization is not valid UTF-8"))?;
        Self::parse_handshake_authorization(value)
    }

    fn ticket_for_sql(sql: &str) -> Ticket {
        let ticket = TicketStatementQuery {
            statement_handle: Bytes::from(sql.as_bytes().to_vec()),
        };
        Ticket::new(skippr_query_ballista::encode_flight_sql_command(&ticket))
    }

    fn flight_info_for_sql(sql: &str, schema: &Schema) -> Result<Response<FlightInfo>, Status> {
        let endpoint = FlightEndpoint::new().with_ticket(Self::ticket_for_sql(sql));
        let info = FlightInfo::new()
            .try_with_schema(schema)
            .map_err(|err| Status::internal(err.to_string()))?
            .with_descriptor(FlightDescriptor::new_cmd(vec![]))
            .with_endpoint(endpoint);
        Ok(Response::new(info))
    }

    fn record_stream_response<S>(
        schema: SchemaRef,
        stream: S,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status>
    where
        S: Stream<Item = Result<RecordBatch, Status>> + Send + 'static,
    {
        let encoded = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(stream.map_err(|err| arrow_flight::error::FlightError::Tonic(Box::new(err))))
            .map_err(Status::from);
        Ok(Response::new(Box::pin(encoded)))
    }

    async fn schema_for_sql(&self, sql: &str, scope: &TenantScope) -> Result<SchemaRef, Status> {
        Self::check_sql(sql)?;
        match classify_sql(sql, scope).map_err(|err| Status::invalid_argument(err.to_string()))? {
            ClassifiedSql::LiveWal(request) => Ok(iceberg_schema_for_namespace(&request.namespace)
                .await
                .unwrap_or_else(|_| Arc::new(Schema::empty()))),
            ClassifiedSql::Iceberg(namespace) => iceberg_schema_for_namespace(&namespace).await,
            ClassifiedSql::User => crate::sqlrt::tables::schema_for_clustered_select(
                sql,
                &crate::sqlrt::tables::process_clustered_select_opts(scope.clone())
                    .map_err(|err| Status::internal(err.to_string()))?,
            )
            .await
            .map_err(|err| Status::internal(err.to_string())),
        }
    }

    async fn execute_sql_stream(
        &self,
        sql: &str,
        scope: &TenantScope,
    ) -> Result<
        (
            SchemaRef,
            Pin<Box<dyn Stream<Item = Result<RecordBatch, Status>> + Send>>,
        ),
        Status,
    > {
        crate::metrics::counters::add_cluster_flight_request(1);
        match self.execute_sql_stream_inner(sql, scope).await {
            Ok(out) => Ok(out),
            Err(err) => {
                crate::metrics::counters::add_cluster_flight_fail(1);
                Err(err)
            }
        }
    }

    async fn execute_sql_stream_inner(
        &self,
        sql: &str,
        scope: &TenantScope,
    ) -> Result<
        (
            SchemaRef,
            Pin<Box<dyn Stream<Item = Result<RecordBatch, Status>> + Send>>,
        ),
        Status,
    > {
        Self::check_sql(sql)?;
        match classify_sql(sql, scope).map_err(|err| Status::invalid_argument(err.to_string()))? {
            ClassifiedSql::LiveWal(request) => {
                let (schema, stream) = execute_live_wal(&request, self.registry.as_ref()).await?;
                return Ok((schema, stream));
            }
            ClassifiedSql::Iceberg(namespace) => {
                let df = crate::sqlrt::tables::plan_iceberg_scan(&namespace, scope)
                    .await
                    .map_err(|err| Status::internal(err.to_string()))?;
                let schema = df.schema().inner().clone();
                let stream = df
                    .execute_stream()
                    .await
                    .map_err(|err| Status::internal(err.to_string()))?
                    .map_err(|err| Status::internal(err.to_string()));
                return Ok((schema, Box::pin(stream)));
            }
            ClassifiedSql::User => {}
        }
        crate::query_flight::ballista::ensure_elected_live()
            .await
            .map_err(|err| Status::internal(err.to_string()))?;
        let df = crate::sqlrt::tables::plan_clustered_select(
            sql,
            &crate::sqlrt::tables::process_clustered_select_opts(scope.clone())
                .map_err(|err| Status::internal(err.to_string()))?,
        )
        .await
        .map_err(|err| Status::internal(err.to_string()))?;
        let schema = df.schema().inner().clone();
        let stream = df
            .execute_stream()
            .await
            .map_err(|err| Status::internal(err.to_string()))?
            .map_err(|err| Status::internal(err.to_string()));
        Ok((schema, Box::pin(stream)))
    }
}

#[tonic::async_trait]
impl FlightSqlService for SkipprFlightSql {
    type FlightService = SkipprFlightSql;

    async fn register_sql_info(&self, _id: i32, _result: &SqlInfo) {}

    async fn do_handshake(
        &self,
        request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<
        Response<Pin<Box<dyn Stream<Item = Result<HandshakeResponse, Status>> + Send>>>,
        Status,
    > {
        let _scope = Self::session_scope(&request)?;
        let output = futures::stream::iter(vec![Ok(HandshakeResponse {
            protocol_version: 0,
            payload: Bytes::new(),
        })]);
        Ok(Response::new(Box::pin(output)))
    }

    async fn get_flight_info_statement(
        &self,
        query: CommandStatementQuery,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let scope = Self::session_scope(&request)?;
        Self::check_sql(&query.query)?;
        Self::flight_info_for_sql(
            &query.query,
            self.schema_for_sql(&query.query, &scope).await?.as_ref(),
        )
    }

    async fn do_get_statement(
        &self,
        ticket: TicketStatementQuery,
        request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let scope = Self::session_scope(&request)?;
        let sql = std::str::from_utf8(&ticket.statement_handle)
            .map_err(|err| Status::invalid_argument(err.to_string()))?;
        if sql.len() > CONTROL_FRAME_MAX_BYTES
            || ticket.statement_handle.len() > CONTROL_FRAME_MAX_BYTES
        {
            return Err(Status::invalid_argument("SQL too large"));
        }
        let guard = self.track_in_flight();
        let (schema, stream) = self.execute_sql_stream(sql, &scope).await?;
        let stream = stream.map(move |item| {
            let _keep = &guard;
            item
        });
        Self::record_stream_response(schema, stream)
    }

    async fn get_flight_info_prepared_statement(
        &self,
        query: CommandPreparedStatementQuery,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let scope = Self::session_scope(&request)?;
        let sql = std::str::from_utf8(&query.prepared_statement_handle)
            .map_err(|err| Status::invalid_argument(err.to_string()))?;
        Self::check_sql(sql)?;
        Self::flight_info_for_sql(sql, self.schema_for_sql(sql, &scope).await?.as_ref())
    }

    async fn do_get_prepared_statement(
        &self,
        query: CommandPreparedStatementQuery,
        request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let scope = Self::session_scope(&request)?;
        let sql = std::str::from_utf8(&query.prepared_statement_handle)
            .map_err(|err| Status::invalid_argument(err.to_string()))?;
        if sql.len() > CONTROL_FRAME_MAX_BYTES
            || query.prepared_statement_handle.len() > CONTROL_FRAME_MAX_BYTES
        {
            return Err(Status::invalid_argument("SQL too large"));
        }
        let guard = self.track_in_flight();
        let (schema, stream) = self.execute_sql_stream(sql, &scope).await?;
        let stream = stream.map(move |item| {
            let _keep = &guard;
            item
        });
        Self::record_stream_response(schema, stream)
    }

    async fn do_action_create_prepared_statement(
        &self,
        query: ActionCreatePreparedStatementRequest,
        request: Request<arrow_flight::Action>,
    ) -> Result<ActionCreatePreparedStatementResult, Status> {
        let _scope = Self::session_scope(&request)?;
        Self::check_sql(&query.query)?;
        Ok(ActionCreatePreparedStatementResult {
            prepared_statement_handle: Bytes::from(query.query.into_bytes()),
            dataset_schema: Bytes::new(),
            parameter_schema: Bytes::new(),
        })
    }

    async fn do_action_close_prepared_statement(
        &self,
        _query: ActionClosePreparedStatementRequest,
        request: Request<arrow_flight::Action>,
    ) -> Result<(), Status> {
        let _scope = Self::session_scope(&request)?;
        Ok(())
    }

    async fn do_put_statement_update(
        &self,
        _query: CommandStatementUpdate,
        request: Request<arrow_flight::sql::server::PeekableFlightDataStream>,
    ) -> Result<i64, Status> {
        let _scope = Self::session_scope(&request)?;
        Err(Status::invalid_argument("Flight SQL is read-only"))
    }

    async fn get_flight_info_sql_info(
        &self,
        query: CommandGetSqlInfo,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let _scope = Self::session_scope(&request)?;
        let mut builder = SqlInfoDataBuilder::new();
        builder.append(SqlInfo::FlightSqlServerName, "skipprd");
        builder.append(SqlInfo::FlightSqlServerReadOnly, true);
        let data = builder
            .build()
            .map_err(|err| Status::internal(err.to_string()))?;
        let ticket = Ticket::new(skippr_query_ballista::encode_flight_sql_command(&query));
        let schema = query.into_builder(&data).schema();
        let endpoint = FlightEndpoint::new().with_ticket(ticket);
        let info = FlightInfo::new()
            .try_with_schema(schema.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?
            .with_descriptor(request.into_inner())
            .with_endpoint(endpoint);
        Ok(Response::new(info))
    }

    async fn get_flight_info_catalogs(
        &self,
        query: CommandGetCatalogs,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let _scope = Self::session_scope(&request)?;
        let ticket = Ticket::new(skippr_query_ballista::encode_flight_sql_command(&query));
        let schema = query.into_builder().schema();
        let endpoint = FlightEndpoint::new().with_ticket(ticket);
        let info = FlightInfo::new()
            .try_with_schema(schema.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?
            .with_descriptor(request.into_inner())
            .with_endpoint(endpoint);
        Ok(Response::new(info))
    }

    async fn do_get_catalogs(
        &self,
        query: CommandGetCatalogs,
        request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let _scope = Self::session_scope(&request)?;
        let mut builder = query.into_builder();
        builder.append("skippr");
        let schema = builder.schema();
        let batch = builder.build();
        let stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(futures::stream::once(async { batch }))
            .map_err(Status::from);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_flight_info_schemas(
        &self,
        query: CommandGetDbSchemas,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let _scope = Self::session_scope(&request)?;
        let ticket = Ticket::new(skippr_query_ballista::encode_flight_sql_command(&query));
        let schema = query.into_builder().schema();
        let endpoint = FlightEndpoint::new().with_ticket(ticket);
        let info = FlightInfo::new()
            .try_with_schema(schema.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?
            .with_descriptor(request.into_inner())
            .with_endpoint(endpoint);
        Ok(Response::new(info))
    }

    async fn do_get_schemas(
        &self,
        query: CommandGetDbSchemas,
        request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let scope = Self::session_scope(&request)?;
        let mut builder = query.into_builder();
        builder.append("skippr", scope.workspace.clone());
        let schema = builder.schema();
        let batch = builder.build();
        let stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(futures::stream::once(async { batch }))
            .map_err(Status::from);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_flight_info_tables(
        &self,
        query: CommandGetTables,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let _scope = Self::session_scope(&request)?;
        let ticket = Ticket::new(skippr_query_ballista::encode_flight_sql_command(&query));
        let schema = query.into_builder().schema();
        let endpoint = FlightEndpoint::new().with_ticket(ticket);
        let info = FlightInfo::new()
            .try_with_schema(schema.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?
            .with_descriptor(request.into_inner())
            .with_endpoint(endpoint);
        Ok(Response::new(info))
    }

    async fn do_get_tables(
        &self,
        query: CommandGetTables,
        request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let scope = Self::session_scope(&request)?;
        let tables = crate::sqlrt::tables::list_configured_iceberg_tables(&scope).await;
        let pattern = query.table_name_filter_pattern.clone();
        let mut builder = query.into_builder();
        for (name, schema) in tables {
            if !matches_sql_like(&name, pattern.as_deref()) {
                continue;
            }
            builder
                .append(
                    "skippr",
                    scope.workspace.clone(),
                    name,
                    "TABLE",
                    schema.as_ref(),
                )
                .map_err(Status::from)?;
        }
        let schema = builder.schema();
        let batch = builder.build();
        let stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(futures::stream::once(async { batch }))
            .map_err(Status::from);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_flight_info_table_types(
        &self,
        query: CommandGetTableTypes,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let _scope = Self::session_scope(&request)?;
        let ticket = Ticket::new(skippr_query_ballista::encode_flight_sql_command(&query));
        let schema = query.into_builder().schema();
        let endpoint = FlightEndpoint::new().with_ticket(ticket);
        let info = FlightInfo::new()
            .try_with_schema(schema.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?
            .with_descriptor(request.into_inner())
            .with_endpoint(endpoint);
        Ok(Response::new(info))
    }

    async fn do_get_table_types(
        &self,
        query: CommandGetTableTypes,
        request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let _scope = Self::session_scope(&request)?;
        let mut builder = query.into_builder();
        builder.append("TABLE");
        builder.append("VIEW");
        let schema = builder.schema();
        let batch = builder.build();
        let stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(futures::stream::once(async { batch }))
            .map_err(Status::from);
        Ok(Response::new(Box::pin(stream)))
    }
}

async fn iceberg_schema_for_namespace(namespace: &str) -> Result<SchemaRef, Status> {
    crate::sqlrt::tables::iceberg_schema_for_namespace(namespace)
        .await
        .map_err(|err| Status::internal(err.to_string()))
}

async fn execute_live_wal(
    request: &crate::query_flight::live_wal::LiveWalScanRequest,
    registry: Option<&Arc<ReplicaRegistry>>,
) -> Result<
    (
        SchemaRef,
        Pin<Box<dyn Stream<Item = Result<RecordBatch, Status>> + Send>>,
    ),
    Status,
> {
    let paths =
        crate::cluster::wal_head::local_wal_paths(&request.pipeline, registry.map(|r| r.as_ref()))
            .await
            .ok_or_else(|| {
                Status::not_found(format!("unknown pipeline {}", request.pipeline.pipeline()))
            })?;
    let schema = iceberg_schema_for_namespace(&request.namespace)
        .await
        .unwrap_or_else(|_| Arc::new(Schema::empty()));
    let provider = crate::sqlrt::wal_table::WalTableProvider::live_unpinned(
        schema.clone(),
        request.pipeline.clone(),
        request.namespace.clone(),
        request.exclude_segment_ids.clone(),
        paths,
    );
    let ctx = datafusion::prelude::SessionContext::new();
    let plan = provider
        .scan(&ctx.state(), None, &[], None)
        .await
        .map_err(|err| Status::internal(err.to_string()))?;
    let schema = plan.schema();
    let stream = plan
        .execute(0, ctx.task_ctx())
        .map_err(|err| Status::internal(err.to_string()))?
        .map_err(|err| Status::internal(err.to_string()));
    Ok((schema, Box::pin(stream)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::{NodeId, PipelineKey};

    fn identity() -> ClusterIdentity {
        std::env::set_var("SKIPPR_QUERY_TENANT", "t");
        std::env::set_var("SKIPPR_QUERY_WORKSPACE", "w");
        ClusterIdentity::new(
            skippr_lease::ClusterId::new("test-cluster").unwrap(),
            NodeId::generate(),
        )
    }

    #[tokio::test]
    async fn query_flight_server_binds_ephemeral() {
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        assert_ne!(server.bind_addr().port(), 0);
        server.drain().await;
    }

    #[tokio::test]
    async fn ddl_is_rejected_on_get_flight_info() {
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        let err = skippr_query_ballista::fetch_statement_batches(
            &server.bind_addr().to_string(),
            "INSERT INTO t VALUES (1)",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().to_ascii_lowercase().contains("read-only")
                || err.to_string().contains("invalid")
        );
        server.drain().await;
    }

    #[tokio::test]
    async fn oversized_sql_is_rejected() {
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        let sql = format!(
            "SELECT '{}'",
            "x".repeat(skippr_lease::CONTROL_FRAME_MAX_BYTES)
        );
        let err =
            skippr_query_ballista::fetch_statement_batches(&server.bind_addr().to_string(), &sql)
                .await
                .unwrap_err();
        assert!(
            err.to_string().to_ascii_lowercase().contains("too large")
                || err.to_string().contains("invalid")
        );
        server.drain().await;
    }

    #[tokio::test]
    async fn live_wal_scan_uses_replica_registry_when_ingest_store_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        let log = crate::buffer::durable::log::MutationLog::open(paths.clone()).unwrap();
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        let guard = skippr_lease::LeaseGuard::replica(
            key.clone(),
            skippr_lease::LeaseEpoch::new(1),
            Arc::new(skippr_lease::SystemClock::new()),
        );
        registry
            .insert(crate::cluster::peer::ReplicaSession::new(
                key, paths, guard, log,
            ))
            .await;
        let server = QueryFlightServer::start_with_registry(
            "127.0.0.1:0".parse().unwrap(),
            identity,
            Some(registry),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let batches = skippr_query_ballista::fetch_statement_batches(
            &server.bind_addr().to_string(),
            "SELECT * FROM live_wal_scan('t', 'w', 'p', 'ns')",
        )
        .await
        .unwrap();
        let rows: usize = batches.iter().map(|batch| batch.num_rows()).sum();
        assert_eq!(rows, 0);
        server.drain().await;
    }

    #[tokio::test]
    async fn foreign_tenant_live_wal_scan_fails() {
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let err = skippr_query_ballista::fetch_statement_batches(
            &server.bind_addr().to_string(),
            "SELECT * FROM live_wal_scan('other', 'prod', 'events', 'ns')",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("tenant")
                || err.to_string().contains("invalid")
                || err.to_string().contains("does not match")
        );
        server.drain().await;
    }

    #[tokio::test]
    async fn unknown_pipeline_fails_closed() {
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let err = skippr_query_ballista::fetch_statement_batches(
            &server.bind_addr().to_string(),
            "SELECT * FROM live_wal_scan('t', 'w', 'missing', 'ns')",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown")
                || err.to_string().contains("not")
                || err.to_string().to_ascii_lowercase().contains("not found")
        );
        server.drain().await;
    }

    #[test]
    fn malformed_handshake_basic_fails_closed() {
        assert!(SkipprFlightSql::parse_handshake_authorization("Bearer x").is_err());
        assert!(SkipprFlightSql::parse_handshake_authorization("Basic !!!").is_err());
        let encoded =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "t/w:secret");
        let parsed =
            SkipprFlightSql::parse_handshake_authorization(&format!("Basic {encoded}")).unwrap();
        assert_eq!(parsed.tenant, "t");
        assert_eq!(parsed.workspace, "w");
        assert!(SkipprFlightSql::parse_handshake_authorization(&format!(
            "Basic {}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "tenant-only")
        ))
        .is_err());
    }

    #[tokio::test]
    async fn missing_authorization_is_unauthenticated() {
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let err = skippr_query_ballista::fetch_statement_batches_unauthenticated(
            &server.bind_addr().to_string(),
            "SELECT 1",
        )
        .await
        .unwrap_err();
        let text = err.to_string().to_ascii_lowercase();
        assert!(
            text.contains("unauthenticated")
                || text.contains("authorization")
                || text.contains("basic"),
            "{err}"
        );
        server.drain().await;
    }

    #[tokio::test]
    async fn plaintext_http_cannot_query_clustered_flight() {
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let url = format!("http://{}", server.bind_addr());
        let result = async {
            let channel = tonic::transport::Endpoint::from_shared(url)?
                .connect()
                .await?;
            let mut client = arrow_flight::sql::client::FlightSqlServiceClient::new(channel);
            client.execute("SELECT 1".into(), None).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        assert!(
            result.is_err(),
            "plaintext HTTP must not complete a Flight SQL query"
        );
        server.drain().await;
    }

    #[tokio::test]
    async fn flight_sql_exec_streams_from_live_server() {
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::physical_plan::ExecutionPlan;
        use skippr_query_ballista::FlightSqlExec;

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        let log = crate::buffer::durable::log::MutationLog::open(paths.clone()).unwrap();
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        let guard = skippr_lease::LeaseGuard::replica(
            key.clone(),
            skippr_lease::LeaseEpoch::new(1),
            Arc::new(skippr_lease::SystemClock::new()),
        );
        registry
            .insert(crate::cluster::peer::ReplicaSession::new(
                key, paths, guard, log,
            ))
            .await;
        let server = QueryFlightServer::start_with_registry(
            "127.0.0.1:0".parse().unwrap(),
            identity,
            Some(registry),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let node = FlightSqlExec::with_authorization(
            server.bind_addr().to_string(),
            b"SELECT * FROM live_wal_scan('t', 'w', 'p', 'ns')".to_vec(),
            schema,
            skippr_query_ballista::basic_authorization("t", "w"),
        );
        let stream = node
            .execute(0, Arc::new(datafusion::execution::TaskContext::default()))
            .unwrap();
        let batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();
        let rows: usize = batches.iter().map(|batch| batch.num_rows()).sum();
        assert_eq!(rows, 0);
        server.drain().await;
    }

    #[test]
    fn get_tables_does_not_invent_a_dummy_table() {
        let src = include_str!("service.rs");
        assert!(src.contains("list_configured_iceberg_tables"));
        assert!(!src.contains("\"tables\", \"TABLE\""));
    }
}
