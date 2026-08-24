use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use tokio::sync::mpsc;

use crate::config::{OtlpConfig, OtlpSignal, OTLP_MAX_REQUEST_BYTES};
use crate::decode::{
    decode_logs, decode_logs_json, decode_metrics, decode_metrics_json, decode_traces,
    decode_traces_json, DecodedSignal,
};

#[derive(Clone)]
pub struct HttpState {
    pub tx: mpsc::Sender<DecodedSignal>,
    pub config: Arc<OtlpConfig>,
    pub inject: Arc<BTreeMap<String, String>>,
}

pub fn otlp_http_router(state: HttpState) -> Router {
    Router::new()
        .route("/v1/traces", post(traces))
        .route("/v1/logs", post(logs))
        .route("/v1/metrics", post(metrics))
        .layer(DefaultBodyLimit::max(OTLP_MAX_REQUEST_BYTES))
        .with_state(state)
}

fn unauthorized() -> axum::response::Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

fn payload_too_large() -> axum::response::Response {
    (StatusCode::PAYLOAD_TOO_LARGE, "payload too large").into_response()
}

fn check_auth(headers: &HeaderMap, config: &OtlpConfig) -> Result<(), axum::response::Response> {
    let Some(expected) = &config.auth_token else {
        return Ok(());
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided == format!("Bearer {expected}") {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

fn check_size(headers: &HeaderMap, body: &Bytes) -> Result<(), axum::response::Response> {
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if len > OTLP_MAX_REQUEST_BYTES {
            return Err(payload_too_large());
        }
    }
    if body.len() > OTLP_MAX_REQUEST_BYTES {
        return Err(payload_too_large());
    }
    Ok(())
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase().contains("json"))
        .unwrap_or(false)
}

async fn traces(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    handle(state, headers, body, OtlpSignal::Traces).await
}

async fn logs(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    handle(state, headers, body, OtlpSignal::Logs).await
}

async fn metrics(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    handle(state, headers, body, OtlpSignal::Metrics).await
}

async fn handle(
    state: HttpState,
    headers: HeaderMap,
    body: Bytes,
    signal: OtlpSignal,
) -> axum::response::Response {
    if let Err(resp) = check_auth(&headers, &state.config) {
        return resp;
    }
    if let Err(resp) = check_size(&headers, &body) {
        return resp;
    }
    if !state.config.accepts(signal) {
        return (StatusCode::BAD_REQUEST, "signal not enabled").into_response();
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty body").into_response();
    }
    let json = is_json(&headers);
    let decoded = match (signal, json) {
        (OtlpSignal::Traces, false) => {
            decode_traces(&body, &state.config, &state.inject).map(DecodedSignal::Traces)
        }
        (OtlpSignal::Traces, true) => {
            decode_traces_json(&body, &state.config, &state.inject).map(DecodedSignal::Traces)
        }
        (OtlpSignal::Logs, false) => {
            decode_logs(&body, &state.config, &state.inject).map(DecodedSignal::Logs)
        }
        (OtlpSignal::Logs, true) => {
            decode_logs_json(&body, &state.config, &state.inject).map(DecodedSignal::Logs)
        }
        (OtlpSignal::Metrics, false) => {
            decode_metrics(&body, &state.config, &state.inject).map(DecodedSignal::Metrics)
        }
        (OtlpSignal::Metrics, true) => {
            decode_metrics_json(&body, &state.config, &state.inject).map(DecodedSignal::Metrics)
        }
    };
    match decoded {
        Ok(signal) => {
            if state.tx.send(signal).await.is_err() {
                return (StatusCode::SERVICE_UNAVAILABLE, "ingest channel closed").into_response();
            }
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/x-protobuf")],
                Vec::<u8>::new(),
            )
                .into_response()
        }
        Err(err) => (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::traces_fixture_bytes;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn oneshot(router: Router, req: Request<Body>) -> (StatusCode, Bytes) {
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, bytes)
    }

    #[tokio::test]
    async fn protobuf_traces_enqueues() {
        let (tx, mut rx) = mpsc::channel(4);
        let cfg: OtlpConfig =
            serde_json::from_value(serde_json::json!({"signals":["traces"]})).unwrap();
        let router = otlp_http_router(HttpState {
            tx,
            config: Arc::new(cfg),
            inject: Arc::new(BTreeMap::new()),
        });
        let body = traces_fixture_bytes();
        let req = Request::builder()
            .method("POST")
            .uri("/v1/traces")
            .header(header::CONTENT_TYPE, "application/x-protobuf")
            .body(Body::from(body))
            .unwrap();
        let (status, _) = oneshot(router, req).await;
        assert_eq!(status, StatusCode::OK);
        let item = rx.recv().await.unwrap();
        let DecodedSignal::Traces(t) = item else {
            panic!("traces")
        };
        assert_eq!(t.spans.len(), 2);
    }

    #[tokio::test]
    async fn empty_body_400() {
        let (tx, _rx) = mpsc::channel(1);
        let cfg: OtlpConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        let router = otlp_http_router(HttpState {
            tx,
            config: Arc::new(cfg),
            inject: Arc::new(BTreeMap::new()),
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/traces")
            .body(Body::from(Vec::<u8>::new()))
            .unwrap();
        let (status, _) = oneshot(router, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn wrong_path_404() {
        let (tx, _rx) = mpsc::channel(1);
        let cfg: OtlpConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        let router = otlp_http_router(HttpState {
            tx,
            config: Arc::new(cfg),
            inject: Arc::new(BTreeMap::new()),
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/unknown")
            .body(Body::from(vec![1, 2, 3]))
            .unwrap();
        let (status, _) = oneshot(router, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn malformed_json_400() {
        let (tx, _rx) = mpsc::channel(1);
        let cfg: OtlpConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        let router = otlp_http_router(HttpState {
            tx,
            config: Arc::new(cfg),
            inject: Arc::new(BTreeMap::new()),
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/traces")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{not-json"))
            .unwrap();
        let (status, _) = oneshot(router, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_bearer_401() {
        let (tx, _rx) = mpsc::channel(1);
        let cfg: OtlpConfig =
            serde_json::from_value(serde_json::json!({"auth_token":"secret"})).unwrap();
        let router = otlp_http_router(HttpState {
            tx,
            config: Arc::new(cfg),
            inject: Arc::new(BTreeMap::new()),
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/traces")
            .body(Body::from(vec![1]))
            .unwrap();
        let (status, _) = oneshot(router, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn traces_only_rejects_logs() {
        let (tx, _rx) = mpsc::channel(1);
        let cfg: OtlpConfig =
            serde_json::from_value(serde_json::json!({"signals":["traces"]})).unwrap();
        let router = otlp_http_router(HttpState {
            tx,
            config: Arc::new(cfg),
            inject: Arc::new(BTreeMap::new()),
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .body(Body::from(vec![1, 2, 3]))
            .unwrap();
        let (status, _) = oneshot(router, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn content_length_413() {
        let (tx, _rx) = mpsc::channel(1);
        let cfg: OtlpConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        let router = otlp_http_router(HttpState {
            tx,
            config: Arc::new(cfg),
            inject: Arc::new(BTreeMap::new()),
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/traces")
            .header(
                header::CONTENT_LENGTH,
                (OTLP_MAX_REQUEST_BYTES + 1).to_string(),
            )
            .body(Body::from(vec![0u8; 8]))
            .unwrap();
        let (status, _) = oneshot(router, req).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
