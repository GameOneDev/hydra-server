use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// Extra fields merged into the JSON body next to `message`.
    ///
    /// Most errors only need a message. The souvenir sync is the exception:
    /// the launcher reads a machine-readable `reason` (and echoes the
    /// `clientId` back) to decide whether to retry, re-upload or give up, so
    /// those handlers attach the same fields the official API sends.
    pub extra: Option<Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            extra: None,
        }
    }

    /// Attaches extra top-level fields to the error body. Ignored unless
    /// `extra` is a JSON object.
    pub fn with_extra(mut self, extra: Value) -> Self {
        self.extra = Some(extra);
        self
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "message": self.message });

        if let (Some(object), Some(extra)) = (
            body.as_object_mut(),
            self.extra.as_ref().and_then(Value::as_object),
        ) {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }

        (self.status, Json(body)).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!("database error: {err}");
        Self::internal("database error")
    }
}

impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        tracing::error!("io error: {err}");
        Self::internal("storage error")
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
