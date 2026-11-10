//! API errors: a stable machine-readable code, an HTTP status, and a human message.
//! Internal failures are logged with detail and answered with a generic message, so a client
//! never sees database or engine internals.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Every code the gateway itself answers with; every other code is an engine reject reason
/// (`RejectReason`). `docs/api/openapi.yaml` lists exactly these plus the reject reasons.
pub const GATEWAY_CODES: &[&str] = &[
    "bad_request",
    "unauthorized",
    "forbidden",
    "not_found",
    "username_taken",
    "adult_required",
    "terms_required",
    "too_many_signups",
    "signups_paused",
    "too_large",
    "rate_limited",
    "too_many_in_flight",
    "unavailable",
    "timeout",
    "invalid_rulebook",
    "conflict",
    "too_early",
    "totp_required",
    "lease_expired",
];

/// An error answered to a client.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    /// The sequence number of the command, when the engine sequenced and rejected it.
    seq: Option<u64>,
}

impl ApiError {
    /// An error with a status, a stable code, and a message for the client.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        debug_assert!(
            GATEWAY_CODES.contains(&code),
            "undocumented gateway error code {code}"
        );
        Self {
            status,
            code,
            message: message.into(),
            seq: None,
        }
    }

    /// 400: the request is malformed.
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    /// 401: no valid credentials.
    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    /// 403: valid credentials, insufficient role.
    #[must_use]
    pub fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "this action needs the admin role",
        )
    }

    /// 403 with a specific reason.
    #[must_use]
    pub fn forbidden_because(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    /// 409: the request conflicts with the current state (a market that exists, a live
    /// resolution, a dispute already filed).
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    /// 409: allowed later, not now (before a market's release time, inside the dispute window).
    #[must_use]
    pub fn too_early(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "too_early", message)
    }

    /// 429: a rate limit was exceeded.
    #[must_use]
    pub fn rate_limited() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many requests; slow down",
        )
    }

    /// 500 or 503: something failed inside. The detail is logged, not returned.
    #[must_use]
    pub fn internal(detail: &dyn std::fmt::Display) -> Self {
        eprintln!(
            "{}",
            json!({ "level": "error", "msg": "internal error", "detail": detail.to_string() })
        );
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "the exchange could not complete the request",
        )
    }

    /// 404: no such resource.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// 422: the engine sequenced the command and rejected it; `code` is the reject reason.
    #[must_use]
    pub fn rejected(code: &'static str, seq: u64) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code,
            message: "the exchange rejected the command".to_owned(),
            seq: Some(seq),
        }
    }

    /// The error object clients see: `code`, `message`, and `seq` when the engine sequenced
    /// the rejected command.
    #[must_use]
    pub fn body(&self) -> serde_json::Value {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let (Some(seq), Some(map)) = (self.seq, error.as_object_mut()) {
            map.insert("seq".to_owned(), seq.into());
        }
        error
    }

    /// The HTTP status.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// The stable code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.body() }))).into_response()
    }
}

impl From<tokio_postgres::Error> for ApiError {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::internal(&brier_readmodel::describe_db_error(&e))
    }
}

impl From<brier_core::CoreError> for ApiError {
    fn from(e: brier_core::CoreError) -> Self {
        Self::internal(&e)
    }
}
