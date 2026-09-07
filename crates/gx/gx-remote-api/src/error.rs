//! One error envelope for every route: `{ "error": "<code>", "message": "…" }`.
//!
//! The shape matches shed's hub envelope (`shed/internal/ext/rc/hub.go`) so a mobile client parses
//! gx errors with the code it already has. `error` is a stable machine code; `message` is prose and
//! may change.
//!
//! C4 raised four of the plan's codes, C5 added `not_accepting`, and C6 completes the list with
//! `unknown_approval`, `already_submitted` and `already_resolved`.

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
    /// No such open or recently-resolved interaction. Approvals are keyed by
    /// `(sessionId, toolCallId)`, so both halves are named.
    #[error("no approval {tool_call_id} for session {session_id}")]
    UnknownApproval {
        session_id: String,
        tool_call_id: String,
    },
    /// This lane already put an answer for that interaction on the wire.
    #[error("{0}")]
    AlreadySubmitted(String),
    /// The agent closed the interaction — our answer, another client's, or a cancel. Which one is
    /// not observable: the leader acknowledges no individual answer.
    #[error("{0}")]
    AlreadyResolved(String),
    /// The session's current state does not admit this verb. Not a permission failure — the caller
    /// is authorized; the *session* is not accepting. See [`crate::policy`].
    #[error("{0}")]
    NotAccepting(String),
    /// The leader did not answer, answered an error, or is gone. From a client's point of view
    /// these are one condition: retry later.
    ///
    /// Also the answer to a POST against an approval this lane knows about only from a
    /// `pending_interaction` hint: the request it would reply to has not arrived, so there is
    /// nothing to answer *yet* — a retry, not a client error. See [`crate::routes::approvals`].
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
            Self::UnknownApproval { .. } => "unknown_approval",
            Self::AlreadySubmitted(_) => "already_submitted",
            Self::AlreadyResolved(_) => "already_resolved",
            Self::NotAccepting(_) => "not_accepting",
            Self::LeaderUnavailable(_) => "leader_unavailable",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::UnknownSession(_) => StatusCode::NOT_FOUND,
            Self::UnknownApproval { .. } => StatusCode::NOT_FOUND,
            Self::AlreadySubmitted(_) | Self::AlreadyResolved(_) => StatusCode::CONFLICT,
            Self::NotAccepting(_) => StatusCode::CONFLICT,
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
                ApiError::UnknownApproval {
                    session_id: "s".into(),
                    tool_call_id: "tc".into(),
                },
                "unknown_approval",
                404,
            ),
            (
                ApiError::AlreadySubmitted("x".into()),
                "already_submitted",
                409,
            ),
            (
                ApiError::AlreadyResolved("x".into()),
                "already_resolved",
                409,
            ),
            (ApiError::NotAccepting("x".into()), "not_accepting", 409),
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
    fn an_unknown_approval_names_both_halves_of_its_key() {
        // Approvals are keyed by `(sessionId, toolCallId)`; a message naming only one of them
        // would not tell a client which lookup missed.
        let message = ApiError::UnknownApproval {
            session_id: "sess-1".into(),
            tool_call_id: "tc-9".into(),
        }
        .to_string();
        assert!(message.contains("sess-1"), "{message}");
        assert!(message.contains("tc-9"), "{message}");
    }

    #[test]
    fn an_acp_failure_maps_to_leader_unavailable() {
        let err: ApiError = AcpError::Closed.into();
        assert_eq!(err.code(), "leader_unavailable");
        assert_eq!(err.status().as_u16(), 503);
    }
}
