//! Shared state behind every handler.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::OnceCell;

use crate::acp_client::AcpClient;
use crate::auth::Token;

/// What `GET /v1/healthz` reports. Fixed for the life of the lane.
#[derive(Debug, Clone)]
pub struct HealthInfo {
    pub version: String,
    /// PID of the leader process hosting the lane.
    pub leader_pid: u32,
    pub instance_id: String,
}

/// Everything a handler needs. Held behind an `Arc` as axum's router state.
pub struct AppState {
    pub acp: AcpClient,
    pub token: Token,
    pub health: HealthInfo,
    pub attachments: Attachments,
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

    /// Has a `session/load` for `session_id` completed successfully?
    pub fn is_attached(&self, session_id: &str) -> bool {
        let slots = self.slots.lock().unwrap();
        slots.get(session_id).is_some_and(|slot| slot.initialized())
    }

    /// Ids of every successfully attached session. Used to stamp `attached` on roster rows, so it
    /// is a set: the roster is scanned against it once per row.
    pub fn attached_ids(&self) -> HashSet<String> {
        let slots = self.slots.lock().unwrap();
        slots
            .iter()
            .filter(|(_, slot)| slot.initialized())
            .map(|(id, _)| id.clone())
            .collect()
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
        assert_eq!(
            attachments.attached_ids(),
            HashSet::from(["s1".to_string()])
        );
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
