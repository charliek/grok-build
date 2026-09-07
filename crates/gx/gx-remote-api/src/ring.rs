//! The live-frame ring: what an SSE client can be resumed from without touching the disk.
//!
//! Every session-scoped notification the lane receives is normalized once and kept here, per
//! session, so a phone that drops its connection for a few seconds gets the exact frames it missed
//! from memory instead of re-reading `updates.jsonl`. Two bounds keep it from growing without
//! limit: [`PER_SESSION_CAPACITY`] frames for any one session, and [`GLOBAL_CAPACITY`] across all
//! of them, evicting the globally oldest frame first (a single busy session must not be able to
//! starve every other session's resume window, and thirty idle sessions must not be able to pin
//! 60,000 frames between them).
//!
//! # Ordering, and what is *not* promised
//!
//! Frames are ordered by the counter suffix of `params._meta.eventId` (`"<sessionId>-<counter>"`).
//! That counter is **process-global across sessions**
//! (`xai-grok-shell-base/src/util/event_id.rs`), so a single session's counters are strictly
//! increasing but full of gaps — every gap is another session's event. The ring therefore promises
//! *order* and *exact set*, never contiguity; anything that tries to detect a missed frame by
//! looking for a gap will see one on every busy machine.
//!
//! A frame with no `eventId` (older lines, and the non-persisted `pending_interaction` /
//! `interaction_resolved` notifications) is still stored and still delivered live — it simply
//! cannot be a resume point. See [`EventRing::entries_after`] for how those are replayed.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::envelope::NormalizedEnvelope;

/// Frames kept for any one session.
pub const PER_SESSION_CAPACITY: usize = 2_000;

/// Frames kept across every session. The oldest frame anywhere is evicted first.
pub const GLOBAL_CAPACITY: usize = 50_000;

/// Split an `eventId` into its session prefix and its counter.
///
/// The format is `"<sessionId>-<counter>"` and a session id may itself contain `-`, so the split is
/// on the **last** `-`. Returns `None` for anything that does not end in a `-`-separated integer,
/// which is how a malformed resume cursor is detected.
pub fn split_event_id(event_id: &str) -> Option<(&str, u64)> {
    let (prefix, counter) = event_id.rsplit_once('-')?;
    let counter = counter.parse().ok()?;
    Some((prefix, counter))
}

/// The counter suffix of an `eventId`, ignoring which session it names.
pub fn counter_of(event_id: &str) -> Option<u64> {
    split_event_id(event_id).map(|(_, counter)| counter)
}

struct Entry {
    /// Ring-local arrival sequence. Distinct from the event counter: every frame has one, even the
    /// frames that carry no `eventId`, so it can order the global eviction index exactly.
    seq: u64,
    envelope: Arc<NormalizedEnvelope>,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, VecDeque<Entry>>,
    /// Arrival sequence -> session id, so the globally oldest frame is `order.first_key_value()`.
    order: BTreeMap<u64, String>,
    next_seq: u64,
}

/// A bounded, per-session store of recent normalized frames.
///
/// Cheap to share: one mutex over the whole structure. Every operation is O(log n) or O(frames
/// returned), and the lock is never held across an `await`.
pub struct EventRing {
    inner: Mutex<Inner>,
    per_session: usize,
    global: usize,
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}

impl EventRing {
    /// A ring with the production bounds.
    pub fn new() -> Self {
        Self::with_capacity(PER_SESSION_CAPACITY, GLOBAL_CAPACITY)
    }

    /// A ring with explicit bounds. Tests use tiny ones so eviction is reachable without pushing
    /// fifty thousand frames.
    pub fn with_capacity(per_session: usize, global: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            per_session: per_session.max(1),
            global: global.max(1),
        }
    }

    /// Store one frame for `session_id`, evicting to stay inside both bounds.
    pub fn push(&self, session_id: &str, envelope: NormalizedEnvelope) {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.next_seq;
        inner.next_seq += 1;

        inner
            .sessions
            .entry(session_id.to_string())
            .or_default()
            .push_back(Entry {
                seq,
                envelope: Arc::new(envelope),
            });
        inner.order.insert(seq, session_id.to_string());

        // Per-session bound first: it can only ever drop this session's own oldest frame.
        while inner
            .sessions
            .get(session_id)
            .is_some_and(|q| q.len() > self.per_session)
        {
            let dropped = inner
                .sessions
                .get_mut(session_id)
                .and_then(VecDeque::pop_front);
            if let Some(entry) = dropped {
                inner.order.remove(&entry.seq);
            }
        }

        // Then the global bound, oldest frame anywhere first.
        while inner.order.len() > self.global {
            let Some((&seq, owner)) = inner.order.iter().next() else {
                break;
            };
            let owner = owner.clone();
            inner.order.remove(&seq);
            if let Some(queue) = inner.sessions.get_mut(&owner) {
                queue.pop_front();
                if queue.is_empty() {
                    inner.sessions.remove(&owner);
                }
            }
        }
    }

    /// `(oldest, newest)` event counters held for `session_id`, or `None` when the ring holds no
    /// frame for it that carries an `eventId`.
    ///
    /// Both are read under one lock so a concurrent push cannot produce a pair that never existed.
    pub fn bounds(&self, session_id: &str) -> Option<(u64, u64)> {
        let inner = self.inner.lock().unwrap();
        let queue = inner.sessions.get(session_id)?;
        let mut bounds: Option<(u64, u64)> = None;
        for entry in queue {
            let Some(counter) = entry.envelope.counter() else {
                continue;
            };
            bounds = Some(match bounds {
                None => (counter, counter),
                Some((oldest, newest)) => (oldest.min(counter), newest.max(counter)),
            });
        }
        bounds
    }

    /// The frames to replay to a client resuming at `cursor`.
    ///
    /// Arrival order is preserved, starting immediately after the **last** frame the cursor proves
    /// the client already has — the newest frame whose counter is `<= cursor`.
    ///
    /// Cutting there rather than at the first frame with a greater counter is what keeps the
    /// id-less frames. A `pending_interaction` that arrives between two persisted events has no
    /// counter of its own; cutting on the next counter-bearing frame would drop it, and it is
    /// exactly the kind of frame `/history` cannot hand back either, because it is never persisted.
    /// Cutting on the last *seen* frame delivers it, in place, in order.
    ///
    /// When the cursor is older than everything held (nothing has a counter `<= cursor`) the whole
    /// ring is returned: the caller's disk read covered the frames before it.
    pub fn entries_after(&self, session_id: &str, cursor: u64) -> Vec<Arc<NormalizedEnvelope>> {
        let inner = self.inner.lock().unwrap();
        let Some(queue) = inner.sessions.get(session_id) else {
            return Vec::new();
        };
        let start = queue
            .iter()
            .rposition(|entry| entry.envelope.counter().is_some_and(|c| c <= cursor))
            .map_or(0, |index| index + 1);
        queue
            .iter()
            .skip(start)
            .map(|entry| entry.envelope.clone())
            .collect()
    }

    /// Total frames held across every session.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop every frame, for every session.
    ///
    /// Called when the pump's broadcast receiver reports `Lagged`: the ring has then missed a run
    /// of events, and a resume whose cursor sits *before* the hole would otherwise satisfy the
    /// in-ring check (`cursor >= oldest`) and be replayed straight across the gap, silently
    /// skipping what was dropped. Emptying the ring makes every resume take the persisted path
    /// (or answer `cursor_unresolvable`), which is slower and always correct. Live delivery to
    /// already-connected streams is a separate subscription and is unaffected.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.sessions.clear();
        inner.order.clear();
    }

    /// Frames held for one session.
    pub fn session_len(&self, session_id: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .get(session_id)
            .map_or(0, VecDeque::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(session_id: &str, counter: u64) -> NormalizedEnvelope {
        NormalizedEnvelope::from_stored(&json!({
            "timestamp": 1,
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "_meta": { "eventId": format!("{session_id}-{counter}") },
            }
        }))
    }

    fn idless_frame(session_id: &str) -> NormalizedEnvelope {
        NormalizedEnvelope::from_stored(&json!({
            "timestamp": 1,
            "method": "x.ai/session_notification",
            "params": { "sessionId": session_id, "update": { "sessionUpdate": "pending_interaction" } }
        }))
    }

    fn counters(frames: &[Arc<NormalizedEnvelope>]) -> Vec<Option<u64>> {
        frames.iter().map(|f| f.counter()).collect()
    }

    #[test]
    fn an_event_id_splits_on_its_last_dash() {
        // Session ids contain dashes of their own; only the final one separates the counter.
        assert_eq!(split_event_id("01a0-b2c3-57"), Some(("01a0-b2c3", 57)));
        assert_eq!(counter_of("sess-1-0"), Some(0));
        assert_eq!(counter_of("sess-1"), Some(1));
    }

    #[test]
    fn a_malformed_event_id_has_no_counter() {
        for bad in [
            "",
            "no-dash-here-x",
            "sess-",
            "sess-1.5",
            "sess-99999999999999999999",
        ] {
            assert_eq!(counter_of(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn entries_after_returns_exactly_the_frames_past_the_cursor_in_order() {
        let ring = EventRing::new();
        for counter in [10, 11, 12] {
            ring.push("s1", frame("s1", counter));
        }
        assert_eq!(
            counters(&ring.entries_after("s1", 10)),
            vec![Some(11), Some(12)]
        );
        assert_eq!(counters(&ring.entries_after("s1", 12)), Vec::new());
        assert_eq!(
            counters(&ring.entries_after("s1", 0)),
            vec![Some(10), Some(11), Some(12)]
        );
        assert_eq!(ring.entries_after("nobody", 0).len(), 0);
    }

    #[test]
    fn interleaved_counters_from_another_session_do_not_disturb_a_sessions_order() {
        // The counter is process-global, so a session's own counters are sparse. Order and the
        // exact set are the promise; contiguity is not.
        let ring = EventRing::new();
        ring.push("s1", frame("s1", 1));
        ring.push("s2", frame("s2", 2));
        ring.push("s1", frame("s1", 3));
        ring.push("s2", frame("s2", 4));
        ring.push("s1", frame("s1", 5));

        assert_eq!(
            counters(&ring.entries_after("s1", 1)),
            vec![Some(3), Some(5)]
        );
        assert_eq!(counters(&ring.entries_after("s2", 2)), vec![Some(4)]);
        assert_eq!(ring.bounds("s1"), Some((1, 5)));
        assert_eq!(ring.bounds("s2"), Some((2, 4)));
    }

    #[test]
    fn an_id_less_frame_rides_along_after_the_cursor_but_never_before_it() {
        let ring = EventRing::new();
        ring.push("s1", idless_frame("s1")); // before any cursor position
        ring.push("s1", frame("s1", 10));
        ring.push("s1", idless_frame("s1")); // after 10
        ring.push("s1", frame("s1", 11));

        assert_eq!(
            counters(&ring.entries_after("s1", 10)),
            vec![None, Some(11)],
            "a non-persisted frame after the cursor is the client's only chance to see it"
        );
        // A cursor older than anything held replays the whole ring; the caller's disk read is what
        // covers the frames before it.
        assert_eq!(
            counters(&ring.entries_after("s1", 9)),
            vec![None, Some(10), None, Some(11)]
        );
        // …and a cursor at the newest frame replays nothing at all.
        assert_eq!(counters(&ring.entries_after("s1", 11)), Vec::new());
    }

    #[test]
    fn bounds_ignore_frames_that_carry_no_event_id() {
        let ring = EventRing::new();
        ring.push("s1", idless_frame("s1"));
        assert_eq!(
            ring.bounds("s1"),
            None,
            "nothing here can be a resume point"
        );
        ring.push("s1", frame("s1", 7));
        assert_eq!(ring.bounds("s1"), Some((7, 7)));
        assert_eq!(ring.bounds("never-seen"), None);
    }

    #[test]
    fn the_per_session_bound_drops_that_sessions_oldest_and_nobody_elses() {
        let ring = EventRing::with_capacity(3, 100);
        for counter in 1..=5 {
            ring.push("s1", frame("s1", counter));
        }
        ring.push("s2", frame("s2", 99));

        assert_eq!(ring.session_len("s1"), 3);
        assert_eq!(ring.session_len("s2"), 1, "s1's churn must not evict s2");
        assert_eq!(ring.bounds("s1"), Some((3, 5)));
        assert_eq!(ring.len(), 4);
    }

    #[test]
    fn the_global_bound_evicts_the_oldest_frame_across_sessions() {
        let ring = EventRing::with_capacity(100, 3);
        ring.push("s1", frame("s1", 1)); // oldest anywhere
        ring.push("s2", frame("s2", 2));
        ring.push("s1", frame("s1", 3));
        ring.push("s2", frame("s2", 4)); // pushes the global count to 4

        assert_eq!(ring.len(), 3);
        assert_eq!(
            ring.bounds("s1"),
            Some((3, 3)),
            "s1's counter-1 frame was the globally oldest and had to go"
        );
        assert_eq!(ring.bounds("s2"), Some((2, 4)));
    }

    #[test]
    fn a_session_emptied_by_the_global_bound_is_forgotten_entirely() {
        let ring = EventRing::with_capacity(100, 2);
        ring.push("s1", frame("s1", 1));
        ring.push("s2", frame("s2", 2));
        ring.push("s2", frame("s2", 3));

        assert_eq!(ring.session_len("s1"), 0);
        assert_eq!(ring.bounds("s1"), None);
        assert_eq!(ring.session_len("s2"), 2);
        // The eviction index and the per-session queues stayed consistent.
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn the_production_bounds_are_the_plans_bounds() {
        assert_eq!(PER_SESSION_CAPACITY, 2_000);
        assert_eq!(GLOBAL_CAPACITY, 50_000);
    }

    /// A lagged broadcast leaves the ring with a hole, and `bounds` cannot express one: it reports
    /// a single contiguous span, so a resume whose cursor sits before the hole would pass the
    /// in-ring check and be replayed straight across the gap. `clear` is what the pump uses to make
    /// that impossible — afterwards there are no bounds at all, so every resume takes the
    /// persisted path.
    ///
    /// The pump's own `Lagged` arm is not unit-tested here: forcing a real broadcast overflow means
    /// racing a spawned task against a channel bound, which is not deterministic. This pins the
    /// mechanism the arm relies on.
    #[test]
    fn clearing_the_ring_removes_the_bounds_a_resume_would_have_trusted() {
        let ring = EventRing::new();
        for counter in [10_u64, 11, 12] {
            ring.push("s", frame("s", counter));
        }
        assert_eq!(ring.bounds("s"), Some((10, 12)));
        // A cursor inside those bounds is served from the ring, never from disk.
        assert!(!ring.entries_after("s", 10).is_empty());

        ring.clear();

        assert_eq!(
            ring.bounds("s"),
            None,
            "after a lag the ring must not claim to span the hole"
        );
        assert!(ring.entries_after("s", 10).is_empty());
        assert!(ring.is_empty());
        assert_eq!(ring.session_len("s"), 0);
    }
}
