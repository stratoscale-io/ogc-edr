//! EDR exception responses.
//!
//! OGC API - EDR returns errors as a JSON object with `code` and `description`
//! (Requirement `/req/core/rc-exceptions`), so every failure path in the API
//! surfaces through [`EdrError`] rather than a bare status code.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug)]
pub enum EdrError {
    /// A query parameter is missing, malformed, or out of range.
    BadRequest(String),
    /// No collection / instance with the requested id.
    NotFound(String),
    /// Well-formed request that this server does not support (e.g. an
    /// unimplemented output format or CRS).
    NotSupported(String),
    /// The request is valid but would read more of the store than the server
    /// is willing to serve in one response.
    TooLarge(String),
    /// Anything that went wrong while reading the store or running the query.
    Internal(String),
}

impl EdrError {
    pub fn status(&self) -> StatusCode {
        match self {
            EdrError::BadRequest(_) => StatusCode::BAD_REQUEST,
            EdrError::NotFound(_) => StatusCode::NOT_FOUND,
            EdrError::NotSupported(_) => StatusCode::BAD_REQUEST,
            EdrError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            EdrError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            EdrError::BadRequest(m)
            | EdrError::NotFound(m)
            | EdrError::NotSupported(m)
            | EdrError::TooLarge(m)
            | EdrError::Internal(m) => m,
        }
    }
}

impl std::fmt::Display for EdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description())
    }
}

impl std::error::Error for EdrError {}

impl IntoResponse for EdrError {
    fn into_response(self) -> Response {
        let status = self.status();
        // Internal failures often quote DataFusion/object-store errors; log the
        // detail and still return it, since this server is an open data facade.
        if matches!(self, EdrError::Internal(_)) {
            tracing::error!(error = %self.description(), "request failed");
        }
        let body = json!({
            "code": status.as_u16(),
            "description": self.description(),
        });
        (status, axum::Json(body)).into_response()
    }
}

pub type EdrResult<T> = Result<T, EdrError>;
