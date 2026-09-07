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
    /// `true` while `pendingApprovals` is inferred from `activity` rather than counted. C6 keeps a
    /// real per-session map and clears this flag for attached sessions.
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
    let attached = state.attachments.attached_ids();
    let sessions = fetch_roster(&state)
        .await?
        .into_iter()
        .map(|entry| {
            let attached = attached.contains(&entry.session_id);
            summarize_roster_entry(&entry, attached)
        })
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
    let attached = state.attachments.is_attached(id);

    if let Some(entry) = fetch_roster(state)
        .await?
        .into_iter()
        .find(|e| e.session_id == id)
    {
        return Ok(summarize_roster_entry(&entry, attached));
    }

    if let Some(summary) = unified_list_fallback(state, id, attached).await? {
        return Ok(summary);
    }

    Err(ApiError::UnknownSession(id.to_string()))
}

/// Attach to `session_id` if this lane has not already, and block until the leader confirms.
///
/// `_meta.noReplay` suppresses the transcript replay: a phone reads history over
/// `/v1/sessions/{id}/history`, so replaying it down the ACP link would be a large duplicate for
/// nothing. `mcpServers: []` matches what every other attaching client sends — the servers belong
/// to the session, not to the attaching client.
///
/// Called only by routes that touch a session's *content*; see the module docs.
pub async fn ensure_attached(
    state: &Arc<AppState>,
    session_id: &str,
    cwd: &str,
) -> Result<(), ApiError> {
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

fn summarize_roster_entry(entry: &RosterEntry, attached: bool) -> SessionSummary {
    SessionSummary {
        session_id: entry.session_id.clone(),
        title: entry.title.clone(),
        cwd: entry.cwd.clone(),
        activity: activity_wire_name(entry.activity),
        resident: entry.resident,
        model_id: entry.model_id.clone(),
        last_change_unix_ms: entry.last_change_unix_ms,
        attached,
        // C6 replaces this with the real pending-interaction map for attached sessions.
        pending_approvals: u32::from(entry.activity == RosterActivity::NeedsInput),
        approximate: true,
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

    #[test]
    fn needs_input_is_the_only_activity_that_infers_a_pending_approval() {
        for activity in [
            RosterActivity::Working,
            RosterActivity::Idle,
            RosterActivity::Dormant,
            RosterActivity::Completed,
            RosterActivity::Dead,
        ] {
            let summary = summarize_roster_entry(&roster_entry(activity), false);
            assert_eq!(summary.pending_approvals, 0, "{activity:?}");
            assert!(summary.approximate);
        }
        let summary = summarize_roster_entry(&roster_entry(RosterActivity::NeedsInput), true);
        assert_eq!(summary.pending_approvals, 1);
        assert!(summary.approximate);
        assert!(summary.attached);
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
