//! Shared state behind every handler.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::acp_client::AcpClient;
use crate::approvals::ApprovalStore;
use crate::auth::Token;
use crate::envelope::NormalizedEnvelope;
use crate::ring::EventRing;

/// What `GET /v1/healthz` reports. Fixed for the life of the lane.
#[derive(Debug, Clone)]
pub struct HealthInfo {
    pub version: String,
    /// PID of the leader process hosting the lane.
    pub leader_pid: u32,
    pub instance_id: String,
}

/// Tunables for the SSE lane.
///
/// Both are fields rather than constants for one reason each. The keepalive interval is 15 s in
/// production, which no test can afford to wait for; the per-connection queue is 256 frames, which
/// a test would have to overflow with 257 real frames to exercise the slow-consumer path. Making
/// them settings keeps both behaviours reachable without a `sleep` or a synthetic flood.
#[derive(Debug, Clone)]
pub struct SseSettings {
    /// Gap after which a `: keepalive` comment is emitted. Reset by every real frame.
    pub keepalive: Duration,
    /// Frames a single connection may fall behind by before it is told to re-sync.
    pub queue_capacity: usize,
}

impl Default for SseSettings {
    fn default() -> Self {
        Self {
            keepalive: Duration::from_secs(15),
            queue_capacity: 256,
        }
    }
}

/// Everything a handler needs. Held behind an `Arc` as axum's router state.
pub struct AppState {
    pub acp: AcpClient,
    pub token: Token,
    pub health: HealthInfo,
    /// Shared with the [`ApprovalStore`], which declines any interaction for a session this lane
    /// has not attached to — hence the `Arc` rather than a plain field.
    pub attachments: Arc<Attachments>,
    /// Open and recently-resolved interactions. The same handle the ACP link task captures into.
    pub approvals: Arc<ApprovalStore>,
    /// Recent live frames, per session. Filled by [`spawn_event_pump`], read by the SSE resume.
    pub ring: EventRing,
    pub sse: SseSettings,
}

/// Keep [`AppState::ring`] filled from the leader's notification fan-out.
///
/// One task for the whole lane, not one per SSE connection: the ring is what makes a *reconnect*
/// cheap, so it has to be filling while nobody is connected at all. Live delivery to a connected
/// client is a separate subscription — see [`crate::routes::events`] — because a client that is
/// only ever fed from the ring would have to poll it.
///
/// Only sessions this lane has attached are stored. The leader fans out on subscription, so in
/// practice nothing else arrives; the check is what keeps a stray broadcast from spending the
/// global cap on a session no client can ask about.
///
/// It is also where `pending_interaction` / `interaction_resolved` reach the approval store
/// ([`ApprovalStore::observe`]) — here rather than per SSE connection, because an interaction has
/// to resolve whether or not anybody is watching the stream.
///
/// The task ends with `cancel`, or when the link closes and the broadcast sender drops with it.
pub fn spawn_event_pump(state: Arc<AppState>, cancel: CancellationToken) {
    let mut notifications = state.acp.subscribe();
    tokio::spawn(async move {
        loop {
            let notification = tokio::select! {
                () = cancel.cancelled() => break,
                received = notifications.recv() => received,
            };
            match notification {
                Ok(notification) => {
                    state.approvals.observe(&notification);
                    let Some(session_id) = notification.session_id.clone() else {
                        // Machine-wide broadcasts (`x.ai/sessions/changed` and friends) are not
                        // session frames; SSE renders them as `event: session` from its own
                        // subscription instead of storing them.
                        continue;
                    };
                    if !state.attachments.is_attached(&session_id) {
                        continue;
                    }
                    state.ring.push(
                        &session_id,
                        NormalizedEnvelope::from_notification(&notification),
                    );
                }
                // The pump fell behind and the broadcast dropped `missed` events, so the ring
                // now has a hole. It is NOT harmless: `bounds` still reports one contiguous span,
                // so a resume whose cursor sits before the hole passes the in-ring check and is
                // replayed straight across the gap, silently skipping every dropped event. Empty
                // the ring instead — every resume then takes the persisted path, which has the
                // dropped events (they are the same `session/update` notifications the agent
                // writes to `updates.jsonl`) or answers `cursor_unresolvable`.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    state.ring.clear();
                    warn!(
                        missed,
                        "gx-remote-api: event ring lagged the leader's fan-out; cleared the ring so no resume replays across the hole"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    debug!("gx-remote-api: event ring stopping, leader link closed");
                    break;
                }
            }
        }
    });
}

/// Which sessions this lane has `session/load`-ed, and the machinery that guarantees it loads each
/// one exactly once.
///
/// Attachment is lazy (plan D3): the first request that touches a session's *content* sends
/// `session/load` and awaits it. Two concurrent first-touches must not send two loads — a second
/// load of a resident session makes the agent flush and replay — so each session gets a
/// [`OnceCell`]: the first caller runs the load, the rest await the same result. A failed load does
/// not poison the cell, so the next request retries rather than being stuck on
/// `leader_unavailable` forever.
#[derive(Default)]
pub struct Attachments {
    slots: Mutex<HashMap<String, Arc<OnceCell<()>>>>,
}

impl Attachments {
    /// The (possibly new) slot for `session_id`.
    pub fn slot(&self, session_id: &str) -> Arc<OnceCell<()>> {
        let mut slots = self.slots.lock().unwrap();
        slots
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone()
    }

    /// Record `session_id` as attached without sending a `session/load`.
    ///
    /// For `POST /v1/sessions`: `session/new` already subscribes this client to the session it
    /// created, so a `session/load` on the phone's next touch would be a redundant round trip that
    /// makes the agent flush and replay a session that has not said anything yet.
    pub fn mark_attached(&self, session_id: &str) {
        // `set` fails only when the cell is already initialized, which is the same end state.
        let _ = self.slot(session_id).set(());
    }

    /// Has a `session/load` for `session_id` completed successfully?
    pub fn is_attached(&self, session_id: &str) -> bool {
        let slots = self.slots.lock().unwrap();
        slots.get(session_id).is_some_and(|slot| slot.initialized())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_session_is_attached_only_after_a_successful_load() {
        let attachments = Attachments::default();
        assert!(!attachments.is_attached("s1"));

        // A slot alone is not an attachment.
        let slot = attachments.slot("s1");
        assert!(!attachments.is_attached("s1"));

        // A failed load leaves the cell uninitialized, so the next request retries.
        let failed: Result<(), &str> = slot
            .get_or_try_init(|| async { Err("boom") })
            .await
            .map(|_| ());
        assert!(failed.is_err());
        assert!(!attachments.is_attached("s1"));

        slot.get_or_try_init(|| async { Ok::<(), &str>(()) })
            .await
            .unwrap();
        assert!(attachments.is_attached("s1"));
        assert!(!attachments.is_attached("s2"));
    }

    #[tokio::test]
    async fn concurrent_first_touches_load_once() {
        let attachments = Arc::new(Attachments::default());
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let attachments = attachments.clone();
            let loads = loads.clone();
            handles.push(tokio::spawn(async move {
                let slot = attachments.slot("s1");
                slot.get_or_try_init(|| async {
                    loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok::<(), &str>(())
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            loads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a second session/load would make the agent flush and replay"
        );
    }
}
