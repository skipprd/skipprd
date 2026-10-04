use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use iceberg::ErrorKind;
use serde_json::json;

#[derive(Debug)]
pub enum RestError {
    NoSuchTable(String),
    NoSuchNamespace(String),
    AlreadyExists(String),
    CommitFailed(String),
    Forbidden(String),
    BadRequest(String),
    Unauthorized,
    Internal(String),
}

impl RestError {
    fn status_and_type(&self) -> (StatusCode, &'static str) {
        match self {
            Self::NoSuchTable(_) => (StatusCode::NOT_FOUND, "NoSuchTableException"),
            Self::NoSuchNamespace(_) => (StatusCode::NOT_FOUND, "NoSuchNamespaceException"),
            Self::AlreadyExists(_) => (StatusCode::CONFLICT, "AlreadyExistsException"),
            Self::CommitFailed(_) => (StatusCode::CONFLICT, "CommitFailedException"),
            Self::Forbidden(_) => (StatusCode::FORBIDDEN, "ForbiddenException"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "BadRequestException"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "NotAuthorizedException"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "InternalServerError"),
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::NoSuchTable(m)
            | Self::NoSuchNamespace(m)
            | Self::AlreadyExists(m)
            | Self::CommitFailed(m)
            | Self::Forbidden(m)
            | Self::BadRequest(m)
            | Self::Internal(m) => m,
            Self::Unauthorized => "missing or wrong bearer token",
        }
    }
}

impl IntoResponse for RestError {
    fn into_response(self) -> Response {
        let (status, typ) = self.status_and_type();
        (
            status,
            Json(json!({
                "error": {
                    "message": self.message(),
                    "type": typ,
                    "code": status.as_u16()
                }
            })),
        )
            .into_response()
    }
}

impl From<iceberg::Error> for RestError {
    fn from(err: iceberg::Error) -> Self {
        match err.kind() {
            ErrorKind::TableNotFound => Self::NoSuchTable(err.to_string()),
            ErrorKind::NamespaceNotFound => Self::NoSuchNamespace(err.to_string()),
            ErrorKind::TableAlreadyExists | ErrorKind::NamespaceAlreadyExists => {
                Self::AlreadyExists(err.to_string())
            }
            ErrorKind::CatalogCommitConflicts => Self::CommitFailed(err.to_string()),
            ErrorKind::DataInvalid | ErrorKind::PreconditionFailed => {
                Self::BadRequest(err.to_string())
            }
            _ => Self::Internal(err.to_string()),
        }
    }
}
