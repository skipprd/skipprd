mod auth;
mod error;
mod tables;

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::extract::State;
use axum::middleware;
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use iceberg::Catalog;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct RestState {
    pub catalog: Arc<dyn Catalog>,
    pub warehouse: String,
    pub ingest_namespaces: BTreeSet<String>,
    pub pipeline_names: BTreeSet<String>,
    pub token: Arc<str>,
    pub prefix: String,
}

pub fn ingest_write_forbidden(state: &RestState, namespace: &str) -> bool {
    state.ingest_namespaces.contains(namespace)
}

pub fn pipeline_namespace_conflict(state: &RestState, namespace: &str) -> bool {
    state.pipeline_names.contains(namespace)
}

pub fn router(state: RestState) -> Router {
    let catalog_routes = Router::new()
        .route(
            "/namespaces",
            get(tables::list_namespaces).post(tables::create_namespace),
        )
        .route(
            "/namespaces/{namespace}",
            get(tables::get_namespace)
                .head(tables::namespace_exists)
                .delete(tables::drop_namespace),
        )
        .route(
            "/namespaces/{namespace}/properties",
            post(tables::update_namespace),
        )
        .route(
            "/namespaces/{namespace}/tables",
            get(tables::list_tables).post(tables::create_table),
        )
        .route(
            "/namespaces/{namespace}/tables/{table}",
            get(tables::load_table)
                .head(tables::table_exists)
                .post(tables::commit_table)
                .delete(tables::drop_table),
        )
        .route("/tables/rename", post(tables::rename_table));

    let nested = if state.prefix.is_empty() {
        Router::new().merge(catalog_routes)
    } else {
        Router::new().nest(&format!("/{}", state.prefix), catalog_routes)
    };

    Router::new()
        .route("/v1/config", get(config))
        .nest("/v1", nested)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ))
        .with_state(state)
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    state: RestState,
) -> Result<(), std::io::Error> {
    axum::serve(listener, router(state).into_make_service()).await
}

pub async fn serve_tls(
    listener: std::net::TcpListener,
    state: RestState,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
) -> Result<(), std::io::Error> {
    let config = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
        .await
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    axum_server::from_tcp_rustls(listener, config)?
        .serve(router(state).into_make_service())
        .await
}

async fn config(State(state): State<RestState>) -> Json<Value> {
    let overrides = if state.prefix.is_empty() {
        json!({})
    } else {
        json!({ "prefix": state.prefix })
    };
    Json(json!({
        "defaults": {
            "warehouse": state.warehouse
        },
        "overrides": overrides,
        "endpoints": [
            "GET /v1/config",
            "GET /v1/{prefix}/namespaces",
            "POST /v1/{prefix}/namespaces",
            "GET /v1/{prefix}/namespaces/{namespace}",
            "HEAD /v1/{prefix}/namespaces/{namespace}",
            "DELETE /v1/{prefix}/namespaces/{namespace}",
            "POST /v1/{prefix}/namespaces/{namespace}/properties",
            "GET /v1/{prefix}/namespaces/{namespace}/tables",
            "POST /v1/{prefix}/namespaces/{namespace}/tables",
            "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
            "HEAD /v1/{prefix}/namespaces/{namespace}/tables/{table}",
            "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}",
            "DELETE /v1/{prefix}/namespaces/{namespace}/tables/{table}",
            "POST /v1/{prefix}/tables/rename"
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use iceberg::memory::{MemoryCatalogBuilder, MEMORY_CATALOG_WAREHOUSE};
    use iceberg::CatalogBuilder;
    use tower::ServiceExt;

    async fn memory_catalog() -> Arc<dyn Catalog> {
        let catalog = MemoryCatalogBuilder::default()
            .load(
                "memory",
                HashMap::from([(
                    MEMORY_CATALOG_WAREHOUSE.to_string(),
                    "s3://warehouse".into(),
                )]),
            )
            .await
            .unwrap();
        Arc::new(catalog)
    }

    async fn state() -> RestState {
        RestState {
            catalog: memory_catalog().await,
            warehouse: "s3://warehouse".into(),
            token: Arc::from("secret"),
            ingest_namespaces: ["bronze".into()].into_iter().collect(),
            pipeline_names: ["shop".into()].into_iter().collect(),
            prefix: String::new(),
        }
    }

    fn auth(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder.header(header::AUTHORIZATION, "Bearer secret")
    }

    async fn body_json(res: axum::http::Response<Body>) -> Value {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn ingest_and_pipeline_policy() {
        let s = state().await;
        assert!(ingest_write_forbidden(&s, "bronze"));
        assert!(!ingest_write_forbidden(&s, "analytics"));
        assert!(pipeline_namespace_conflict(&s, "shop"));
        assert!(!pipeline_namespace_conflict(&s, "analytics"));
    }

    #[tokio::test]
    async fn missing_bearer_is_401() {
        let app = router(state().await);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/v1/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let v = body_json(res).await;
        assert_eq!(v["error"]["type"], "NotAuthorizedException");
    }

    #[tokio::test]
    async fn bearer_config_ok() {
        let app = router(state().await);
        let res = app
            .oneshot(
                auth(Request::builder())
                    .uri("/v1/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["defaults"], json!({ "warehouse": "s3://warehouse" }));
        assert_eq!(v["overrides"], json!({}));
        let endpoints = v["endpoints"].as_array().expect("endpoints");
        assert!(endpoints.iter().any(|e| e == "GET /v1/config"), "{v}");
        assert!(
            endpoints.iter().any(|e| e == "GET /v1/{prefix}/namespaces"),
            "{v}"
        );
        assert!(
            endpoints
                .iter()
                .any(|e| e == "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}"),
            "{v}"
        );
        assert!(
            endpoints
                .iter()
                .any(|e| e == "POST /v1/{prefix}/tables/rename"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn create_pipeline_namespace_is_409() {
        let app = router(state().await);
        let res = app
            .oneshot(
                auth(Request::builder())
                    .method("POST")
                    .uri("/v1/namespaces")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"namespace":["shop"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let v = body_json(res).await;
        assert_eq!(v["error"]["type"], "AlreadyExistsException");
    }

    #[tokio::test]
    async fn create_existing_namespace_is_409() {
        let app = router(state().await);
        let first = app
            .clone()
            .oneshot(
                auth(Request::builder())
                    .method("POST")
                    .uri("/v1/namespaces")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"namespace":["analytics"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = app
            .oneshot(
                auth(Request::builder())
                    .method("POST")
                    .uri("/v1/namespaces")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"namespace":["analytics"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT);
        let v = body_json(second).await;
        assert_eq!(v["error"]["type"], "AlreadyExistsException");
    }

    #[tokio::test]
    async fn ingest_namespace_create_table_is_403() {
        let app = router(state().await);
        let res = app
            .oneshot(
                auth(Request::builder())
                    .method("POST")
                    .uri("/v1/namespaces/bronze/tables")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"name":"orders","schema":{"type":"struct","schema-id":0,"fields":[{"id":1,"name":"id","required":false,"type":"long"}]}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let v = body_json(res).await;
        assert_eq!(v["error"]["type"], "ForbiddenException");
    }

    #[tokio::test]
    async fn ingest_namespace_drop_is_403() {
        let app = router(state().await);
        let res = app
            .oneshot(
                auth(Request::builder())
                    .method("DELETE")
                    .uri("/v1/namespaces/bronze")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let v = body_json(res).await;
        assert_eq!(v["error"]["type"], "ForbiddenException");
    }

    #[tokio::test]
    async fn create_and_list_user_namespace() {
        let st = state().await;
        let created = router(st.clone())
            .oneshot(
                auth(Request::builder())
                    .method("POST")
                    .uri("/v1/namespaces")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"namespace":["analytics"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let listed = router(st)
            .oneshot(
                auth(Request::builder())
                    .uri("/v1/namespaces")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let v = body_json(listed).await;
        assert!(
            v["namespaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|ns| ns == &json!(["analytics"])),
            "{v}"
        );
    }

    #[tokio::test]
    async fn load_missing_table_is_404() {
        let st = state().await;
        let created = router(st.clone())
            .oneshot(
                auth(Request::builder())
                    .method("POST")
                    .uri("/v1/namespaces")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"namespace":["analytics"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let res = router(st)
            .oneshot(
                auth(Request::builder())
                    .uri("/v1/namespaces/analytics/tables/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let v = body_json(res).await;
        assert_eq!(v["error"]["type"], "NoSuchTableException");
    }

    #[tokio::test]
    async fn ingest_namespace_commit_drop_rename_are_403() {
        let st = state().await;
        let commit = router(st.clone())
            .oneshot(
                auth(Request::builder())
                    .method("POST")
                    .uri("/v1/namespaces/bronze/tables/orders")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"requirements":[],"updates":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(commit.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            body_json(commit).await["error"]["type"],
            "ForbiddenException"
        );

        let drop = router(st.clone())
            .oneshot(
                auth(Request::builder())
                    .method("DELETE")
                    .uri("/v1/namespaces/bronze/tables/orders")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(drop.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(drop).await["error"]["type"], "ForbiddenException");

        let rename = router(st)
            .oneshot(
                auth(Request::builder())
                    .method("POST")
                    .uri("/v1/tables/rename")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"source":{"namespace":["bronze"],"name":"orders"},"destination":{"namespace":["analytics"],"name":"orders"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rename.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            body_json(rename).await["error"]["type"],
            "ForbiddenException"
        );
    }
}
