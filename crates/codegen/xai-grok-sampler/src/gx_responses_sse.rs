//! gx: per-backend Responses SSE frame policy for ChatGPT/Codex vs strict xAI.
//!
//! Inner layer only: `None` = skip this frame, `Some(Ok/Err)` = yield.
//! Callers in the scan loop MUST wrap with outer `Some(...)`.
//! Skipped frames do not reset L2 idle (300s default); that is accepted.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use xai_grok_sampling_types::{SamplingError, rs};

use crate::client::deserialize_response_event;

const LIVENESS_KINDS: [&str; 3] = ["keepalive", "ping", "heartbeat"];
const UNKNOWN_KIND_WARN_CAP: usize = 32;

/// gx: per-backend Responses SSE frame policy (see module docs for skip/yield).
pub(crate) trait ResponsesSseFrameHandler: Send + Sync {
    fn handle_frame(
        &self,
        sse_event_name: &str,
        data: &str,
    ) -> Option<Result<rs::ResponseStreamEvent, SamplingError>>;
}

struct StrictResponsesSseHandler;
struct CodexResponsesSseHandler;

/// Select the Codex or strict Responses SSE policy from `codex_compat`.
pub(crate) fn handler(codex_compat: bool) -> &'static dyn ResponsesSseFrameHandler {
    if codex_compat {
        &CodexResponsesSseHandler
    } else {
        &StrictResponsesSseHandler
    }
}

impl ResponsesSseFrameHandler for StrictResponsesSseHandler {
    fn handle_frame(
        &self,
        _sse_event_name: &str,
        data: &str,
    ) -> Option<Result<rs::ResponseStreamEvent, SamplingError>> {
        Some(deserialize_response_event(data))
    }
}

impl ResponsesSseFrameHandler for CodexResponsesSseHandler {
    fn handle_frame(
        &self,
        sse_event_name: &str,
        data: &str,
    ) -> Option<Result<rs::ResponseStreamEvent, SamplingError>> {
        // `[DONE]` is handled by the scan loop before this arm.
        if is_liveness_kind(sse_event_name) || json_type_is_liveness(data) {
            return None;
        }

        let classify = serde_json::from_str::<rs::ResponseStreamEvent>(data);
        if classify.is_err() {
            if let Some(kind) = peek_top_level_type(data) {
                // Probe the tag in isolation so a nested unknown variant whose
                // name equals the top-level `"type"` cannot skip a known event
                // (e.g. output item `"type": "response.completed"`).
                if tag_is_unknown_stream_variant(&kind) {
                    warn_unknown_top_level_type(&kind);
                    return None;
                }
            }
        }

        Some(deserialize_response_event(data))
    }
}

fn is_liveness_kind(s: &str) -> bool {
    LIVENESS_KINDS.contains(&s)
}

fn json_type_is_liveness(data: &str) -> bool {
    // Substring is a cheap precheck only; the parsed top-level `"type"` is authoritative.
    LIVENESS_KINDS.iter().any(|k| data.contains(k))
        && peek_top_level_type(data).is_some_and(|t| is_liveness_kind(&t))
}

fn peek_top_level_type(data: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|v| v.get("type")?.as_str().map(str::to_owned))
}

/// True when `kind` is not a `ResponseStreamEvent` tag, even as a one-field object.
fn tag_is_unknown_stream_variant(kind: &str) -> bool {
    let probe = serde_json::json!({ "type": kind }).to_string();
    match serde_json::from_str::<rs::ResponseStreamEvent>(&probe) {
        Err(err) => err.to_string().contains(&format!("unknown variant `{kind}`")),
        Ok(_) => false,
    }
}

fn warn_unknown_top_level_type(kind: &str) {
    static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
    let mut seen = SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if seen.len() >= UNKNOWN_KIND_WARN_CAP && !seen.contains(kind) {
        return;
    }
    if seen.insert(kind.to_string()) {
        tracing::warn!(
            event_type = %kind,
            "skipping unknown Codex Responses SSE event type"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEPALIVE: &str = r#"{"type":"keepalive"}"#;
    const TEXT_DELTA: &str = r#"{
        "type": "response.output_text.delta",
        "sequence_number": 0,
        "item_id": "item-1",
        "output_index": 0,
        "content_index": 0,
        "delta": "hello",
        "logprobs": []
    }"#;
    const TEXT_DELTA_KEEPALIVE_CONTENT: &str = r#"{
        "type": "response.output_text.delta",
        "sequence_number": 0,
        "item_id": "item-1",
        "output_index": 0,
        "content_index": 0,
        "delta": "please keepalive the connection",
        "logprobs": []
    }"#;
    const CREATED: &str = r#"{
        "type": "response.created",
        "sequence_number": 0,
        "response": {
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "model": "grok-build",
            "status": "in_progress",
            "output": []
        }
    }"#;
    const COMPLETED: &str = r#"{
        "type": "response.completed",
        "sequence_number": 0,
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 0,
            "model": "grok-build",
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "input_tokens_details": { "cached_tokens": 0 },
                "output_tokens": 1,
                "output_tokens_details": { "reasoning_tokens": 0 },
                "total_tokens": 2
            }
        }
    }"#;
    const FAILED: &str = r#"{
        "type": "response.failed",
        "sequence_number": 1,
        "response": {
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "model": "grok-build",
            "status": "failed",
            "output": [],
            "error": { "code": "server_error", "message": "boom" }
        }
    }"#;
    const TYPED_ERROR: &str =
        r#"{"type":"error","sequence_number":0,"code":"server_error","message":"boom"}"#;
    const UNKNOWN_TOP_LEVEL: &str = r#"{"type":"response.not_a_real_event"}"#;
    const INCOMPLETE_DELTA: &str = r#"{
        "type": "response.output_text.delta",
        "sequence_number": 0,
        "output_index": 0,
        "content_index": 0,
        "delta": "hello",
        "logprobs": []
    }"#;
    const NESTED_UNKNOWN: &str = r#"{
        "type": "response.completed",
        "sequence_number": 0,
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 0,
            "model": "grok-build",
            "status": "completed",
            "output": [{ "type": "not_a_real_item" }],
            "usage": {
                "input_tokens": 1,
                "input_tokens_details": { "cached_tokens": 0 },
                "output_tokens": 1,
                "output_tokens_details": { "reasoning_tokens": 0 },
                "total_tokens": 2
            }
        }
    }"#;
    const NESTED_UNKNOWN_COLLIDES_WITH_TOP_LEVEL: &str = r#"{
        "type": "response.completed",
        "sequence_number": 0,
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 0,
            "model": "grok-build",
            "status": "completed",
            "output": [{ "type": "response.completed" }],
            "usage": {
                "input_tokens": 1,
                "input_tokens_details": { "cached_tokens": 0 },
                "output_tokens": 1,
                "output_tokens_details": { "reasoning_tokens": 0 },
                "total_tokens": 2
            }
        }
    }"#;
    const MALFORMED: &str = r#"{"type":"response.created""#;
    const MALFORMED_KEEPALIVE_SUBSTRING: &str = r#"{"type":"response.created", keepalive"#;

    fn is_serialization_err(got: Option<Result<rs::ResponseStreamEvent, SamplingError>>) -> bool {
        matches!(got, Some(Err(SamplingError::Serialization(_))))
    }

    #[test]
    fn codex_skips_keepalive_json() {
        let h = handler(true);
        assert!(h.handle_frame("", KEEPALIVE).is_none());
        assert!(h.handle_frame("message", KEEPALIVE).is_none());
    }

    #[test]
    fn codex_skips_keepalive_sse_name_with_empty_or_non_json_data() {
        let h = handler(true);
        assert!(h.handle_frame("keepalive", "").is_none());
        assert!(h.handle_frame("keepalive", "not-json").is_none());
    }

    #[test]
    fn codex_skips_ping_and_heartbeat_by_json_type_and_sse_name() {
        let h = handler(true);
        for kind in ["ping", "heartbeat"] {
            let json = format!(r#"{{"type":"{kind}"}}"#);
            assert!(
                h.handle_frame("", &json).is_none(),
                "json type {kind} with empty SSE name"
            );
            assert!(
                h.handle_frame("message", &json).is_none(),
                "json type {kind} with SSE name message"
            );
            assert!(
                h.handle_frame(kind, "").is_none(),
                "SSE name {kind} with empty data"
            );
            assert!(
                h.handle_frame(kind, "not-json").is_none(),
                "SSE name {kind} with non-JSON data"
            );
        }
    }

    #[test]
    fn codex_yields_output_text_delta() {
        let got = handler(true).handle_frame("", TEXT_DELTA);
        assert!(matches!(
            got,
            Some(Ok(rs::ResponseStreamEvent::ResponseOutputTextDelta(_)))
        ));
    }

    #[test]
    fn codex_does_not_skip_delta_whose_text_contains_keepalive() {
        let got = handler(true).handle_frame("", TEXT_DELTA_KEEPALIVE_CONTENT);
        assert!(matches!(
            got,
            Some(Ok(rs::ResponseStreamEvent::ResponseOutputTextDelta(_)))
        ));
    }

    #[test]
    fn codex_skips_unknown_top_level_type() {
        assert!(handler(true).handle_frame("", UNKNOWN_TOP_LEVEL).is_none());
    }

    #[test]
    fn strict_keepalive_is_serialization_error() {
        assert!(is_serialization_err(
            handler(false).handle_frame("", KEEPALIVE)
        ));
    }

    #[test]
    fn both_handlers_yield_err_on_malformed_json() {
        assert!(is_serialization_err(
            handler(true).handle_frame("", MALFORMED)
        ));
        assert!(is_serialization_err(
            handler(false).handle_frame("", MALFORMED)
        ));
    }

    #[test]
    fn codex_known_type_missing_required_field_is_err() {
        assert!(is_serialization_err(
            handler(true).handle_frame("", INCOMPLETE_DELTA)
        ));
    }

    #[test]
    fn codex_nested_unknown_variant_on_known_type_is_err_not_skip() {
        let got = handler(true).handle_frame("", NESTED_UNKNOWN);
        assert!(
            is_serialization_err(got),
            "nested unknown discriminant must not skip a known top-level type"
        );
        let collided = handler(true).handle_frame("", NESTED_UNKNOWN_COLLIDES_WITH_TOP_LEVEL);
        assert!(
            is_serialization_err(collided),
            "nested unknown variant named like the top-level type must not skip"
        );
    }

    #[test]
    fn codex_sequence_created_keepalive_delta_completed() {
        let h = handler(true);
        let created = h.handle_frame("", CREATED);
        let keep = h.handle_frame("", KEEPALIVE);
        let delta = h.handle_frame("", TEXT_DELTA);
        let completed = h.handle_frame("", COMPLETED);
        assert!(matches!(
            created,
            Some(Ok(rs::ResponseStreamEvent::ResponseCreated(_)))
        ));
        assert!(keep.is_none());
        assert!(matches!(
            delta,
            Some(Ok(rs::ResponseStreamEvent::ResponseOutputTextDelta(_)))
        ));
        assert!(matches!(
            completed,
            Some(Ok(rs::ResponseStreamEvent::ResponseCompleted(_)))
        ));
    }

    #[test]
    fn codex_typed_error_and_failed_are_not_skipped() {
        let h = handler(true);
        let failed = h.handle_frame("", FAILED);
        assert!(
            failed.is_some(),
            "typed response.failed must be yielded, not skipped"
        );
        assert!(matches!(
            failed,
            Some(Ok(rs::ResponseStreamEvent::ResponseFailed(_)))
        ));

        let err = h.handle_frame("", TYPED_ERROR);
        assert!(
            err.is_some(),
            "typed error event must be yielded, not skipped"
        );
        assert!(
            matches!(err, Some(Ok(rs::ResponseStreamEvent::ResponseError(_))))
                || is_serialization_err(err),
            "typed error must not be skipped"
        );
    }

    #[test]
    fn handler_false_is_strict_handler_true_is_codex() {
        assert!(handler(true).handle_frame("", KEEPALIVE).is_none());
        assert!(is_serialization_err(
            handler(false).handle_frame("", KEEPALIVE)
        ));
    }

    #[test]
    fn malformed_json_containing_keepalive_substring_is_err() {
        assert!(is_serialization_err(
            handler(true).handle_frame("message", MALFORMED_KEEPALIVE_SUBSTRING)
        ));
        assert!(is_serialization_err(
            handler(true).handle_frame("", MALFORMED_KEEPALIVE_SUBSTRING)
        ));
    }
}
