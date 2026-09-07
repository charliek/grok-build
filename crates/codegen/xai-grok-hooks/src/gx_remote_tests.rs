//! gx: tests for the `gxRemote` hook stamp ([`crate::gx_remote`]).

use crate::event::{HookEventEnvelope, HookEventName, HookPayload};

fn envelope() -> HookEventEnvelope {
    HookEventEnvelope {
        hook_event_name: HookEventName::SessionStart,
        session_id: "sess-gx".into(),
        cwd: "/repo".into(),
        workspace_root: "/repo".into(),
        timestamp: "2025-01-01T00:00:00Z".into(),
        transcript_path: None,
        client_identifier: None,
        prompt_id: None,
        permission_mode: None,
        payload: HookPayload::SessionStart {
            source: "startup".into(),
            model_id: None,
            agent_type: None,
        },
    }
}

/// Both halves of the contract in **one** test, deliberately.
///
/// [`crate::gx_remote::URL`] is a process-global `OnceLock`, so "not announced" is a state this
/// test binary passes through exactly once. Splitting the assertions into two `#[test]`s would
/// make the "omitted" half depend on cargo's parallel scheduling; keeping them in one function
/// pins the order. Nothing else in this crate calls `announce`.
#[test]
fn gx_remote_url_is_stamped_into_the_hook_payload_only_once_announced() {
    let before = envelope().to_hook_json();
    assert!(
        before.get("gxRemote").is_none(),
        "an un-announced payload must be byte-for-byte what upstream emits, key included: {before}"
    );

    crate::gx_remote::announce("http://127.0.0.1:2421");

    let after = envelope().to_hook_json();
    assert_eq!(
        after.get("gxRemote").and_then(serde_json::Value::as_str),
        Some("http://127.0.0.1:2421"),
        "roost reads this as `gx.remote` (roost#425)"
    );

    // First announce wins: a retried lane start must not leave a stale second URL behind.
    crate::gx_remote::announce("http://127.0.0.1:9999");
    assert_eq!(crate::gx_remote::url(), Some("http://127.0.0.1:2421"));

    // Nothing else moved.
    assert_eq!(after["sessionId"], "sess-gx");
    assert_eq!(after["hookEventName"], "session_start");
    assert_eq!(after["hook_event_name"], "SessionStart");
}
