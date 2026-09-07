//! A blocking reverse-request, turned into addressable HTTP state.
//!
//! # Why this has to exist
//!
//! An approval in gx is an agent→client JSON-RPC **reverse-request**: the agent parks a tool-loop
//! future on a oneshot and waits for a client to answer *that request*, on the connection it
//! arrived on. A phone does not have a connection that lives that long. So the lane keeps custody
//! of the request — id and all — and lets a later HTTP call, over a completely different
//! connection, supply the answer.
//!
//! That is safe only because the leader already treats these four methods as **shared**: it
//! broadcasts them to every subscriber, caches the open ones per `(sessionId, toolCallId)`, replays
//! them to a client that attaches afterwards, and the agent takes the **first** answer
//! (`leader/server.rs::is_interaction_request`, and `SHARED_INTERACTIVE_MODALS.md`). The lane is
//! one more such subscriber, not a privileged one.
//!
//! # The four methods, and their answers
//!
//! Routing is on the **logical** name ([`crate::acp_client::logical_method`]): the three `x.ai/…`
//! ones travel as `_x.ai/…` on the wire, `session/request_permission` does not.
//!
//! | logical method | kind | the `response` object a client POSTs |
//! |---|---|---|
//! | `session/request_permission` | `permission` | `{"outcome":{"outcome":"selected","optionId":"…"}}` |
//! | `x.ai/ask_user_question` | `question` | `{"outcome":"accepted","answers":{"<questionId>":["<choice>"]}}` |
//! | `x.ai/exit_plan_mode` | `plan_approval` | `{"outcome":"approved"}` |
//! | `x.ai/mcp/elicit` | `mcp_elicitation` | `{"outcome":"accept","content":{…}}` |
//!
//! The body is passed through **verbatim** as the JSON-RPC `result`. Nothing here validates option
//! ids or answer shapes: the agent is the authority on what it will accept, and a lane that
//! type-checked these would have to be redeployed every time a new option kind appears.
//!
//! # Keying, and why never on `toolCallId` alone
//!
//! `(sessionId, toolCallId)` — the same key the leader's own interaction cache uses. A tool call id
//! is unique within a session's transcript; nothing documents it as unique across sessions, and a
//! collision under a global key would let one session's answer resolve another's modal.
//!
//! # Lifecycle
//!
//! ```text
//!   reverse-request  ─┐
//!                     ├─> pending ──POST──> submitted ──interaction_resolved──> resolved
//!   pending_interaction ┘   (placeholder until the request itself arrives)
//! ```
//!
//! `pending_interaction` and `interaction_resolved` are `x.ai/session_notification` updates
//! (`extensions/notification.rs`), fire-and-forget and never persisted. The first can arrive
//! **before** the reverse-request it describes — `PendingInteractionGuard::new` broadcasts it
//! before the gateway sends the request — so it creates a placeholder that the request then merges
//! into rather than duplicating.
//!
//! Only `interaction_resolved` moves an entry to `resolved`. A submitted answer gets **no
//! acknowledgement**: first-answer-wins means a losing answer is discarded by the agent in silence,
//! so an API that marked `resolved` on its own POST would be claiming a race it may have lost.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::debug;
use xai_grok_shell::session::pending_interaction::PendingKind;

use crate::acp_client::{AcpError, Notification};
use crate::error::ApiError;
use crate::state::Attachments;

/// The four interaction methods, by logical name. Broadcast to every subscriber and cached by the
/// leader for replay-on-attach; every other reverse-request is driver-only and never reaches us.
pub const REQUEST_PERMISSION: &str = "session/request_permission";
pub const ASK_USER_QUESTION: &str = "x.ai/ask_user_question";
pub const EXIT_PLAN_MODE: &str = "x.ai/exit_plan_mode";
pub const MCP_ELICIT: &str = "x.ai/mcp/elicit";

/// The `x.ai/session_notification` update that says an interaction opened.
const PENDING_INTERACTION: &str = "pending_interaction";
/// …and the one that says it closed, however it closed.
const INTERACTION_RESOLVED: &str = "interaction_resolved";

/// Resolved entries kept per session, so a phone that reconnects can still see what it answered.
const MAX_RESOLVED_PER_SESSION: usize = 50;

/// …and for how long. Both bounds apply; whichever bites first wins.
const RESOLVED_TTL_MS: i64 = 60 * 60 * 1000;

/// Ring for the approval-change broadcast. Only SSE connections subscribe, and each one filters to
/// its own session, so this is sized for burst tolerance rather than throughput.
const CHANGE_BUFFER: usize = 256;

/// Where an entry is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    /// Waiting for an answer. The only status a POST is accepted in.
    Pending,
    /// This lane put an answer on the wire. Whether the agent took it is not observable.
    Submitted,
    /// The agent said the interaction closed — by our answer, someone else's, or a cancel.
    Resolved,
}

/// One approval, exactly as the GET routes and the `event: approval` frame render it.
///
/// `method` and `request` are `null` on an entry that exists only because a `pending_interaction`
/// hint arrived first: the lane knows something is waiting and what kind it is, but not yet what it
/// says. They fill in when the reverse-request lands.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    /// The `toolCallId`. Unique within its session, which is why the routes are session-scoped.
    pub id: String,
    pub session_id: String,
    /// `permission` | `question` | `plan_approval` | `mcp_elicitation`, spelled by the leader's own
    /// [`PendingKind`] so the two cannot drift.
    pub kind: PendingKind,
    /// Logical method name, or `null` while only a hint has arrived.
    pub method: Option<String>,
    pub status: ApprovalStatus,
    /// The reverse-request's params, verbatim, or `null` while only a hint has arrived.
    pub request: Value,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
}

/// The resource plus the one thing a client must never see: the JSON-RPC id to answer with.
#[derive(Debug, Clone)]
struct Entry {
    approval: Approval,
    /// `None` on a placeholder — there is no request to answer yet.
    rpc_id: Option<Value>,
}

/// The kind a logical method raises, or `None` if it is not an interaction at all.
pub fn kind_for_method(method: &str) -> Option<PendingKind> {
    match method {
        REQUEST_PERMISSION => Some(PendingKind::Permission),
        ASK_USER_QUESTION => Some(PendingKind::Question),
        EXIT_PLAN_MODE => Some(PendingKind::PlanApproval),
        MCP_ELICIT => Some(PendingKind::McpElicitation),
        _ => None,
    }
}

/// Every open and recently-resolved approval this lane holds, keyed by `(sessionId, toolCallId)`.
///
/// One `std::sync::Mutex` over the whole map, deliberately: every mutation is a hash lookup and a
/// field write, the POST path has to be atomic across "check the status" and "put the answer on the
/// wire" (that send is a non-blocking channel push, so it is legal and cheap under the lock), and a
/// finer-grained scheme would buy nothing but a way to get the race wrong.
pub struct ApprovalStore {
    /// The same handle [`crate::routes`] attaches through. An interaction for a session this lane
    /// never loaded is not ours to hold — see [`Self::intercept`].
    attachments: Arc<Attachments>,
    sessions: Mutex<HashMap<String, HashMap<String, Entry>>>,
    changes: broadcast::Sender<Approval>,
}

impl ApprovalStore {
    pub fn new(attachments: Arc<Attachments>) -> Self {
        Self {
            attachments,
            sessions: Mutex::new(HashMap::new()),
            changes: broadcast::channel(CHANGE_BUFFER).0,
        }
    }

    /// Every create/submit/resolve, for the SSE lane's `event: approval` frames.
    pub fn subscribe(&self) -> broadcast::Receiver<Approval> {
        self.changes.subscribe()
    }

    /// Take custody of a reverse-request, or decline it.
    ///
    /// `Some` means the lane now owns the request and **must not** answer it on the link; `None`
    /// means the caller answers `-32601` immediately, which is the load-bearing default: an
    /// unanswered request the lane merely dropped would park the agent's tool loop forever if the
    /// lane were the only subscriber.
    ///
    /// Declined when the method is not one of the four, when the params carry no session id, when
    /// there is no tool call id to key on, when this lane has not attached to the session, or when
    /// the entry is no longer `pending`. The attachment check is the same guard
    /// [`crate::state::spawn_event_pump`] applies: the leader fans out on subscription, so in
    /// practice everything that arrives is ours, and the check is what keeps a stray broadcast out
    /// of a map no client can address. The status check covers the leader's replay-on-attach: a
    /// re-broadcast of a request this lane has already answered is stale, and taking custody of it
    /// a second time would hand a client an id whose answer can no longer be the first one.
    pub fn intercept(&self, method: &str, rpc_id: &Value, params: &Value) -> Option<Approval> {
        let kind = kind_for_method(method)?;
        let params = crate::envelope::inner_params(params);
        let session_id = string_field(params, "sessionId", "session_id")?;
        if !self.attachments.is_attached(&session_id) {
            debug!(
                method,
                session_id, "gx-remote-api: declining an interaction for an unattached session"
            );
            return None;
        }
        let tool_call_id = tool_call_id_of(params)?;

        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions
            .entry(session_id.clone())
            .or_default()
            .entry(tool_call_id.clone())
            .or_insert_with(|| Entry {
                approval: new_approval(&session_id, &tool_call_id, kind),
                rpc_id: None,
            });

        // A re-broadcast (replay-on-attach sends the cached request again) merges into the entry it
        // already made rather than creating a second one. An entry that is no longer pending is
        // left exactly as it is: re-opening it would resurrect a modal that already has an answer.
        if entry.approval.status != ApprovalStatus::Pending {
            return None;
        }
        entry.approval.kind = kind;
        entry.approval.method = Some(method.to_string());
        entry.approval.request = params.clone();
        entry.rpc_id = Some(rpc_id.clone());
        let approval = entry.approval.clone();
        drop(sessions);

        self.announce(&approval);
        Some(approval)
    }

    /// Apply a `pending_interaction` / `interaction_resolved` notification, if it is one.
    ///
    /// Driven by the single event pump rather than per SSE connection: an interaction must resolve
    /// whether or not anybody is watching the stream.
    pub fn observe(&self, notification: &Notification) {
        let params = crate::envelope::inner_params(&notification.params);
        let Some(update) = params.get("update") else {
            return;
        };
        let Some(session_id) = notification.session_id.clone() else {
            return;
        };
        let Some(tool_call_id) = string_field(update, "toolCallId", "tool_call_id") else {
            return;
        };
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some(PENDING_INTERACTION) => {
                let kind = update
                    .get("kind")
                    .and_then(|k| serde_json::from_value::<PendingKind>(k.clone()).ok());
                self.note_pending(&session_id, &tool_call_id, kind);
            }
            Some(INTERACTION_RESOLVED) => self.note_resolved(&session_id, &tool_call_id),
            _ => {}
        }
    }

    /// The hint that an interaction is open, ahead of (or after) the request itself.
    ///
    /// Creates a placeholder when the request has not arrived; otherwise only refreshes the kind,
    /// so it can never overwrite a captured request or re-open a resolved entry.
    fn note_pending(&self, session_id: &str, tool_call_id: &str, kind: Option<PendingKind>) {
        if !self.attachments.is_attached(session_id) {
            return;
        }
        let mut sessions = self.sessions.lock().unwrap();
        let entries = sessions.entry(session_id.to_string()).or_default();
        let changed = match entries.get_mut(tool_call_id) {
            Some(entry) => {
                if entry.approval.status == ApprovalStatus::Resolved {
                    return;
                }
                match kind {
                    Some(kind)
                        if entry.approval.method.is_none() && entry.approval.kind != kind =>
                    {
                        entry.approval.kind = kind;
                        entry.approval.clone()
                    }
                    // The request itself is authoritative about its own kind, and a repeated hint
                    // that changes nothing is not news worth a frame.
                    _ => return,
                }
            }
            None => {
                let approval = new_approval(
                    session_id,
                    tool_call_id,
                    // A hint with no readable kind still has to raise `needs_input`; permission is
                    // the overwhelmingly common case and the request will correct it in a moment.
                    kind.unwrap_or(PendingKind::Permission),
                );
                entries.insert(
                    tool_call_id.to_string(),
                    Entry {
                        approval: approval.clone(),
                        rpc_id: None,
                    },
                );
                approval
            }
        };
        drop(sessions);
        self.announce(&changed);
    }

    /// The interaction closed — by our answer, another client's, or a cancel.
    fn note_resolved(&self, session_id: &str, tool_call_id: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(entries) = sessions.get_mut(session_id) else {
            return;
        };
        let Some(entry) = entries.get_mut(tool_call_id) else {
            return;
        };
        if entry.approval.status == ApprovalStatus::Resolved {
            return;
        }
        entry.approval.status = ApprovalStatus::Resolved;
        entry.approval.resolved_at = Some(now_ms());
        // The id is useless now and answering it would be a stale write; drop it with the status.
        entry.rpc_id = None;
        let approval = entry.approval.clone();
        prune(entries, now_ms());
        drop(sessions);
        self.announce(&approval);
    }

    /// Answer a held reverse-request: the plan's four outcomes, decided under the lock.
    ///
    /// `send` puts the JSON-RPC response on the link and is called **inside** the lock, so two
    /// concurrent POSTs cannot both send. A failed send leaves the entry `pending`: the answer
    /// never reached the leader, so claiming `submitted` would be a lie a retry could not undo.
    pub fn submit<F>(
        &self,
        session_id: &str,
        tool_call_id: &str,
        response: Value,
        send: F,
    ) -> Result<Approval, ApiError>
    where
        F: FnOnce(&Value, Value) -> Result<(), AcpError>,
    {
        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions
            .get_mut(session_id)
            .and_then(|entries| entries.get_mut(tool_call_id))
            .ok_or_else(|| ApiError::UnknownApproval {
                session_id: session_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
            })?;

        match entry.approval.status {
            ApprovalStatus::Submitted => {
                return Err(ApiError::AlreadySubmitted(format!(
                    "approval {tool_call_id} of session {session_id} already has an answer on the wire"
                )));
            }
            ApprovalStatus::Resolved => {
                return Err(ApiError::AlreadyResolved(format!(
                    "approval {tool_call_id} of session {session_id} was already resolved — another client or a cancel got there first"
                )));
            }
            ApprovalStatus::Pending => {}
        }

        // A placeholder: the hint arrived, the request has not. There is no id to answer, so this
        // is a retry-later, not a client error — see the route docs.
        let Some(rpc_id) = entry.rpc_id.clone() else {
            return Err(ApiError::LeaderUnavailable(format!(
                "approval {tool_call_id} of session {session_id} is open but its request has not reached this lane yet; retry shortly"
            )));
        };

        send(&rpc_id, response)?;

        entry.approval.status = ApprovalStatus::Submitted;
        entry.approval.submitted_at = Some(now_ms());
        // Answered: the id must never be used twice.
        entry.rpc_id = None;
        let approval = entry.approval.clone();
        drop(sessions);

        self.announce(&approval);
        Ok(approval)
    }

    /// One approval, or `unknown_approval`.
    pub fn get(&self, session_id: &str, tool_call_id: &str) -> Result<Approval, ApiError> {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .and_then(|entries| entries.get(tool_call_id))
            .map(|entry| entry.approval.clone())
            .ok_or_else(|| ApiError::UnknownApproval {
                session_id: session_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
            })
    }

    /// Every approval for a session: open ones first, oldest first, then resolved ones, newest
    /// first. A phone renders the list top-down and the thing it has to act on is at the top.
    pub fn list(&self, session_id: &str) -> Vec<Approval> {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(entries) = sessions.get_mut(session_id) else {
            return Vec::new();
        };
        prune(entries, now_ms());
        let mut approvals: Vec<Approval> = entries
            .values()
            .map(|entry| entry.approval.clone())
            .collect();
        approvals.sort_by(|a, b| {
            let rank = |approval: &Approval| u8::from(approval.status == ApprovalStatus::Resolved);
            rank(a)
                .cmp(&rank(b))
                // Open: oldest first. Resolved: newest first.
                .then_with(|| match a.status {
                    ApprovalStatus::Resolved => b.resolved_at.cmp(&a.resolved_at),
                    _ => a.created_at.cmp(&b.created_at),
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        approvals
    }

    /// Interactions still waiting for an answer. A `submitted` one is not counted: its answer is on
    /// the wire, and asking a phone to answer it again is exactly the double-answer this lane is
    /// built to avoid.
    pub fn pending_count(&self, session_id: &str) -> u32 {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .map(|entries| {
                entries
                    .values()
                    .filter(|entry| entry.approval.status == ApprovalStatus::Pending)
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Whether the session is blocked on a client right now. The signal [`crate::policy`] trusts
    /// over the roster, which lags a turn boundary by up to one broadcast.
    pub fn has_pending(&self, session_id: &str) -> bool {
        self.pending_count(session_id) > 0
    }

    /// A send error only means no SSE connection is open, which is the normal case.
    fn announce(&self, approval: &Approval) {
        let _ = self.changes.send(approval.clone());
    }

    /// Backdate a resolved entry so the TTL half of the cap is reachable without sleeping an hour.
    #[cfg(test)]
    fn backdate_resolved(&self, session_id: &str, tool_call_id: &str, resolved_at: i64) {
        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions
            .get_mut(session_id)
            .and_then(|entries| entries.get_mut(tool_call_id))
            .expect("backdating an approval that does not exist");
        entry.approval.resolved_at = Some(resolved_at);
    }
}

/// A fresh `pending` entry with no request behind it yet.
fn new_approval(session_id: &str, tool_call_id: &str, kind: PendingKind) -> Approval {
    Approval {
        id: tool_call_id.to_string(),
        session_id: session_id.to_string(),
        kind,
        method: None,
        status: ApprovalStatus::Pending,
        request: Value::Null,
        created_at: now_ms(),
        submitted_at: None,
        resolved_at: None,
    }
}

/// Hold at most [`MAX_RESOLVED_PER_SESSION`] resolved entries, none older than
/// [`RESOLVED_TTL_MS`]. Open entries are never dropped — the agent is still parked on them, and
/// forgetting one would make an answerable approval unanswerable.
fn prune(entries: &mut HashMap<String, Entry>, now: i64) {
    entries.retain(|_, entry| match entry.approval.resolved_at {
        Some(resolved_at) => now - resolved_at < RESOLVED_TTL_MS,
        None => true,
    });

    let mut resolved: Vec<(String, i64)> = entries
        .iter()
        .filter_map(|(id, entry)| Some((id.clone(), entry.approval.resolved_at?)))
        .collect();
    if resolved.len() <= MAX_RESOLVED_PER_SESSION {
        return;
    }
    // Newest first, then drop everything past the cap.
    resolved.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (id, _) in resolved.drain(MAX_RESOLVED_PER_SESSION..) {
        entries.remove(&id);
    }
}

/// The tool call id an interaction request carries.
///
/// The three ext methods put it at the top level (`toolCallId`); `session/request_permission` nests
/// it inside its `toolCall`, which is an ACP `ToolCallUpdate` (`tool_call.rs:165`). Snake-case and
/// a bare `id` are accepted for the same reason the leader's own
/// `extract_interaction_tool_call_id` accepts them: the payload crosses two serializers.
fn tool_call_id_of(params: &Value) -> Option<String> {
    if let Some(id) = string_field(params, "toolCallId", "tool_call_id") {
        return Some(id);
    }
    let tool_call = params.get("toolCall").or_else(|| params.get("tool_call"))?;
    string_field(tool_call, "toolCallId", "tool_call_id").or_else(|| {
        tool_call
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
    })
}

/// A string field under either spelling.
fn string_field(value: &Value, camel: &str, snake: &str) -> Option<String> {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .and_then(Value::as_str)
        .map(String::from)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store_attached_to(session_id: &str) -> ApprovalStore {
        let attachments = Arc::new(Attachments::default());
        attachments.mark_attached(session_id);
        ApprovalStore::new(attachments)
    }

    fn permission_params(session_id: &str, tool_call_id: &str) -> Value {
        json!({
            "sessionId": session_id,
            "toolCall": { "toolCallId": tool_call_id, "title": "rm -rf /" },
            "options": [{ "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" }],
        })
    }

    fn notification(session_id: &str, update: Value) -> Notification {
        Notification {
            method: "x.ai/session_notification".into(),
            params: json!({ "sessionId": session_id, "update": update }),
            session_id: Some(session_id.to_string()),
        }
    }

    #[test]
    fn every_interaction_method_maps_to_the_leaders_own_kind() {
        // The wire spellings are `PendingKind`'s, which is why this crate reuses that enum.
        for (method, kind, wire) in [
            (REQUEST_PERMISSION, PendingKind::Permission, "permission"),
            (ASK_USER_QUESTION, PendingKind::Question, "question"),
            (EXIT_PLAN_MODE, PendingKind::PlanApproval, "plan_approval"),
            (MCP_ELICIT, PendingKind::McpElicitation, "mcp_elicitation"),
        ] {
            assert_eq!(kind_for_method(method), Some(kind), "{method}");
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(wire));
        }
    }

    #[test]
    fn nothing_else_is_an_interaction() {
        // `_`-prefixed spellings included: routing is on the logical name, never the wire one.
        for method in [
            "session/update",
            "fs/read_text_file",
            "terminal/create",
            "_x.ai/ask_user_question",
            "x.ai/session_notification",
        ] {
            assert_eq!(kind_for_method(method), None, "{method}");
        }
    }

    #[test]
    fn the_permission_tool_call_id_is_read_out_of_its_tool_call() {
        assert_eq!(
            tool_call_id_of(&permission_params("s1", "tc-1")).as_deref(),
            Some("tc-1")
        );
        // The ext methods carry it flat.
        assert_eq!(
            tool_call_id_of(&json!({ "toolCallId": "tc-2" })).as_deref(),
            Some("tc-2")
        );
        // Snake case, and the bare `id` the leader also tolerates.
        assert_eq!(
            tool_call_id_of(&json!({ "tool_call": { "id": "tc-3" } })).as_deref(),
            Some("tc-3")
        );
        assert_eq!(tool_call_id_of(&json!({ "sessionId": "s1" })), None);
    }

    #[test]
    fn an_interaction_for_an_unattached_session_is_declined() {
        let store = store_attached_to("sess-1");
        assert!(
            store
                .intercept(
                    REQUEST_PERMISSION,
                    &json!(7),
                    &permission_params("sess-other", "tc-1"),
                )
                .is_none(),
            "holding a request the lane cannot address would park the agent"
        );
        assert!(store.list("sess-other").is_empty());
    }

    #[test]
    fn a_hint_before_the_request_becomes_one_entry_not_two() {
        let store = store_attached_to("sess-1");
        store.observe(&notification(
            "sess-1",
            json!({
                "sessionUpdate": PENDING_INTERACTION,
                "tool_call_id": "tc-1",
                "kind": "plan_approval",
            }),
        ));

        let placeholder = store.get("sess-1", "tc-1").unwrap();
        assert_eq!(placeholder.kind, PendingKind::PlanApproval);
        assert_eq!(placeholder.method, None);
        assert_eq!(placeholder.request, Value::Null);
        let created_at = placeholder.created_at;

        store
            .intercept(
                EXIT_PLAN_MODE,
                &json!(11),
                &json!({ "sessionId": "sess-1", "toolCallId": "tc-1", "planContent": "# Plan" }),
            )
            .unwrap();

        let merged = store.get("sess-1", "tc-1").unwrap();
        assert_eq!(store.list("sess-1").len(), 1, "the hint must not duplicate");
        assert_eq!(merged.method.as_deref(), Some(EXIT_PLAN_MODE));
        assert_eq!(merged.request["planContent"], "# Plan");
        assert_eq!(merged.created_at, created_at, "the entry is the same one");
    }

    #[test]
    fn a_hint_after_the_request_does_not_blank_it() {
        let store = store_attached_to("sess-1");
        store
            .intercept(
                REQUEST_PERMISSION,
                &json!(3),
                &permission_params("sess-1", "tc-1"),
            )
            .unwrap();
        store.observe(&notification(
            "sess-1",
            json!({ "sessionUpdate": PENDING_INTERACTION, "tool_call_id": "tc-1", "kind": "question" }),
        ));

        let approval = store.get("sess-1", "tc-1").unwrap();
        assert_eq!(approval.method.as_deref(), Some(REQUEST_PERMISSION));
        assert_eq!(
            approval.kind,
            PendingKind::Permission,
            "the request is authoritative about its own kind"
        );
    }

    #[test]
    fn a_replayed_request_merges_and_a_resolved_one_is_not_reopened() {
        let store = store_attached_to("sess-1");
        let params = permission_params("sess-1", "tc-1");
        store
            .intercept(REQUEST_PERMISSION, &json!(3), &params)
            .unwrap();
        // Replay-on-attach: the leader sends the cached request again.
        store
            .intercept(REQUEST_PERMISSION, &json!(4), &params)
            .unwrap();
        assert_eq!(store.list("sess-1").len(), 1);

        store.observe(&notification(
            "sess-1",
            json!({ "sessionUpdate": INTERACTION_RESOLVED, "tool_call_id": "tc-1" }),
        ));
        assert!(
            store
                .intercept(REQUEST_PERMISSION, &json!(5), &params)
                .is_none(),
            "a resolved modal must not come back to life"
        );
        assert_eq!(
            store.get("sess-1", "tc-1").unwrap().status,
            ApprovalStatus::Resolved
        );
    }

    #[test]
    fn submitting_sends_the_answer_under_the_id_the_request_arrived_with() {
        let store = store_attached_to("sess-1");
        store
            .intercept(
                REQUEST_PERMISSION,
                &json!(42),
                &permission_params("sess-1", "tc-1"),
            )
            .unwrap();

        let sent = std::cell::RefCell::new(None);
        let approval = store
            .submit(
                "sess-1",
                "tc-1",
                json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
                |id, result| {
                    *sent.borrow_mut() = Some((id.clone(), result));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(approval.status, ApprovalStatus::Submitted);
        assert!(approval.submitted_at.is_some());

        let (id, result) = sent.into_inner().expect("the answer must reach the link");
        assert_eq!(id, json!(42));
        assert_eq!(result["outcome"]["optionId"], "allow-once");
    }

    #[test]
    fn a_failed_send_leaves_the_entry_answerable() {
        let store = store_attached_to("sess-1");
        store
            .intercept(
                REQUEST_PERMISSION,
                &json!(1),
                &permission_params("sess-1", "tc-1"),
            )
            .unwrap();

        let err = store
            .submit("sess-1", "tc-1", json!({}), |_, _| Err(AcpError::Closed))
            .unwrap_err();
        assert_eq!(err.code(), "leader_unavailable");
        assert_eq!(
            store.get("sess-1", "tc-1").unwrap().status,
            ApprovalStatus::Pending,
            "a dropped answer must not read as submitted"
        );
    }

    #[test]
    fn the_three_refusals_are_the_plans_three_refusals() {
        let store = store_attached_to("sess-1");
        let unknown = store
            .submit("sess-1", "nope", json!({}), |_, _| Ok(()))
            .unwrap_err();
        assert_eq!(unknown.code(), "unknown_approval");

        store
            .intercept(
                REQUEST_PERMISSION,
                &json!(1),
                &permission_params("sess-1", "tc-1"),
            )
            .unwrap();
        store
            .submit("sess-1", "tc-1", json!({}), |_, _| Ok(()))
            .unwrap();
        let twice = store
            .submit("sess-1", "tc-1", json!({}), |_, _| Ok(()))
            .unwrap_err();
        assert_eq!(twice.code(), "already_submitted");

        store.observe(&notification(
            "sess-1",
            json!({ "sessionUpdate": INTERACTION_RESOLVED, "toolCallId": "tc-1" }),
        ));
        let resolved = store
            .submit("sess-1", "tc-1", json!({}), |_, _| Ok(()))
            .unwrap_err();
        assert_eq!(resolved.code(), "already_resolved");
    }

    #[test]
    fn a_placeholder_cannot_be_answered_because_there_is_no_id_to_answer() {
        let store = store_attached_to("sess-1");
        store.observe(&notification(
            "sess-1",
            json!({ "sessionUpdate": PENDING_INTERACTION, "tool_call_id": "tc-1", "kind": "permission" }),
        ));
        let err = store
            .submit("sess-1", "tc-1", json!({}), |_, _| {
                panic!("nothing to send to")
            })
            .unwrap_err();
        assert_eq!(err.code(), "leader_unavailable");
        assert_eq!(
            err.status().as_u16(),
            503,
            "retry later, not a client error"
        );
    }

    #[test]
    fn only_pending_entries_count_towards_needs_input() {
        let store = store_attached_to("sess-1");
        for (n, id) in ["tc-1", "tc-2"].iter().enumerate() {
            store
                .intercept(
                    REQUEST_PERMISSION,
                    &json!(n as i64),
                    &permission_params("sess-1", id),
                )
                .unwrap();
        }
        assert_eq!(store.pending_count("sess-1"), 2);
        assert!(store.has_pending("sess-1"));

        store
            .submit("sess-1", "tc-1", json!({}), |_, _| Ok(()))
            .unwrap();
        assert_eq!(store.pending_count("sess-1"), 1, "submitted is answered");

        store.observe(&notification(
            "sess-1",
            json!({ "sessionUpdate": INTERACTION_RESOLVED, "tool_call_id": "tc-2" }),
        ));
        assert_eq!(store.pending_count("sess-1"), 0);
        assert!(!store.has_pending("sess-1"));
        assert!(!store.has_pending("sess-other"));
    }

    #[test]
    fn the_listing_puts_what_needs_answering_first() {
        let store = store_attached_to("sess-1");
        for id in ["tc-open", "tc-sent", "tc-done"] {
            store
                .intercept(
                    REQUEST_PERMISSION,
                    &json!(1),
                    &permission_params("sess-1", id),
                )
                .unwrap();
        }
        store
            .submit("sess-1", "tc-sent", json!({}), |_, _| Ok(()))
            .unwrap();
        store.observe(&notification(
            "sess-1",
            json!({ "sessionUpdate": INTERACTION_RESOLVED, "tool_call_id": "tc-done" }),
        ));

        let listed: Vec<String> = store.list("sess-1").into_iter().map(|a| a.id).collect();
        assert_eq!(listed, vec!["tc-open", "tc-sent", "tc-done"]);
        assert!(store.list("sess-unknown").is_empty());
    }

    #[test]
    fn resolved_history_is_capped_by_count_and_by_age() {
        let store = store_attached_to("sess-1");
        for n in 0..(MAX_RESOLVED_PER_SESSION + 10) {
            let id = format!("tc-{n:03}");
            store
                .intercept(
                    REQUEST_PERMISSION,
                    &json!(n as i64),
                    &permission_params("sess-1", &id),
                )
                .unwrap();
            store.observe(&notification(
                "sess-1",
                json!({ "sessionUpdate": INTERACTION_RESOLVED, "tool_call_id": id }),
            ));
        }
        let kept = store.list("sess-1");
        assert_eq!(kept.len(), MAX_RESOLVED_PER_SESSION);

        // An open entry is never evicted, however much resolved history piles up on top of it.
        store
            .intercept(
                REQUEST_PERMISSION,
                &json!(999),
                &permission_params("sess-1", "tc-open"),
            )
            .unwrap();
        for n in 100..140 {
            let id = format!("tc-{n:03}");
            store
                .intercept(
                    REQUEST_PERMISSION,
                    &json!(n as i64),
                    &permission_params("sess-1", &id),
                )
                .unwrap();
            store.observe(&notification(
                "sess-1",
                json!({ "sessionUpdate": INTERACTION_RESOLVED, "tool_call_id": id }),
            ));
        }
        let kept = store.list("sess-1");
        assert_eq!(kept.len(), MAX_RESOLVED_PER_SESSION + 1);
        assert_eq!(kept[0].id, "tc-open");
        assert_eq!(kept[0].status, ApprovalStatus::Pending);

        // Age out everything but the open one.
        let stale = now_ms() - RESOLVED_TTL_MS - 1;
        for approval in &kept {
            if approval.status == ApprovalStatus::Resolved {
                store.backdate_resolved("sess-1", &approval.id, stale);
            }
        }
        let kept = store.list("sess-1");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "tc-open");
    }

    #[test]
    fn every_change_is_announced_once() {
        let store = store_attached_to("sess-1");
        let mut changes = store.subscribe();

        // A repeated hint that changes nothing is not a change.
        for _ in 0..2 {
            store.observe(&notification(
                "sess-1",
                json!({
                    "sessionUpdate": PENDING_INTERACTION,
                    "tool_call_id": "tc-1",
                    "kind": "permission",
                }),
            ));
        }
        store
            .intercept(
                REQUEST_PERMISSION,
                &json!(1),
                &permission_params("sess-1", "tc-1"),
            )
            .unwrap();
        store
            .submit("sess-1", "tc-1", json!({}), |_, _| Ok(()))
            .unwrap();
        store.observe(&notification(
            "sess-1",
            json!({ "sessionUpdate": INTERACTION_RESOLVED, "tool_call_id": "tc-1" }),
        ));
        // A second resolve is not a change.
        store.observe(&notification(
            "sess-1",
            json!({ "sessionUpdate": INTERACTION_RESOLVED, "tool_call_id": "tc-1" }),
        ));

        let statuses: Vec<ApprovalStatus> = std::iter::from_fn(|| changes.try_recv().ok())
            .map(|approval| approval.status)
            .collect();
        assert_eq!(
            statuses,
            vec![
                // The hint opened it, the request filled it in, the POST answered it, the agent
                // closed it — four changes, and the duplicate hint is not one of them.
                ApprovalStatus::Pending,
                ApprovalStatus::Pending,
                ApprovalStatus::Submitted,
                ApprovalStatus::Resolved
            ]
        );
    }

    #[test]
    fn an_unrelated_session_notification_changes_nothing() {
        let store = store_attached_to("sess-1");
        for update in [
            json!({ "sessionUpdate": "agent_message_chunk" }),
            json!({ "sessionUpdate": PENDING_INTERACTION }),
            json!({ "tool_call_id": "tc-1" }),
        ] {
            store.observe(&notification("sess-1", update));
        }
        assert!(store.list("sess-1").is_empty());
    }

    #[test]
    fn the_resource_serializes_as_the_documented_shape() {
        let store = store_attached_to("sess-1");
        let approval = store
            .intercept(
                MCP_ELICIT,
                &json!(9),
                &json!({
                    "sessionId": "sess-1",
                    "toolCallId": "mcp-elicit-abc",
                    "serverName": "files",
                    "message": "Which file?",
                    "mode": "form",
                }),
            )
            .unwrap();
        let json = serde_json::to_value(&approval).unwrap();
        assert_eq!(json["id"], "mcp-elicit-abc");
        assert_eq!(json["sessionId"], "sess-1");
        assert_eq!(json["kind"], "mcp_elicitation");
        assert_eq!(json["method"], MCP_ELICIT);
        assert_eq!(json["status"], "pending");
        assert_eq!(json["request"]["serverName"], "files");
        assert!(json["createdAt"].is_i64());
        // Absent, not null, until they happen.
        assert!(json.get("submittedAt").is_none());
        assert!(json.get("resolvedAt").is_none());
    }
}
