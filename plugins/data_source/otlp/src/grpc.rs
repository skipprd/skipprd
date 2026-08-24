use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::mpsc;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::{
    LogsService, LogsServiceServer,
};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
};

use crate::config::{OtlpConfig, OtlpSignal, OTLP_MAX_REQUEST_BYTES};
use crate::decode::{
    decode_logs_request, decode_metrics_request, decode_traces_request, DecodedSignal,
};

#[derive(Clone)]
pub struct OtlpGrpcService {
    tx: mpsc::Sender<DecodedSignal>,
    config: Arc<OtlpConfig>,
    inject: Arc<BTreeMap<String, String>>,
}

impl OtlpGrpcService {
    pub fn new(
        tx: mpsc::Sender<DecodedSignal>,
        config: Arc<OtlpConfig>,
        inject: Arc<BTreeMap<String, String>>,
    ) -> Self {
        Self { tx, config, inject }
    }

    fn check_auth<T>(&self, req: &Request<T>) -> Result<(), Status> {
        let Some(expected) = &self.config.auth_token else {
            return Ok(());
        };
        let provided = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if provided == format!("Bearer {expected}") {
            Ok(())
        } else {
            Err(Status::unauthenticated("missing or invalid bearer token"))
        }
    }

    fn require_signal(&self, signal: OtlpSignal) -> Result<(), Status> {
        if self.config.accepts(signal) {
            Ok(())
        } else {
            Err(Status::failed_precondition(format!(
                "{} is not enabled",
                signal.as_str()
            )))
        }
    }
}

#[tonic::async_trait]
impl TraceService for OtlpGrpcService {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        self.check_auth(&request)?;
        self.require_signal(OtlpSignal::Traces)?;
        let decoded = decode_traces_request(request.get_ref(), &self.config, &self.inject)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.tx
            .send(DecodedSignal::Traces(decoded))
            .await
            .map_err(|_| Status::unavailable("ingest channel closed"))?;
        Ok(Response::new(ExportTraceServiceResponse {
            partial_success: Some(ExportTracePartialSuccess {
                rejected_spans: 0,
                error_message: String::new(),
            }),
        }))
    }
}

#[tonic::async_trait]
impl LogsService for OtlpGrpcService {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        self.check_auth(&request)?;
        self.require_signal(OtlpSignal::Logs)?;
        let decoded = decode_logs_request(request.get_ref(), &self.config, &self.inject)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.tx
            .send(DecodedSignal::Logs(decoded))
            .await
            .map_err(|_| Status::unavailable("ingest channel closed"))?;
        Ok(Response::new(ExportLogsServiceResponse {
            partial_success: Some(ExportLogsPartialSuccess {
                rejected_log_records: 0,
                error_message: String::new(),
            }),
        }))
    }
}

#[tonic::async_trait]
impl MetricsService for OtlpGrpcService {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        self.check_auth(&request)?;
        self.require_signal(OtlpSignal::Metrics)?;
        let decoded = decode_metrics_request(request.get_ref(), &self.config, &self.inject)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.tx
            .send(DecodedSignal::Metrics(decoded))
            .await
            .map_err(|_| Status::unavailable("ingest channel closed"))?;
        Ok(Response::new(ExportMetricsServiceResponse {
            partial_success: Some(ExportMetricsPartialSuccess {
                rejected_data_points: 0,
                error_message: String::new(),
            }),
        }))
    }
}

pub async fn serve_otlp_grpc(
    addr: SocketAddr,
    service: OtlpGrpcService,
) -> Result<(), tonic::transport::Error> {
    Server::builder()
        .add_service(
            TraceServiceServer::new(service.clone())
                .max_decoding_message_size(OTLP_MAX_REQUEST_BYTES),
        )
        .add_service(
            LogsServiceServer::new(service.clone())
                .max_decoding_message_size(OTLP_MAX_REQUEST_BYTES),
        )
        .add_service(
            MetricsServiceServer::new(service).max_decoding_message_size(OTLP_MAX_REQUEST_BYTES),
        )
        .serve(addr)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::traces_fixture_request;
    use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
    use tokio::net::TcpListener;
    use tonic::transport::Endpoint;

    #[tokio::test]
    async fn grpc_traces_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let (tx, mut rx) = mpsc::channel(4);
        let cfg: OtlpConfig =
            serde_json::from_value(serde_json::json!({"signals":["traces"]})).unwrap();
        let svc = OtlpGrpcService::new(tx, Arc::new(cfg), Arc::new(BTreeMap::new()));
        let handle = tokio::spawn(async move {
            serve_otlp_grpc(addr, svc).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut client = TraceServiceClient::connect(format!("http://{addr}"))
            .await
            .unwrap();
        client.export(traces_fixture_request()).await.unwrap();
        let item = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let DecodedSignal::Traces(t) = item else {
            panic!("traces")
        };
        assert_eq!(t.spans.len(), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn grpc_disabled_signal_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let (tx, _rx) = mpsc::channel(1);
        let cfg: OtlpConfig =
            serde_json::from_value(serde_json::json!({"signals":["logs"]})).unwrap();
        let svc = OtlpGrpcService::new(tx, Arc::new(cfg), Arc::new(BTreeMap::new()));
        let handle = tokio::spawn(async move {
            serve_otlp_grpc(addr, svc).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut client = TraceServiceClient::connect(format!("http://{addr}"))
            .await
            .unwrap();
        let err = client.export(traces_fixture_request()).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        handle.abort();
    }

    #[allow(dead_code)]
    fn _endpoint() {
        let _ = std::any::type_name::<Endpoint>();
    }
}
