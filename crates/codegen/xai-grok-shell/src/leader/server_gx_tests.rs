//! gx: tests for the `ClientCapabilities::observer` capability.
//!
//! An observer is the in-process ACP client the gx remote lane (HTTP/SSE) attaches to the leader
//! with. It lives for the leader's whole life and must be invisible to the TUI's routing: it never
//! becomes a session driver, never becomes the machine-wide fallback target (`last_active_client`),
//! never keeps the leader alive past the last real client, and never overwrites the shared
//! session's capability meta. It IS a normal subscriber (fan-out, interaction replay, residency).
//!
//! These live in a gx-owned file so upstream's `server_tests.rs` never conflicts on a rebase.
//! The helpers below are deliberate copies of `server_tests.rs`'s: those are private to a sibling
//! module, so they are not reachable from here.
//!
//! Where an assertion could pass for the wrong reason under a race, the test uses a real barrier
//! instead of a fixed sleep: a forwarded request observed on the agent side, a payload observed on
//! a client, or `client_count` (bumped inside the main loop's own `Registered`/`Disconnected` arms).
//! Negative assertions are paired with a positive control so "nothing arrived" can never be the
//! whole test.
use std::time::Duration;

use super::*;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness (copies of the `server_tests.rs` helpers; see the module comment)
// ---------------------------------------------------------------------------

/// Bound on the registration handshake. A registration regression must fail this suite, not hang it.
const GX_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on a `client_count` barrier (`wait_for_client_count`).
const GX_BARRIER_TIMEOUT: Duration = Duration::from_secs(2);

/// Spawn a real leader server on a temp socket.
/// Returns the socket path, the cancel token, the agent-side response injector, the agent-side
/// receiver of forwarded client traffic, the leader's registered-client counter (a barrier on the
/// main loop having processed a registration or a disconnect), and the server task handle, whose
/// join value is `run_leader_server`'s own `Result` so a test can assert a *clean* exit.
async fn spawn_server(
    temp: &TempDir,
    no_exit_on_disconnect: bool,
) -> (
    PathBuf,
    CancellationToken,
    mpsc::UnboundedSender<String>,
    mpsc::UnboundedReceiver<String>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<Result<(), ServerError>>,
) {
    let sock_path = temp.path().join("gx-observer.sock");
    let (acp_tx, acp_rx) = mpsc::unbounded_channel();
    let (response_tx, response_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let control_state = default_test_control_state(&sock_path);
    let client_count = Arc::new(AtomicUsize::new(0));

    let sock_clone = sock_path.clone();
    let cancel_clone = cancel.clone();
    let client_count_clone = client_count.clone();
    let handle = tokio::spawn(async move {
        run_leader_server(
            sock_clone,
            acp_tx,
            response_rx,
            cancel_clone,
            no_exit_on_disconnect,
            client_count_clone,
            Arc::new(AtomicBool::new(false)),
            AgentActivity::default(),
            watch::channel(true).1,
            watch::channel(false).0,
            watch::channel(super::super::protocol::ShutdownReason::Manual).0,
            None,
            control_state,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (sock_path, cancel, response_tx, acp_rx, client_count, handle)
}

/// Barrier: wait until the leader's main loop reports exactly `expected` registered clients.
/// `client_count` is bumped inside the `Registered` arm and decremented at the top of the
/// `Disconnected` arm, so this proves the main loop actually ran that arm — a fixed sleep does not.
async fn wait_for_client_count(client_count: &Arc<AtomicUsize>, expected: usize) {
    let deadline = tokio::time::Instant::now() + GX_BARRIER_TIMEOUT;
    loop {
        let seen = client_count.load(Ordering::Relaxed);
        if seen == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the leader to report {expected} registered clients (saw {seen})"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Capabilities for a gx remote-lane observer: every per-client flag a TUI would set is ALSO set
/// here, so a test that sees none of them injected has proven the observer branch, not a default.
/// `yolo_mode` is left off deliberately: `autoMode` is only injected when yolo is NOT set, and
/// `yoloMode` only applies to `session/new`, so `auto_mode` is the flag that exercises both paths.
fn observer_caps() -> ClientCapabilities {
    ClientCapabilities {
        observer: true,
        yolo_mode: false,
        auto_mode: true,
        default_model: Some("gx-remote-model".to_string()),
        code_nav_enabled: true,
        terminal: true,
        fs_read: true,
        fs_write: true,
        status_line: true,
        ..Default::default()
    }
}

/// The same flags with `observer: false`: the control for the injection test.
fn tui_caps() -> ClientCapabilities {
    ClientCapabilities {
        observer: false,
        ..observer_caps()
    }
}

/// Register a client, returning the split stream and the leader's `Registered` acknowledgement.
/// The whole handshake (connect, write, ack) is bounded: a hung registration must fail loudly here
/// rather than stalling the suite on an unrelated regression.
async fn register_capturing(
    sock_path: &std::path::Path,
    client_type: &str,
    capabilities: ClientCapabilities,
) -> (
    tokio::io::ReadHalf<LeaderStream>,
    tokio::io::WriteHalf<LeaderStream>,
    ServerMessage,
) {
    let (reader, writer, msg) = tokio::time::timeout(GX_REGISTRATION_TIMEOUT, async {
        let stream = LeaderStream::connect(sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: client_type.into(),
                mode: ClientMode::Stdio,
                capabilities,
            },
        )
        .await
        .unwrap();
        let msg: ServerMessage = read_message(&mut reader).await.unwrap();
        (reader, writer, msg)
    })
    .await
    .expect("registration timed out");
    assert!(
        matches!(msg, ServerMessage::Registered { .. }),
        "expected Registered, got {msg:?}"
    );
    (reader, writer, msg)
}

async fn register_with(
    sock_path: &std::path::Path,
    client_type: &str,
    capabilities: ClientCapabilities,
) -> (
    tokio::io::ReadHalf<LeaderStream>,
    tokio::io::WriteHalf<LeaderStream>,
) {
    let (reader, writer, _ack) = register_capturing(sock_path, client_type, capabilities).await;
    (reader, writer)
}

/// Read the next `ServerMessage::Acp` payload for a client, ignoring other frames, with a deadline.
async fn next_acp_payload(reader: &mut tokio::io::ReadHalf<LeaderStream>) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
    loop {
        // Saturate explicitly rather than leaning on `Sub`'s saturation: an expired deadline is
        // the normal exit from this loop (that is what the `is_zero()` guard below is for), so
        // the subtraction must never be the thing that decides what happens on it.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, read_message::<_, ServerMessage>(reader)).await {
            Ok(Ok(ServerMessage::Acp { payload })) => return Some(payload),
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => return None,
        }
    }
}

/// Drain a few payloads looking for one containing `needle`; `None` powers "must NOT receive".
async fn next_acp_payload_matching(
    reader: &mut tokio::io::ReadHalf<LeaderStream>,
    needle: &str,
) -> Option<String> {
    for _ in 0..8 {
        match next_acp_payload(reader).await {
            Some(p) if p.contains(needle) => return Some(p),
            Some(_) => continue,
            None => return None,
        }
    }
    None
}

async fn send_acp(writer: &mut tokio::io::WriteHalf<LeaderStream>, payload: String) {
    write_message(writer, &ClientMessage::Acp { payload })
        .await
        .unwrap();
}

async fn load_session(writer: &mut tokio::io::WriteHalf<LeaderStream>, session_id: &str) {
    send_acp(
        writer,
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/load","id":1,"params":{{"sessionId":"{session_id}"}}}}"#
        ),
    )
    .await;
}

/// Wait for the leader to forward a request whose method is `method`, returning it parsed.
async fn next_forwarded(
    acp_rx: &mut mpsc::UnboundedReceiver<String>,
    method: &str,
) -> serde_json::Value {
    loop {
        let forwarded = tokio::time::timeout(Duration::from_secs(2), acp_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for a forwarded {method}"))
            .expect("agent channel closed");
        let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
        if json.get("method").and_then(|m| m.as_str()) == Some(method) {
            return json;
        }
    }
}

/// Barrier: wait until the leader has forwarded the request `client_request_id` of `method`.
/// The leader namespaces a client request id to `"<client id>|<original id JSON>"`, so the suffix
/// identifies which client's request this is. Seeing it on the agent side proves the main loop has
/// finished the `Message` arm for that payload (subscription, driver claim and `last_active_client`
/// are all decided before the forward), which a fixed sleep only assumes.
async fn wait_forwarded_request(
    acp_rx: &mut mpsc::UnboundedReceiver<String>,
    method: &str,
    client_request_id: u64,
) -> serde_json::Value {
    let suffix = format!("{ID_NAMESPACE_SEP}{client_request_id}");
    loop {
        let json = next_forwarded(acp_rx, method).await;
        if json
            .get("id")
            .and_then(|id| id.as_str())
            .is_some_and(|id| id.ends_with(&suffix))
        {
            return json;
        }
    }
}

/// Complete an in-flight `session/load`, optionally naming a session id in the result.
/// Completing the load matters: the leader buffers live traffic to a loading client until then,
/// and the interaction replay fires off the load response.
async fn complete_load(
    acp_rx: &mut mpsc::UnboundedReceiver<String>,
    response_tx: &mpsc::UnboundedSender<String>,
    result_session_id: Option<&str>,
) -> serde_json::Value {
    let forwarded = next_forwarded(acp_rx, "session/load").await;
    let id = forwarded.get("id").cloned().unwrap();
    let result = match result_session_id {
        Some(sid) => serde_json::json!({ "models": [], "sessionId": sid }),
        None => serde_json::json!({ "models": [] }),
    };
    response_tx
        .send(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}).to_string())
        .unwrap();
    forwarded
}

/// A reverse-request the leader routes to the session DRIVER only (non-interaction, has an id).
fn driver_only_request(id: u64, session_id: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"fs/read_text_file","params":{{"sessionId":"{session_id}","path":"/tmp/x"}}}}"#
    )
}

fn meta_of(request: &serde_json::Value) -> &serde_json::Map<String, serde_json::Value> {
    request
        .get("params")
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.as_object())
        .expect("forwarded session request must carry a _meta object")
}

// ---------------------------------------------------------------------------
// 1. An observer subscribes but never becomes the driver
// ---------------------------------------------------------------------------

/// An observer's `session/load` must subscribe it (it needs the fan-out) without claiming the
/// driver slot. With only the observer attached the session is DRIVERLESS, so a driver-only
/// reverse-request drops rather than being answered by the phone; a real client that attaches
/// afterwards becomes the driver even though the observer got there first.
#[tokio::test]
async fn observer_load_subscribes_but_never_becomes_driver() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let (mut obs_reader, mut obs_writer) =
        register_with(&sock_path, "gx-remote-api", observer_caps()).await;
    load_session(&mut obs_writer, "sess-obs").await;
    complete_load(&mut acp_rx, &response_tx, None).await;
    // The load response is the barrier: the leader subscribes the client and decides the driver
    // slot in the same arm, *before* handing the response back.
    let _ = next_acp_payload(&mut obs_reader).await;

    // The observer IS a subscriber: a plain session notification reaches it.
    response_tx
        .send(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-obs","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"OBS_FANOUT"}}}}"#.into())
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut obs_reader, "OBS_FANOUT")
            .await
            .is_some(),
        "an observer must still receive session fan-out"
    );

    // But it is NOT the driver: the session has no driver at all, so this drops.
    response_tx
        .send(driver_only_request(11, "sess-obs"))
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut obs_reader, "read_text_file")
            .await
            .is_none(),
        "a driver-only reverse-request must NOT be routed to an observer"
    );

    // A real client attaches second and becomes the driver anyway.
    let (mut tui_reader, mut tui_writer) =
        register_with(&sock_path, "grok-tui", ClientCapabilities::default()).await;
    load_session(&mut tui_writer, "sess-obs").await;
    complete_load(&mut acp_rx, &response_tx, None).await;
    let _ = next_acp_payload(&mut tui_reader).await;

    response_tx
        .send(driver_only_request(12, "sess-obs"))
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut tui_reader, "read_text_file")
            .await
            .is_some(),
        "the non-observer client must become the driver even though the observer attached first"
    );
    assert!(
        next_acp_payload_matching(&mut obs_reader, "read_text_file")
            .await
            .is_none(),
        "the observer must never receive a driver-only reverse-request"
    );

    cancel.cancel();
}

/// The response path subscribes a client to the session id its `session/load` result names.
/// That path must skip the driver insert for an observer too, or the phone would claim the driver
/// slot for a brand-new session whose id it only learns from the response.
#[tokio::test]
async fn observer_never_becomes_driver_from_a_load_response() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let (mut obs_reader, mut obs_writer) =
        register_with(&sock_path, "gx-remote-api", observer_caps()).await;
    // The request names no session; the RESPONSE does, which is what subscribes the client.
    send_acp(
        &mut obs_writer,
        r#"{"jsonrpc":"2.0","method":"session/load","id":5,"params":{}}"#.to_string(),
    )
    .await;
    complete_load(&mut acp_rx, &response_tx, Some("sess-from-result")).await;
    let _ = next_acp_payload(&mut obs_reader).await; // the load response: subscription decided

    // Subscribed (fan-out arrives) ...
    response_tx
        .send(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-from-result","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"RESULT_FANOUT"}}}}"#.into())
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut obs_reader, "RESULT_FANOUT")
            .await
            .is_some(),
        "the response path must still subscribe an observer"
    );
    // ... but not the driver.
    response_tx
        .send(driver_only_request(21, "sess-from-result"))
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut obs_reader, "read_text_file")
            .await
            .is_none(),
        "the response path must NOT make an observer the driver"
    );

    // Positive control for that negative: a NON-observer learning the same session id the same way
    // (from its load result) does claim the driver slot, and a second driver-only request reaches
    // it. Without this, a regression that dropped every driver-only message would pass the test
    // above for entirely the wrong reason.
    let (mut tui_reader, mut tui_writer) =
        register_with(&sock_path, "grok-tui", ClientCapabilities::default()).await;
    send_acp(
        &mut tui_writer,
        r#"{"jsonrpc":"2.0","method":"session/load","id":6,"params":{}}"#.to_string(),
    )
    .await;
    complete_load(&mut acp_rx, &response_tx, Some("sess-from-result")).await;
    let _ = next_acp_payload(&mut tui_reader).await;

    response_tx
        .send(driver_only_request(22, "sess-from-result"))
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut tui_reader, "read_text_file")
            .await
            .is_some(),
        "the response path must still make a non-observer the driver (driver-only dispatch works)"
    );
    assert!(
        next_acp_payload_matching(&mut obs_reader, "read_text_file")
            .await
            .is_none(),
        "the observer must never receive a driver-only reverse-request"
    );

    cancel.cancel();
}

// ---------------------------------------------------------------------------
// 2. An observer never becomes `last_active_client`
// ---------------------------------------------------------------------------

/// A sessionless notification the leader cannot route falls back to the last active Stdio client.
/// An observer's traffic must not steal that slot, or the TUI would silently stop seeing them.
#[tokio::test]
async fn observer_traffic_does_not_steal_last_active_client() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let (mut tui_reader, mut tui_writer) =
        register_with(&sock_path, "grok-tui", ClientCapabilities::default()).await;
    send_acp(
        &mut tui_writer,
        r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{}}"#.to_string(),
    )
    .await;
    // Barrier, not a sleep: the leader sets `last_active_client` and then forwards, so seeing the
    // forwarded `initialize` proves the TUI has actually taken the slot.
    wait_forwarded_request(&mut acp_rx, "initialize", 1).await;

    // The observer connects LAST and talks: upstream's rule would make it the fallback target.
    let (mut obs_reader, mut obs_writer) =
        register_with(&sock_path, "gx-remote-api", observer_caps()).await;
    send_acp(
        &mut obs_writer,
        r#"{"jsonrpc":"2.0","method":"initialize","id":2,"params":{}}"#.to_string(),
    )
    .await;
    // Same barrier for the observer: its `initialize` is provably processed before the probe, so
    // "the TUI still has the slot" cannot be an artifact of the probe simply arriving first.
    wait_forwarded_request(&mut acp_rx, "initialize", 2).await;

    // A sessionless, non-machine-wide notification: routed by fallback only.
    response_tx
        .send(r#"{"jsonrpc":"2.0","method":"x.ai/unroutable_probe","params":{"marker":"FALLBACK_PROBE"}}"#.into())
        .unwrap();

    assert!(
        next_acp_payload_matching(&mut tui_reader, "FALLBACK_PROBE")
            .await
            .is_some(),
        "the fallback notification must go to the last non-observer Stdio client"
    );
    assert!(
        next_acp_payload_matching(&mut obs_reader, "FALLBACK_PROBE")
            .await
            .is_none(),
        "an observer must never be the machine-wide fallback target"
    );

    cancel.cancel();
}

// ---------------------------------------------------------------------------
// 3. Driver reassignment on disconnect skips observers
// ---------------------------------------------------------------------------

/// When the driver disconnects and only an observer is left subscribed, the driver slot is CLEARED
/// rather than handed to the observer. The session stays subscribed (no `EvictSessions`), so it
/// stays resident for the handoff, but driver-only reverse-requests drop instead of reaching the
/// phone with a modal the TUI owns.
#[tokio::test]
async fn driver_reassignment_on_disconnect_skips_observers() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, client_count, _srv) =
        spawn_server(&temp, true).await;

    let (mut tui_reader, mut tui_writer) =
        register_with(&sock_path, "grok-tui", ClientCapabilities::default()).await;
    load_session(&mut tui_writer, "sess-xfer").await;
    complete_load(&mut acp_rx, &response_tx, None).await;

    let (mut obs_reader, mut obs_writer) =
        register_with(&sock_path, "gx-remote-api", observer_caps()).await;
    load_session(&mut obs_writer, "sess-xfer").await;
    complete_load(&mut acp_rx, &response_tx, None).await;
    let _ = next_acp_payload(&mut obs_reader).await;

    // Positive control BEFORE the disconnect: the TUI really is the driver, so the post-disconnect
    // "nothing is routed" assertion is about reassignment and not about a broken dispatch path.
    response_tx
        .send(driver_only_request(30, "sess-xfer"))
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut tui_reader, "read_text_file")
            .await
            .is_some(),
        "the first non-observer client must hold the driver slot before it disconnects"
    );

    // Drain everything the leader has forwarded so far, so a later recv can only be an eviction.
    while acp_rx.try_recv().is_ok() {}

    // The driver disconnects.
    write_message(&mut tui_writer, &ClientMessage::Disconnect)
        .await
        .unwrap();
    drop(tui_reader);
    drop(tui_writer);

    // Real disconnect barrier, in two steps. `client_count` drops at the top of the `Disconnected`
    // arm; the fan-out below is handled by a *later* iteration of the same single-threaded main
    // loop, so receiving it proves the disconnect arm (driver reassignment included) already ran.
    wait_for_client_count(&client_count, 1).await;
    response_tx
        .send(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-xfer","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"STILL_SUBSCRIBED"}}}}"#.into())
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut obs_reader, "STILL_SUBSCRIBED")
            .await
            .is_some(),
        "the observer must remain a subscriber after the driver leaves"
    );

    assert!(
        acp_rx.try_recv().is_err(),
        "the session must NOT be evicted while the observer is still subscribed (residency)"
    );

    // But the driver slot was cleared, not transferred to the observer.
    response_tx
        .send(driver_only_request(31, "sess-xfer"))
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut obs_reader, "read_text_file")
            .await
            .is_none(),
        "driver reassignment must skip observers and leave the session driverless"
    );

    cancel.cancel();
}

// ---------------------------------------------------------------------------
// 4. Exit-on-disconnect ignores observers
// ---------------------------------------------------------------------------

/// A leader auto-spawned for a TUI exits when its last real client leaves. The always-attached
/// remote-lane observer must not pin it alive forever, so "all clients disconnected" means "every
/// remaining registered client is an observer".
#[tokio::test]
async fn exit_on_disconnect_ignores_observers() {
    let temp = TempDir::new().unwrap();
    let (sock_path, _cancel, _response_tx, _acp_rx, client_count, server) =
        spawn_server(&temp, false).await;

    // The observer attaches first and never leaves (the in-process remote lane).
    let (_obs_reader, _obs_writer) =
        register_with(&sock_path, "gx-remote-api", observer_caps()).await;
    // Barrier: the exit predicate reads the *registered* capabilities, so the observer's
    // registration must have reached the main loop before the TUI's disconnect does.
    wait_for_client_count(&client_count, 1).await;

    // A real client connects, then goes away.
    let (tui_reader, mut tui_writer) =
        register_with(&sock_path, "grok-tui", ClientCapabilities::default()).await;
    wait_for_client_count(&client_count, 2).await;
    write_message(&mut tui_writer, &ClientMessage::Disconnect)
        .await
        .unwrap();
    drop(tui_reader);
    drop(tui_writer);

    // Join the server's own `Result`: a task that merely *finished* proves nothing, because an
    // unrelated server error would end it too. The leader must exit cleanly.
    let exit = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the leader must exit once only observers remain connected")
        .expect("leader task panicked");
    assert!(
        exit.is_ok(),
        "the leader must exit cleanly on last-real-client disconnect, not with an error: {exit:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Interaction replay reaches the observer
// ---------------------------------------------------------------------------

/// A pending interaction cached by the leader must replay to an observer on its `session/load`.
/// That is how the phone picks up a permission modal the TUI raised (or that was raised with
/// nobody attached at all) instead of showing a turn stuck at "Waiting".
#[tokio::test]
async fn cached_interaction_replays_to_an_observer_on_load() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    // A real client attaches first and receives the interaction as a live broadcast. That receipt
    // is the ordering barrier: the leader caches the interaction in the same arm that broadcasts
    // it, so the cache is provably populated BEFORE the observer connects — which in turn means the
    // observer can only get it by replay, never as a late live broadcast.
    let (mut tui_reader, mut tui_writer) =
        register_with(&sock_path, "grok-tui", ClientCapabilities::default()).await;
    load_session(&mut tui_writer, "sess-int").await;
    complete_load(&mut acp_rx, &response_tx, None).await;
    let _ = next_acp_payload(&mut tui_reader).await; // the load response

    response_tx
        .send(r#"{"jsonrpc":"2.0","id":601,"method":"session/request_permission","params":{"sessionId":"sess-int","toolCall":{"toolCallId":"tc-obs"},"options":[]}}"#.into())
        .unwrap();
    assert!(
        next_acp_payload_matching(&mut tui_reader, "request_permission")
            .await
            .is_some(),
        "the interaction must reach the attached client (which proves the leader cached it)"
    );

    let (mut obs_reader, mut obs_writer) =
        register_with(&sock_path, "gx-remote-api", observer_caps()).await;
    load_session(&mut obs_writer, "sess-int").await;
    complete_load(&mut acp_rx, &response_tx, None).await;

    assert!(
        next_acp_payload_matching(&mut obs_reader, "request_permission")
            .await
            .is_some(),
        "a cached interaction must replay to an observer that attaches (it is a full subscriber)"
    );

    cancel.cancel();
}

// ---------------------------------------------------------------------------
// 6. Identity-only meta injection
// ---------------------------------------------------------------------------

/// The agent re-applies a session request's `_meta` capability flags to the SHARED resident session
/// actor. So an observer gets identity keys only: `clientIdentifier` and `x.ai/leaderClientId`
/// (which route the load response and its replay back to it), and NOTHING that would reconfigure
/// the session the TUI is driving. A normal client's identical capabilities still inject in full.
#[tokio::test]
async fn observer_session_load_gets_identity_only_meta() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let capability_keys = [
        "codeNavEnabled",
        "clientTerminal",
        "clientFsRead",
        "clientFsWrite",
        xai_grok_status_line::CLIENT_STATUS_LINE_META,
        "autoMode",
    ];

    // Observer: identity keys only.
    let (_obs_reader, mut obs_writer) =
        register_with(&sock_path, "gx-remote-api", observer_caps()).await;
    load_session(&mut obs_writer, "sess-meta-obs").await;
    let obs_load = complete_load(&mut acp_rx, &response_tx, None).await;
    let obs_meta = meta_of(&obs_load);

    assert_eq!(
        obs_meta.get("clientIdentifier").and_then(|v| v.as_str()),
        Some("gx-remote-api"),
        "an observer still needs clientIdentifier: {obs_meta:?}"
    );
    assert!(
        obs_meta.contains_key("x.ai/leaderClientId"),
        "an observer still needs x.ai/leaderClientId to route its load response and replay: {obs_meta:?}"
    );
    for key in capability_keys {
        assert!(
            !obs_meta.contains_key(key),
            "an observer must NOT inject `{key}` into a shared session's meta: {obs_meta:?}"
        );
    }

    // Control: the same capabilities on a non-observer inject in full.
    let (_tui_reader, mut tui_writer) = register_with(&sock_path, "grok-tui", tui_caps()).await;
    load_session(&mut tui_writer, "sess-meta-tui").await;
    let tui_load = complete_load(&mut acp_rx, &response_tx, None).await;
    let tui_meta = meta_of(&tui_load);

    assert_eq!(
        tui_meta.get("clientIdentifier").and_then(|v| v.as_str()),
        Some("grok-tui")
    );
    assert!(tui_meta.contains_key("x.ai/leaderClientId"));
    for key in capability_keys {
        assert!(
            tui_meta.contains_key(key),
            "a non-observer client must still get `{key}` injected: {tui_meta:?}"
        );
    }

    // `modelId` is the session/new-only half of the injection: skipped for an observer too, so the
    // phone attaching never reconfigures the model of a session the TUI is driving.
    send_acp(
        &mut obs_writer,
        r#"{"jsonrpc":"2.0","method":"session/new","id":8,"params":{"cwd":"/repo"}}"#.to_string(),
    )
    .await;
    let obs_new = next_forwarded(&mut acp_rx, "session/new").await;
    assert!(
        !meta_of(&obs_new).contains_key("modelId"),
        "an observer must NOT inject modelId into session/new: {:?}",
        meta_of(&obs_new)
    );

    send_acp(
        &mut tui_writer,
        r#"{"jsonrpc":"2.0","method":"session/new","id":9,"params":{"cwd":"/repo"}}"#.to_string(),
    )
    .await;
    let tui_new = next_forwarded(&mut acp_rx, "session/new").await;
    assert_eq!(
        meta_of(&tui_new).get("modelId").and_then(|v| v.as_str()),
        Some("gx-remote-model"),
        "a non-observer client must still get its default model injected into session/new"
    );

    cancel.cancel();
}

// ---------------------------------------------------------------------------
// 7. Wire compatibility
// ---------------------------------------------------------------------------

/// `observer` is `#[serde(default)]`, so a registration from any older/stock client - which never
/// sends the key - deserializes as a normal, non-observer client.
#[test]
fn client_capabilities_without_observer_deserializes_to_false() {
    let caps: ClientCapabilities = serde_json::from_str(
        r#"{"yolo_mode":true,"auto_mode":false,"terminal":true,"fs_read":true,"fs_write":true,"status_line":true,"code_nav_enabled":true}"#,
    )
    .expect("a payload with no `observer` key must still deserialize");
    assert!(
        !caps.observer,
        "a client that never heard of `observer` must not be treated as one"
    );
    assert!(
        caps.yolo_mode,
        "sanity: the rest of the payload still parses"
    );
    assert!(!ClientCapabilities::default().observer);

    // And an explicit `true` round-trips.
    let observer: ClientCapabilities =
        serde_json::from_str(r#"{"observer":true}"#).expect("explicit observer must parse");
    assert!(observer.observer);
    assert_eq!(
        serde_json::from_str::<ClientCapabilities>(
            &serde_json::to_string(&observer_caps()).unwrap()
        )
        .unwrap(),
        observer_caps(),
        "ClientCapabilities must round-trip through the wire with `observer` set"
    );
}

/// Forward compatibility, the other direction: a client cannot tell from `ClientCapabilities` alone
/// whether the leader it adopted honours `observer`, so the leader advertises `observer_v1` in the
/// registration acknowledgement. An out-of-process lane can then refuse to attach to an older
/// leader (which would silently let it claim the driver slot) instead of corrupting the TUI's
/// routing. Today's only observer is in-process, so nothing consumes this yet — it exists so a
/// future standalone lane has something to negotiate against.
#[tokio::test]
async fn registration_advertises_the_observer_capability() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, _response_tx, _acp_rx, _count, _srv) = spawn_server(&temp, true).await;

    let (_reader, _writer, ack) =
        register_capturing(&sock_path, "gx-remote-api", observer_caps()).await;
    let caps = match ack {
        ServerMessage::Registered {
            leader_capabilities,
            ..
        } => leader_capabilities.expect("the leader must send its capabilities on registration"),
        other => panic!("expected Registered, got {other:?}"),
    };
    assert!(
        caps.observer_v1,
        "a gx leader must advertise observer_v1 so a client can negotiate the observer contract: {caps:?}"
    );
    assert!(
        caps.control_v1,
        "sanity: the upstream capabilities are still advertised alongside it"
    );

    cancel.cancel();
}

/// `observer_v1` is `#[serde(default)]`: a payload from an older leader that never heard of the
/// contract must decode as `false`, so a lane treats it as "does not honour observers".
#[test]
fn leader_capabilities_without_observer_v1_deserializes_to_false() {
    let caps: LeaderCapabilities = serde_json::from_str(
        r#"{"control_v1":true,"runtime_cpu_profile":false,"profile_formats":[],"workspace_exposure":true,"relaunch_v1":true}"#,
    )
    .expect("a payload with no `observer_v1` key must still deserialize");
    assert!(
        !caps.observer_v1,
        "an older leader must never look like it honours the observer contract"
    );
    assert!(
        caps.control_v1,
        "sanity: the rest of the payload still parses"
    );
}

// ---------------------------------------------------------------------------
// 8. Roost hook identity (`gx/hookEnv`, issue #14)
// ---------------------------------------------------------------------------
//
// The leader runs every session's hooks, and inherits the environment of whichever TUI spawned it,
// so the identity has to travel per client: registered as a capability, stamped by the leader into
// that client's session requests. These tests pin the properties that make the stamp safe - it
// comes from the registration and only the registration, and the PRESENT/ABSENT distinction the
// agent reads off it:
//
//   - a non-observer's session request carries the key even when the client registered no identity
//     (an empty object, which tells the agent to CLEAR the session's identity);
//   - an observer's never carries it, which tells the agent to leave the session's identity
//     exactly as it is. Stamping an empty object for an observer would be the bug in reverse:
//     merely opening a session from the phone would unhook the owning TUI's session from its tab.

/// A tab identity as a TUI would register it.
fn hook_env_of(tab: &str) -> std::collections::BTreeMap<String, String> {
    [
        ("ROOST_AGENT_HOOK", "/usr/local/bin/roost"),
        ("ROOST_SOCKET", "/run/roost.sock"),
        ("ROOST_TAB_ID", tab),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn caps_with_hook_env(observer: bool, tab: &str) -> ClientCapabilities {
    ClientCapabilities {
        observer,
        hook_env: hook_env_of(tab),
        ..Default::default()
    }
}

/// The stamped `_meta` key, read back as a plain map.
fn hook_env_meta(
    request: &serde_json::Value,
) -> Option<std::collections::BTreeMap<String, String>> {
    let value = meta_of(request).get(crate::agent::gx_hook_env::META_KEY)?;
    Some(
        value
            .as_object()
            .expect("gx/hookEnv must be stamped as an object")
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.as_str()
                        .expect("every value must be a string")
                        .to_string(),
                )
            })
            .collect(),
    )
}

/// A `session/load` that supplies a `gx/hookEnv` in its own body.
async fn load_session_claiming(
    writer: &mut tokio::io::WriteHalf<LeaderStream>,
    session_id: &str,
    tab: &str,
) {
    let forged = serde_json::json!({
        "ROOST_TAB_ID": tab,
        "ROOST_SOCKET": "/tmp/forged.sock",
        "ROOST_AGENT_HOOK": "/tmp/forged",
    });
    send_acp(
        writer,
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/load",
            "id": 1,
            "params": {
                "sessionId": session_id,
                "_meta": { crate::agent::gx_hook_env::META_KEY: forged },
            },
        })
        .to_string(),
    )
    .await;
}

/// (c) A non-observer TUI that registered an identity gets it stamped: the agent SETS it on the
/// session.
#[tokio::test]
async fn tui_session_load_gets_hook_env_stamped_from_its_registration() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let (_reader, mut writer) =
        register_with(&sock_path, "grok-tui", caps_with_hook_env(false, "5")).await;
    load_session(&mut writer, "sess-hookenv").await;
    let load = complete_load(&mut acp_rx, &response_tx, None).await;

    assert_eq!(
        hook_env_meta(&load),
        Some(hook_env_of("5")),
        "the session must carry the identity the client registered: {:?}",
        meta_of(&load)
    );

    cancel.cancel();
}

/// (a) An observer's `session/load` carries NO key at all, even when it asks for one in the
/// request body - so the agent leaves the resident session's identity intact.
///
/// Both halves matter. `ROOST_AGENT_HOOK` names an executable every hook of the session then runs,
/// and the remote lane attaches to sessions other clients are driving, so it must not choose that
/// executable (no stamp from its own registration). And the key must be ABSENT rather than empty,
/// because an empty object is the agent's instruction to clear: stamping one here would mean that
/// merely opening a session from the phone wipes the owning TUI's roost identity and the session
/// silently stops reporting to its tab.
#[tokio::test]
async fn observer_session_load_never_gets_hook_env() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    // The observer registers WITH an identity and asks for another in the body: both must go.
    let (_obs_reader, mut obs_writer) =
        register_with(&sock_path, "gx-remote-api", caps_with_hook_env(true, "9")).await;
    load_session_claiming(&mut obs_writer, "sess-hookenv-obs", "9").await;
    let obs_load = complete_load(&mut acp_rx, &response_tx, None).await;

    assert_eq!(
        hook_env_meta(&obs_load),
        None,
        "an observer must neither set nor CLEAR a shared session's roost identity, so the key has \
         to be absent rather than empty: {:?}",
        meta_of(&obs_load)
    );

    // Positive control: the identical request from a non-observer IS stamped, so the assertion
    // above cannot be passing because the stamp is broken for everyone.
    let (_tui_reader, mut tui_writer) =
        register_with(&sock_path, "grok-tui", caps_with_hook_env(false, "5")).await;
    load_session(&mut tui_writer, "sess-hookenv-obs-control").await;
    let tui_load = complete_load(&mut acp_rx, &response_tx, None).await;
    assert_eq!(hook_env_meta(&tui_load), Some(hook_env_of("5")));

    cancel.cancel();
}

/// (c) A client's own `gx/hookEnv` is replaced by its registration's, never trusted.
#[tokio::test]
async fn a_request_body_hook_env_is_replaced_by_the_registration() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let (_reader, mut writer) =
        register_with(&sock_path, "grok-tui", caps_with_hook_env(false, "5")).await;
    load_session_claiming(&mut writer, "sess-hookenv-forged", "99").await;
    let load = complete_load(&mut acp_rx, &response_tx, None).await;

    assert_eq!(
        hook_env_meta(&load),
        Some(hook_env_of("5")),
        "the body's claim must be discarded in favour of the registration's: {:?}",
        meta_of(&load)
    );

    cancel.cancel();
}

/// (b) A non-observer TUI that registered NO identity still gets the key, stamped as an empty
/// object - the agent's instruction to CLEAR whatever the session was carrying.
///
/// Present-but-empty is the whole point: a TUI launched outside a roost tab, attaching to a
/// session another tab used to own, must stop that session reporting to the old tab. Absent would
/// mean "leave it alone", which is the observer's contract, not this one.
///
/// Paired with the forged-body case, so the client is shown unable to supply the value both on a
/// bare request and on one that tried to fill the key in itself.
#[tokio::test]
async fn a_non_observer_with_no_identity_stamps_an_empty_object() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let (_reader, mut writer) =
        register_with(&sock_path, "grok-tui", ClientCapabilities::default()).await;
    load_session(&mut writer, "sess-hookenv-none").await;
    let load = complete_load(&mut acp_rx, &response_tx, None).await;
    assert_eq!(
        hook_env_meta(&load),
        Some(std::collections::BTreeMap::new()),
        "a non-observer with no identity must stamp an EMPTY object, which clears - not nothing, \
         which would leave the previous tab claimed: {:?}",
        meta_of(&load)
    );

    load_session_claiming(&mut writer, "sess-hookenv-none-forged", "99").await;
    let forged_load = complete_load(&mut acp_rx, &response_tx, None).await;
    assert_eq!(
        hook_env_meta(&forged_load),
        Some(std::collections::BTreeMap::new()),
        "a client with no registered identity must not be able to supply one: {:?}",
        meta_of(&forged_load)
    );

    cancel.cancel();
}

/// The strip must beat `inject_session_request_context`'s capability early-return, which a client
/// reaches deliberately by registering with no capabilities AND an empty client type. Without the
/// pre-guard strip this is the shape that smuggles a forged identity straight through.
///
/// That path deliberately strips without stamping, so a request upstream would forward untouched
/// still is; no real client reaches it (every one registers a non-empty client type), and for the
/// anonymous shape that can, an absent key is the conservative reading.
#[tokio::test]
async fn a_client_with_no_capabilities_at_all_cannot_smuggle_a_hook_env() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let (_reader, mut writer) = register_with(&sock_path, "", ClientCapabilities::default()).await;
    load_session_claiming(&mut writer, "sess-hookenv-bare", "99").await;
    let load = complete_load(&mut acp_rx, &response_tx, None).await;

    assert_eq!(
        hook_env_meta(&load),
        None,
        "the early-return path is strip-only, so the forged identity is gone and no key is left \
         behind: this anonymous shape can neither set a session's identity nor clear one: {:?}",
        meta_of(&load)
    );

    cancel.cancel();
}

/// Registration-time validation: a forged map never reaches a session at all, so the leader holds
/// nothing it would have to re-check later.
#[tokio::test]
async fn an_invalid_registered_hook_env_is_dropped_at_registration() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let mut forged = hook_env_of("5");
    forged.insert("PATH".to_string(), "/tmp/evil".to_string());
    let (_reader, mut writer) = register_with(
        &sock_path,
        "grok-tui",
        ClientCapabilities {
            hook_env: forged,
            ..Default::default()
        },
    )
    .await;
    load_session(&mut writer, "sess-hookenv-invalid").await;
    let load = complete_load(&mut acp_rx, &response_tx, None).await;

    assert_eq!(
        hook_env_meta(&load),
        Some(std::collections::BTreeMap::new()),
        "a registration carrying a key outside the carried set must be dropped whole, leaving the \
         client indistinguishable from one that registered no identity: {:?}",
        meta_of(&load)
    );

    cancel.cancel();
}

/// Registration-time validation, the incomplete case (finding 2): a client carrying only
/// `ROOST_AGENT_HOOK` - the executable a hook runs - must not register an identity out of it.
/// Registration calls `validate` directly, so the all-or-none rule has to live there.
#[tokio::test]
async fn an_incomplete_registered_hook_env_is_dropped_at_registration() {
    let temp = TempDir::new().unwrap();
    let (sock_path, cancel, response_tx, mut acp_rx, _count, _srv) =
        spawn_server(&temp, true).await;

    let partial = [("ROOST_AGENT_HOOK", "/tmp/attacker")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (_reader, mut writer) = register_with(
        &sock_path,
        "grok-tui",
        ClientCapabilities {
            hook_env: partial,
            ..Default::default()
        },
    )
    .await;
    load_session(&mut writer, "sess-hookenv-partial").await;
    let load = complete_load(&mut acp_rx, &response_tx, None).await;

    assert_eq!(
        hook_env_meta(&load),
        Some(std::collections::BTreeMap::new()),
        "a hook executable with no tab and no socket is not an identity; the whole map must go: \
         {:?}",
        meta_of(&load)
    );

    cancel.cancel();
}

/// Wire compatibility, same shape as `observer`: an older client never sends `hook_env`, and its
/// registration must still decode - as "no identity", which stamps nothing.
#[test]
fn client_capabilities_without_hook_env_deserializes_to_empty() {
    let caps: ClientCapabilities = serde_json::from_str(
        r#"{"yolo_mode":true,"observer":false,"terminal":true,"fs_read":true,"fs_write":true,"status_line":true,"code_nav_enabled":true}"#,
    )
    .expect("a payload with no `hook_env` key must still deserialize");
    assert!(
        caps.hook_env.is_empty(),
        "a client that never heard of `hook_env` must carry no identity"
    );
    assert!(
        caps.yolo_mode,
        "sanity: the rest of the payload still parses"
    );
    assert!(ClientCapabilities::default().hook_env.is_empty());

    let with_env = ClientCapabilities {
        hook_env: hook_env_of("5"),
        ..Default::default()
    };
    assert_eq!(
        serde_json::from_str::<ClientCapabilities>(&serde_json::to_string(&with_env).unwrap())
            .unwrap(),
        with_env,
        "ClientCapabilities must round-trip through the wire with `hook_env` set"
    );
}
