use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::RestError;
use crate::RestState;

pub async fn require_bearer(
    State(state): State<RestState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let expected = format!("Bearer {}", state.token.as_ref());
    match req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some(got) if got == expected => next.run(req).await,
        _ => RestError::Unauthorized.into_response(),
    }
}
