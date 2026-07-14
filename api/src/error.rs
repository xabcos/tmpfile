use axum::{
    extract::multipart::MultipartError,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::fmt;
use tracing::error;

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    NotFound(String),
    Unauthorized,
    PayloadTooLarge,
    Gone(String),
    InternalServerError(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "Bad Request: {msg}"),
            Self::NotFound(msg) => write!(f, "Not Found: {msg}"),
            Self::Unauthorized => write!(f, "Unauthorized"),
            Self::PayloadTooLarge => write!(f, "Payload Too Large"),
            Self::Gone(msg) => write!(f, "Gone: {msg}"),
            Self::InternalServerError(msg) => write!(f, "Internal Server Error: {msg}"),
        }
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".to_string()),
            Self::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "Payload Too Large".to_string(),
            ),
            Self::Gone(msg) => (StatusCode::GONE, msg),
            Self::InternalServerError(msg) => {
                error!(error = %msg, "Request failed with an internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal Server Error".to_string(),
                )
            }
        };

        (status, message).into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self::InternalServerError(format!("IO Error: {err}"))
    }
}

impl From<redis::RedisError> for AppError {
    fn from(err: redis::RedisError) -> Self {
        Self::InternalServerError(format!("Redis Error: {err}"))
    }
}

impl From<MultipartError> for AppError {
    fn from(err: MultipartError) -> Self {
        match err.status() {
            StatusCode::PAYLOAD_TOO_LARGE => Self::PayloadTooLarge,
            StatusCode::BAD_REQUEST => Self::BadRequest(err.body_text()),
            StatusCode::INTERNAL_SERVER_ERROR => Self::InternalServerError(err.body_text()),
            status => Self::InternalServerError(format!("Multipart error: {status}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn internal_error_details_are_not_exposed_to_clients() {
        let response = AppError::InternalServerError(
            "Redis connection failed at redis.internal:6379".to_string(),
        )
        .into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1_024).await.unwrap();
        assert_eq!(&body[..], b"Internal Server Error");
    }
}
