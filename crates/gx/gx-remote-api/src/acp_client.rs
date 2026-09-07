//! A single-connection ACP client: id assignment, response correlation, notification fan-out.
//!
//! One task owns the [`LeaderLink`]; every HTTP handler talks to it through channels. The task
//! classifies each inbound payload by shape, which is the only reliable discriminator in JSON-RPC
//! 2.0 over a multiplexed socket:
//!
//! | shape | meaning | handling |
//! |---|---|---|
//! | `id` + (`result` \| `error`), no `method` | response to one of our requests | resolve the waiting oneshot |
//! | `method`, no `id` | notification (session update, roster change, interjection) | broadcast |
//! | `method` + `id` | **reverse**-request: the agent asking *us* something | answered `-32601` (see below) |
//!
//! A response's JSON-RPC `result` is not necessarily the method's payload: most `_x.ai/…`
//! extension methods wrap it once more in an `ExtMethodResult` envelope. [`unwrap_ext_envelope`]
//! is the single place that is undone, and its doc comment carries the live-leader evidence.
//!
//! Reverse-requests are the permission / question / plan-approval / MCP-elicitation interactions.
//! C6 intercepts them and turns them into approval resources; until then every one is refused with
//! `-32601 method not found` the instant it arrives. Refusing is strictly better than dropping: the
//! agent broadcasts an interaction to *all* subscribers and takes the first answer, so a fast
//! `-32601` from the lane is discarded in favour of the TUI's real answer, whereas silence would
//! leave the request outstanding forever if the lane were the only subscriber.
//!
//! Reconnection is deliberately absent (plan D3): the lane is hosted inside the leader process and
//! dies with it. A closed link cancels [`AcpClient::cancel_token`], which is what stops the HTTP
//! server — cleanly, never by panicking.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::link::LeaderLink;

/// How long a request waits for its response before the caller gets `leader_unavailable`.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Ring size for the notification broadcast. Lagging receivers are told they lagged rather than
/// silently losing frames; C5's SSE lane turns a lag into an `event: reset`.
const NOTIFICATION_BUFFER: usize = 1024;

/// JSON-RPC "method not found". Our standing answer to any reverse-request until C6.
const METHOD_NOT_FOUND: i64 = -32601;

/// Extension ACP methods travel with a **leading underscore** on the wire.
///
/// `agent-client-protocol`'s `decode_request` only reaches `ext_method()` via
/// `method.strip_prefix('_')`; the bare `x.ai/…` name answers "Method not found". Proven
/// empirically in commit 691153cc — see `docs/gx/HANDOFF_MATRIX.md` and
/// `scripts/gx/handoff/acp_client.py`. Standard ACP methods (`initialize`, `session/new`,
/// `session/load`, `session/prompt`, `session/cancel`) take no prefix.
///
/// Callers everywhere in this crate write the logical name (`x.ai/sessions/list`); this is the one
/// place the underscore is applied.
pub fn wire_method(method: &str) -> Cow<'_, str> {
    if method.starts_with("x.ai/") {
        Cow::Owned(format!("_{method}"))
    } else {
        Cow::Borrowed(method)
    }
}

/// Inverse of [`wire_method`]: the logical name for a method seen on the wire.
pub fn logical_method(method: &str) -> &str {
    method.strip_prefix('_').unwrap_or(method)
}

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("leader link is closed")]
    Closed,
    #[error("timed out after {0:?} waiting for a response to {1}")]
    Timeout(Duration, String),
    #[error("leader returned an error for {method}: {message} (code {code})")]
    Rpc {
        method: String,
        code: i64,
        message: String,
    },
    /// A JSON-RPC **success** whose [`ExtMethodResult`] envelope carried an `error`. Distinct from
    /// [`AcpError::Rpc`] because the envelope's code is a string (`"not_found"`), not a JSON-RPC
    /// integer, and because the transport succeeded — only the extension method failed.
    ///
    /// [`ExtMethodResult`]: unwrap_ext_envelope
    #[error("leader returned an extension error for {method}: {detail}")]
    Ext { method: String, detail: String },
}

/// Unwrap the gx extension-method response envelope, when the value is one.
///
/// Most `_x.ai/…` extension methods answer through `ExtMethodResult`
/// (`crates/codegen/xai-grok-shell/src/session/result.rs`), which wraps the payload a **second**
/// time inside the JSON-RPC `result` — exactly two fields, `result` and an omitted-when-absent
/// `error`. Captured against a live leader in commit 691153cc:
///
/// - `docs/gx/handoff/list.md` — `_x.ai/session/list` and `_x.ai/sessions/list` both answer
///   `{"jsonrpc":"2.0","id":N,"result":{"result":{"sessions":[…]}}}`.
/// - `docs/gx/handoff/interject.md` — `_x.ai/interject` answers `{"result":{"result":{"status":
///   "queued"}}}`.
///
/// Not every extension method wraps. `_x.ai/session/updates` builds
/// `{"updates":[…],"totalCount":N,"hasMore":bool,"lastEventId":"…"}` directly
/// (`extensions/session_updates.rs::response_from_page`), and handlers routed through
/// `extensions::to_raw_response` skip the envelope too; standard ACP methods (`initialize`,
/// `session/new`, `session/load`, `session/prompt`) never wrap at all.
///
/// So the **shape** is the discriminator, never the method name: unwrap exactly one level when the
/// value is a JSON object whose key set is a subset of `{"result", "error"}` *and* which carries at
/// least one of them. `{"updates": …}` has other keys and passes through untouched; so does an
/// object that merely happens to carry a `result` key alongside something else. A present, non-null
/// `error` becomes `Err(detail)`; a missing or null inner `result` becomes [`Value::Null`].
///
/// Getting this wrong is not cosmetic: reading `.sessions` off the *outer* object finds nothing, so
/// `GET /v1/sessions` answers an empty roster against a real leader.
pub fn unwrap_ext_envelope(value: Value) -> Result<Value, String> {
    let Some(object) = value.as_object() else {
        return Ok(value);
    };
    let is_envelope =
        !object.is_empty() && object.keys().all(|key| key == "result" || key == "error");
    if !is_envelope {
        return Ok(value);
    }

    if let Some(error) = object.get("error").filter(|e| !e.is_null()) {
        return Err(describe_ext_error(error));
    }
    Ok(object.get("result").cloned().unwrap_or(Value::Null))
}

/// Render an envelope `error` — a bare string, or an `ExtMethodError` `{code, message, data}` — as
/// one line of prose. Never drops the code: it is the only machine-readable part.
fn describe_ext_error(error: &Value) -> String {
    if let Some(text) = error.as_str() {
        return text.to_string();
    }
    let as_text = |v: &Value| {
        v.as_str()
            .map(String::from)
            .unwrap_or_else(|| v.to_string())
    };
    let message = error.get("message").filter(|v| !v.is_null()).map(&as_text);
    let code = error.get("code").filter(|v| !v.is_null()).map(&as_text);
    match (message, code) {
        (Some(message), Some(code)) => format!("{message} (code {code})"),
        (Some(message), None) => message,
        (None, Some(code)) => format!("code {code}"),
        (None, None) => error.to_string(),
    }
}

/// One notification from the agent, as it arrived.
#[derive(Debug, Clone)]
pub struct Notification {
    /// Logical method name, i.e. with the extension underscore stripped
    /// (`x.ai/session_notification`, not `_x.ai/session_notification`).
    pub method: String,
    pub params: Value,
    /// `params.sessionId` when present; the key the leader itself fans out on.
    pub session_id: Option<String>,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, (i64, String)>>>>>;

/// Handle to the ACP connection. Cheap to clone; every clone talks to the same task.
#[derive(Clone)]
pub struct AcpClient {
    outbound: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: Arc<AtomicI64>,
    notifications: broadcast::Sender<Notification>,
    cancel: CancellationToken,
    timeout: Duration,
}

impl AcpClient {
    /// Take ownership of `link` in a background task and return a handle to it.
    ///
    /// `cancel` is shared, not cloned-from: cancelling it stops the task, and the task cancels it
    /// when the link closes. Callers use that second direction to shut the HTTP server down.
    pub fn spawn<L: LeaderLink>(link: L, cancel: CancellationToken, timeout: Duration) -> Self {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (notifications, _) = broadcast::channel(NOTIFICATION_BUFFER);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let client = Self {
            outbound: outbound_tx.clone(),
            pending: pending.clone(),
            next_id: Arc::new(AtomicI64::new(1)),
            notifications: notifications.clone(),
            cancel: cancel.clone(),
            timeout,
        };

        tokio::spawn(run_link(
            link,
            outbound_rx,
            outbound_tx,
            pending,
            notifications,
            cancel,
        ));

        client
    }

    /// The ACP handshake. Must precede any session method.
    pub async fn initialize(&self) -> Result<Value, AcpError> {
        self.request("initialize", json!({ "protocolVersion": "0.1" }))
            .await
    }

    /// Send a JSON-RPC request and await its response.
    ///
    /// `method` is the **logical** name; the extension underscore is applied by [`wire_method`].
    /// The JSON-RPC `result` is then run through [`unwrap_ext_envelope`], so every caller in this
    /// crate sees the method's own payload whether or not the leader wrapped it.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        // The task has already gone; skip the round trip and the timeout it would burn.
        if self.cancel.is_cancelled() {
            return Err(AcpError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let wire = wire_method(method);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": wire,
            "params": params,
        })
        .to_string();

        if self.outbound.send(payload).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(AcpError::Closed);
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(Ok(result))) => unwrap_ext_envelope(result).map_err(|detail| AcpError::Ext {
                method: method.to_string(),
                detail,
            }),
            Ok(Ok(Err((code, message)))) => Err(AcpError::Rpc {
                method: method.to_string(),
                code,
                message,
            }),
            // The task dropped the sender: the link died while we waited.
            Ok(Err(_)) => Err(AcpError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(AcpError::Timeout(self.timeout, method.to_string()))
            }
        }
    }

    /// Send a JSON-RPC notification (no id, no response).
    pub fn notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": wire_method(method),
            "params": params,
        })
        .to_string();
        self.outbound.send(payload).map_err(|_| AcpError::Closed)
    }

    /// Subscribe to every notification the agent broadcasts to this client.
    ///
    /// Any message the lane sends that carries a `sessionId` subscribes it to that session's
    /// broadcasts, so this stream only carries sessions the lane has touched.
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.notifications.subscribe()
    }

    /// Cancelled when the link closes (or when the caller shuts the lane down).
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// The task that owns the link.
async fn run_link<L: LeaderLink>(
    mut link: L,
    mut outbound_rx: mpsc::UnboundedReceiver<String>,
    outbound_tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    notifications: broadcast::Sender<Notification>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                debug!("gx-remote-api: acp client cancelled");
                break;
            }
            outbound = outbound_rx.recv() => {
                let Some(payload) = outbound else { break };
                if let Err(err) = link.send(payload) {
                    warn!(%err, "gx-remote-api: leader link send failed");
                    break;
                }
            }
            // `LeaderLink::recv` is required to be cancel-safe; see the trait docs.
            inbound = link.recv() => {
                let Some(payload) = inbound else {
                    debug!("gx-remote-api: leader link closed");
                    break;
                };
                handle_inbound(&payload, &pending, &outbound_tx, &notifications).await;
            }
        }
    }

    // Wake every waiter rather than leaving handlers parked until their timeout.
    pending.lock().await.clear();
    // Tell `serve()` to stop. Idempotent; harmless if the caller cancelled us.
    cancel.cancel();
}

async fn handle_inbound(
    payload: &str,
    pending: &Pending,
    outbound: &mpsc::UnboundedSender<String>,
    notifications: &broadcast::Sender<Notification>,
) {
    let Ok(msg) = serde_json::from_str::<Value>(payload) else {
        warn!("gx-remote-api: dropping unparseable payload from the leader");
        return;
    };

    let method = msg.get("method").and_then(Value::as_str);
    // A literal `"id": null` is not an id; some senders spell notifications that way.
    let id = msg.get("id").filter(|v| !v.is_null());

    match (method, id) {
        // Response to one of our requests.
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else {
                warn!("gx-remote-api: response with a non-integer id");
                return;
            };
            let Some(tx) = pending.lock().await.remove(&id) else {
                debug!(id, "gx-remote-api: response for an unknown request id");
                return;
            };
            let outcome = if let Some(error) = msg.get("error") {
                Err((
                    error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                ))
            } else {
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = tx.send(outcome);
        }
        // Reverse-request from the agent. Refuse it so nothing can hang; see the module docs.
        (Some(method), Some(id)) => {
            debug!(
                method,
                "gx-remote-api: refusing a reverse-request (C6 will handle these)"
            );
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": METHOD_NOT_FOUND,
                    "message": format!("gx-remote-api does not handle {method}"),
                },
            })
            .to_string();
            let _ = outbound.send(reply);
        }
        // Notification.
        (Some(method), None) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .map(String::from);
            // A send error only means nobody is subscribed yet; that is normal until C5.
            let _ = notifications.send(Notification {
                method: logical_method(method).to_string(),
                params,
                session_id,
            });
        }
        (None, None) => warn!("gx-remote-api: payload with neither method nor id"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::FakeLink;

    fn spawn_over_fake() -> (AcpClient, crate::link::FakeLinkHandle) {
        let (link, handle) = FakeLink::new();
        let client = AcpClient::spawn(link, CancellationToken::new(), Duration::from_secs(5));
        (client, handle)
    }

    #[test]
    fn extension_methods_take_a_leading_underscore_on_the_wire() {
        assert_eq!(wire_method("x.ai/sessions/list"), "_x.ai/sessions/list");
        assert_eq!(wire_method("x.ai/session/list"), "_x.ai/session/list");
        assert_eq!(wire_method("x.ai/session/updates"), "_x.ai/session/updates");
        assert_eq!(wire_method("x.ai/interject"), "_x.ai/interject");
    }

    #[test]
    fn standard_methods_are_left_alone() {
        for method in [
            "initialize",
            "session/new",
            "session/load",
            "session/prompt",
            "session/cancel",
        ] {
            assert_eq!(wire_method(method), method, "{method} must not be prefixed");
        }
    }

    #[test]
    fn logical_method_strips_the_underscore() {
        assert_eq!(
            logical_method("_x.ai/session_notification"),
            "x.ai/session_notification"
        );
        assert_eq!(logical_method("session/update"), "session/update");
    }

    // -----------------------------------------------------------------------
    // The `ExtMethodResult` envelope
    // -----------------------------------------------------------------------

    #[test]
    fn a_wrapped_success_is_unwrapped_one_level() {
        // Verbatim from `docs/gx/handoff/list.md`'s `_x.ai/sessions/list`, minus the JSON-RPC frame.
        let wrapped = json!({ "result": { "sessions": [{ "sessionId": "sess-1" }] } });
        let inner = unwrap_ext_envelope(wrapped).unwrap();
        assert_eq!(inner["sessions"][0]["sessionId"], "sess-1");

        // `docs/gx/handoff/interject.md`'s `_x.ai/interject`.
        assert_eq!(
            unwrap_ext_envelope(json!({ "result": { "status": "queued" } })).unwrap(),
            json!({ "status": "queued" })
        );
    }

    #[test]
    fn a_wrapped_error_becomes_an_error_carrying_its_code_and_message() {
        let detail = unwrap_ext_envelope(json!({
            "result": null,
            "error": { "code": "not_found", "message": "no session sess-9" }
        }))
        .unwrap_err();
        assert!(detail.contains("no session sess-9"), "{detail}");
        assert!(detail.contains("not_found"), "{detail}");

        // The envelope's `error` is `serde_json::Value`, so a bare string is legal too.
        assert_eq!(
            unwrap_ext_envelope(json!({ "error": "boom" })).unwrap_err(),
            "boom"
        );
    }

    #[test]
    fn an_unwrapped_updates_page_passes_through_untouched() {
        // `_x.ai/session/updates` is built by `response_from_page` without the envelope. Unwrapping
        // it would hand the history route a null page.
        let page = json!({
            "updates": [{ "timestamp": 1, "method": "session/update", "params": {} }],
            "totalCount": 1,
            "hasMore": false,
            "lastEventId": "sess-1-1"
        });
        assert_eq!(unwrap_ext_envelope(page.clone()).unwrap(), page);
    }

    #[test]
    fn an_object_that_merely_has_a_result_key_is_not_an_envelope() {
        // Key set must be a *subset* of {result, error}: a `result` sitting next to anything else
        // is the method's own payload, not a wrapper.
        let payload = json!({ "result": { "a": 1 }, "sessions": [] });
        assert_eq!(unwrap_ext_envelope(payload.clone()).unwrap(), payload);

        let payload = json!({ "error": "x", "totalCount": 0 });
        assert_eq!(unwrap_ext_envelope(payload.clone()).unwrap(), payload);
    }

    #[test]
    fn non_objects_and_the_empty_object_pass_through() {
        for value in [
            Value::Null,
            json!(7),
            json!("text"),
            json!([{ "result": 1 }]),
            // Carries neither key, so there is nothing to unwrap to.
            json!({}),
        ] {
            assert_eq!(
                unwrap_ext_envelope(value.clone()).unwrap(),
                value,
                "{value}"
            );
        }
    }

    #[test]
    fn a_wrapped_null_result_unwraps_to_null_not_to_the_wrapper() {
        assert_eq!(
            unwrap_ext_envelope(json!({ "result": null })).unwrap(),
            Value::Null
        );
        // `error` is `skip_serializing_if = "Option::is_none"`, so success omits it; an explicit
        // null is still success.
        assert_eq!(
            unwrap_ext_envelope(json!({ "result": { "ok": true }, "error": null })).unwrap(),
            json!({ "ok": true })
        );
    }

    #[tokio::test]
    async fn request_correlates_its_response_and_unwraps_the_envelope() {
        let (client, handle) = spawn_over_fake();
        // The live shape: `_x.ai/sessions/list` wraps its payload in `ExtMethodResult`.
        handle.respond_ok(
            "x.ai/sessions/list",
            json!({ "result": { "sessions": ["a"] } }),
        );

        let result = client
            .request("x.ai/sessions/list", json!({}))
            .await
            .unwrap();
        assert_eq!(result["sessions"][0], "a");
        assert_eq!(handle.outbound_methods(), vec!["_x.ai/sessions/list"]);
    }

    #[tokio::test]
    async fn an_envelope_error_on_a_successful_rpc_is_still_an_error() {
        let (client, handle) = spawn_over_fake();
        handle.respond_ok(
            "x.ai/interject",
            json!({ "result": null, "error": { "code": "not_accepting", "message": "busy" } }),
        );

        let err = client
            .request("x.ai/interject", json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AcpError::Ext { method, detail }
                if method == "x.ai/interject" && detail.contains("not_accepting")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn concurrent_requests_do_not_cross_their_responses() {
        let (client, handle) = spawn_over_fake();
        // Echo the request's own params back so a crossed response is detectable.
        handle.respond_with("x.ai/session/updates", |params| Ok(params.clone()));

        let a = client.request("x.ai/session/updates", json!({ "sessionId": "a" }));
        let b = client.request("x.ai/session/updates", json!({ "sessionId": "b" }));
        let (a, b) = tokio::join!(a, b);
        assert_eq!(a.unwrap()["sessionId"], "a");
        assert_eq!(b.unwrap()["sessionId"], "b");
    }

    #[tokio::test]
    async fn an_rpc_error_becomes_an_error_not_a_hang() {
        let (client, handle) = spawn_over_fake();
        handle.respond_err("x.ai/session/updates", -32602, "bad params");

        let err = client
            .request("x.ai/session/updates", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, AcpError::Rpc { code: -32602, .. }), "{err}");
    }

    #[tokio::test]
    async fn a_reverse_request_is_answered_with_method_not_found() {
        let (client, handle) = spawn_over_fake();
        // Get the task running so the push below is definitely observed.
        client.initialize().await.unwrap();

        handle.push(json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "session/request_permission",
            "params": { "sessionId": "s1" },
        }));

        // The refusal is sent back over the same link, so it shows up as outbound traffic.
        let reply = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(v) = handle
                    .outbound()
                    .into_iter()
                    .find(|v| v.get("id").and_then(Value::as_i64) == Some(99))
                {
                    return v;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the lane must answer a reverse-request");
        assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn notifications_are_broadcast_with_their_session_id() {
        let (client, handle) = spawn_over_fake();
        let mut rx = client.subscribe();
        client.initialize().await.unwrap();

        handle.push(json!({
            "jsonrpc": "2.0",
            "method": "_x.ai/session_notification",
            "params": { "sessionId": "sess-1", "update": {} },
        }));

        let note = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.method, "x.ai/session_notification");
        assert_eq!(note.session_id.as_deref(), Some("sess-1"));
    }

    #[tokio::test]
    async fn a_closed_link_cancels_the_token_and_fails_requests() {
        let (link, handle) = FakeLink::new();
        let cancel = CancellationToken::new();
        let client = AcpClient::spawn(link, cancel.clone(), Duration::from_secs(5));
        client.initialize().await.unwrap();

        // Dropping the handle is not enough — the link owns its own inbound sender. Cancelling
        // stands in for the leader going away; the task must then fail requests, not hang.
        drop(handle);
        cancel.cancel();

        let err = client
            .request("x.ai/sessions/list", json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AcpError::Closed),
            "a dead link must fail fast, not park until the timeout: {err}"
        );
        assert!(client.cancel_token().is_cancelled());
    }
}
