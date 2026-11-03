use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("unprocessable: {0}")]
    Unprocessable(String),
    #[error("too many requests: {0}")]
    TooManyRequests(String),
    /// An upstream the server called (a git remote) failed.
    #[error("bad gateway: {0}")]
    BadGateway(String),
    #[error("internal error: {0}")]
    Internal(anyhow::Error),
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: String,
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::Unsupported(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            AppError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            AppError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The text clients see: the detail of a client error, nothing of an internal one.
    pub fn message(&self) -> String {
        match self {
            AppError::BadRequest(m)
            | AppError::Forbidden(m)
            | AppError::Conflict(m)
            | AppError::Unsupported(m)
            | AppError::Unprocessable(m)
            | AppError::TooManyRequests(m)
            | AppError::BadGateway(m) => m.clone(),
            AppError::Unauthorized => "unauthorized".to_string(),
            AppError::NotFound => "not found".to_string(),
            AppError::PayloadTooLarge => "payload too large".to_string(),
            AppError::Internal(_) => "internal error".to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let AppError::Internal(e) = &self {
            tracing::error!(error = ?e, "internal error");
        }
        let body = ErrorBody {
            error: self.message(),
        };
        (self.status(), Json(body)).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Internal(e.into())
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Internal(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses() {
        let cases: Vec<(AppError, u16)> = vec![
            (AppError::BadRequest("x".into()), 400),
            (AppError::Unauthorized, 401),
            (AppError::Forbidden("x".into()), 403),
            (AppError::NotFound, 404),
            (AppError::Conflict("x".into()), 409),
            (AppError::PayloadTooLarge, 413),
            (AppError::Unsupported("x".into()), 415),
            (AppError::Unprocessable("x".into()), 422),
            (AppError::TooManyRequests("x".into()), 429),
            (AppError::BadGateway("x".into()), 502),
            (AppError::Internal(anyhow::anyhow!("boom")), 500),
        ];
        for (err, code) in cases {
            assert_eq!(err.status().as_u16(), code, "{err}");
        }
    }

    #[test]
    fn internal_message_is_hidden() {
        let e = AppError::Internal(anyhow::anyhow!("secret detail"));
        assert_eq!(e.message(), "internal error");
    }
}
