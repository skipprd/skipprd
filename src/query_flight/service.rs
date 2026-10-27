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
    ActionCreatePreparedStatementResult, Any as ProstAny, CommandGetCatalogs, CommandGetDbSchemas,
    CommandGetSqlInfo, CommandGetTableTypes, CommandGetTables, CommandPreparedStatementQuery,
    CommandStatementQuery, CommandStatementUpdate, SqlInfo, TicketStatementQuery,
};
use arrow_flight::{
    Action, ActionType, FlightDescriptor, FlightEndpoint, FlightInfo, HandshakeRequest,
    HandshakeResponse, Ticket,
};
use bytes::Bytes;
use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::{Schema, SchemaRef};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;
use futures::{Stream, StreamExt, TryStreamExt};
use skippr_lease::{DurableError, CONTROL_FRAME_MAX_BYTES};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};

use crate::cluster::identity::{ClusterIdentity, TenantScope};
use crate::cluster::peer::ReplicaRegistry;
use crate::helpers::configuration::Config;
use crate::query_flight::sql::{matches_sql_like, reject_ddl};
use skippr_query_plan::{
    decode_live_wal_ticket, LiveWalPinRequest, LiveWalPinResponse, LiveWalTicket, PinnedSliceRef,
    LIVE_WAL_PIN_ACTION, LIVE_WAL_TICKET_TYPE_URL,
};

#[derive(Clone)]
pub enum FlightAuth {
    ClusterBasic,
    Bearer { token: Arc<str>, scope: TenantScope },
}

#[derive(Clone)]
pub enum FlightEngine {
    Clustered {
        registry: Option<Arc<ReplicaRegistry>>,
    },
    Local,
}

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
        Self::start_with_registry(bind, identity, None, Config::new()).await
    }

    pub async fn start_with_registry(
        bind: SocketAddr,
        identity: ClusterIdentity,
        registry: Option<Arc<ReplicaRegistry>>,
        app_config: Config,
    ) -> Result<Self, DurableError> {
        let _ = identity;
        let tls = crate::cluster::tls::tonic_server_tls()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        Self::start_with(
            bind,
            FlightAuth::ClusterBasic,
            FlightEngine::Clustered { registry },
            app_config,
            Some(tls),
        )
        .await
    }

    pub async fn start_with(
        bind: SocketAddr,
        auth: FlightAuth,
        engine: FlightEngine,
        app_config: Config,
        tls: Option<ServerTlsConfig>,
    ) -> Result<Self, DurableError> {
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let local = listener
            .local_addr()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let (stop, mut rx) = watch::channel(false);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let svc = SkipprFlightSql {
            auth,
            engine,
            in_flight: in_flight.clone(),
            app_config,
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
            if let Some(tls) = tls {
                match builder.tls_config(tls) {
                    Ok(configured) => builder = configured,
                    Err(err) => {
                        tracing::error!(error = %err, "Flight SQL TLS config failed");
                        return;
                    }
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
    auth: FlightAuth,
    engine: FlightEngine,
    in_flight: Arc<AtomicUsize>,
    app_config: Config,
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

    fn session_scope<T>(&self, request: &Request<T>) -> Result<TenantScope, Status> {
        let auth = request.metadata().get("authorization").ok_or_else(|| {
            Status::unauthenticated("Flight SQL requires an Authorization header")
        })?;
        let value = auth
            .to_str()
            .map_err(|_| Status::unauthenticated("Flight SQL authorization is not valid UTF-8"))?;
        match &self.auth {
            FlightAuth::ClusterBasic => Self::parse_handshake_authorization(value),
            FlightAuth::Bearer { token, scope } => {
                let expected = format!("Bearer {}", token.as_ref());
                if value == expected {
                    Ok(scope.clone())
                } else {
                    Err(Status::unauthenticated(
                        "Flight SQL requires a valid bearer token",
                    ))
                }
            }
        }
    }

    fn ticket_for_sql(sql: &str) -> Ticket {
        let ticket = TicketStatementQuery {
            statement_handle: Bytes::from(sql.as_bytes().to_vec()),
        };
        Ticket::new(skippr_query_plan::encode_flight_sql_command(&ticket))
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

    async fn schema_for_sql(
        &self,
        config: &Config,
        sql: &str,
        scope: &TenantScope,
    ) -> Result<SchemaRef, Status> {
        Self::check_sql(sql)?;
        if matches!(self.engine, FlightEngine::Local) {
            let ctx = crate::sqlrt::query::new_context_iceberg_namespaces(config)
                .await
                .map_err(|err| Status::internal(err.to_string()))?;
            let df = ctx
                .sql(sql)
                .await
                .map_err(|err| Status::internal(err.to_string()))?;
            return Ok(df.schema().inner().clone());
        }
        crate::sqlrt::tables::schema_for_clustered_select(
            config,
            sql,
            &crate::sqlrt::tables::process_clustered_select_opts(scope.clone())
                .map_err(|err| Status::internal(err.to_string()))?,
        )
        .await
        .map_err(|err| Status::internal(err.to_string()))
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
        if matches!(self.engine, FlightEngine::Local) {
            let ctx = crate::sqlrt::query::new_context_iceberg_namespaces(&self.app_config)
                .await
                .map_err(|err| Status::internal(err.to_string()))?;
            let df = ctx
                .sql(sql)
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
        crate::query_flight::runtime::query_context()
            .map_err(|err| Status::internal(err.to_string()))?;
        let plan = crate::sqlrt::tables::plan_clustered_select(
            &self.app_config,
            sql,
            &crate::sqlrt::tables::process_clustered_select_opts(scope.clone())
                .map_err(|err| Status::internal(err.to_string()))?,
        )
        .await
        .map_err(|err| Status::internal(err.to_string()))?;
        if !plan.wal_only.is_empty() {
            tracing::info!(
                wal_only = ?plan.wal_only,
                "clustered query does not register WAL-only pipelines"
            );
        }
        let df = plan.df;
        let schema = df.schema().inner().clone();
        let stream = df
            .execute_stream()
            .await
            .map_err(datafusion_status)?
            .map_err(datafusion_status);
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
        let _scope = self.session_scope(&request)?;
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
        let scope = self.session_scope(&request)?;
        Self::check_sql(&query.query)?;
        Self::flight_info_for_sql(
            &query.query,
            self.schema_for_sql(&self.app_config, &query.query, &scope)
                .await?
                .as_ref(),
        )
    }

    async fn do_get_statement(
        &self,
        ticket: TicketStatementQuery,
        request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let scope = self.session_scope(&request)?;
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
        let scope = self.session_scope(&request)?;
        let sql = std::str::from_utf8(&query.prepared_statement_handle)
            .map_err(|err| Status::invalid_argument(err.to_string()))?;
        Self::check_sql(sql)?;
        Self::flight_info_for_sql(
            sql,
            self.schema_for_sql(&self.app_config, sql, &scope)
                .await?
                .as_ref(),
        )
    }

    async fn do_get_prepared_statement(
        &self,
        query: CommandPreparedStatementQuery,
        request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let scope = self.session_scope(&request)?;
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
        let _scope = self.session_scope(&request)?;
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
        let _scope = self.session_scope(&request)?;
        Ok(())
    }

    async fn list_custom_actions(&self) -> Option<Vec<Result<ActionType, Status>>> {
        Some(vec![Ok(ActionType {
            r#type: LIVE_WAL_PIN_ACTION.to_string(),
            description: "Pin live WAL slices for one pipeline namespace".into(),
        })])
    }

    async fn do_action_fallback(
        &self,
        request: Request<Action>,
    ) -> Result<Response<<Self as FlightService>::DoActionStream>, Status> {
        let scope = self.session_scope(&request)?;
        let action = request.get_ref();
        if action.r#type != LIVE_WAL_PIN_ACTION {
            return Err(Status::invalid_argument(format!(
                "unknown Flight action {}",
                action.r#type
            )));
        }
        if action.body.len() > CONTROL_FRAME_MAX_BYTES {
            return Err(Status::invalid_argument("live WAL pin request too large"));
        }
        let pin = LiveWalPinRequest::decode_bytes(action.body.as_ref())
            .map_err(|err| Status::invalid_argument(err.to_string()))?;
        let body = pin_live_wal(&scope, &pin)?;
        let output = futures::stream::iter(vec![Ok(arrow_flight::Result { body: body.into() })]);
        Ok(Response::new(Box::pin(output)))
    }

    async fn do_get_fallback(
        &self,
        request: Request<Ticket>,
        message: ProstAny,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let scope = self.session_scope(&request)?;
        if request.get_ref().ticket.len() > CONTROL_FRAME_MAX_BYTES {
            return Err(Status::invalid_argument("LiveWalTicket too large"));
        }
        if message.type_url != LIVE_WAL_TICKET_TYPE_URL {
            return Err(Status::invalid_argument(format!(
                "unknown Flight ticket {}",
                message.type_url
            )));
        }
        let ticket = decode_live_wal_ticket(&message)
            .map_err(|err| Status::invalid_argument(err.to_string()))?;
        let guard = self.track_in_flight();
        let (schema, stream) = execute_live_wal_ticket(&self.app_config, &scope, &ticket).await?;
        let stream = stream.map(move |item| {
            let _keep = &guard;
            item
        });
        Self::record_stream_response(schema, stream)
    }

    async fn do_put_statement_update(
        &self,
        _query: CommandStatementUpdate,
        request: Request<arrow_flight::sql::server::PeekableFlightDataStream>,
    ) -> Result<i64, Status> {
        let _scope = self.session_scope(&request)?;
        Err(Status::invalid_argument("Flight SQL is read-only"))
    }

    async fn get_flight_info_sql_info(
        &self,
        query: CommandGetSqlInfo,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let _scope = self.session_scope(&request)?;
        let mut builder = SqlInfoDataBuilder::new();
        builder.append(SqlInfo::FlightSqlServerName, "skipprd");
        builder.append(SqlInfo::FlightSqlServerReadOnly, true);
        let data = builder
            .build()
            .map_err(|err| Status::internal(err.to_string()))?;
        let ticket = Ticket::new(skippr_query_plan::encode_flight_sql_command(&query));
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
        let _scope = self.session_scope(&request)?;
        let ticket = Ticket::new(skippr_query_plan::encode_flight_sql_command(&query));
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
        let _scope = self.session_scope(&request)?;
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
        let _scope = self.session_scope(&request)?;
        let ticket = Ticket::new(skippr_query_plan::encode_flight_sql_command(&query));
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
        let scope = self.session_scope(&request)?;
        let tables = crate::sqlrt::tables::list_configured_iceberg_tables(&self.app_config, &scope)
            .await
            .map_err(|err| Status::internal(err.to_string()))?;
        let mut builder = query.into_builder();
        let mut seen = std::collections::BTreeSet::new();
        for table in tables {
            if seen.insert(table.namespace.clone()) {
                builder.append("datafusion", table.namespace);
            }
        }
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
        let _scope = self.session_scope(&request)?;
        let ticket = Ticket::new(skippr_query_plan::encode_flight_sql_command(&query));
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
        let scope = self.session_scope(&request)?;
        let tables = crate::sqlrt::tables::list_configured_iceberg_tables(&self.app_config, &scope)
            .await
            .map_err(|err| Status::internal(err.to_string()))?;
        let pattern = query.table_name_filter_pattern.clone();
        let mut builder = query.into_builder();
        for table in tables {
            if !matches_sql_like(&table.name, pattern.as_deref()) {
                continue;
            }
            builder
                .append(
                    "datafusion",
                    table.namespace,
                    table.name,
                    "TABLE",
                    table.schema.as_ref(),
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
        let _scope = self.session_scope(&request)?;
        let ticket = Ticket::new(skippr_query_plan::encode_flight_sql_command(&query));
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
        let _scope = self.session_scope(&request)?;
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

fn datafusion_status(err: datafusion::error::DataFusionError) -> Status {
    if crate::sqlrt::wal_table::is_wal_pin_lost(&err) {
        Status::failed_precondition(err.to_string())
    } else {
        Status::internal(err.to_string())
    }
}

async fn iceberg_schema_for_namespace(
    config: &Config,
    pipeline: &str,
    namespace: &str,
) -> Result<SchemaRef, Status> {
    crate::sqlrt::tables::iceberg_schema_for_namespace(config, pipeline, namespace)
        .await
        .map_err(|err| Status::internal(err.to_string()))
}

fn pin_live_wal(scope: &TenantScope, pin: &LiveWalPinRequest) -> Result<Bytes, Status> {
    if pin.tenant != scope.tenant || pin.workspace != scope.workspace {
        return Err(Status::invalid_argument(
            "live WAL pin tenant/workspace does not match session",
        ));
    }
    let key = skippr_lease::PipelineKey::new(&pin.tenant, &pin.workspace, &pin.pipeline)
        .map_err(|err| Status::invalid_argument(err.to_string()))?;
    let reader = crate::buffer::wal_reader::wal_reader_for(&key)
        .ok_or_else(|| Status::not_found(format!("unknown pipeline {}", key.pipeline())))?;
    let slices = reader
        .live_slices(&pin.namespace)
        .map_err(|err| Status::internal(err.to_string()))?;
    let response = LiveWalPinResponse {
        slices: slices
            .into_iter()
            .map(|slice| PinnedSliceRef {
                segment_id: slice.segment_id.as_str().to_string(),
                start: slice.entry.start,
                seq: slice.seq.get(),
                ordinal: slice.entry.ordinal,
            })
            .collect(),
    };
    Ok(Bytes::from(
        response
            .encode_bytes()
            .map_err(|err| Status::internal(err.to_string()))?,
    ))
}

async fn execute_live_wal_ticket(
    config: &Config,
    scope: &TenantScope,
    ticket: &LiveWalTicket,
) -> Result<
    (
        SchemaRef,
        Pin<Box<dyn Stream<Item = Result<RecordBatch, Status>> + Send>>,
    ),
    Status,
> {
    let target = ticket
        .target
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("LiveWalTicket target is required"))?;
    if target.tenant != scope.tenant || target.workspace != scope.workspace {
        return Err(Status::invalid_argument(
            "live WAL ticket tenant/workspace does not match session",
        ));
    }
    let key = skippr_lease::PipelineKey::new(&target.tenant, &target.workspace, &target.pipeline)
        .map_err(|err| Status::invalid_argument(err.to_string()))?;
    let reader = crate::buffer::wal_reader::wal_reader_for(&key)
        .ok_or_else(|| Status::not_found(format!("unknown pipeline {}", key.pipeline())))?;
    let mut pinned = Vec::with_capacity(ticket.slices.len());
    for slice in &ticket.slices {
        let got = reader
            .pinned_slice(&slice.segment_id, slice.start)
            .map_err(|err| match err {
                crate::buffer::wal_reader::PinReadError::Lost(lost) => {
                    Status::failed_precondition(lost.to_string())
                }
                crate::buffer::wal_reader::PinReadError::Durable(err) => {
                    Status::internal(err.to_string())
                }
            })?;
        if got.entry.namespace != target.namespace {
            return Err(Status::invalid_argument(format!(
                "LiveWalTicket slice {}@{} namespace does not match target",
                slice.segment_id, slice.start
            )));
        }
        pinned.push(got);
    }
    let schema = iceberg_schema_for_namespace(config, &target.pipeline, &target.namespace).await?;
    let schema = if ticket.cdc_sidecar {
        crate::sqlrt::cdc_reconcile::cdc_sidecar_schema(&schema)
    } else {
        schema
    };
    let plan = crate::sqlrt::wal_table::WalScanExec::new(
        schema.clone(),
        pinned,
        None,
        ticket.cdc_sidecar,
        target.namespace.clone(),
    )
    .map_err(|err| Status::internal(err.to_string()))?;
    let ctx = SessionContext::new();
    let stream = plan
        .execute(0, ctx.task_ctx())
        .map_err(|err| Status::internal(err.to_string()))?
        .map_err(datafusion_status);
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
        let err = skippr_query_plan::fetch_statement_batches(
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
        let err = skippr_query_plan::fetch_statement_batches(&server.bind_addr().to_string(), &sql)
            .await
            .unwrap_err();
        assert!(
            err.to_string().to_ascii_lowercase().contains("too large")
                || err.to_string().contains("invalid")
        );
        server.drain().await;
    }

    async fn live_wal_do_get_err(
        endpoint: &str,
        ticket: &skippr_query_plan::LiveWalTicket,
    ) -> datafusion::error::DataFusionError {
        match skippr_query_plan::stream_live_wal_batches_with_auth(
            endpoint,
            ticket,
            &skippr_query_plan::basic_authorization("t", "w"),
        )
        .await
        {
            Err(err) => err,
            Ok(stream) => {
                use futures::TryStreamExt;
                stream
                    .try_collect::<Vec<_>>()
                    .await
                    .expect_err("expected LiveWal DoGet to fail closed")
            }
        }
    }

    fn live_ticket(pipeline: &str, namespace: &str) -> skippr_query_plan::LiveWalTicket {
        skippr_query_plan::LiveWalTicket {
            target: Some(skippr_query_plan::LiveWalPinRequest {
                tenant: "t".into(),
                workspace: "w".into(),
                pipeline: pipeline.into(),
                namespace: namespace.into(),
            }),
            cdc_sidecar: false,
            slices: Vec::new(),
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn live_wal_pin_without_reader_fails_closed() {
        crate::buffer::wal_handle::clear_wal_handles();
        crate::buffer::wal_reader::clear_query_readers();
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        let identity = identity();
        let registry = ReplicaRegistry::new(identity.clone());
        let guard = skippr_lease::LeaseGuard::replica(
            key.clone(),
            skippr_lease::LeaseEpoch::new(1),
            Arc::new(skippr_lease::SystemClock::new()),
        );
        registry
            .insert(crate::cluster::peer::ReplicaSession::new(key, paths, guard))
            .await;
        let server = QueryFlightServer::start_with_registry(
            "127.0.0.1:0".parse().unwrap(),
            identity,
            Some(registry),
            Config::new(),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let err = skippr_query_plan::pin_live_wal_with_auth(
            &server.bind_addr().to_string(),
            &skippr_query_plan::LiveWalPinRequest {
                tenant: "t".into(),
                workspace: "w".into(),
                pipeline: "p".into(),
                namespace: "ns".into(),
            },
            &skippr_query_plan::basic_authorization("t", "w"),
        )
        .await
        .unwrap_err();
        let text = err.to_string().to_ascii_lowercase();
        assert!(
            text.contains("unknown") || text.contains("not found"),
            "{err}"
        );
        server.drain().await;
    }

    #[tokio::test]
    async fn foreign_tenant_live_wal_ticket_fails() {
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut ticket = live_ticket("events", "ns");
        ticket.target.as_mut().unwrap().tenant = "other".into();
        ticket.target.as_mut().unwrap().workspace = "prod".into();
        let err = live_wal_do_get_err(&server.bind_addr().to_string(), &ticket).await;
        assert!(
            err.to_string().contains("tenant")
                || err.to_string().contains("invalid")
                || err.to_string().contains("does not match"),
            "{err}"
        );
        server.drain().await;
    }

    #[tokio::test]
    async fn unknown_pipeline_live_wal_ticket_fails_closed() {
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let err = live_wal_do_get_err(
            &server.bind_addr().to_string(),
            &live_ticket("missing", "ns"),
        )
        .await;
        assert!(
            err.to_string().contains("unknown")
                || err.to_string().contains("not")
                || err.to_string().to_ascii_lowercase().contains("not found"),
            "{err}"
        );
        server.drain().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn live_wal_ticket_missing_slice_is_wal_pin_lost() {
        crate::buffer::wal_handle::clear_wal_handles();
        crate::buffer::wal_reader::clear_query_readers();
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "pinlost").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        let identity = identity();
        let clock = Arc::new(skippr_lease::SystemClock::new());
        let guard = skippr_lease::LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = crate::buffer::wal_handle::memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
            Arc::new(crate::helpers::offset_store::SledOffsetStore::new(tree));
        let handle = crate::buffer::wal_handle::PipelineWalHandle::new(
            key.clone(),
            paths,
            guard,
            catalog,
            offsets,
        );
        crate::buffer::wal_handle::install_wal_handle(handle);
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut ticket = live_ticket("pinlost", "ns");
        ticket.slices.push(skippr_query_plan::PinnedSliceRef {
            segment_id: "gone".into(),
            start: 0,
            seq: 1,
            ordinal: 0,
        });
        let err = live_wal_do_get_err(&server.bind_addr().to_string(), &ticket).await;
        assert!(
            crate::sqlrt::wal_table::is_wal_pin_lost_text(&err.to_string()),
            "{err}"
        );
        server.drain().await;
        crate::buffer::wal_handle::remove_wal_handle(&key);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn live_wal_ticket_missing_iceberg_schema_fails_closed() {
        crate::buffer::wal_handle::clear_wal_handles();
        crate::buffer::wal_reader::clear_query_readers();
        let key = PipelineKey::new("t", "w", "noschema").unwrap();
        crate::buffer::wal_reader::install_query_reader(
            crate::buffer::wal_reader::reader_with_live_slice(key.clone(), "ns", "seg-a"),
        )
        .unwrap();
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut ticket = live_ticket("noschema", "ns");
        ticket.slices.push(skippr_query_plan::PinnedSliceRef {
            segment_id: "seg-a".into(),
            start: 0,
            seq: 1,
            ordinal: 0,
        });
        let err = live_wal_do_get_err(&server.bind_addr().to_string(), &ticket).await;
        let text = err.to_string();
        assert!(
            text.contains("no Iceberg schema") || text.contains("Iceberg"),
            "missing Iceberg schema must fail closed, got {text}"
        );
        assert!(
            !crate::sqlrt::wal_table::is_wal_pin_lost_text(&text),
            "schema miss is not WalPinLost, got {text}"
        );
        server.drain().await;
        crate::buffer::wal_reader::clear_query_readers();
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
        let err = skippr_query_plan::fetch_statement_batches_unauthenticated(
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
    #[serial_test::serial]
    async fn flight_sql_exec_streams_from_live_server() {
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::physical_plan::ExecutionPlan;
        use skippr_query_plan::FlightSqlExec;

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "flightsqllive").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        let identity = identity();
        let clock = Arc::new(skippr_lease::SystemClock::new());
        let guard = skippr_lease::LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = crate::buffer::wal_handle::memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn crate::helpers::offset_store::OffsetStore> =
            Arc::new(crate::helpers::offset_store::SledOffsetStore::new(tree));
        let handle = crate::buffer::wal_handle::PipelineWalHandle::new(
            key.clone(),
            paths,
            guard,
            catalog,
            offsets,
        );
        crate::buffer::wal_handle::install_wal_handle(handle);
        let server = QueryFlightServer::start("127.0.0.1:0".parse().unwrap(), identity)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let node = FlightSqlExec::live_wal(
            server.bind_addr().to_string(),
            live_ticket("flightsqllive", "ns"),
            schema,
            skippr_query_plan::basic_authorization("t", "w"),
        );
        let stream = node
            .execute(0, Arc::new(datafusion::execution::TaskContext::default()))
            .unwrap();
        let err = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no Iceberg schema"),
            "owner DoGet must fail closed without Iceberg schema, got {err}"
        );
        server.drain().await;
        crate::buffer::wal_handle::remove_wal_handle(&key);
    }

    #[test]
    fn clustered_select_and_live_wal_share_typed_pin_lost_status() {
        let src = include_str!("service.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(prod.contains("fn datafusion_status"));
        assert!(prod.contains("is_wal_pin_lost"));
        assert!(prod.contains("failed_precondition"));
        let ticket = prod
            .split("async fn execute_live_wal_ticket")
            .nth(1)
            .expect("execute_live_wal_ticket")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(
            ticket.contains("target.pipeline"),
            "owner DoGet schema must use the ticket pipeline"
        );
        assert!(ticket.contains("iceberg_schema_for_namespace(config, &target.pipeline"));
    }

    #[test]
    fn get_tables_does_not_invent_a_dummy_table() {
        let src = include_str!("service.rs");
        assert!(src.contains("list_configured_iceberg_tables"));
        assert!(!src.contains("\"tables\", \"TABLE\""));
        let get_tables = src
            .split("async fn do_get_tables")
            .nth(1)
            .expect("do_get_tables")
            .split("async fn get_flight_info_table_types")
            .next()
            .unwrap();
        assert!(
            get_tables.contains("table.namespace"),
            "GetTables schema_name is Iceberg namespace, not workspace"
        );
        assert!(
            !get_tables.contains("scope.workspace"),
            "GetTables must not stuff workspace as db_schema"
        );
    }

    #[test]
    fn get_schemas_lists_iceberg_namespaces() {
        let src = include_str!("service.rs");
        let get_schemas = src
            .split("async fn do_get_schemas")
            .nth(1)
            .expect("do_get_schemas")
            .split("async fn get_flight_info_tables")
            .next()
            .unwrap();
        assert!(get_schemas.contains("list_configured_iceberg_tables"));
        assert!(get_schemas.contains("table.namespace"));
        assert!(
            !get_schemas.contains("scope.workspace"),
            "GetDbSchemas must list Iceberg namespaces, not workspace"
        );
    }

    #[test]
    fn local_flight_sql_uses_iceberg_namespace_context() {
        let src = include_str!("service.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            prod.contains("new_context_iceberg_namespaces"),
            "skipprd serve Flight must execute Iceberg namespace.table"
        );
        assert!(
            !prod.contains("new_context_all_namespaces"),
            "skipprd serve Flight must not share skipprd query Iceberg ∪ WAL registration"
        );
    }

    async fn start_local(auth: FlightAuth) -> QueryFlightServer {
        start_local_with(auth, Config::new()).await
    }

    async fn start_local_with(auth: FlightAuth, config: Config) -> QueryFlightServer {
        QueryFlightServer::start_with(
            "127.0.0.1:0".parse().unwrap(),
            auth,
            FlightEngine::Local,
            config,
            None,
        )
        .await
        .unwrap()
    }

    fn wal_only_config() -> Config {
        serde_json::from_value(serde_json::json!({
            "skippr": {
                "workspace": "w",
                "tenant": "t",
                "skipprd_el_storage_mode": "local"
            },
            "pipelines": {
                "p": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "File": { "path": "/tmp/skipprd-flight-local.json" } }
            }
        }))
        .unwrap()
    }

    fn bearer() -> FlightAuth {
        FlightAuth::Bearer {
            token: Arc::from("secret"),
            scope: TenantScope::new("t", "w").unwrap(),
        }
    }

    #[tokio::test]
    async fn flight_bearer_rejects_missing_and_wrong_token() {
        let server = start_local(bearer()).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let addr = server.bind_addr().to_string();
        let missing =
            skippr_query_plan::fetch_statement_batches_plain_unauthenticated(&addr, "SELECT 1")
                .await
                .unwrap_err();
        assert!(
            missing.to_string().to_ascii_lowercase().contains("unauth")
                || missing.to_string().to_ascii_lowercase().contains("bearer")
                || missing
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("authorization")
                || missing
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("credentials"),
            "{missing}"
        );
        let wrong =
            skippr_query_plan::fetch_statement_batches_plain(&addr, "SELECT 1", "Bearer wrong")
                .await
                .unwrap_err();
        assert!(
            wrong.to_string().to_ascii_lowercase().contains("unauth")
                || wrong.to_string().to_ascii_lowercase().contains("bearer")
                || wrong
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("authorization")
                || wrong
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("credentials"),
            "{wrong}"
        );
        server.drain().await;
    }

    #[tokio::test]
    async fn flight_local_serves_select_1() {
        let server = start_local_with(bearer(), wal_only_config()).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let batches = skippr_query_plan::fetch_statement_batches_plain(
            &server.bind_addr().to_string(),
            "SELECT 1",
            "Bearer secret",
        )
        .await
        .unwrap();
        let rows: usize = batches.iter().map(|batch| batch.num_rows()).sum();
        assert_eq!(rows, 1);
        server.drain().await;
    }

    #[tokio::test]
    async fn flight_local_rejects_ddl() {
        let server = start_local_with(bearer(), wal_only_config()).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let err = skippr_query_plan::fetch_statement_batches_plain(
            &server.bind_addr().to_string(),
            "INSERT INTO t VALUES (1)",
            "Bearer secret",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().to_ascii_lowercase().contains("read-only")
                || err.to_string().contains("invalid"),
            "{err}"
        );
        server.drain().await;
    }
}
