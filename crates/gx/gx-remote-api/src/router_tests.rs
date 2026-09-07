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
use crate::approvals::ApprovalStore;
use crate::auth::Token;
use crate::link::{FakeLink, FakeLinkHandle};
use crate::ring::EventRing;
use crate::state::{AppState, Attachments, HealthInfo, SseSettings, spawn_event_pump};

/// The tests' stand-in token: 64 lowercase hex characters, which is the only shape
/// [`crate::auth`] accepts.
///
/// Built at runtime rather than written as a literal. A 64-hex string assigned to a constant named
/// `TOKEN` is exactly the shape a secret scanner flags, and CI's gitleaks job did flag it — an
/// obviously-fake value still costs a red build and trains people to ignore the scanner. There is
/// no long hex literal anywhere in this repo as a result (CLAUDE.md § Secrets).
fn token() -> String {
    "ab".repeat(32)
}

fn test_app() -> (Router, FakeLinkHandle) {
    test_app_with(SseSettings::default(), EventRing::new())
}

/// The router the production `serve` builds, over a scripted link and with the SSE lane's two
/// bounds under the test's control.
///
/// `spawn_event_pump` is started here for the same reason `serve` starts it: the ring has to be
/// filling before any connection exists, because that is exactly the window a resume is for.
fn test_app_with(sse: SseSettings, ring: EventRing) -> (Router, FakeLinkHandle) {
    let (link, handle) = FakeLink::new();
    let attachments = Arc::new(Attachments::default());
    let approvals = Arc::new(ApprovalStore::new(attachments.clone()));
    let acp = AcpClient::spawn(
        link,
        CancellationToken::new(),
        Duration::from_secs(5),
        approvals.clone(),
    );
    let state = Arc::new(AppState {
        acp,
        token: Token::from_secret(token()),
        health: HealthInfo {
            version: "1.0.16+gx.10".into(),
            leader_pid: 4242,
            instance_id: "inst-abc".into(),
        },
        attachments,
        approvals,
        ring,
        sse,
    });
    spawn_event_pump(state.clone(), state.acp.cancel_token());
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
    get_with_header(uri, &format!("Bearer {}", token()))
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
    let (status, _) = call(&app, get(&format!("/v1/sessions?token={}", token()))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_wrong_token_is_rejected_in_either_form() {
    let (app, handle) = test_app();
    handle.respond_ext_ok("x.ai/sessions/list", json!({ "sessions": [] }));

    let wrong = "0".repeat(token().len());
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

// ---------------------------------------------------------------------------
// POST verbs — authorization, and the four shapes a write takes
// ---------------------------------------------------------------------------

/// A `POST` with the crate's valid token and a JSON body.
fn authed_post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {}", token()))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// `session_id` in `activity`, attachable, with every write verb scripted.
///
/// `session/prompt` is deliberately answered by nothing: its real response does not arrive until
/// the turn ends, and a handler that waited for it would hang here exactly as it would in
/// production. `session/cancel` is a notification and needs no responder at all.
fn stub_writable_session(handle: &FakeLinkHandle, session_id: &str, activity: &str) {
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [roster_row(session_id, activity)] }),
    );
    handle.respond_ok("session/load", json!({}));
    handle.never_respond("session/prompt");
    handle.respond_ext_ok("x.ai/interject", json!({ "status": "queued" }));
}

/// Every cell of the plan's admission table, over the real router.
///
/// `queue` is allowed everywhere, `interject` only into a running turn, `cancel` only when there is
/// something to interrupt. A denial is `409 not_accepting` — not `403`: the caller's credential is
/// fine, the session's state is what refuses.
#[tokio::test]
async fn the_authorization_matrix_holds_for_every_activity_and_verb() {
    // activity, queue, interject, cancel
    let matrix = [
        ("working", true, true, true),
        ("needs_input", true, false, true),
        ("idle", true, false, false),
        ("completed", true, false, false),
        ("dormant", true, false, false),
        ("dead", true, false, false),
    ];

    for (activity, queue, interject, cancel) in matrix {
        for (verb, allowed) in [
            ("queue", queue),
            ("interject", interject),
            ("cancel", cancel),
        ] {
            let (app, handle) = test_app();
            stub_writable_session(&handle, "sess-1", activity);

            let request = if verb == "cancel" {
                authed_post("/v1/sessions/sess-1/cancel", json!({}))
            } else {
                authed_post(
                    "/v1/sessions/sess-1/messages",
                    json!({ "text": "hello", "mode": verb }),
                )
            };
            let (status, body) = call(&app, request).await;

            if allowed {
                assert_eq!(status, StatusCode::ACCEPTED, "{activity} / {verb}: {body}");
                assert_eq!(body["accepted"], true, "{activity} / {verb}");
            } else {
                assert_eq!(status, StatusCode::CONFLICT, "{activity} / {verb}: {body}");
                assert_eq!(body["error"], "not_accepting", "{activity} / {verb}");
                let message = body["message"].as_str().unwrap();
                assert!(message.contains(activity), "{activity} / {verb}: {message}");
                assert!(message.contains("queue"), "{activity} / {verb}: {message}");
            }
        }
    }
}

#[tokio::test]
async fn a_denied_verb_never_reaches_the_leader() {
    let (app, handle) = test_app();
    stub_writable_session(&handle, "sess-1", "idle");

    let (status, _) = call(
        &app,
        authed_post(
            "/v1/sessions/sess-1/messages",
            json!({ "text": "hi", "mode": "interject" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Attaching is fine — it precedes the decision by design — but the verb itself must not go out.
    let sent = handle.outbound_methods();
    assert!(!sent.contains(&"_x.ai/interject".to_string()), "{sent:?}");
    assert!(!sent.contains(&"session/prompt".to_string()), "{sent:?}");
}

#[tokio::test]
async fn a_queued_message_answers_while_the_turn_is_still_running() {
    let (app, handle) = test_app();
    stub_writable_session(&handle, "sess-1", "working");

    // `session/prompt` is scripted to never answer. A handler that awaited the turn would hang here
    // until the test's own timeout, not return 202.
    let (status, body) = tokio::time::timeout(
        Duration::from_secs(5),
        call(
            &app,
            authed_post("/v1/sessions/sess-1/messages", json!({ "text": "hello" })),
        ),
    )
    .await
    .expect("a queued prompt must not block on the turn it starts");

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["accepted"], true);
    assert_eq!(body["mode"], "queue", "queue is the default mode");

    // Attach first, then the prompt — and the prompt is really on the wire, in ACP's content-block
    // shape, not merely accepted and dropped.
    let prompt = handle
        .wait_for_outbound("session/prompt", Duration::from_secs(5))
        .await;
    assert_eq!(
        handle.outbound_methods(),
        vec!["_x.ai/sessions/list", "session/load", "session/prompt"]
    );
    assert_eq!(prompt["params"]["sessionId"], "sess-1");
    assert_eq!(prompt["params"]["prompt"][0]["type"], "text");
    assert_eq!(prompt["params"]["prompt"][0]["text"], "hello");
}

#[tokio::test]
async fn an_interjection_returns_the_status_the_leader_reported() {
    let (app, handle) = test_app();
    stub_writable_session(&handle, "sess-1", "working");
    // The live shape: `_x.ai/interject` wraps `{status}` in an `ExtMethodResult` envelope.
    handle.respond_ext_ok("x.ai/interject", json!({ "status": "queued" }));

    let (status, body) = call(
        &app,
        authed_post(
            "/v1/sessions/sess-1/messages",
            json!({ "text": "stop that", "mode": "interject" }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["mode"], "interject");
    assert_eq!(
        body["status"], "queued",
        "the envelope must be unwrapped before the status is read: {body}"
    );
    let interject = handle.first_outbound("_x.ai/interject").unwrap();
    assert_eq!(interject["params"]["sessionId"], "sess-1");
    assert_eq!(interject["params"]["text"], "stop that");
}

#[tokio::test]
async fn cancel_sends_a_notification_with_no_id() {
    let (app, handle) = test_app();
    stub_writable_session(&handle, "sess-1", "working");

    let (status, body) = call(&app, authed_post("/v1/sessions/sess-1/cancel", json!({}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["accepted"], true);

    let cancel = handle
        .wait_for_outbound("session/cancel", Duration::from_secs(5))
        .await;
    assert_eq!(cancel["params"]["sessionId"], "sess-1");
    assert!(
        cancel.get("id").is_none(),
        "session/cancel is a notification; an id would make the leader owe us a response it never \
         sends: {cancel}"
    );
}

#[tokio::test]
async fn a_bad_message_body_is_a_bad_request_before_the_leader_is_touched() {
    for body in [
        json!({}),                                // no text
        json!({ "text": "   " }),                 // blank text
        json!({ "text": "hi", "mode": "steer" }), // a mode this API has no verb for
    ] {
        let (app, handle) = test_app();
        stub_writable_session(&handle, "sess-1", "working");

        let (status, response) = call(
            &app,
            authed_post("/v1/sessions/sess-1/messages", body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(response["error"], "bad_request", "{body}");
        assert!(
            handle.outbound_methods().is_empty(),
            "{body} reached the leader: {:?}",
            handle.outbound_methods()
        );
    }
}

#[tokio::test]
async fn a_write_to_an_unknown_session_is_404() {
    let (app, handle) = test_app();
    stub_no_such_session(&handle);

    for request in [
        authed_post("/v1/sessions/nope/messages", json!({ "text": "hi" })),
        authed_post("/v1/sessions/nope/cancel", json!({})),
    ] {
        let (status, body) = call(&app, request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "unknown_session");
    }
}

// ---------------------------------------------------------------------------
// POST /v1/sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_session_returns_its_id_and_does_not_load_it_again() {
    let (app, handle) = test_app();
    handle.respond_ok("session/new", json!({ "sessionId": "sess-new" }));
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [roster_row("sess-new", "idle")] }),
    );

    let (status, body) = call(&app, authed_post("/v1/sessions", json!({ "cwd": "/repo" }))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["sessionId"], "sess-new");

    let new = handle.first_outbound("session/new").unwrap();
    assert_eq!(new["params"]["cwd"], "/repo");
    assert_eq!(new["params"]["mcpServers"], json!([]));
    assert_eq!(
        handle.outbound_methods(),
        vec!["session/new"],
        "no prompt was asked for, and `session/new` already subscribed us"
    );

    // The lane counts itself attached, so the next touch does not send a redundant `session/load`.
    let (_, roster) = call(&app, authed_get("/v1/sessions")).await;
    assert_eq!(roster["sessions"][0]["attached"], true);
}

#[tokio::test]
async fn creating_a_session_with_text_sends_the_first_prompt() {
    let (app, handle) = test_app();
    handle.respond_ok("session/new", json!({ "sessionId": "sess-new" }));
    handle.never_respond("session/prompt");

    let (status, body) = call(
        &app,
        authed_post(
            "/v1/sessions",
            json!({ "cwd": "/repo", "text": "start here" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["sessionId"], "sess-new");

    let prompt = handle
        .wait_for_outbound("session/prompt", Duration::from_secs(5))
        .await;
    assert_eq!(
        handle.outbound_methods(),
        vec!["session/new", "session/prompt"],
        "the session has to exist before it can be prompted"
    );
    assert_eq!(prompt["params"]["sessionId"], "sess-new");
    assert_eq!(prompt["params"]["prompt"][0]["text"], "start here");
}

#[tokio::test]
async fn creating_a_session_reports_a_leader_that_answers_without_an_id() {
    let (app, handle) = test_app();
    handle.respond_ok("session/new", json!({}));

    let (status, body) = call(&app, authed_post("/v1/sessions", json!({ "cwd": "/repo" }))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "leader_unavailable");
}

#[tokio::test]
async fn creating_a_session_without_a_cwd_is_a_bad_request() {
    let (app, handle) = test_app();
    handle.respond_ok("session/new", json!({ "sessionId": "sess-new" }));

    for body in [json!({}), json!({ "cwd": "" })] {
        let (status, response) = call(&app, authed_post("/v1/sessions", body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(response["error"], "bad_request", "{body}");
    }
    assert!(handle.outbound_methods().is_empty());
}

// ---------------------------------------------------------------------------
// GET /v1/sessions/{id}/events — the SSE lane
// ---------------------------------------------------------------------------

/// How long a test waits for a frame that should already be on its way.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a test waits to conclude that **no** further frame is coming. Short on purpose: this is
/// the only wall-clock cost in the SSE tests, and it is bounded rather than slept through.
const QUIET_TIMEOUT: Duration = Duration::from_millis(150);

/// One SSE frame, parsed off the wire.
#[derive(Debug, Clone, Default, PartialEq)]
struct SseFrame {
    event: Option<String>,
    id: Option<String>,
    data: String,
    comments: Vec<String>,
}

impl SseFrame {
    fn parse(block: &str) -> Self {
        let mut frame = Self::default();
        for line in block.lines().filter(|line| !line.is_empty()) {
            if let Some(comment) = line.strip_prefix(':') {
                frame.comments.push(comment.to_string());
                continue;
            }
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => frame.event = Some(value.to_string()),
                "id" => frame.id = Some(value.to_string()),
                "data" => {
                    if !frame.data.is_empty() {
                        frame.data.push('\n');
                    }
                    frame.data.push_str(value);
                }
                _ => {}
            }
        }
        frame
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.data)
            .unwrap_or_else(|err| panic!("frame data is not JSON ({err}): {:?}", self.data))
    }

    fn is(&self, event: &str) -> bool {
        self.event.as_deref() == Some(event)
    }
}

/// A live SSE response body, read frame by frame.
///
/// The stream never ends on its own, so every read is bounded: [`Self::next_frame`] for a frame that
/// must arrive, [`Self::expect_quiet`] for the assertion that nothing more will.
struct SseStream {
    body: std::pin::Pin<Box<axum::body::BodyDataStream>>,
    buffer: String,
}

impl SseStream {
    async fn open(
        app: &Router,
        request: Request<Body>,
    ) -> (StatusCode, axum::http::HeaderMap, Self) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        (
            status,
            headers,
            Self {
                body: Box::pin(response.into_body().into_data_stream()),
                buffer: String::new(),
            },
        )
    }

    async fn try_next_frame(&mut self, timeout: Duration) -> Option<SseFrame> {
        use tokio_stream::StreamExt as _;
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                return Some(SseFrame::parse(&block));
            }
            let chunk = tokio::time::timeout(timeout, self.body.next())
                .await
                .ok()??;
            self.buffer
                .push_str(std::str::from_utf8(&chunk.expect("stream error")).unwrap());
        }
    }

    async fn next_frame(&mut self) -> SseFrame {
        self.try_next_frame(FRAME_TIMEOUT)
            .await
            .expect("the stream ended or stalled while a frame was still expected")
    }

    async fn next_frames(&mut self, count: usize) -> Vec<SseFrame> {
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            frames.push(self.next_frame().await);
        }
        frames
    }

    /// Assert nothing more arrives. The keepalive is set to an hour in these tests, so a frame here
    /// is a real one.
    async fn expect_quiet(&mut self) {
        if let Some(frame) = self.try_next_frame(QUIET_TIMEOUT).await {
            panic!("expected no further frame, got {frame:?}");
        }
    }
}

/// SSE settings a test can reason about: a keepalive far beyond any test's life, so no comment
/// frame can be mistaken for a real one, and the production queue bound unless a test says
/// otherwise.
fn test_sse(queue_capacity: usize) -> SseSettings {
    SseSettings {
        keepalive: Duration::from_secs(3600),
        queue_capacity,
    }
}

/// One stored/live frame for `session_id` at `counter`.
fn ring_frame(session_id: &str, counter: u64) -> crate::envelope::NormalizedEnvelope {
    crate::envelope::NormalizedEnvelope::from_stored(&stored_update(session_id, counter))
}

/// The `updates.jsonl` envelope for `session_id` at `counter`, as `x.ai/session/updates` returns it.
fn stored_update(session_id: &str, counter: u64) -> Value {
    json!({
        "timestamp": 1_700_000_000_000_i64 + counter as i64,
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": { "sessionUpdate": "agent_message_chunk" },
            "_meta": { "eventId": format!("{session_id}-{counter}") }
        }
    })
}

/// The live notification for the same event, in the form the leader broadcasts it.
fn live_update(session_id: &str, counter: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": stored_update(session_id, counter)["params"].clone(),
    })
}

fn events_request(session_id: &str, cursor: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(format!("/v1/sessions/{session_id}/events"))
        .header("authorization", format!("Bearer {}", token()));
    if let Some(cursor) = cursor {
        builder = builder.header("last-event-id", cursor);
    }
    builder.body(Body::empty()).unwrap()
}

fn ids(frames: &[SseFrame]) -> Vec<Option<String>> {
    frames.iter().map(|frame| frame.id.clone()).collect()
}

/// A ring holding `counters` for `session_id`, in that order.
fn ring_with(session_id: &str, counters: &[u64]) -> EventRing {
    let ring = EventRing::new();
    for counter in counters {
        ring.push(session_id, ring_frame(session_id, *counter));
    }
    ring
}

#[tokio::test]
async fn a_cursor_inside_the_ring_replays_exactly_the_frames_after_it_in_order() {
    let (app, handle) = test_app_with(test_sse(256), ring_with("sess-1", &[10, 11, 12]));
    stub_attachable_session(&handle, "sess-1");

    let (status, headers, mut stream) =
        SseStream::open(&app, events_request("sess-1", Some("sess-1-10"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["content-type"], "text/event-stream",
        "an SSE client dispatches on this"
    );

    let frames = stream.next_frames(2).await;
    assert!(frames.iter().all(|f| f.is("update")), "{frames:?}");
    assert_eq!(
        ids(&frames),
        vec![Some("sess-1-11".into()), Some("sess-1-12".into())],
        "exactly the frames after the cursor, in order, and nothing already seen"
    );
    // The replay is from memory: no `x.ai/session/updates` was needed at all.
    assert!(
        !handle
            .outbound_methods()
            .contains(&"_x.ai/session/updates".to_string()),
        "{:?}",
        handle.outbound_methods()
    );
    stream.expect_quiet().await;
}

#[tokio::test]
async fn interleaved_counters_from_another_session_do_not_break_the_replay() {
    // The counter is process-global, so a session's own ids are sparse. A client must get its own
    // events, in order, and must not see another session's — gaps included.
    let ring = EventRing::new();
    for (session, counter) in [
        ("sess-1", 10),
        ("sess-2", 11),
        ("sess-1", 12),
        ("sess-2", 13),
        ("sess-1", 14),
    ] {
        ring.push(session, ring_frame(session, counter));
    }
    let (app, handle) = test_app_with(test_sse(256), ring);
    stub_attachable_session(&handle, "sess-1");

    let (status, _, mut stream) =
        SseStream::open(&app, events_request("sess-1", Some("sess-1-10"))).await;
    assert_eq!(status, StatusCode::OK);

    let frames = stream.next_frames(2).await;
    assert_eq!(
        ids(&frames),
        vec![Some("sess-1-12".into()), Some("sess-1-14".into())],
        "order and exact set are the promise; contiguity is not"
    );
    stream.expect_quiet().await;
}

#[tokio::test]
async fn a_cursor_older_than_the_ring_reads_the_disk_and_does_not_repeat_the_overlap() {
    // The ring holds 20..22; the disk holds 6, 20 and 21 (22 has not been flushed). The client
    // resumes from 5, so it needs 6 from disk, 20 and 21 exactly once, and 22 from the ring.
    let (app, handle) = test_app_with(test_sse(256), ring_with("sess-1", &[20, 21, 22]));
    stub_attachable_session(&handle, "sess-1");
    handle.respond_ok(
        "x.ai/session/updates",
        json!({
            "updates": [
                stored_update("sess-1", 5),
                stored_update("sess-1", 6),
                stored_update("sess-1", 20),
                stored_update("sess-1", 21),
            ],
            "totalCount": 4,
            "hasMore": false,
            "lastEventId": "sess-1-21"
        }),
    );

    let (status, _, mut stream) =
        SseStream::open(&app, events_request("sess-1", Some("sess-1-5"))).await;
    assert_eq!(status, StatusCode::OK);

    let frames = stream.next_frames(3).await;
    assert_eq!(
        ids(&frames),
        vec![
            Some("sess-1-6".into()),
            Some("sess-1-20".into()),
            Some("sess-1-21".into())
        ],
        "the cursor's own event is not replayed, and the disk covers the gap"
    );

    let tail = stream.next_frame().await;
    assert_eq!(
        tail.id.as_deref(),
        Some("sess-1-22"),
        "the ring supplies what the disk had not flushed yet"
    );
    stream.expect_quiet().await;

    assert!(
        handle
            .outbound_methods()
            .contains(&"_x.ai/session/updates".to_string()),
        "a cursor older than the ring has to reach the store"
    );
}

#[tokio::test]
async fn a_cursor_newer_than_anything_known_resets_the_client() {
    let (app, handle) = test_app_with(test_sse(256), ring_with("sess-1", &[10, 11]));
    stub_attachable_session(&handle, "sess-1");

    let (status, _, mut stream) =
        SseStream::open(&app, events_request("sess-1", Some("sess-1-99"))).await;
    assert_eq!(status, StatusCode::OK);

    let frame = stream.next_frame().await;
    assert!(frame.is("reset"), "{frame:?}");
    assert_eq!(frame.json()["reason"], "cursor_unresolvable");
    assert_eq!(frame.id, None, "a reset is not a resume point");
    stream.expect_quiet().await;
}

#[tokio::test]
async fn an_empty_ring_falls_back_to_the_persisted_tail_to_place_a_cursor() {
    // Nothing in memory, so "is this cursor from the future?" can only be answered by the store.
    let (app, handle) = test_app_with(test_sse(256), EventRing::new());
    stub_attachable_session(&handle, "sess-1");
    handle.respond_ok(
        "x.ai/session/updates",
        json!({
            "updates": [stored_update("sess-1", 8)],
            "totalCount": 1,
            "hasMore": false,
            "lastEventId": "sess-1-8"
        }),
    );

    let (_, _, mut stream) =
        SseStream::open(&app, events_request("sess-1", Some("sess-1-9"))).await;
    let frame = stream.next_frame().await;
    assert!(frame.is("reset"), "{frame:?}");
    assert_eq!(frame.json()["reason"], "cursor_unresolvable");

    let probe = handle.first_outbound("_x.ai/session/updates").unwrap();
    assert_eq!(
        probe["params"]["offset"], -64,
        "`lastEventId` is per-page, so the probe has to ask for the tail: {probe}"
    );
}

#[tokio::test]
async fn a_malformed_or_foreign_cursor_resets_the_client() {
    for cursor in ["garbage", "sess-1-", "sess-1-abc", "sess-2-5", "-5"] {
        let (app, handle) = test_app_with(test_sse(256), ring_with("sess-1", &[10, 11]));
        stub_attachable_session(&handle, "sess-1");

        let (status, _, mut stream) =
            SseStream::open(&app, events_request("sess-1", Some(cursor))).await;
        assert_eq!(status, StatusCode::OK, "{cursor}");

        let frame = stream.next_frame().await;
        assert!(frame.is("reset"), "{cursor}: {frame:?}");
        assert_eq!(frame.json()["reason"], "cursor_unresolvable", "{cursor}");
        // Not one frame of somebody else's session, and not a replay of ours either.
        stream.expect_quiet().await;
    }
}

#[tokio::test]
async fn no_cursor_at_all_is_not_a_reset() {
    let (app, handle) = test_app_with(test_sse(256), ring_with("sess-1", &[10, 11]));
    stub_attachable_session(&handle, "sess-1");

    let (status, _, mut stream) = SseStream::open(&app, events_request("sess-1", None)).await;
    assert_eq!(status, StatusCode::OK);
    // A fresh `EventSource` just starts live; the ring is not replayed to it.
    stream.expect_quiet().await;
}

#[tokio::test]
async fn live_update_frames_carry_an_id_and_roster_frames_do_not() {
    let (app, handle) = test_app_with(test_sse(256), EventRing::new());
    stub_attachable_session(&handle, "sess-1");

    let (status, _, mut stream) = SseStream::open(&app, events_request("sess-1", None)).await;
    assert_eq!(status, StatusCode::OK);

    handle.push(live_update("sess-1", 7));
    // A machine-wide roster broadcast: no `sessionId` anywhere, so the stream has to match on the
    // entries it carries.
    handle.push(json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/sessions/changed",
        "params": {
            "upserted": [roster_row("sess-1", "working")],
            "removed": []
        }
    }));

    let update = stream.next_frame().await;
    assert!(update.is("update"), "{update:?}");
    assert_eq!(update.id.as_deref(), Some("sess-1-7"));
    assert_eq!(update.json()["eventId"], "sess-1-7");
    assert_eq!(update.json()["method"], "session/update");
    assert_eq!(
        update.json()["params"]["update"]["sessionUpdate"],
        "agent_message_chunk"
    );

    let session = stream.next_frame().await;
    assert!(session.is("session"), "{session:?}");
    assert_eq!(
        session.id, None,
        "an invalidation is not a resume point; giving it an id would let a browser resume from it"
    );
    assert_eq!(session.json()["sessionId"], "sess-1");
    assert_eq!(session.json()["activity"], "working");
    stream.expect_quiet().await;
}

#[tokio::test]
async fn a_roster_change_for_another_session_is_not_this_streams_business() {
    let (app, handle) = test_app_with(test_sse(256), EventRing::new());
    stub_attachable_session(&handle, "sess-1");

    let (_, _, mut stream) = SseStream::open(&app, events_request("sess-1", None)).await;
    handle.push(json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/sessions/changed",
        "params": { "upserted": [roster_row("sess-2", "working")], "removed": [] }
    }));
    handle.push(live_update("sess-2", 3));

    stream.expect_quiet().await;
}

#[tokio::test]
async fn a_slow_consumer_is_told_once_and_then_continues_from_live() {
    // Queue of one, so a second frame that arrives before the client reads the first is dropped.
    // Production is 256; the behaviour under test is identical and this makes it reachable without
    // flooding 257 frames past a reader that is not reading.
    let (app, handle) = test_app_with(test_sse(1), EventRing::new());
    stub_attachable_session(&handle, "sess-1");

    let (status, _, mut stream) = SseStream::open(&app, events_request("sess-1", None)).await;
    assert_eq!(status, StatusCode::OK);

    for counter in [1, 2, 3] {
        handle.push(live_update("sess-1", counter));
    }
    // Let the link and the connection's producer drain the broadcast before anything reads the
    // body. Nothing polls the response body until `next_frame` below — the stream lives in this
    // test, not in a task — so the producer cannot be rescued by a reader here: it has to overflow.
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }

    let first = stream.next_frame().await;
    assert!(first.is("update"), "{first:?}");
    assert_eq!(first.id.as_deref(), Some("sess-1-1"));

    let reset = stream.next_frame().await;
    assert!(reset.is("reset"), "{reset:?}");
    assert_eq!(reset.json()["reason"], "slow_consumer");
    assert_eq!(reset.id, None, "a reset is not a resume point");

    // Frames 2 and 3 are gone, and the client is told once — not once per dropped frame.
    stream.expect_quiet().await;

    // …and the connection is still live: the next real event still arrives.
    handle.push(live_update("sess-1", 4));
    let resumed = stream.next_frame().await;
    assert!(resumed.is("update"), "{resumed:?}");
    assert_eq!(
        resumed.id.as_deref(),
        Some("sess-1-4"),
        "a slow consumer is re-synced, not disconnected"
    );
    stream.expect_quiet().await;
}

#[tokio::test]
async fn a_stream_attaches_before_it_streams_and_404s_an_unknown_session() {
    let (app, handle) = test_app_with(test_sse(256), EventRing::new());
    stub_attachable_session(&handle, "sess-1");

    let (status, _, _stream) = SseStream::open(&app, events_request("sess-1", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        handle.outbound_methods(),
        vec!["_x.ai/sessions/list", "session/load"],
        "resolve, attach, then stream — a stream for a session the lane never loaded is a stream \
         of nothing"
    );

    let (app, handle) = test_app_with(test_sse(256), EventRing::new());
    stub_no_such_session(&handle);
    let (status, body) = call(&app, events_request("nope", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_session");
}

#[tokio::test]
async fn the_event_stream_needs_a_token_like_everything_else() {
    let (app, handle) = test_app_with(test_sse(256), EventRing::new());
    stub_attachable_session(&handle, "sess-1");

    let (status, body) = call(&app, get("/v1/sessions/sess-1/events")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");
    assert!(handle.outbound_methods().is_empty());
}

#[tokio::test]
async fn the_ring_is_filled_from_the_live_fan_out_so_a_reconnect_resumes_from_memory() {
    // The end-to-end shape of a dropped tunnel: a phone streams, the connection dies, the leader
    // keeps talking, the phone comes back with the last id it saw. Nothing here pre-loads the ring
    // — `spawn_event_pump` fills it from the same broadcast the connection reads.
    let (app, handle) = test_app_with(test_sse(256), EventRing::new());
    stub_attachable_session(&handle, "sess-1");

    let (status, _, mut first) = SseStream::open(&app, events_request("sess-1", None)).await;
    assert_eq!(status, StatusCode::OK);
    for counter in [10, 11, 12] {
        handle.push(live_update("sess-1", counter));
    }
    // Read them on the live connection, which also gives the pump time to file the same frames.
    assert_eq!(
        ids(&first.next_frames(3).await),
        vec![
            Some("sess-1-10".into()),
            Some("sess-1-11".into()),
            Some("sess-1-12".into())
        ]
    );
    drop(first);

    let (status, _, mut resumed) =
        SseStream::open(&app, events_request("sess-1", Some("sess-1-10"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ids(&resumed.next_frames(2).await),
        vec![Some("sess-1-11".into()), Some("sess-1-12".into())],
        "the frames that arrived while nobody was connected are exactly what a resume is for"
    );
    resumed.expect_quiet().await;

    // Purely from memory: the session was never re-read off disk.
    assert!(
        !handle
            .outbound_methods()
            .contains(&"_x.ai/session/updates".to_string()),
        "{:?}",
        handle.outbound_methods()
    );
    // And it attached exactly once across both connections.
    assert_eq!(
        handle
            .outbound_methods()
            .iter()
            .filter(|m| *m == "session/load")
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// Approvals — the reverse-request as state a phone can answer later
// ---------------------------------------------------------------------------

/// A resident `session_id` in `activity` that attaches cleanly and accepts every write verb.
///
/// The approval routes attach exactly like `/history` does, so they need the same roster row and
/// `session/load`; the write stubs are here because the interject/cancel assertions share it.
fn stub_interactive_session(handle: &FakeLinkHandle, session_id: &str, activity: &str) {
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [roster_row(session_id, activity)] }),
    );
    handle.respond_ok("session/load", json!({}));
    handle.never_respond("session/prompt");
    handle.respond_ext_ok("x.ai/interject", json!({ "status": "queued" }));
    handle.respond_ok(
        "x.ai/session/updates",
        json!({ "updates": [], "totalCount": 0, "hasMore": false }),
    );
}

/// One interaction, as the agent really puts it on the wire, with an answer of the right shape.
struct Interaction {
    kind: &'static str,
    /// Wire spelling: the three ext methods are underscored, `session/request_permission` is not.
    wire_method: &'static str,
    /// Everything but `sessionId`, which [`Interaction::request`] adds.
    params: Value,
    /// The `response` object a client POSTs, per that method's response type.
    answer: Value,
}

impl Interaction {
    fn request(&self, session_id: &str, rpc_id: i64) -> Value {
        let mut params = self.params.clone();
        params["sessionId"] = json!(session_id);
        json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": self.wire_method,
            "params": params,
        })
    }
}

/// All four, keyed on `tc-1` throughout. Shapes taken from the types that deserialize them:
/// `RequestPermissionResponse` (ACP schema), `AskUserQuestionExtResponse`,
/// `ExitPlanModeExtResponse` and `McpElicitExtResponse`.
fn interactions() -> Vec<Interaction> {
    vec![
        Interaction {
            kind: "permission",
            wire_method: "session/request_permission",
            params: json!({
                // The only one of the four that nests its tool call id.
                "toolCall": { "toolCallId": "tc-1", "title": "rm -rf build/" },
                "options": [
                    { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                    { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" },
                ],
            }),
            answer: json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
        },
        Interaction {
            kind: "question",
            wire_method: "_x.ai/ask_user_question",
            params: json!({
                "toolCallId": "tc-1",
                "questions": [{ "id": "q1", "question": "Which database?" }],
                "mode": "plan",
            }),
            answer: json!({ "outcome": "accepted", "answers": { "q1": ["Postgres"] } }),
        },
        Interaction {
            kind: "plan_approval",
            wire_method: "_x.ai/exit_plan_mode",
            params: json!({ "toolCallId": "tc-1", "planContent": "# Plan\n1. Do it" }),
            // "approved", never "approve".
            answer: json!({ "outcome": "approved" }),
        },
        Interaction {
            kind: "mcp_elicitation",
            wire_method: "_x.ai/mcp/elicit",
            params: json!({
                "toolCallId": "tc-1",
                "serverName": "files",
                "message": "Which mailbox?",
                "mode": "form",
                "requestedSchema": { "type": "object" },
            }),
            answer: json!({ "outcome": "accept", "content": { "email": "me@example.com" } }),
        },
    ]
}

/// The `x.ai/session_notification` the agent broadcasts when an interaction closes.
///
/// Snake-case `tool_call_id` because that is what `SessionUpdate`'s `rename_all = "snake_case"`
/// actually emits for the *variant's fields* — the leader's own extractor reads it that way too.
fn interaction_resolved(session_id: &str, tool_call_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/session_notification",
        "params": {
            "sessionId": session_id,
            "update": { "sessionUpdate": "interaction_resolved", "tool_call_id": tool_call_id },
        },
    })
}

fn pending_interaction(session_id: &str, tool_call_id: &str, kind: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/session_notification",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "pending_interaction",
                "tool_call_id": tool_call_id,
                "kind": kind,
            },
        },
    })
}

/// Wait for the JSON-RPC **response** the lane put on the link for reverse-request `rpc_id`.
///
/// Matched on "has that id and no `method`", never on the id alone: this lane numbers its own
/// outbound *requests* from 1, so a small reverse-request id would otherwise match one of them.
/// And it has to wait: `submit` queues the payload for the task that owns the link, so the POST can
/// answer before the leader has seen anything.
async fn wait_for_response(handle: &FakeLinkHandle, rpc_id: i64) -> Value {
    tokio::time::timeout(FRAME_TIMEOUT, async {
        loop {
            if let Some(payload) = response_with_id(handle, rpc_id) {
                return payload;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the lane never answered reverse-request {rpc_id}"))
}

/// The response the lane sent for `rpc_id`, if it has sent one.
fn response_with_id(handle: &FakeLinkHandle, rpc_id: i64) -> Option<Value> {
    handle.outbound().into_iter().find(|payload| {
        payload.get("method").is_none() && payload.get("id").and_then(Value::as_i64) == Some(rpc_id)
    })
}

/// The next frame of type `event`, skipping the ones this assertion is not about.
///
/// `interaction_resolved` is both an approval change *and* an ordinary session notification, so a
/// stream sees an `update` frame for it as well as the `approval` frame; which lands first is not
/// something a client should depend on either.
async fn next_frame_named(stream: &mut SseStream, event: &str) -> SseFrame {
    for _ in 0..8 {
        let frame = stream.next_frame().await;
        if frame.is(event) {
            return frame;
        }
    }
    panic!("no `event: {event}` frame arrived");
}

fn approvals_uri(session_id: &str) -> String {
    format!("/v1/sessions/{session_id}/approvals")
}

/// `GET …/approvals` with the crate's token, asserting 200.
async fn list_approvals(app: &Router, session_id: &str) -> Value {
    let (status, body) = call(app, authed_get(&approvals_uri(session_id))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// Poll `…/approvals` until `tool_call_id` reads `status`, or fail with what it did read.
///
/// Needed only for the notification-driven transitions: those travel through the broadcast and the
/// event pump's own task, so unlike a pushed reverse-request they are not ordered against the next
/// HTTP call by the link task alone.
async fn wait_for_status(
    app: &Router,
    session_id: &str,
    tool_call_id: &str,
    status: &str,
) -> Value {
    let mut last = Value::Null;
    let deadline = tokio::time::Instant::now() + FRAME_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        last = list_approvals(app, session_id).await;
        let found = last["approvals"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == tool_call_id))
            .cloned();
        if let Some(found) = found
            && found["status"] == status
        {
            return found;
        }
        tokio::task::yield_now().await;
    }
    panic!("approval {tool_call_id} never reached {status}: {last}");
}

/// The full life of an approval, for each of the four kinds: the request arrives on the link, the
/// GET routes show it, the POST puts a correctly-shaped JSON-RPC response back on the link, and the
/// agent's `interaction_resolved` — not our own POST — is what closes it.
#[tokio::test]
async fn every_interaction_kind_is_capturable_answerable_and_resolvable() {
    for interaction in interactions() {
        let (app, handle) = test_app();
        stub_interactive_session(&handle, "sess-1", "working");

        // Nothing is held before the lane has attached, so the first call is what attaches.
        let body = list_approvals(&app, "sess-1").await;
        assert_eq!(body["approvals"], json!([]), "{}", interaction.kind);

        handle.push(interaction.request("sess-1", 9077));

        // Pending, with the request verbatim and the kind the leader would have named.
        let body = list_approvals(&app, "sess-1").await;
        let row = &body["approvals"][0];
        assert_eq!(row["id"], "tc-1", "{}", interaction.kind);
        assert_eq!(row["sessionId"], "sess-1", "{}", interaction.kind);
        assert_eq!(row["kind"], interaction.kind);
        assert_eq!(row["status"], "pending", "{}", interaction.kind);
        assert_eq!(
            row["method"],
            crate::acp_client::logical_method(interaction.wire_method),
            "{}",
            interaction.kind
        );
        assert!(row["createdAt"].is_i64(), "{}", interaction.kind);

        // …and addressable one at a time.
        let (status, single) = call(
            &app,
            authed_get(&format!("{}/tc-1", approvals_uri("sess-1"))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", interaction.kind);
        assert_eq!(single, *row, "{}", interaction.kind);

        // The answer goes out as a JSON-RPC *response*: the agent's own id, the body verbatim as
        // `result`, and no method at all.
        let (status, body) = call(
            &app,
            authed_post(
                &format!("{}/tc-1", approvals_uri("sess-1")),
                json!({ "response": interaction.answer }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{}: {body}", interaction.kind);
        assert_eq!(body["status"], "submitted", "{}", interaction.kind);

        let answer = wait_for_response(&handle, 9077).await;
        assert_eq!(answer["jsonrpc"], "2.0", "{}", interaction.kind);
        assert_eq!(answer["result"], interaction.answer, "{}", interaction.kind);
        assert!(answer.get("method").is_none(), "{}", interaction.kind);
        assert!(answer.get("error").is_none(), "{}", interaction.kind);

        // Submitted is not resolved: the agent acknowledges no individual answer.
        let row = &list_approvals(&app, "sess-1").await["approvals"][0];
        assert_eq!(row["status"], "submitted", "{}", interaction.kind);
        assert!(row["submittedAt"].is_i64(), "{}", interaction.kind);
        assert!(row.get("resolvedAt").is_none(), "{}", interaction.kind);

        handle.push(interaction_resolved("sess-1", "tc-1"));
        let row = wait_for_status(&app, "sess-1", "tc-1", "resolved").await;
        assert!(row["resolvedAt"].is_i64(), "{}", interaction.kind);
    }
}

#[tokio::test]
async fn a_tui_that_answers_first_leaves_the_phone_a_409() {
    let (app, handle) = test_app();
    stub_interactive_session(&handle, "sess-1", "working");
    list_approvals(&app, "sess-1").await;

    handle.push(interactions()[0].request("sess-1", 9005));
    list_approvals(&app, "sess-1").await;
    // The TUI answered on its own connection; all this lane ever sees is the resolution.
    handle.push(interaction_resolved("sess-1", "tc-1"));
    wait_for_status(&app, "sess-1", "tc-1", "resolved").await;

    let (status, body) = call(
        &app,
        authed_post(
            &format!("{}/tc-1", approvals_uri("sess-1")),
            json!({ "response": { "outcome": { "outcome": "cancelled" } } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "already_resolved");
    assert!(
        response_with_id(&handle, 9005).is_none(),
        "answering a resolved interaction would be a stale write"
    );
}

#[tokio::test]
async fn a_second_post_is_a_409_and_is_not_sent_twice() {
    let (app, handle) = test_app();
    stub_interactive_session(&handle, "sess-1", "working");
    list_approvals(&app, "sess-1").await;
    handle.push(interactions()[0].request("sess-1", 9005));
    list_approvals(&app, "sess-1").await;

    let answer =
        json!({ "response": { "outcome": { "outcome": "selected", "optionId": "allow-once" } } });
    let uri = format!("{}/tc-1", approvals_uri("sess-1"));
    let (status, _) = call(&app, authed_post(&uri, answer.clone())).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, body) = call(&app, authed_post(&uri, answer)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "already_submitted");
    // The first answer is on the wire…
    wait_for_response(&handle, 9005).await;
    // …and exactly once.
    assert_eq!(
        handle
            .outbound()
            .iter()
            .filter(|payload| payload.get("method").is_none()
                && payload.get("id").and_then(Value::as_i64) == Some(9005))
            .count(),
        1,
        "the agent must never see two answers from this lane"
    );
}

#[tokio::test]
async fn a_body_that_is_not_an_object_is_a_400_before_the_lookup() {
    let (app, handle) = test_app();
    stub_interactive_session(&handle, "sess-1", "working");
    list_approvals(&app, "sess-1").await;
    handle.push(interactions()[0].request("sess-1", 9005));
    list_approvals(&app, "sess-1").await;

    let uri = format!("{}/tc-1", approvals_uri("sess-1"));
    for body in [
        json!({ "response": "allow-once" }),
        json!({ "response": ["allow-once"] }),
        json!({ "response": null }),
        // `response` is required: an empty body is not "answer with nothing".
        json!({}),
    ] {
        let (status, answer) = call(&app, authed_post(&uri, body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(answer["error"], "bad_request", "{body}");
    }
    assert!(
        response_with_id(&handle, 9005).is_none(),
        "a malformed answer must not reach the agent, which would cancel the tool call over it"
    );
    // Still answerable afterwards.
    assert_eq!(
        list_approvals(&app, "sess-1").await["approvals"][0]["status"],
        "pending"
    );
}

#[tokio::test]
async fn an_unknown_tool_call_id_is_404_on_both_verbs() {
    let (app, handle) = test_app();
    stub_interactive_session(&handle, "sess-1", "working");

    let (status, body) = call(
        &app,
        authed_get(&format!("{}/tc-nope", approvals_uri("sess-1"))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_approval");
    assert!(
        body["message"].as_str().unwrap().contains("sess-1"),
        "{body}"
    );

    let (status, body) = call(
        &app,
        authed_post(
            &format!("{}/tc-nope", approvals_uri("sess-1")),
            json!({ "response": { "outcome": "approved" } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_approval");

    // And a session that does not exist is still `unknown_session`, resolved before approvals.
    let (app, handle) = test_app();
    stub_no_such_session(&handle);
    let (status, body) = call(&app, authed_get(&approvals_uri("sess-9"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_session");
}

#[tokio::test]
async fn a_hint_that_arrives_before_its_request_becomes_one_approval_not_two() {
    let (app, handle) = test_app();
    stub_interactive_session(&handle, "sess-1", "working");
    list_approvals(&app, "sess-1").await;

    // `PendingInteractionGuard::new` broadcasts this *before* the gateway sends the request.
    handle.push(pending_interaction("sess-1", "tc-1", "plan_approval"));
    let row = wait_for_status(&app, "sess-1", "tc-1", "pending").await;
    assert_eq!(row["kind"], "plan_approval", "the hint carries the kind");
    assert_eq!(row["method"], Value::Null, "…but not the request");
    assert_eq!(row["request"], Value::Null);

    // Answering a placeholder is a retry-later: there is no id to respond to yet.
    let uri = format!("{}/tc-1", approvals_uri("sess-1"));
    let (status, body) = call(
        &app,
        authed_post(&uri, json!({ "response": { "outcome": "approved" } })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "leader_unavailable");

    handle.push(interactions()[2].request("sess-1", 9021));
    let body = list_approvals(&app, "sess-1").await;
    assert_eq!(
        body["approvals"].as_array().unwrap().len(),
        1,
        "the request must merge into the hint's entry, not add another: {body}"
    );
    assert_eq!(body["approvals"][0]["method"], "x.ai/exit_plan_mode");
    assert_eq!(body["approvals"][0]["createdAt"], row["createdAt"]);

    let (status, _) = call(
        &app,
        authed_post(&uri, json!({ "response": { "outcome": "approved" } })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "answerable once it has arrived"
    );
}

#[tokio::test]
async fn a_held_approval_denies_an_interject_a_stale_roster_would_have_allowed() {
    let (app, handle) = test_app();
    // The roster still says `working`: it lags the turn boundary by up to one broadcast, and this
    // is exactly the window `effective_activity` exists to close.
    stub_interactive_session(&handle, "sess-1", "working");
    list_approvals(&app, "sess-1").await;
    handle.push(interactions()[0].request("sess-1", 9005));
    list_approvals(&app, "sess-1").await;

    let (status, body) = call(
        &app,
        authed_post(
            "/v1/sessions/sess-1/messages",
            json!({ "text": "hi", "mode": "interject" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_accepting");
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("needs_input"), "{message}");
    assert!(message.contains("pending approval"), "{message}");

    // The two verbs `needs_input` does admit still work.
    for request in [
        authed_post("/v1/sessions/sess-1/messages", json!({ "text": "queued" })),
        authed_post("/v1/sessions/sess-1/cancel", json!({})),
    ] {
        let (status, body) = call(&app, request).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    }
}

#[tokio::test]
async fn an_attached_sessions_pending_count_is_exact_while_an_unattached_ones_is_not() {
    let (app, handle) = test_app();
    stub_interactive_session(&handle, "sess-1", "needs_input");

    // Never touched by this lane: all it can do is render the roster's bit, and say so.
    let (status, body) = call(&app, authed_get("/v1/sessions")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessions"][0]["pendingApprovals"], 1);
    assert_eq!(body["sessions"][0]["approximate"], true);
    assert_eq!(body["sessions"][0]["attached"], false);

    // Attached, holding nothing: an exact zero, even though the roster still says needs_input.
    list_approvals(&app, "sess-1").await;
    let (_, body) = call(&app, authed_get("/v1/sessions/sess-1")).await;
    assert_eq!(body["pendingApprovals"], 0);
    assert_eq!(body["approximate"], false);
    assert_eq!(body["attached"], true);

    // Two at once — a count the roster's single bit could not have expressed.
    handle.push(interactions()[0].request("sess-1", 9005));
    handle.push(interactions()[1].request("sess-2-unrelated", 9006));
    let mut second = interactions()[1].request("sess-1", 9007);
    second["params"]["toolCallId"] = json!("tc-2");
    handle.push(second);
    list_approvals(&app, "sess-1").await;

    let (_, body) = call(&app, authed_get("/v1/sessions/sess-1")).await;
    assert_eq!(body["pendingApprovals"], 2);
    assert_eq!(body["approximate"], false);
}

#[tokio::test]
async fn an_approval_reaches_the_event_stream_without_an_id_line() {
    let (app, handle) = test_app_with(test_sse(256), EventRing::new());
    stub_interactive_session(&handle, "sess-1", "working");

    let (status, _, mut stream) = SseStream::open(&app, events_request("sess-1", None)).await;
    assert_eq!(status, StatusCode::OK);

    handle.push(interactions()[0].request("sess-1", 9005));
    let frame = next_frame_named(&mut stream, "approval").await;
    assert_eq!(
        frame.id, None,
        "an approval is a state invalidation, not a position a browser may resume from"
    );
    let approval = frame.json();
    assert_eq!(approval["id"], "tc-1");
    assert_eq!(approval["status"], "pending");
    assert_eq!(approval["kind"], "permission");

    // The same resource shape the GET returns, so a client parses one thing.
    assert_eq!(
        approval,
        list_approvals(&app, "sess-1").await["approvals"][0]
    );

    // Answering and resolving are changes too.
    let (status, _) = call(
        &app,
        authed_post(
            &format!("{}/tc-1", approvals_uri("sess-1")),
            json!({ "response": { "outcome": { "outcome": "selected", "optionId": "allow-once" } } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let frame = next_frame_named(&mut stream, "approval").await;
    assert_eq!(frame.json()["status"], "submitted");
    assert_eq!(frame.id, None);

    handle.push(interaction_resolved("sess-1", "tc-1"));
    let frame = next_frame_named(&mut stream, "approval").await;
    assert_eq!(frame.json()["status"], "resolved");
    assert_eq!(frame.id, None);
}

#[tokio::test]
async fn another_sessions_approval_is_not_this_streams_business() {
    let (app, handle) = test_app_with(test_sse(256), EventRing::new());
    handle.respond_ext_ok(
        "x.ai/sessions/list",
        json!({ "sessions": [roster_row("sess-1", "working"), roster_row("sess-2", "working")] }),
    );
    handle.respond_ok("session/load", json!({}));
    handle.respond_ok(
        "x.ai/session/updates",
        json!({ "updates": [], "totalCount": 0, "hasMore": false }),
    );

    // Attach both, so the store would capture either one.
    list_approvals(&app, "sess-1").await;
    list_approvals(&app, "sess-2").await;

    let (_, _, mut stream) = SseStream::open(&app, events_request("sess-1", None)).await;
    handle.push(interactions()[0].request("sess-2", 9005));
    stream.expect_quiet().await;

    // It was captured — just not for this stream.
    assert_eq!(
        list_approvals(&app, "sess-2").await["approvals"][0]["id"],
        "tc-1"
    );
}
