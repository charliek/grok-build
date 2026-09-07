//! One error envelope for every route: `{ "error": "<code>", "message": "…" }`.
//!
//! The shape matches shed's hub envelope (`shed/internal/ext/rc/hub.go`) so a mobile client parses
//! gx errors with the code it already has. `error` is a stable machine code; `message` is prose and
//! may change.
//!
//! C4 raises four of the plan's codes; `unknown_approval`, `already_submitted`, `already_resolved`
//! and `not_accepting` arrive with the approval and messaging routes in C5/C6.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::acp_client::AcpError;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Missing or wrong bearer token. Deliberately says nothing about which.
    #[error("a valid token is required")]
    Unauthorized,
    #[error("{0}")]
    BadRequest(String),
    #[error("no session {0}")]
    UnknownSession(String),
    /// The leader did not answer, answered an error, or is gone. From a client's point of view
    /// these are one condition: retry later.
    #[error("{0}")]
    LeaderUnavailable(String),
}

impl ApiError {
    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::BadRequest(_) => "bad_request",
            Self::UnknownSession(_) => "unknown_session",
            Self::LeaderUnavailable(_) => "leader_unavailable",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::UnknownSession(_) => StatusCode::NOT_FOUND,
            Self::LeaderUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// Any leader-side failure is `leader_unavailable`: the caller's recourse is the same either way.
impl From<AcpError> for ApiError {
    fn from(err: AcpError) -> Self {
        Self::LeaderUnavailable(err.to_string())
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.code(),
            message: self.to_string(),
        };
        (self.status(), Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_its_pinned_code_and_status() {
        let cases = [
            (ApiError::Unauthorized, "unauthorized", 401),
            (ApiError::BadRequest("x".into()), "bad_request", 400),
            (ApiError::UnknownSession("s".into()), "unknown_session", 404),
            (
                ApiError::LeaderUnavailable("x".into()),
                "leader_unavailable",
                503,
            ),
        ];
        for (err, code, status) in cases {
            assert_eq!(err.code(), code);
            assert_eq!(err.status().as_u16(), status);
        }
    }

    #[test]
    fn an_acp_failure_maps_to_leader_unavailable() {
        let err: ApiError = AcpError::Closed.into();
        assert_eq!(err.code(), "leader_unavailable");
        assert_eq!(err.status().as_u16(), 503);
    }
}
