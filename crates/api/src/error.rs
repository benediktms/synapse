use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::backend::BackendError;

#[derive(Clone, Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized,
    NotFound(String),
    Conflict(String),
    Unready(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            Self::Conflict(msg) => (StatusCode::CONFLICT, msg),
            Self::Unready(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<BackendError> for ApiError {
    fn from(err: BackendError) -> Self {
        match err {
            BackendError::UnknownWorkspace(ws) => {
                Self::NotFound(format!("unknown workspace: {ws}"))
            }
            BackendError::Domain(err) => err.into(),
        }
    }
}

impl From<domain::Error> for ApiError {
    fn from(err: domain::Error) -> Self {
        use domain::Error;
        match &err {
            Error::NotFound(_) => Self::NotFound(err.to_string()),
            Error::Conflict(_) => Self::Conflict(err.to_string()),
            Error::Store(_) | Error::Embed(_) => Self::Internal(err.to_string()),
            _ => Self::BadRequest(err.to_string()),
        }
    }
}
