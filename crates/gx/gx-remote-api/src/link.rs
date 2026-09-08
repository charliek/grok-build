//! The one seam between the HTTP lane and the leader socket.
//!
//! Everything above [`LeaderLink`] speaks raw JSON-RPC 2.0 payload strings; everything below it is
//! either [`LeaderClient`]'s framed IPC channels or, in tests, an in-memory script. Keeping the seam
//! this narrow is what lets the whole router be exercised without a Unix socket, a leader process or
//! an agent.
//!
//! [`LeaderClient`]: xai_grok_shell::leader::LeaderClient

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;

/// A bidirectional stream of ACP JSON-RPC payload strings.
///
/// `send` takes `&self` so the owning task can write while it is parked in `recv`; `recv` takes
/// `&mut self` because it owns the read half.
///
/// **`recv` must be cancel-safe.** [`crate::acp_client`] drives it inside a `tokio::select!`, so an
/// implementation that buffers partial state across a dropped future would lose messages. Both
/// implementations here delegate to `mpsc::UnboundedReceiver::recv`, which is cancel-safe.
pub trait LeaderLink: Send + 'static {
    /// Queue one payload for the leader. `Err` means the link is gone for good.
    fn send(&self, payload: String) -> anyhow::Result<()>;

    /// Next payload from the leader, or `None` once the link is closed.
    fn recv(&mut self) -> impl Future<Output = Option<String>> + Send;
}

/// [`LeaderLink`] over a live leader connection.
///
/// Built from [`xai_grok_shell::leader::LeaderClient::into_channels`], which already performed the
/// `Register` handshake and owns the length-prefixed framing.
pub struct ChannelLink {
    tx: mpsc::UnboundedSender<String>,
    rx: mpsc::UnboundedReceiver<String>,
}

impl ChannelLink {
    pub fn new(tx: mpsc::UnboundedSender<String>, rx: mpsc::UnboundedReceiver<String>) -> Self {
        Self { tx, rx }
    }

    /// Consume a registered [`LeaderClient`] into a link.
    pub fn from_leader_client(client: xai_grok_shell::leader::LeaderClient) -> Self {
        let (tx, rx) = client.into_channels();
        Self::new(tx, rx)
    }
}

impl LeaderLink for ChannelLink {
    fn send(&self, payload: String) -> anyhow::Result<()> {
        self.tx
            .send(payload)
            .map_err(|_| anyhow::anyhow!("leader link closed"))
    }

    fn recv(&mut self) -> impl Future<Output = Option<String>> + Send {
        self.rx.recv()
    }
}

/// What a [`FakeLink`] does with an outbound request for a given method.
type Responder = Box<dyn Fn(&Value) -> Result<Value, Value> + Send + Sync>;

struct FakeShared {
    /// Every payload the lane sent, in order.
    outbound: Mutex<Vec<String>>,
    /// Logical method (leading `_` stripped) -> canned answer.
    responders: Mutex<HashMap<String, Responder>>,
    /// Logical methods that are recorded and then deliberately left unanswered.
    silent: Mutex<HashSet<String>>,
    inbound_tx: mpsc::UnboundedSender<String>,
}

/// An in-memory [`LeaderLink`] for router-level tests.
///
/// Requests are answered synchronously inside `send`, so a test never has to sleep or poll: by the
/// time the handler's `await` resumes, the response is already queued on the inbound channel. A
/// method with no registered responder is answered with JSON-RPC `-32601`, which surfaces as
/// `leader_unavailable` rather than a hang.
pub struct FakeLink {
    shared: Arc<FakeShared>,
    inbound_rx: mpsc::UnboundedReceiver<String>,
}

/// Test-side control surface for a [`FakeLink`]; outlives the link itself.
#[derive(Clone)]
pub struct FakeLinkHandle {
    shared: Arc<FakeShared>,
}

impl FakeLink {
    /// A link plus its controller. `initialize` is answered out of the box; override it with
    /// [`FakeLinkHandle::respond_with`] if a test needs the failure path.
    pub fn new() -> (Self, FakeLinkHandle) {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(FakeShared {
            outbound: Mutex::new(Vec::new()),
            responders: Mutex::new(HashMap::new()),
            silent: Mutex::new(HashSet::new()),
            inbound_tx,
        });
        let handle = FakeLinkHandle {
            shared: shared.clone(),
        };
        handle.respond_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": "0.1" }),
        );
        (Self { shared, inbound_rx }, handle)
    }
}

impl LeaderLink for FakeLink {
    fn send(&self, payload: String) -> anyhow::Result<()> {
        self.shared.outbound.lock().unwrap().push(payload.clone());

        let Ok(msg) = serde_json::from_str::<Value>(&payload) else {
            return Ok(());
        };
        // Notifications carry no id and expect no answer.
        let (Some(id), Some(method)) = (msg.get("id"), msg.get("method").and_then(Value::as_str))
        else {
            return Ok(());
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let logical = crate::acp_client::logical_method(method);
        if self.shared.silent.lock().unwrap().contains(logical) {
            // Recorded, never answered: an in-flight turn, as far as the lane can tell.
            return Ok(());
        }
        let answer = {
            let responders = self.shared.responders.lock().unwrap();
            responders.get(logical).map(|r| r(&params))
        };
        let body = match answer {
            Some(Ok(result)) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Some(Err(error)) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": error }),
            None => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("fake link has no responder for {method}") },
            }),
        };
        let _ = self.shared.inbound_tx.send(body.to_string());
        Ok(())
    }

    fn recv(&mut self) -> impl Future<Output = Option<String>> + Send {
        self.inbound_rx.recv()
    }
}

impl FakeLinkHandle {
    /// Answer `method` (write the logical name, e.g. `x.ai/sessions/list`) with a fixed result.
    pub fn respond_ok(&self, method: &str, result: Value) {
        self.respond_with(method, move |_| Ok(result.clone()));
    }

    /// Answer `method` the way a leader answers an `ExtMethodResult`-wrapped extension method:
    /// `result` is nested one level deeper (`{"result":{"result":<payload>}}` on the wire).
    ///
    /// Use this for every `_x.ai/…` method that goes through `to_ext_response` — `x.ai/sessions/
    /// list`, `x.ai/session/list`, `x.ai/interject`. Methods that build their payload directly
    /// (`x.ai/session/updates`) and every standard ACP method take plain [`Self::respond_ok`]. See
    /// [`crate::acp_client::unwrap_ext_envelope`] for the evidence behind the split.
    pub fn respond_ext_ok(&self, method: &str, result: Value) {
        self.respond_ok(method, serde_json::json!({ "result": result }));
    }

    /// Answer `method` with an `ExtMethodResult` envelope carrying an `error` — a JSON-RPC
    /// *success* whose extension method failed.
    pub fn respond_ext_err(&self, method: &str, code: &str, message: &str) {
        let error = serde_json::json!({ "code": code, "message": message });
        self.respond_ok(
            method,
            serde_json::json!({ "result": Value::Null, "error": error }),
        );
    }

    /// Answer `method` with a JSON-RPC error object.
    pub fn respond_err(&self, method: &str, code: i64, message: &str) {
        let message = message.to_string();
        self.respond_with(method, move |_| {
            Err(serde_json::json!({ "code": code, "message": message }))
        });
    }

    /// Answer `method` with a JSON-RPC error object carrying a `data` member.
    ///
    /// `data` is not decoration on this wire: the agent's refusal of a request naming a session it
    /// has unloaded is an ordinary `invalid_params` whose *data* is the only thing that says which
    /// invalid parameter (`acp_agent.rs`). See [`crate::acp_client::AcpError::is_unknown_session`].
    pub fn respond_err_with_data(&self, method: &str, code: i64, message: &str, data: Value) {
        let message = message.to_string();
        self.respond_with(method, move |_| {
            Err(serde_json::json!({ "code": code, "message": message, "data": data }))
        });
    }

    /// Record `method` and never answer it.
    ///
    /// This is what a `session/prompt` looks like from the lane's side for the whole of a turn: the
    /// request is on the wire, the response is minutes away. Any test asserting that a handler does
    /// **not** block on the turn has to have one of these, because every other stub here answers
    /// synchronously inside `send`.
    pub fn never_respond(&self, method: &str) {
        self.shared
            .silent
            .lock()
            .unwrap()
            .insert(method.to_string());
    }

    /// Answer `method` with a closure over the request params.
    pub fn respond_with<F>(&self, method: &str, f: F)
    where
        F: Fn(&Value) -> Result<Value, Value> + Send + Sync + 'static,
    {
        self.shared
            .responders
            .lock()
            .unwrap()
            .insert(method.to_string(), Box::new(f));
    }

    /// Push a server-originated payload (notification or reverse-request) at the lane.
    pub fn push(&self, payload: Value) {
        let _ = self.shared.inbound_tx.send(payload.to_string());
    }

    /// Every payload the lane has sent so far, parsed, in order.
    pub fn outbound(&self) -> Vec<Value> {
        self.shared
            .outbound
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| serde_json::from_str(p).ok())
            .collect()
    }

    /// Wire method names of every payload the lane has sent, in order. This is the assertion the
    /// lazy-attach tests make: `session/load` must precede `_x.ai/session/updates`.
    pub fn outbound_methods(&self) -> Vec<String> {
        self.outbound()
            .iter()
            .filter_map(|v| v.get("method").and_then(Value::as_str).map(String::from))
            .collect()
    }

    /// The first payload whose wire method is `method`, if any.
    pub fn first_outbound(&self, method: &str) -> Option<Value> {
        self.outbound()
            .into_iter()
            .find(|v| v.get("method").and_then(Value::as_str) == Some(method))
    }

    /// Wait until the lane has actually sent `method`, and return that payload.
    ///
    /// "The handler returned" and "the leader saw it" are two different instants: a payload is
    /// queued for the task that owns the link, and that task writes it. A handler that *awaits* a
    /// response has necessarily flushed by the time it returns, but a fire-and-forget send
    /// (`session/prompt`) and a notification (`session/cancel`) have not — so every assertion about
    /// those has to wait here instead of reading `outbound()` the instant the response arrives.
    ///
    /// Bounded, and panics with the traffic it did see, so a genuine regression fails fast and
    /// legibly instead of hanging the suite.
    pub async fn wait_for_outbound(&self, method: &str, timeout: std::time::Duration) -> Value {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(payload) = self.first_outbound(method) {
                    return payload;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the lane never sent {method}; it sent {:?}",
                self.outbound_methods()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_link_answers_a_registered_method_synchronously() {
        let (link, handle) = FakeLink::new();
        handle.respond_ok("x.ai/sessions/list", serde_json::json!({ "sessions": [] }));

        let mut link = link;
        link.send(r#"{"jsonrpc":"2.0","id":7,"method":"_x.ai/sessions/list","params":{}}"#.into())
            .unwrap();

        let reply: Value = serde_json::from_str(&link.recv().await.unwrap()).unwrap();
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["sessions"], serde_json::json!([]));
        assert_eq!(handle.outbound_methods(), vec!["_x.ai/sessions/list"]);
    }

    #[tokio::test]
    async fn fake_link_errors_on_an_unregistered_method() {
        let (mut link, _handle) = FakeLink::new();
        link.send(r#"{"jsonrpc":"2.0","id":1,"method":"x.ai/nope","params":{}}"#.into())
            .unwrap();
        let reply: Value = serde_json::from_str(&link.recv().await.unwrap()).unwrap();
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn fake_link_records_notifications_without_answering_them() {
        let (mut link, handle) = FakeLink::new();
        link.send(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#.into())
            .unwrap();
        assert_eq!(handle.outbound_methods(), vec!["session/cancel"]);

        // Nothing was queued inbound, so a push is the only thing recv can return.
        handle.push(serde_json::json!({ "jsonrpc": "2.0", "method": "ping" }));
        let got: Value = serde_json::from_str(&link.recv().await.unwrap()).unwrap();
        assert_eq!(got["method"], "ping");
    }
}
