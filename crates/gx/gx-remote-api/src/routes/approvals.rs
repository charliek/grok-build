//! `GET`/`POST /v1/sessions/{id}/approvals[/{toolCallId}]` — read what is waiting, answer it from
//! whatever connection the client has now.
//!
//! # Why these are session-scoped
//!
//! An approval is keyed by `(sessionId, toolCallId)`. A tool call id is unique inside its session's
//! transcript and nothing promises more than that, so there is deliberately no
//! `/v1/approvals/{toolCallId}`: it would be a route that works until two sessions collide.
//!
//! # Answering: one example per kind
//!
//! The body is `{"response": <the ACP result object>}` and `response` is forwarded **verbatim** as
//! the JSON-RPC `result`. The four shapes, from the types that deserialize them:
//!
//! - **`permission`** (`RequestPermissionResponse`, ACP schema `client.rs`) — the outcome is an
//!   object, tagged again on `outcome`:
//!   ```json
//!   { "response": { "outcome": { "outcome": "selected", "optionId": "allow-once" } } }
//!   ```
//!   `optionId` must be one the request offered in `request.options[].optionId`; the kinds are
//!   `allow_once`, `allow_always`, `reject_once`, `reject_always`. Declining the whole turn is
//!   `{ "outcome": { "outcome": "cancelled" } }`.
//! - **`question`** (`AskUserQuestionExtResponse`) — tagged on `outcome`, answers keyed by question
//!   id, each a **list** of chosen labels:
//!   ```json
//!   { "response": { "outcome": "accepted", "answers": { "q1": ["Use Postgres"] } } }
//!   ```
//!   The other outcomes are `chat_about_this`, `skip_interview` and `cancelled`.
//! - **`plan_approval`** (`ExitPlanModeExtResponse`) — a bare string outcome, with optional
//!   feedback on a refusal:
//!   ```json
//!   { "response": { "outcome": "approved" } }
//!   ```
//!   The other outcomes are `cancelled` and `abandoned`; `"approved"`, never `"approve"`.
//! - **`mcp_elicitation`** (`McpElicitExtResponse`) — also tagged on `outcome`, and `content` is
//!   whatever the server's `requestedSchema` asked for:
//!   ```json
//!   { "response": { "outcome": "accept", "content": { "email": "me@example.com" } } }
//!   ```
//!   The other outcomes are `decline` and `cancel`.
//!
//! Nothing here validates those shapes. The agent is the authority on what it accepts, and a lane
//! that type-checked them would need a release every time an option kind is added — while a
//! malformed answer is already handled: the agent logs it and cancels the interaction.
//!
//! # Two races a client has to expect
//!
//! **The TUI answers first.** The leader broadcasts an interaction to every subscriber and the
//! agent takes the first answer with no per-answer acknowledgement. So a POST that returns `202`
//! means *sent*, not *accepted*, and the entry only becomes `resolved` when the agent says the
//! interaction closed — by our answer or somebody else's. A POST after that is
//! `409 already_resolved`, which is the honest report of a race this lane lost.
//!
//! **The first read after an attach can be empty.** These routes attach lazily like every other
//! session route, and the leader replays its cached interaction requests *after* the `session/load`
//! response, asynchronously. A client whose very first call on a session is
//! `GET …/approvals` may therefore see nothing and find the approval a moment later on the event
//! stream (`event: approval`) or on the next poll.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approvals::Approval;
use crate::error::ApiError;
use crate::routes::parse_body;
use crate::routes::sessions::{ensure_attached, resolve_session};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct ApprovalList {
    pub approvals: Vec<Approval>,
}

#[derive(Debug, Deserialize)]
struct AnswerBody {
    /// The ACP result object for the interaction's method. Passed through untouched.
    response: Value,
}

/// `GET /v1/sessions/{id}/approvals`
///
/// Open approvals first (oldest first), then recently resolved ones (newest first), capped at 50
/// per session and one hour.
pub async fn list_approvals(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ApprovalList>, ApiError> {
    attach(&state, &id).await?;
    Ok(Json(ApprovalList {
        approvals: state.approvals.list(&id),
    }))
}

/// `GET /v1/sessions/{id}/approvals/{toolCallId}`
pub async fn get_approval(
    State(state): State<Arc<AppState>>,
    Path((id, tool_call_id)): Path<(String, String)>,
) -> Result<Json<Approval>, ApiError> {
    attach(&state, &id).await?;
    Ok(Json(state.approvals.get(&id, &tool_call_id)?))
}

/// `POST /v1/sessions/{id}/approvals/{toolCallId}`
///
/// `202 {"status":"submitted"}` when the answer went out; `409 already_submitted` /
/// `409 already_resolved` when it was too late; `404 unknown_approval` when there is no such entry;
/// `400 bad_request` when `response` is missing or is not a JSON object.
pub async fn post_approval(
    State(state): State<Arc<AppState>>,
    Path((id, tool_call_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body: AnswerBody = parse_body(&body)?;
    // A JSON-RPC `result` for these methods is always an object. Catching a bare string or array
    // here, rather than forwarding it, keeps a typo from cancelling a real interaction: the agent's
    // answer to an undeserializable response is to cancel the tool call.
    if !body.response.is_object() {
        return Err(ApiError::BadRequest(format!(
            "response must be a JSON object, got {}",
            type_name_of(&body.response)
        )));
    }

    attach(&state, &id).await?;

    let approval = state.approvals.submit(
        &id,
        &tool_call_id,
        body.response,
        // Called under the store's lock, which is what makes "still pending?" and "put it on the
        // wire" one step that two concurrent POSTs cannot interleave.
        |rpc_id, result| state.acp.respond(rpc_id, result),
    )?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": approval.status })),
    ))
}

/// 404 the session before anything else, then attach — the same order every content route uses.
///
/// Attaching is what makes approvals arrive at all: the store declines an interaction for a session
/// this lane has not loaded, and the `session/load` is also what asks the leader to replay the ones
/// already open.
async fn attach(state: &Arc<AppState>, session_id: &str) -> Result<(), ApiError> {
    let session = resolve_session(state, session_id).await?;
    ensure_attached(state, &session).await
}

/// What a client sent instead of an object, for the error message.
fn type_name_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_that_is_not_an_object_is_named_in_the_refusal() {
        assert_eq!(type_name_of(&json!("allow-once")), "a string");
        assert_eq!(type_name_of(&json!(["allow-once"])), "an array");
        assert_eq!(type_name_of(&Value::Null), "null");
        assert_eq!(type_name_of(&json!({})), "an object");
    }

    #[test]
    fn the_body_must_carry_a_response_field() {
        assert!(parse_body::<AnswerBody>(&Bytes::from_static(b"{}")).is_err());
        assert!(parse_body::<AnswerBody>(&Bytes::from_static(b"{\"response\":{}}")).is_ok());
        // Any JSON is accepted by the parse; the *object* check is the handler's, so its message
        // can say what arrived instead.
        assert_eq!(
            parse_body::<AnswerBody>(&Bytes::from_static(b"{\"response\":\"yes\"}"))
                .unwrap()
                .response,
            json!("yes")
        );
    }
}
