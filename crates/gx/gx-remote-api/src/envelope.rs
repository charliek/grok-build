//! The one shape a client ever sees for a session event.
//!
//! History (`x.ai/session/updates`) hands back the raw JSONL storage envelope
//! `{ timestamp, method, params }`; C5's SSE lane will hand back live JSON-RPC notifications
//! `{ jsonrpc, method, params }`. Both are normalized to the same four fields so a phone can replay
//! history and then continue on the live stream without a second parser:
//!
//! ```json
//! { "eventId": "sess-abc-41" | null, "method": "session/update", "params": {…}, "timestamp": 1.7e12 }
//! ```
//!
//! `eventId` comes from `params._meta.eventId` and is the durable cursor C5 resumes on. It is
//! **absent** on older lines and on non-persisted events (`pending_interaction`,
//! `interaction_resolved`), hence `Option` rather than a synthesized id: a made-up cursor would
//! resume at the wrong place.

use serde::Serialize;
use serde_json::Value;

use crate::acp_client::{Notification, wire_method};

/// A session event as this API renders it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedEnvelope {
    /// `params._meta.eventId`, or `null` when the source carried none.
    pub event_id: Option<String>,
    /// Logical method name (`session/update`, `x.ai/session_notification`, …).
    pub method: String,
    /// The notification payload, verbatim.
    pub params: Value,
    /// Source timestamp, or `null`. Left as a raw JSON number so whatever unit the store uses
    /// survives the trip unchanged.
    pub timestamp: Option<Value>,
}

impl NormalizedEnvelope {
    /// Normalize one stored `updates.jsonl` envelope, as returned inside
    /// `x.ai/session/updates`' `updates` array.
    ///
    /// Tolerant by design: a line with no `method`, no `params` or no `timestamp` still produces an
    /// envelope, because dropping it would silently punch a hole in a client's transcript.
    pub fn from_stored(stored: &Value) -> Self {
        let method = stored
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = stored
            .get("params")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let event_id = event_id_of(&params);
        let timestamp = stored.get("timestamp").filter(|t| t.is_number()).cloned();
        Self {
            event_id,
            method,
            params,
            timestamp,
        }
    }

    /// Normalize one **live** notification off the leader link.
    ///
    /// Three details make this match [`Self::from_stored`] rather than merely resemble it, which is
    /// the whole point — a client replays history and then continues on the live stream with one
    /// parser:
    ///
    /// - **Method spelling.** [`Notification`] carries the *logical* name (`x.ai/session/update`),
    ///   but `updates.jsonl` stores the wire name (`_x.ai/session/update`). [`wire_method`] is the
    ///   exact inverse of the strip that produced it, so re-applying it reproduces the stored
    ///   spelling for extension updates and leaves standard ACP methods alone.
    /// - **Params nesting.** The gateway forwards some ext notifications *wrapped*
    ///   (`params: { method, params }`, `leader/server.rs::method_of`) and others flat. Disk always
    ///   holds the inner form, so [`inner_params`] unwraps the wrapper when there is one.
    /// - **Timestamp.** Stored lines carry a numeric `timestamp`; a live notification carries
    ///   `_meta.agentTimestampMs`, stamped at the same instant by `ensure_event_id_meta`.
    pub fn from_notification(notification: &Notification) -> Self {
        let params = inner_params(&notification.params).clone();
        Self {
            event_id: event_id_of(&params),
            method: wire_method(&notification.method).into_owned(),
            timestamp: params
                .get("_meta")
                .and_then(|meta| meta.get("agentTimestampMs"))
                .filter(|t| t.is_number())
                .cloned(),
            params,
        }
    }

    /// The ordering counter of this frame's `eventId`, when it has one.
    pub fn counter(&self) -> Option<u64> {
        crate::ring::counter_of(self.event_id.as_deref()?)
    }
}

/// `params._meta.eventId`, when present and a string.
pub fn event_id_of(params: &Value) -> Option<String> {
    params
        .get("_meta")?
        .get("eventId")?
        .as_str()
        .map(String::from)
}

/// Unwrap the gateway's *wrapped* ext-notification form, when the value is one.
///
/// `leader/server.rs` documents the two shapes an ext notification arrives in: direct
/// (`{"method":"_x.ai/foo","params":{…}}`) and wrapped (`{"method":"_x.ai/foo","params":{"method":
/// "x.ai/foo","params":{…}}}`). Everything session-scoped in this crate — the `sessionId` the ring
/// keys on, the `_meta.eventId` it orders on — lives in the inner object, so a lane that only
/// looked at the outer one would file every wrapped notification under "no session".
///
/// The discriminator is the shape, exactly as it is in the leader: a `method` string sitting beside
/// a `params` object. A payload that merely has a `params` key is not a wrapper.
pub fn inner_params(params: &Value) -> &Value {
    let is_wrapper = params.get("method").is_some_and(Value::is_string)
        && params.get("params").is_some_and(Value::is_object);
    if is_wrapper {
        &params["params"]
    } else {
        params
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_stored_line_normalizes_to_all_four_fields() {
        let stored = json!({
            "timestamp": 1_700_000_000_123_i64,
            "method": "session/update",
            "params": {
                "sessionId": "sess-abc",
                "update": { "sessionUpdate": "agent_message_chunk" },
                "_meta": { "eventId": "sess-abc-41" }
            }
        });
        let env = NormalizedEnvelope::from_stored(&stored);
        assert_eq!(env.event_id.as_deref(), Some("sess-abc-41"));
        assert_eq!(env.method, "session/update");
        assert_eq!(env.params["sessionId"], "sess-abc");
        assert_eq!(env.timestamp, Some(json!(1_700_000_000_123_i64)));
    }

    #[test]
    fn a_line_without_an_event_id_normalizes_with_a_null_cursor() {
        // Older lines, and exceptional ones, carry no `_meta.eventId`. They must still be
        // delivered — with `eventId: null`, never a fabricated one.
        let stored = json!({
            "timestamp": 1_700_000_000_123_i64,
            "method": "session/update",
            "params": { "sessionId": "sess-abc", "update": {} }
        });
        let env = NormalizedEnvelope::from_stored(&stored);
        assert_eq!(env.event_id, None);
        assert_eq!(env.method, "session/update");

        // …and neither does a `_meta` without the key, nor a non-string id.
        assert_eq!(
            NormalizedEnvelope::from_stored(&json!({
                "method": "m", "params": { "_meta": { "other": 1 } }
            }))
            .event_id,
            None
        );
        assert_eq!(
            NormalizedEnvelope::from_stored(&json!({
                "method": "m", "params": { "_meta": { "eventId": 7 } }
            }))
            .event_id,
            None
        );
    }

    #[test]
    fn extension_updates_keep_their_wire_method_name() {
        // Stored extension lines are written with the underscore already on them; the transcript
        // reproduces what the store holds rather than re-deriving it.
        let env = NormalizedEnvelope::from_stored(&json!({
            "timestamp": 1,
            "method": "_x.ai/session/update",
            "params": {}
        }));
        assert_eq!(env.method, "_x.ai/session/update");
    }

    #[test]
    fn a_missing_timestamp_or_params_still_produces_an_envelope() {
        let env = NormalizedEnvelope::from_stored(&json!({ "method": "session/update" }));
        assert_eq!(env.timestamp, None);
        assert_eq!(env.params, json!({}));

        // A non-numeric timestamp is dropped rather than passed through as a string.
        let env = NormalizedEnvelope::from_stored(&json!({
            "method": "m", "timestamp": "2026-01-01T00:00:00Z", "params": {}
        }));
        assert_eq!(env.timestamp, None);
    }

    fn notification(method: &str, params: Value) -> Notification {
        Notification {
            method: crate::acp_client::logical_method(method).to_string(),
            session_id: crate::envelope::inner_params(&params)
                .get("sessionId")
                .and_then(Value::as_str)
                .map(String::from),
            params,
        }
    }

    #[test]
    fn a_live_notification_normalizes_to_the_same_shape_as_the_stored_line() {
        let live = notification(
            "session/update",
            json!({
                "sessionId": "sess-abc",
                "update": { "sessionUpdate": "agent_message_chunk" },
                "_meta": { "eventId": "sess-abc-41", "agentTimestampMs": 1_700_000_000_123_i64 }
            }),
        );
        let env = NormalizedEnvelope::from_notification(&live);
        assert_eq!(env.event_id.as_deref(), Some("sess-abc-41"));
        assert_eq!(env.method, "session/update");
        assert_eq!(env.timestamp, Some(json!(1_700_000_000_123_i64)));
        assert_eq!(env.counter(), Some(41));
    }

    #[test]
    fn a_live_extension_update_keeps_the_underscore_the_store_holds() {
        // `updates.jsonl` writes `_x.ai/session/update`; `Notification` carries the logical name.
        // A client that resumes from history onto the live stream must not see the method change.
        let live = notification(
            "_x.ai/session/update",
            json!({ "sessionId": "s", "update": {} }),
        );
        assert_eq!(live.method, "x.ai/session/update", "precondition");
        assert_eq!(
            NormalizedEnvelope::from_notification(&live).method,
            "_x.ai/session/update"
        );
    }

    #[test]
    fn a_wrapped_ext_notification_normalizes_to_its_inner_params() {
        // The gateway's wrapped form. Reading the outer object would find no sessionId, no eventId
        // and no update — the frame would be unroutable and unresumable.
        let live = notification(
            "_x.ai/session_notification",
            json!({
                "method": "x.ai/session_notification",
                "params": {
                    "sessionId": "sess-abc",
                    "update": { "sessionUpdate": "rewind_marker" },
                    "_meta": { "eventId": "sess-abc-9" }
                }
            }),
        );
        assert_eq!(live.session_id.as_deref(), Some("sess-abc"));

        let env = NormalizedEnvelope::from_notification(&live);
        assert_eq!(env.event_id.as_deref(), Some("sess-abc-9"));
        assert_eq!(env.params["sessionId"], "sess-abc");
        assert_eq!(env.params["update"]["sessionUpdate"], "rewind_marker");
    }

    #[test]
    fn inner_params_only_unwraps_an_actual_wrapper() {
        let flat = json!({ "sessionId": "s", "params": { "not": "a wrapper" } });
        assert_eq!(inner_params(&flat), &flat, "no `method`, so not a wrapper");

        let method_without_params = json!({ "method": "x.ai/foo", "sessionId": "s" });
        assert_eq!(inner_params(&method_without_params), &method_without_params);

        let wrapper = json!({ "method": "x.ai/foo", "params": { "sessionId": "s" } });
        assert_eq!(inner_params(&wrapper), &json!({ "sessionId": "s" }));
    }

    #[test]
    fn a_live_notification_without_a_cursor_still_normalizes() {
        let live = notification(
            "x.ai/session_notification",
            json!({ "sessionId": "s", "update": { "sessionUpdate": "pending_interaction" } }),
        );
        let env = NormalizedEnvelope::from_notification(&live);
        assert_eq!(env.event_id, None);
        assert_eq!(env.counter(), None);
        assert_eq!(env.timestamp, None);
    }

    #[test]
    fn serialization_is_camel_case_with_an_explicit_null_cursor() {
        let env = NormalizedEnvelope::from_stored(&json!({ "method": "m", "params": {} }));
        let out = serde_json::to_value(&env).unwrap();
        assert_eq!(out["eventId"], Value::Null);
        assert!(out.get("eventId").is_some(), "eventId must not be omitted");
        assert_eq!(out["timestamp"], Value::Null);
    }
}
