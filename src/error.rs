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

impl ApiError {
    /// The JSON body this error responds with.
    ///
    /// `message` is written last on purpose: an `extra` carrying its own
    /// `message` key would otherwise replace the real one, and the callers of
    /// `with_extra` are exactly the paths whose message a client matches on.
    fn body(&self) -> Value {
        let mut body = serde_json::Map::new();

        if let Some(extra) = self.extra.as_ref().and_then(Value::as_object) {
            body.extend(extra.iter().map(|(key, value)| (key.clone(), value.clone())));
        }

        body.insert("message".to_string(), json!(self.message));

        Value::Object(body)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_fields_sit_beside_the_message() {
        let error = ApiError::new(StatusCode::CONFLICT, "achievements/souvenir-conflict")
            .with_extra(json!({ "reason": "reservation_not_found", "clientId": "c1" }));

        assert_eq!(
            error.body(),
            json!({
                "message": "achievements/souvenir-conflict",
                "reason": "reservation_not_found",
                "clientId": "c1",
            })
        );
    }

    /// The launcher decides how to recover from the message, so an `extra`
    /// must never be able to stand in for it.
    #[test]
    fn extra_cannot_replace_the_message() {
        let error = ApiError::bad_request("real message")
            .with_extra(json!({ "message": "impostor" }));

        assert_eq!(error.body()["message"], "real message");
    }

    #[test]
    fn a_non_object_extra_is_ignored() {
        let error = ApiError::bad_request("plain").with_extra(json!("not an object"));

        assert_eq!(error.body(), json!({ "message": "plain" }));
    }
}
