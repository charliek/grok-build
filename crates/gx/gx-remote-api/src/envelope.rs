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
}

/// `params._meta.eventId`, when present and a string.
pub fn event_id_of(params: &Value) -> Option<String> {
    params
        .get("_meta")?
        .get("eventId")?
        .as_str()
        .map(String::from)
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

    #[test]
    fn serialization_is_camel_case_with_an_explicit_null_cursor() {
        let env = NormalizedEnvelope::from_stored(&json!({ "method": "m", "params": {} }));
        let out = serde_json::to_value(&env).unwrap();
        assert_eq!(out["eventId"], Value::Null);
        assert!(out.get("eventId").is_some(), "eventId must not be omitted");
        assert_eq!(out["timestamp"], Value::Null);
    }
}
