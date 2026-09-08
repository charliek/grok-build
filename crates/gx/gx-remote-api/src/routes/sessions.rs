//! `GET /v1/sessions` and `GET /v1/sessions/{id}` — the roster, plus lazy attach.
//!
//! Neither route attaches. Listing sessions and reading one session's metadata are the two things a
//! phone does before it has decided which session it cares about; attaching to all of them would
//! make every roster poll pin every session resident in the leader.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::warn;
use xai_grok_shell::agent::roster::{RosterActivity, RosterEntry};

use crate::acp_client::AcpError;
use crate::error::ApiError;
use crate::state::AppState;

/// Logical names of the ACP methods this module drives. The leading `_` is applied on the wire by
/// [`crate::acp_client::wire_method`].
const SESSIONS_LIST: &str = "x.ai/sessions/list";
const SESSION_LIST: &str = "x.ai/session/list";
const SESSION_LOAD: &str = "session/load";

/// One row as this API renders it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: String,
    /// Roster activity, snake_case as the leader spells it: `working`, `idle`, `needs_input`,
    /// `dormant`, `completed`, `dead`.
    pub activity: String,
    pub resident: bool,
    pub model_id: Option<String>,
    pub last_change_unix_ms: i64,
    /// Whether *this lane* has `session/load`-ed the session. Not a property of the session: a TUI
    /// may well be attached to a row this says `false` for.
    pub attached: bool,
    /// Number of interactions waiting for an answer.
    pub pending_approvals: u32,
    /// `true` while `pendingApprovals` is inferred from `activity` rather than counted.
    ///
    /// A session this lane has attached is counted from the approval store and reads `false`; one
    /// it has not is the roster's `needs_input` bit rendered as 0-or-1, and stays `true`. The flag
    /// is not decoration: a client deciding whether to show "1 approval" or "needs your attention"
    /// has to know which it is holding.
    pub approximate: bool,
}

#[derive(Debug, Serialize)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
}

/// `GET /v1/sessions`
pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SessionList>, ApiError> {
    let sessions = fetch_roster(&state)
        .await?
        .iter()
        .map(|entry| summarize(&state, entry))
        .collect();
    Ok(Json(SessionList { sessions }))
}

/// `GET /v1/sessions/{id}`
pub async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SessionSummary>, ApiError> {
    Ok(Json(resolve_session(&state, &id).await?))
}

/// The roster entry for `id`, else a dormant row from the unified list, else `unknown_session`.
///
/// The two-step is what makes a session the phone has never touched addressable: the roster only
/// carries resident sessions plus the most recent persisted summaries, while `x.ai/session/list` is
/// the full local listing.
pub async fn resolve_session(state: &Arc<AppState>, id: &str) -> Result<SessionSummary, ApiError> {
    if let Some(entry) = fetch_roster(state)
        .await?
        .into_iter()
        .find(|e| e.session_id == id)
    {
        return Ok(summarize(state, &entry));
    }

    if let Some(summary) =
        unified_list_fallback(state, id, state.attachments.is_attached(id)).await?
    {
        return Ok(summary);
    }

    Err(ApiError::UnknownSession(id.to_string()))
}

/// Attach to `session` if this lane has not already, and block until the leader confirms.
///
/// Takes the **resolved** row rather than a bare id because residency is half the decision. The
/// lane stays subscribed to a session across an unload — the leader detaches a session when its
/// last non-observer subscriber disconnects but keeps the observer's subscription (see
/// `leader/server.rs`'s disconnect handler), and the agent then unloads it if it is idle. The
/// cached attachment survives that, so a roster row saying the session is **not resident** is the
/// lane's cue that its cache describes a session the agent no longer holds: drop it first, so the
/// `session/load` is actually re-issued. Every caller already resolves the row immediately before
/// this, so nothing pays for an extra round trip.
///
/// `_meta.noReplay` suppresses the transcript replay: a phone reads history over
/// `/v1/sessions/{id}/history`, so replaying it down the ACP link would be a large duplicate for
/// nothing. `mcpServers: []` matches what every other attaching client sends — the servers belong
/// to the session, not to the attaching client.
///
/// Called once per request, only by routes that touch a session's *content*; see the module docs.
pub async fn ensure_attached(
    state: &Arc<AppState>,
    session: &SessionSummary,
) -> Result<(), ApiError> {
    if !session.resident {
        state.attachments.detach(&session.session_id);
    }
    attach(state, &session.session_id, &session.cwd).await
}

/// Drop the cached attachment and `session/load` again, whatever the cache said.
///
/// The recovery half of [`session_scoped_request`]: the roster's `resident` bit can be a moment
/// stale, and the only other evidence that the agent unloaded a session is its refusal of the
/// request itself.
async fn reattach(state: &Arc<AppState>, session: &SessionSummary) -> Result<(), ApiError> {
    state.attachments.detach(&session.session_id);
    attach(state, &session.session_id, &session.cwd).await
}

/// Run one session-scoped ACP request, recovering **once** from the agent having unloaded the
/// session underneath the lane.
///
/// [`ensure_attached`] closes the window the roster can see; this closes the one it cannot. Between
/// the roster read that said `resident` and this request, the session's last TUI can disconnect and
/// the agent can unload an idle session — and because the lane's attachment is cached, no
/// `session/load` went out to bring it back. The agent answers with `invalid_params` /
/// `unknown session id` (`agent/mvp_agent/acp_agent.rs`), which is specific enough to act on: drop
/// the cache, re-issue the load, replay the request.
///
/// Exactly once. A second failure is a real one — a dead session actor, a session genuinely gone —
/// and surfaces as the `503 leader_unavailable` it always did, rather than a retry loop.
///
/// The caller must have attached already: this is the *request*, not the attach.
pub async fn session_scoped_request(
    state: &Arc<AppState>,
    session: &SessionSummary,
    method: &str,
    params: Value,
) -> Result<Value, ApiError> {
    match state.acp.request(method, params.clone()).await {
        Err(err) if err.is_unknown_session() => {
            warn!(
                session_id = %session.session_id,
                method,
                "gx-remote-api: the agent no longer holds this session; re-attaching and retrying once"
            );
            reattach(state, session).await?;
            Ok(state.acp.request(method, params).await?)
        }
        other => Ok(other?),
    }
}

/// [`session_scoped_request`] for a call whose answer arrives far too late to hold an HTTP request
/// open for: same one-shot recovery, performed on the background task instead of in the handler.
///
/// `session/prompt` is the only caller. Its JSON-RPC response does not arrive until the **turn
/// ends**, so the route puts the prompt on the wire and answers `202 accepted` immediately (see
/// [`crate::routes::messages`]) — and that is exactly what made the plain
/// [`AcpClient::request_detached`] wrong here. The unloaded-session window
/// [`session_scoped_request`] exists to close is *widest* on this path: a prompt sent from a phone
/// lands on a session whose last TUI may have exited seconds ago, and the refusal it earns was only
/// ever logged. The caller had already been told `202`; the prompt was gone.
///
/// So the return value keeps its meaning — `Ok(())` still says only "queued for the leader", never
/// "the agent accepted it" — and the recovery moves to where the answer actually arrives. Exactly
/// once, on the same discriminator ([`AcpError::is_unknown_session`]) and via the same
/// [`reattach`], so the two flows cannot drift on when a retry is legitimate.
///
/// A second refusal is the last word: it is logged by the ACP client like every other detached
/// failure and goes no further. There is no HTTP request left to fail by then — the client is on
/// the event stream, which is where a turn that does not happen is visible as silence.
///
/// [`AcpClient::request_detached`]: crate::acp_client::AcpClient::request_detached
/// [`AcpError::is_unknown_session`]: crate::acp_client::AcpError::is_unknown_session
pub async fn session_scoped_request_detached(
    state: &Arc<AppState>,
    session: &SessionSummary,
    method: &str,
    params: Value,
) -> Result<(), ApiError> {
    let on_outcome = {
        let state = state.clone();
        let session = session.clone();
        let method = method.to_string();
        let params = params.clone();
        move |outcome: Result<Value, AcpError>| async move {
            let Err(err) = outcome else { return };
            if !err.is_unknown_session() {
                // Everything else is either fine or not ours to fix; the client already logged it.
                return;
            }
            warn!(
                session_id = %session.session_id,
                method = %method,
                "gx-remote-api: the agent no longer holds this session; re-attaching and replaying the queued request once"
            );
            if let Err(err) = reattach(&state, &session).await {
                warn!(
                    session_id = %session.session_id,
                    method = %method,
                    %err,
                    "gx-remote-api: re-attach failed; the queued request is lost"
                );
                return;
            }
            // Detached again, so a second refusal is logged rather than retried: this is the one
            // retry, not the first of many.
            if let Err(err) = state.acp.request_detached(&method, params).await {
                warn!(
                    session_id = %session.session_id,
                    method = %method,
                    %err,
                    "gx-remote-api: replaying the queued request failed"
                );
            }
        }
    };

    state
        .acp
        .request_detached_with(method, params, on_outcome)
        .await?;
    Ok(())
}

/// The `session/load` itself, behind the per-session [`OnceCell`] that makes two concurrent
/// first-touches send exactly one.
///
/// [`OnceCell`]: tokio::sync::OnceCell
async fn attach(state: &Arc<AppState>, session_id: &str, cwd: &str) -> Result<(), ApiError> {
    let slot = state.attachments.slot(session_id);
    slot.get_or_try_init(|| async {
        state
            .acp
            .request(
                SESSION_LOAD,
                json!({
                    "sessionId": session_id,
                    "cwd": cwd,
                    "mcpServers": [],
                    "_meta": { "noReplay": true },
                }),
            )
            .await
            .map(|_| ())
    })
    .await
    .map_err(|err| {
        ApiError::LeaderUnavailable(format!("attaching to session {session_id}: {err}"))
    })?;
    Ok(())
}

/// The `sessions` array of an `x.ai/sessions/list` or `x.ai/session/list` response, or empty when
/// the field is missing or not an array.
fn sessions_array(response: &Value) -> Vec<Value> {
    response
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `x.ai/sessions/list` → the roster.
///
/// Rows are decoded one at a time and a row that fails is skipped with a warning, not fatal: one
/// malformed entry must not blank the whole list for a phone.
async fn fetch_roster(state: &Arc<AppState>) -> Result<Vec<RosterEntry>, ApiError> {
    let response = state.acp.request(SESSIONS_LIST, json!({})).await?;

    Ok(sessions_array(&response)
        .into_iter()
        .filter_map(|row| match serde_json::from_value::<RosterEntry>(row) {
            Ok(entry) => Some(entry),
            Err(err) => {
                warn!(%err, "gx-remote-api: skipping an undecodable roster entry");
                None
            }
        })
        .collect())
}

/// One roster row as this API renders it, against the lane's own view of the session.
///
/// The single place a summary is built — the roster GET, the per-session GET, and the SSE lane's
/// `event: session` frame all come through here, so a client parses one shape and the three can
/// never disagree about `attached` or `pendingApprovals`.
pub fn summarize(state: &AppState, entry: &RosterEntry) -> SessionSummary {
    let attached = state.attachments.is_attached(&entry.session_id);
    // Counted only where counting means something. For a session this lane never attached, the
    // approval store is empty because nothing was ever captured — not because nothing is pending —
    // so the roster's own bit is the honest answer, flagged as the approximation it is.
    let counted = attached.then(|| state.approvals.pending_count(&entry.session_id));

    SessionSummary {
        session_id: entry.session_id.clone(),
        title: entry.title.clone(),
        cwd: entry.cwd.clone(),
        activity: activity_wire_name(entry.activity),
        resident: entry.resident,
        model_id: entry.model_id.clone(),
        last_change_unix_ms: entry.last_change_unix_ms,
        attached,
        pending_approvals: counted
            .unwrap_or_else(|| u32::from(entry.activity == RosterActivity::NeedsInput)),
        approximate: counted.is_none(),
    }
}

/// The leader's own spelling of an activity, obtained from its `Serialize` impl so the two can
/// never drift.
fn activity_wire_name(activity: RosterActivity) -> String {
    serde_json::to_value(activity)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "unknown".to_string())
}

/// `x.ai/session/list` filtered to local build rows, for a session the roster has aged out.
async fn unified_list_fallback(
    state: &Arc<AppState>,
    id: &str,
    attached: bool,
) -> Result<Option<SessionSummary>, ApiError> {
    let response = state.acp.request(SESSION_LIST, json!({})).await?;

    let Some(row) = sessions_array(&response)
        .into_iter()
        .filter(is_local_build_row)
        .find(|row| row.get("sessionId").and_then(Value::as_str) == Some(id))
    else {
        return Ok(None);
    };

    Ok(Some(SessionSummary {
        session_id: id.to_string(),
        title: row
            .get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(String::from),
        cwd: row
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // The unified list is a disk listing: anything only it knows about is by definition not
        // resident in the leader.
        activity: activity_wire_name(RosterActivity::Dormant),
        resident: false,
        model_id: row.get("modelId").and_then(Value::as_str).map(String::from),
        last_change_unix_ms: rfc3339_to_unix_ms(row.get("updatedAt").and_then(Value::as_str))
            .unwrap_or(0),
        attached,
        pending_approvals: 0,
        approximate: true,
    }))
}

/// `_meta["x.ai/session"].kind == "build"`: a local coding session, as opposed to an imported
/// cloud chat, which this API cannot drive.
fn is_local_build_row(row: &Value) -> bool {
    row.get("_meta")
        .and_then(|m| m.get("x.ai/session"))
        .and_then(|s| s.get("kind"))
        .and_then(Value::as_str)
        == Some("build")
}

fn rfc3339_to_unix_ms(value: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value?)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster_entry(activity: RosterActivity) -> RosterEntry {
        serde_json::from_value(json!({
            "sessionId": "sess-1",
            "title": "Fix the flaky test",
            "cwd": "/home/u/proj",
            "isWorktree": false,
            "modelId": "grok-4",
            "yolo": false,
            "activity": serde_json::to_value(activity).unwrap(),
            "resident": true,
            "lastChangeUnixMs": 1_700_000_000_000_i64,
            "origin": { "kind": "local" },
        }))
        .unwrap()
    }

    #[test]
    fn activity_names_match_the_leaders_own_spelling() {
        assert_eq!(activity_wire_name(RosterActivity::Working), "working");
        assert_eq!(
            activity_wire_name(RosterActivity::NeedsInput),
            "needs_input"
        );
        assert_eq!(activity_wire_name(RosterActivity::Dormant), "dormant");
        assert_eq!(activity_wire_name(RosterActivity::Completed), "completed");
        assert_eq!(activity_wire_name(RosterActivity::Dead), "dead");
        assert_eq!(activity_wire_name(RosterActivity::Idle), "idle");
    }

    /// An `AppState` with no leader behind it: enough to exercise [`summarize`], which only reads
    /// the attachment set and the approval store.
    fn state_for_summaries() -> Arc<AppState> {
        let attachments = Arc::new(crate::state::Attachments::default());
        let approvals = Arc::new(crate::approvals::ApprovalStore::new(attachments.clone()));
        let (link, _handle) = crate::link::FakeLink::new();
        Arc::new(AppState {
            acp: crate::acp_client::AcpClient::spawn(
                link,
                tokio_util::sync::CancellationToken::new(),
                std::time::Duration::from_secs(5),
                approvals.clone(),
            ),
            token: crate::auth::Token::from_secret("0000"),
            health: crate::state::HealthInfo {
                version: "test".into(),
                leader_pid: 1,
                instance_id: "inst".into(),
            },
            attachments,
            approvals,
            ring: crate::ring::EventRing::new(),
            sse: crate::state::SseSettings::default(),
        })
    }

    #[tokio::test]
    async fn an_unattached_session_falls_back_to_the_roster_bit_and_says_so() {
        let state = state_for_summaries();
        for activity in [
            RosterActivity::Working,
            RosterActivity::Idle,
            RosterActivity::Dormant,
            RosterActivity::Completed,
            RosterActivity::Dead,
        ] {
            let summary = summarize(&state, &roster_entry(activity));
            assert_eq!(summary.pending_approvals, 0, "{activity:?}");
            assert!(summary.approximate, "{activity:?}");
            assert!(!summary.attached, "{activity:?}");
        }
        let summary = summarize(&state, &roster_entry(RosterActivity::NeedsInput));
        assert_eq!(summary.pending_approvals, 1);
        assert!(summary.approximate, "the roster's bit is not a count");
    }

    #[tokio::test]
    async fn an_attached_session_is_counted_from_the_store_and_says_that_too() {
        let state = state_for_summaries();
        state.attachments.mark_attached("sess-1");

        // Attached, nothing captured: an exact zero, even though the roster still says needs_input.
        let summary = summarize(&state, &roster_entry(RosterActivity::NeedsInput));
        assert_eq!(summary.pending_approvals, 0);
        assert!(!summary.approximate);
        assert!(summary.attached);

        for id in ["tc-1", "tc-2"] {
            state
                .approvals
                .intercept(
                    crate::approvals::REQUEST_PERMISSION,
                    &json!(1),
                    &json!({ "sessionId": "sess-1", "toolCall": { "toolCallId": id } }),
                )
                .unwrap();
        }
        // Two at once is a count the roster's single bit cannot express.
        let summary = summarize(&state, &roster_entry(RosterActivity::Working));
        assert_eq!(summary.pending_approvals, 2);
        assert!(!summary.approximate);
    }

    #[test]
    fn only_local_build_rows_are_eligible_for_the_fallback() {
        assert!(is_local_build_row(
            &json!({ "_meta": { "x.ai/session": { "kind": "build" } } })
        ));
        assert!(!is_local_build_row(
            &json!({ "_meta": { "x.ai/session": { "kind": "chat" } } })
        ));
        assert!(!is_local_build_row(&json!({ "_meta": {} })));
        assert!(!is_local_build_row(&json!({})));
    }

    #[test]
    fn updated_at_becomes_unix_millis() {
        assert_eq!(
            rfc3339_to_unix_ms(Some("2026-01-01T00:00:00Z")),
            Some(1_767_225_600_000)
        );
        assert_eq!(rfc3339_to_unix_ms(Some("not a date")), None);
        assert_eq!(rfc3339_to_unix_ms(None), None);
    }
}
