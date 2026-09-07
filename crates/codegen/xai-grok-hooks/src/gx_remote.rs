//! gx: the remote lane's base URL, published to hook payloads.
//!
//! `gx-remote-api` (the loopback HTTP façade over the leader, `docs/gx/REMOTE_API.md`) binds a
//! port inside the leader process. Anything that receives a hook from that process — roost, in
//! particular, which forwards a hook's fields into its clients' metadata map as `gx.remote`
//! (roost#425) — wants to know the URL so it can talk to the lane directly instead of
//! re-discovering it.
//!
//! # Why a process-global, and why here
//!
//! The stamp has to reach [`crate::event::HookEventEnvelope::to_hook_json`], which is called deep
//! inside the runner with no path back to whoever started the lane. Threading a URL through every
//! caller for one optional string would touch far more upstream code than a `OnceLock` does.
//!
//! It lives in *this* crate rather than in `gx-remote-api` so the hooks crate keeps no dependency
//! on the API crate (which depends on `xai-grok-shell`, which is downstream of this one — the
//! reverse edge would be a cycle). The lane's **host** announces; see
//! `xai-grok-pager/src/gx_remote_lane.rs`.
//!
//! Set once, at bind time. A process that never starts a lane never calls [`announce`], and
//! [`url`] stays `None` so `to_hook_json` emits no `gxRemote` key at all — an unannounced payload
//! is byte-for-byte what upstream produces.

use std::sync::OnceLock;

/// The lane's base URL (`http://127.0.0.1:<port>`), once it is bound.
static URL: OnceLock<String> = OnceLock::new();

/// Publish the lane's base URL. First call wins; later calls are ignored.
///
/// Idempotent on purpose: only one lane runs per process, and a caller that retries a failed
/// startup must not be able to leave a second, stale URL in the payloads.
pub fn announce(url: &str) {
    let _ = URL.set(url.to_string());
}

/// The announced base URL, or `None` in a process with no lane.
pub fn url() -> Option<&'static str> {
    URL.get().map(String::as_str)
}

#[cfg(test)]
#[path = "gx_remote_tests.rs"]
mod tests;
