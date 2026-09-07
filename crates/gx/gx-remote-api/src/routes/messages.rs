//! The write verbs: `POST /v1/sessions`, `…/{id}/messages`, `…/{id}/cancel`.
//!
//! Every one of them is attach → authorize → one ACP call, in that order. Attaching first is not
//! bureaucracy: a dormant session has no actor, so a prompt sent before the load would be routed at
//! nothing, and the authorization table's `dormant` row says "allow *after* attach" precisely
//! because the attach is what makes the allowance meaningful.
//!
//! # Why a queued prompt answers before the turn does
//!
//! `session/prompt`'s JSON-RPC response arrives when the **turn ends**. Awaiting it would hold an
//! HTTP request open for the length of a coding turn and then time out. The lane instead puts the
//! prompt on the wire, answers `202 accepted`, and lets the turn play out over
//! `/v1/sessions/{id}/events` — see [`crate::acp_client::AcpClient::request_detached`], which owns
//! the not-leaking part. `202` is the honest status: the prompt is queued, not completed.
//!
//! `_x.ai/interject` is the opposite shape — it answers immediately with `{status}` — so that one
//! is awaited and its status is passed through.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use xai_message_delivery_core::Operation;

use crate::error::ApiError;
use crate::policy;
use crate::routes::parse_body;
use crate::routes::sessions::{ensure_attached, resolve_session};
use crate::state::AppState;

/// Logical ACP method names; the leading `_` is applied on the wire by
/// [`crate::acp_client::wire_method`].
const SESSION_PROMPT: &str = "session/prompt";
const SESSION_CANCEL: &str = "session/cancel";
const SESSION_NEW: &str = "session/new";
const INTERJECT: &str = "x.ai/interject";

/// How a message is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `session/prompt`: the agent's own queue absorbs it. Always available.
    Queue,
    /// `_x.ai/interject`: text injected into a turn that is already running.
    Interject,
}

impl Mode {
    fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        match raw {
            None | Some("queue") => Ok(Self::Queue),
            Some("interject") => Ok(Self::Interject),
            Some(other) => Err(ApiError::BadRequest(format!(
                "mode must be \"queue\" or \"interject\", got {other:?}"
            ))),
        }
    }

    fn wire_name(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Interject => "interject",
        }
    }

    /// The shared-vocabulary operation this mode asks for.
    fn operation(self) -> Operation {
        match self {
            Self::Queue => Operation::Queue,
            Self::Interject => Operation::Interject,
        }
    }
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    text: String,
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    cwd: String,
    #[serde(default)]
    text: Option<String>,
}

/// `POST /v1/sessions/{id}/messages`
pub async fn post_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body: MessageBody = parse_body(&body)?;
    let mode = Mode::parse(body.mode.as_deref())?;
    if body.text.trim().is_empty() {
        return Err(ApiError::BadRequest("text must not be empty".into()));
    }

    let session = resolve_session(&state, &id).await?;
    ensure_attached(&state, &id, &session.cwd).await?;
    policy::authorize(
        &id,
        &policy::effective_activity(&state.approvals, &id, &session.activity),
        mode.operation(),
    )?;

    match mode {
        Mode::Queue => {
            state
                .acp
                .request_detached(SESSION_PROMPT, prompt_params(&id, &body.text))
                .await?;
            Ok(accepted(
                json!({ "accepted": true, "mode": mode.wire_name() }),
            ))
        }
        Mode::Interject => {
            let response = state
                .acp
                .request(INTERJECT, json!({ "sessionId": id, "text": body.text }))
                .await?;
            Ok(accepted(json!({
                "accepted": true,
                "mode": mode.wire_name(),
                "status": interject_status(&response),
            })))
        }
    }
}

/// `POST /v1/sessions/{id}/cancel`
///
/// `session/cancel` is a **notification**: no id, no response, nothing to await. The 202 says the
/// cancel was handed to the leader, not that the turn has stopped — the turn's actual end arrives
/// on the event stream like every other turn boundary.
pub async fn post_cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let session = resolve_session(&state, &id).await?;
    ensure_attached(&state, &id, &session.cwd).await?;
    policy::authorize(
        &id,
        &policy::effective_activity(&state.approvals, &id, &session.activity),
        Operation::InterruptAndSend,
    )?;

    state
        .acp
        .notify(SESSION_CANCEL, json!({ "sessionId": id }))?;
    Ok(accepted(json!({ "accepted": true })))
}

/// `POST /v1/sessions`
///
/// The lane is an **observer**, so the session this creates has no driver until a TUI attaches to
/// it. That is expected and documented (plan D3): driver-only reverse-requests and scheduled prompt
/// injections are dropped by the leader in the meantime, and the session is otherwise ordinary.
pub async fn create_session(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body: CreateBody = parse_body(&body)?;
    if body.cwd.trim().is_empty() {
        return Err(ApiError::BadRequest("cwd must not be empty".into()));
    }

    let response = state
        .acp
        .request(SESSION_NEW, json!({ "cwd": body.cwd, "mcpServers": [] }))
        .await?;
    let session_id = response
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::LeaderUnavailable(format!("session/new returned no sessionId: {response}"))
        })?
        .to_string();

    // `session/new` already subscribed this client; a `session/load` on the next touch would be a
    // redundant round trip that makes the agent flush and replay.
    state.attachments.mark_attached(&session_id);

    // A brand-new session is idle, and idle admits `Queue` — the authorization table's first
    // column. Sent detached for the same reason as any other queued prompt.
    if let Some(text) = body.text.as_deref().filter(|t| !t.trim().is_empty()) {
        state
            .acp
            .request_detached(SESSION_PROMPT, prompt_params(&session_id, text))
            .await?;
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({ "sessionId": session_id })),
    ))
}

/// The ACP prompt shape: a content block array, never a bare string.
fn prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
    })
}

/// `_x.ai/interject` answers `{"status":"queued"}` (`docs/gx/handoff/interject.md`), already
/// unwrapped from its `ExtMethodResult` envelope by the ACP client. A leader that answers without
/// the field still queued the text, so the field is defaulted rather than treated as a failure.
fn interject_status(response: &Value) -> &str {
    response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("queued")
}

fn accepted(body: Value) -> (StatusCode, Json<Value>) {
    (StatusCode::ACCEPTED, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_defaults_to_queue_and_rejects_anything_else() {
        assert_eq!(Mode::parse(None).unwrap(), Mode::Queue);
        assert_eq!(Mode::parse(Some("queue")).unwrap(), Mode::Queue);
        assert_eq!(Mode::parse(Some("interject")).unwrap(), Mode::Interject);

        let err = Mode::parse(Some("steer")).unwrap_err();
        assert_eq!(err.code(), "bad_request");
        assert!(err.to_string().contains("steer"), "{err}");
        // Case matters: the wire spelling is exact, and a near miss must not silently queue.
        assert_eq!(
            Mode::parse(Some("Queue")).unwrap_err().code(),
            "bad_request"
        );
    }

    #[test]
    fn each_mode_asks_for_the_operation_the_policy_table_names() {
        assert_eq!(Mode::Queue.operation(), Operation::Queue);
        assert_eq!(Mode::Interject.operation(), Operation::Interject);
    }

    #[test]
    fn a_prompt_is_a_content_block_array() {
        let params = prompt_params("sess-1", "hello");
        assert_eq!(params["sessionId"], "sess-1");
        assert_eq!(params["prompt"][0]["type"], "text");
        assert_eq!(params["prompt"][0]["text"], "hello");
    }

    #[test]
    fn an_interject_status_falls_back_rather_than_failing() {
        assert_eq!(interject_status(&json!({ "status": "queued" })), "queued");
        assert_eq!(interject_status(&json!({ "status": "steered" })), "steered");
        assert_eq!(interject_status(&json!({})), "queued");
        assert_eq!(interject_status(&Value::Null), "queued");
    }

    #[test]
    fn a_malformed_body_is_a_bad_request_in_our_own_envelope() {
        let err = parse_body::<MessageBody>(&Bytes::from_static(b"{not json")).unwrap_err();
        assert_eq!(err.code(), "bad_request");
        assert_eq!(err.status().as_u16(), 400);

        let err = parse_body::<MessageBody>(&Bytes::from_static(b"{}")).unwrap_err();
        assert_eq!(err.code(), "bad_request", "`text` is required");
    }
}
