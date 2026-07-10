use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Structured API error with HTTP status code and JSON body.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Requested resource was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Request body or parameters are invalid.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Authentication failed.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Agent encountered an error during processing.
    #[error("agent error: {0}")]
    AgentError(String),

    /// Internal server error (database, IO, etc.).
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    code: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "BAD_REQUEST"),
            ApiError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED"),
            ApiError::AgentError(_) => (StatusCode::SERVICE_UNAVAILABLE, "AGENT_ERROR"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
        };
        let body = ErrorBody {
            error: self.to_string(),
            code,
        };
        (status, axum::Json(body)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_not_found() {
        let err = ApiError::NotFound("session".into());
        assert_eq!(err.to_string(), "not found: session");
    }

    #[test]
    fn api_error_bad_request() {
        let err = ApiError::BadRequest("missing field".into());
        assert!(err.to_string().contains("missing field"));
    }

    #[test]
    fn api_error_agent() {
        let err = ApiError::AgentError("timeout".into());
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn api_error_internal() {
        let err = ApiError::Internal("db".into());
        assert!(err.to_string().contains("db"));
    }

    #[test]
    fn api_error_from_anyhow() {
        let err: ApiError = anyhow::anyhow!("oops").into();
        assert!(err.to_string().contains("oops"));
    }
}
