//! Router-level tests: the real axum `Router`, driven through `tower::ServiceExt::oneshot`, over a
//! scripted [`FakeLink`] instead of a leader socket.
//!
//! These are the tests that matter for this crate. A handler is a thin thing; what can actually be
//! wrong is the *contract* — which routes need a token, what a client gets back, and above all in
//! **what order** the lane talks to the leader (attach before read). All of that is observable here
//! without a leader, an agent, a socket or a model.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::acp_client::AcpClient;
use crate::auth::Token;
use crate::link::{FakeLink, FakeLinkHandle};
use crate::state::{AppState, Attachments, HealthInfo};

/// Obviously fake; never a real credential (CLAUDE.md § Secrets).
const TOKEN: &str = "00000000000000000000000000000000deadbeefdeadbeefdeadbeefdeadbeef";

fn test_app() -> (Router, FakeLinkHandle) {
    let (link, handle) = FakeLink::new();
    let acp = AcpClient::spawn(link, CancellationToken::new(), Duration::from_secs(5));
    let state = Arc::new(AppState {
        acp,
        token: Token::from_secret(TOKEN),
        health: HealthInfo {
            version: "1.0.16+gx.10".into(),
            leader_pid: 4242,
            instance_id: "inst-abc".into(),
        },
        attachments: Attachments::default(),
    });
    (crate::routes::router(state), handle)
}

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("every route answers JSON")
    };
    (status, body)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn get_with_header(uri: &str, authorization: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", authorization)
        .body(Body::empty())
        .unwrap()
}

/// `get_with_header` with the crate's own valid token — what almost every test below wants.
fn authed_get(uri: &str) -> Request<Body> {
    get_with_header(uri, &format!("Bearer {TOKEN}"))
}

/// A realistic roster row. Built as JSON rather than as a struct literal so the test pins the
/// **wire** shape the leader actually emits, not this crate's view of it.
fn roster_row(session_id: &str, activity: &str) -> Value {
    json!({
        "sessionId": session_id,
        "title": "Fix the flaky test",
        "cwd": "/home/u/proj",
        "isWorktree": false,
        "modelId": "grok-4",
        "yolo": false,
        "activity": activity,
        "resident": true,
        "lastChangeUnixMs": 1_700_000_000_000_i64,
        "origin": { "kind": "local" }
    })
}

/// Which stub helper to reach for is not a style choice — it is the wire shape.
///
/// `respond_ext_ok` scripts the `ExtMethodResult` envelope (`{"result":{"result":<payload>}}` on
/// the wire) that `x.ai/sessions/list`, `x.ai/session/list` and `x.ai/interject` really answer with;
/// `respond_ok` scripts the plain form that `x.ai/session/updates` and every standard ACP method
/// (`initialize`, `session/load`, …) use. Scripting the plain form for a wrapped method is how a
/// green test suite once blessed an empty production roster — see
/// [`crate::acp_client::unwrap_ext_envelope`] and
/// `a_wrapped_roster_response_reaches_the_client_unwrapped` below.
///
/// Neither the roster nor the unified list knows `id` — the shape `resolve_session` sees on its way
/// to `unknown_session`.
fn stub_no_such_session(handle: &FakeLinkHandle) {
    handle.respond_ext_ok("x.ai/sessions/list", json!({ "sessions": [] }));
    handle.respond_ext_ok("x.ai/session/list", json!({ "sessions": [] }));
}

/// An `idle`, resident `session_id` that attaches cleanly: the roster row, a `session/load` that
/// succeeds, and an empty `x.ai/session/updates` page. Covers the common case for the history
/// route's lazy-attach tests; a test that cares about the page contents overrides
/// `x.ai/session/updates` again afterward.
fn stub_attachable_session(handle: &FakeLinkHandle, session_id: &str) {
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [roster_row(session_id, "idle")] }),
    );
    handle.respond_ok("session/load", json!({}));
    handle.respond_ok(
        "x.ai/session/updates",
        json!({ "updates": [], "totalCount": 0, "hasMore": false }),
    );
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn healthz_needs_no_token() {
    let (app, _handle) = test_app();
    let (status, body) = call(&app, get("/v1/healthz")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["version"], "1.0.16+gx.10");
    assert_eq!(body["leaderPid"], 4242);
    assert_eq!(body["instanceId"], "inst-abc");
    assert_eq!(body["build"], "gx");
}

#[tokio::test]
async fn every_other_route_needs_a_token() {
    let (app, handle) = test_app();
    handle.respond_ext_ok("x.ai/sessions/list", json!({ "sessions": [] }));

    for uri in [
        "/v1/sessions",
        "/v1/sessions/sess-1",
        "/v1/sessions/sess-1/history",
    ] {
        let (status, body) = call(&app, get(uri)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(body["error"], "unauthorized", "{uri}");
        assert!(body["message"].is_string(), "{uri}");
    }

    // Rejected before the leader was consulted at all.
    assert!(
        handle.outbound_methods().is_empty(),
        "an unauthorized request must not reach the leader: {:?}",
        handle.outbound_methods()
    );
}

#[tokio::test]
async fn a_header_token_is_accepted() {
    let (app, handle) = test_app();
    handle.respond_ext_ok("x.ai/sessions/list", json!({ "sessions": [] }));

    let (status, body) = call(&app, authed_get("/v1/sessions")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessions"], json!([]));
}

#[tokio::test]
async fn a_query_token_is_accepted() {
    let (app, handle) = test_app();
    handle.respond_ext_ok("x.ai/sessions/list", json!({ "sessions": [] }));

    // The header-less form, for `EventSource` clients in C5.
    let (status, _) = call(&app, get(&format!("/v1/sessions?token={TOKEN}"))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_wrong_token_is_rejected_in_either_form() {
    let (app, handle) = test_app();
    handle.respond_ext_ok("x.ai/sessions/list", json!({ "sessions": [] }));

    let wrong = "0".repeat(TOKEN.len());
    let (status, body) = call(
        &app,
        get_with_header("/v1/sessions", &format!("Bearer {wrong}")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");

    let (status, _) = call(&app, get(&format!("/v1/sessions?token={wrong}"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A right-length-but-wrong token and a truncated one are equally rejected.
    let (status, _) = call(&app, get("/v1/sessions?token=short")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// GET /v1/sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sessions_maps_the_roster_and_does_not_attach() {
    let (app, handle) = test_app();
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [roster_row("sess-1", "working"), roster_row("sess-2", "needs_input")] }),
    );

    let (status, body) = call(&app, authed_get("/v1/sessions")).await;
    assert_eq!(status, StatusCode::OK);

    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);

    assert_eq!(sessions[0]["sessionId"], "sess-1");
    assert_eq!(sessions[0]["title"], "Fix the flaky test");
    assert_eq!(sessions[0]["cwd"], "/home/u/proj");
    assert_eq!(sessions[0]["activity"], "working");
    assert_eq!(sessions[0]["resident"], true);
    assert_eq!(sessions[0]["modelId"], "grok-4");
    assert_eq!(sessions[0]["lastChangeUnixMs"], 1_700_000_000_000_i64);
    assert_eq!(sessions[0]["attached"], false);
    assert_eq!(sessions[0]["pendingApprovals"], 0);
    assert_eq!(sessions[0]["approximate"], true);

    // `needs_input` is the approximation C6 replaces.
    assert_eq!(sessions[1]["activity"], "needs_input");
    assert_eq!(sessions[1]["pendingApprovals"], 1);
    assert_eq!(sessions[1]["approximate"], true);

    // Listing must never pin a session resident.
    assert_eq!(
        handle.outbound_methods(),
        vec!["_x.ai/sessions/list"],
        "listing sessions must not attach to any of them"
    );
}

#[tokio::test]
async fn a_wrapped_roster_response_reaches_the_client_unwrapped() {
    // The regression. `_x.ai/sessions/list` answers `{"result":{"result":{"sessions":[…]}}}` —
    // captured off a live leader in commit 691153cc, `docs/gx/handoff/list.md`. Written out
    // literally here rather than through `respond_ext_ok` so the shape under test is visible.
    //
    // Delete the `unwrap_ext_envelope` call in `AcpClient::request` and this test fails with an
    // empty `sessions` array — which is exactly what a phone saw in production.
    let (app, handle) = test_app();
    handle.respond_ok(
        "x.ai/sessions/list",
        json!({ "result": { "sessions": [roster_row("sess-1", "working")] } }),
    );

    let (status, body) = call(&app, authed_get("/v1/sessions")).await;
    assert_eq!(status, StatusCode::OK);
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "an ExtMethodResult-wrapped roster must not read as an empty roster: {body}"
    );
    assert_eq!(sessions[0]["sessionId"], "sess-1");
    assert_eq!(sessions[0]["activity"], "working");
}

#[tokio::test]
async fn an_unwrapped_updates_page_is_not_unwrapped_again() {
    // The other half of the same contract: `_x.ai/session/updates` carries no envelope
    // (`extensions/session_updates.rs::response_from_page`), so unwrapping it unconditionally
    // would blank every transcript.
    let (app, handle) = test_app();
    stub_attachable_session(&handle, "sess-1");
    handle.respond_ok(
        "x.ai/session/updates",
        json!({
            "updates": [{ "timestamp": 1, "method": "session/update", "params": {} }],
            "totalCount": 1,
            "hasMore": false
        }),
    );

    let (status, body) = call(&app, authed_get("/v1/sessions/sess-1/history")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["totalCount"], 1);
    assert_eq!(body["updates"].as_array().unwrap().len(), 1, "{body}");
}

#[tokio::test]
async fn an_envelope_error_from_the_leader_is_503_not_an_empty_roster() {
    // A JSON-RPC *success* whose `ExtMethodResult` carried an `error`. Reporting it as an empty
    // roster would look like "no sessions" to a phone.
    let (app, handle) = test_app();
    handle.respond_ext_err("x.ai/sessions/list", "internal", "roster actor is wedged");

    let (status, body) = call(&app, authed_get("/v1/sessions")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "leader_unavailable");
    assert!(
        body["message"].as_str().unwrap().contains("wedged"),
        "the envelope's message must survive: {body}"
    );
}

#[tokio::test]
async fn an_undecodable_roster_row_is_skipped_not_fatal() {
    let (app, handle) = test_app();
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [json!({ "sessionId": "broken" }), roster_row("sess-1", "idle")] }),
    );

    let (status, body) = call(&app, authed_get("/v1/sessions")).await;
    assert_eq!(status, StatusCode::OK);
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["sessionId"], "sess-1");
}

#[tokio::test]
async fn a_leader_error_on_the_roster_is_503() {
    let (app, handle) = test_app();
    handle.respond_err("x.ai/sessions/list", -32603, "internal error");

    let (status, body) = call(&app, authed_get("/v1/sessions")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "leader_unavailable");
}

// ---------------------------------------------------------------------------
// GET /v1/sessions/{id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_metadata_comes_from_the_roster_without_attaching() {
    let (app, handle) = test_app();
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [roster_row("sess-1", "idle")] }),
    );

    let (status, body) = call(&app, authed_get("/v1/sessions/sess-1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessionId"], "sess-1");
    assert_eq!(body["activity"], "idle");
    assert_eq!(body["attached"], false);

    assert_eq!(
        handle.outbound_methods(),
        vec!["_x.ai/sessions/list"],
        "the metadata GET must not attach"
    );
}

#[tokio::test]
async fn a_session_the_roster_aged_out_falls_back_to_the_unified_list() {
    let (app, handle) = test_app();
    handle.respond_ext_ok("x.ai/sessions/list", json!({ "sessions": [] }));
    handle.respond_ext_ok(
        "x.ai/session/list",
        json!({
            "sessions": [
                // A cloud chat row: right id, wrong kind. Must not match.
                {
                    "sessionId": "sess-old",
                    "cwd": "/home/u/proj",
                    "title": "An imported chat",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "_meta": { "x.ai/session": { "kind": "chat" } }
                },
                {
                    "sessionId": "sess-old",
                    "cwd": "/home/u/proj",
                    "title": "Last week's work",
                    "updatedAt": "2026-01-02T03:04:05Z",
                    "modelId": "grok-4",
                    "_meta": { "x.ai/session": { "kind": "build" } }
                }
            ]
        }),
    );

    let (status, body) = call(&app, authed_get("/v1/sessions/sess-old")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessionId"], "sess-old");
    assert_eq!(body["title"], "Last week's work");
    assert_eq!(body["cwd"], "/home/u/proj");
    assert_eq!(body["activity"], "dormant");
    assert_eq!(body["resident"], false);
    assert_eq!(body["modelId"], "grok-4");
    assert_eq!(body["lastChangeUnixMs"], 1_767_323_045_000_i64);

    assert_eq!(
        handle.outbound_methods(),
        vec!["_x.ai/sessions/list", "_x.ai/session/list"]
    );
}

#[tokio::test]
async fn an_unknown_session_is_404_with_the_error_envelope() {
    let (app, handle) = test_app();
    stub_no_such_session(&handle);

    let (status, body) = call(&app, authed_get("/v1/sessions/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_session");
    assert!(body["message"].as_str().unwrap().contains("nope"), "{body}");
}

// ---------------------------------------------------------------------------
// GET /v1/sessions/{id}/history — lazy attach
// ---------------------------------------------------------------------------

#[tokio::test]
async fn history_attaches_with_no_replay_before_asking_for_updates() {
    let (app, handle) = test_app();
    stub_attachable_session(&handle, "sess-1");

    let (status, _) = call(&app, authed_get("/v1/sessions/sess-1/history")).await;
    assert_eq!(status, StatusCode::OK);

    // The order is the contract: resolve the session, attach, *then* read.
    assert_eq!(
        handle.outbound_methods(),
        vec![
            "_x.ai/sessions/list",
            "session/load",
            "_x.ai/session/updates"
        ]
    );

    let load = handle.first_outbound("session/load").unwrap();
    assert_eq!(load["params"]["sessionId"], "sess-1");
    assert_eq!(load["params"]["cwd"], "/home/u/proj");
    assert_eq!(load["params"]["mcpServers"], json!([]));
    assert_eq!(
        load["params"]["_meta"]["noReplay"], true,
        "a phone reads history over HTTP; replaying it down the link is pure duplicate"
    );
}

#[tokio::test]
async fn history_normalizes_every_stored_envelope() {
    let (app, handle) = test_app();
    stub_attachable_session(&handle, "sess-1");
    // Override the stub's empty page with one that exercises normalization.
    handle.respond_ok(
        "x.ai/session/updates",
        json!({
            "updates": [
                {
                    "timestamp": 1_700_000_000_001_i64,
                    "method": "session/update",
                    "params": {
                        "sessionId": "sess-1",
                        "update": { "sessionUpdate": "agent_message_chunk" },
                        "_meta": { "eventId": "sess-1-7" }
                    }
                },
                {
                    "timestamp": 1_700_000_000_002_i64,
                    "method": "session/update",
                    "params": { "sessionId": "sess-1", "update": {} }
                }
            ],
            "totalCount": 42,
            "hasMore": true,
            "lastEventId": "sess-1-7"
        }),
    );

    let (status, body) = call(&app, authed_get("/v1/sessions/sess-1/history")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["totalCount"], 42);
    assert_eq!(body["hasMore"], true);
    assert_eq!(body["lastEventId"], "sess-1-7");

    let updates = body["updates"].as_array().unwrap();
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0]["eventId"], "sess-1-7");
    assert_eq!(updates[0]["method"], "session/update");
    assert_eq!(updates[0]["timestamp"], 1_700_000_000_001_i64);
    assert_eq!(
        updates[0]["params"]["update"]["sessionUpdate"],
        "agent_message_chunk"
    );
    // A line with no `_meta.eventId` is still delivered, with an explicit null cursor.
    assert_eq!(updates[1]["eventId"], Value::Null);
}

#[tokio::test]
async fn history_passes_offset_and_limit_through_and_marks_the_session_attached() {
    let (app, handle) = test_app();
    stub_attachable_session(&handle, "sess-1");

    let (status, _) = call(
        &app,
        authed_get("/v1/sessions/sess-1/history?offset=-100&limit=50"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let updates = handle.first_outbound("_x.ai/session/updates").unwrap();
    assert_eq!(updates["params"]["sessionId"], "sess-1");
    assert_eq!(updates["params"]["cwd"], "/home/u/proj");
    assert_eq!(updates["params"]["offset"], -100);
    assert_eq!(updates["params"]["limit"], 50);

    // The roster now reports the session as attached to *this lane*.
    let (_, body) = call(&app, authed_get("/v1/sessions")).await;
    assert_eq!(body["sessions"][0]["attached"], true);
}

#[tokio::test]
async fn a_second_history_request_does_not_attach_again() {
    let (app, handle) = test_app();
    stub_attachable_session(&handle, "sess-1");

    for _ in 0..3 {
        let (status, _) = call(&app, authed_get("/v1/sessions/sess-1/history")).await;
        assert_eq!(status, StatusCode::OK);
    }

    let loads = handle
        .outbound_methods()
        .into_iter()
        .filter(|m| m == "session/load")
        .count();
    assert_eq!(
        loads, 1,
        "a second session/load makes the agent flush and replay the session"
    );
}

#[tokio::test]
async fn a_failed_attach_is_503_and_updates_are_never_requested() {
    let (app, handle) = test_app();
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [roster_row("sess-1", "idle")] }),
    );
    handle.respond_err("session/load", -32603, "session actor is dead");

    let (status, body) = call(&app, authed_get("/v1/sessions/sess-1/history")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "leader_unavailable");
    assert!(
        !handle
            .outbound_methods()
            .contains(&"_x.ai/session/updates".to_string()),
        "updates must not be read for a session the lane failed to attach to"
    );
}

#[tokio::test]
async fn history_for_an_unknown_session_is_404_and_never_attaches() {
    let (app, handle) = test_app();
    stub_no_such_session(&handle);

    let (status, body) = call(&app, authed_get("/v1/sessions/nope/history")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_session");
    assert!(
        !handle
            .outbound_methods()
            .contains(&"session/load".to_string())
    );
}

#[tokio::test]
async fn a_malformed_pagination_bound_is_a_bad_request() {
    let (app, handle) = test_app();
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [roster_row("sess-1", "idle")] }),
    );

    let (status, body) = call(&app, authed_get("/v1/sessions/sess-1/history?limit=lots")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "bad_request");
    assert!(
        handle.outbound_methods().is_empty(),
        "a bad request must be rejected before the leader is touched"
    );
}
