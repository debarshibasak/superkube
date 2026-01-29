use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Resource not found: {0}")]
    NotFound(String),

    #[error("Resource already exists: {0}")]
    AlreadyExists(String),

    #[error("Invalid request: {0}")]
    BadRequest(String),

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("HTTP client error: {0}")]
    HttpClient(#[from] reqwest::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, kind, message) = match &self {
            Error::NotFound(msg) => (StatusCode::NOT_FOUND, "NotFound", msg.clone()),
            Error::AlreadyExists(msg) => (StatusCode::CONFLICT, "AlreadyExists", msg.clone()),
            Error::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BadRequest", msg.clone()),
            Error::Database(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                e.to_string(),
            ),
            Error::Serialization(e) => (StatusCode::BAD_REQUEST, "BadRequest", e.to_string()),
            Error::HttpClient(e) => (
                StatusCode::BAD_GATEWAY,
                "BadGateway",
                e.to_string(),
            ),
            Error::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                msg.clone(),
            ),
        };

        // K8s-compatible error response format
        let body = Json(json!({
            "apiVersion": "v1",
            "kind": "Status",
            "metadata": {},
            "status": "Failure",
            "message": message,
            "reason": kind,
            "code": status.as_u16()
        }));

        (status, body).into_response()
    }
}
