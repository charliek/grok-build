//! `GET /v1/sessions/{id}/events` — the live stream, and the resume that makes it survive a tunnel.
//!
//! # Frames
//!
//! | `event:` | `id:` | `data` | meaning |
//! |---|---|---|---|
//! | `update` | the full `eventId`, when the frame has one | [`NormalizedEnvelope`] | one session event, same shape `/history` returns |
//! | `session` | never | the session's summary row | the roster changed; re-fetch `/v1/sessions/{id}` |
//! | `approval` | never | the approval resource | an interaction opened, was answered, or resolved |
//! | `reset` | never | `{"reason": …}` | the client's view is not resumable; re-fetch |
//!
//! Only `update` carries an `id:`, and it carries the **whole** opaque string (`01a0…-57`), never
//! the counter alone — the counter is only ever parsed for ordering, and a client that echoed a
//! bare number back in `Last-Event-ID` would be sending a cursor this API cannot resolve. `session`
//! and `approval` are state *invalidations*: they say something changed, the client re-reads it
//! over the ordinary GET routes, and giving them an `id:` would let a browser resume from a cursor
//! that is not a session event at all.
//!
//! # Resume
//!
//! The client sends the last `update` id it saw in `Last-Event-ID` (which `EventSource` does by
//! itself). [`plan_replay`] evaluates the plan's four cases **in order**, because they overlap: a
//! cursor can be both newer than everything known *and* older than the ring's oldest, and only the
//! order says which answer wins.
//!
//! Per-session contiguity is **not** promised. The counter is process-global
//! (`xai-grok-shell-base/src/util/event_id.rs`), so one session's ids are sparse; the promise is
//! order and exact set. What makes a cursor survive a leader restart is `ensure_event_counter_at_
//! least`, which re-seeds the global counter above the persisted maximum on session load.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;
use xai_grok_shell::agent::roster::{RosterChanged, RosterEntry};

use crate::acp_client::Notification;
use crate::approvals::Approval;
use crate::envelope::NormalizedEnvelope;
use crate::error::ApiError;
use crate::ring::split_event_id;
use crate::routes::history::fetch_updates;
use crate::routes::sessions::{SessionSummary, ensure_attached, resolve_session, summarize};
use crate::state::AppState;

/// The two reasons a stream tells its client to start over.
mod reason {
    /// The cursor cannot be placed in this session's history at all.
    pub const CURSOR_UNRESOLVABLE: &str = "cursor_unresolvable";
    /// The connection fell far enough behind that frames were dropped.
    pub const SLOW_CONSUMER: &str = "slow_consumer";
}

/// How far back the disk path reads when a cursor predates the ring.
///
/// `None` means the whole transcript — the same read `/history` performs with no bounds. A resume
/// this far back happens once, on a reconnect after a long outage, and reading short would silently
/// drop the frames in between; the plan's step 4 says "read `x.ai/session/updates`", and this is it.
const DISK_REPLAY_LIMIT: Option<u64> = None;

/// Tail size for the "what is the newest id in this session" probe.
///
/// `x.ai/session/updates` reports `lastEventId` by reverse-scanning **the page it returned**
/// (`extensions/session_updates.rs::extract_last_event_id`), so the probe has to ask for the tail,
/// and for more than one line: the very last line may be one of the exceptional ones that carries
/// no id.
const NEWEST_PROBE_TAIL: i64 = -64;

/// One thing the stream can emit.
#[derive(Debug, Clone)]
enum Frame {
    Update(Arc<NormalizedEnvelope>),
    Session(Value),
    Approval(Arc<Approval>),
    Reset(&'static str),
}

impl Frame {
    fn into_event(self) -> Event {
        match self {
            Self::Update(envelope) => {
                let id = envelope.event_id.as_deref().filter(|id| is_sse_safe(id));
                json_event("update", id, &*envelope)
            }
            Self::Session(value) => json_event("session", None, &value),
            Self::Approval(approval) => json_event("approval", None, &*approval),
            Self::Reset(reason) => json_event("reset", None, &json!({ "reason": reason })),
        }
    }
}

/// An SSE event named `name`, optionally carrying `id`, whose `data` is `value` as JSON.
///
/// Serialization of any of our own types is infallible in practice; a failure still must not take
/// the connection down, so it degrades to an empty object and says so in the log.
fn json_event<T: serde::Serialize>(name: &str, id: Option<&str>, value: &T) -> Event {
    let mut event = Event::default().event(name);
    if let Some(id) = id {
        event = event.id(id);
    }
    match event.json_data(value) {
        Ok(event) => event,
        Err(err) => {
            warn!(%err, name, "gx-remote-api: could not serialize an SSE frame");
            let mut event = Event::default().event(name);
            if let Some(id) = id {
                event = event.id(id);
            }
            event.data("{}")
        }
    }
}

/// `Event::id` **panics** on a newline, carriage return or NUL, which would take the connection's
/// task down. An `eventId` is `"<sessionId>-<counter>"` and can never contain one, so this only
/// ever fires if the leader's id format changes underneath us — and then the frame is delivered
/// without an `id:` (unresumable, but present) rather than not at all.
fn is_sse_safe(id: &str) -> bool {
    !id.contains(['\r', '\n', '\0'])
}

/// `GET /v1/sessions/{id}/events`
pub async fn get_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    // 404 before anything else, and attach exactly as the history route does: a stream for a
    // session the lane never loaded would be a stream of nothing.
    let session = resolve_session(&state, &id).await?;
    ensure_attached(&state, &id, &session.cwd).await?;

    // Subscribe *before* reading the ring or the disk. Anything that arrives while the replay is
    // being computed is then either already in the replay or still in this receiver; the
    // `last_emitted` counter below decides which, so nothing is lost and nothing is duplicated.
    let live = state.acp.subscribe();
    // Approvals are a separate stream because they are not notifications at all: a reverse-request
    // never reaches the notification fan-out, and a POST that answers one happens on an HTTP task.
    let approvals = state.approvals.subscribe();

    let cursor = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let (reset, replay) = plan_replay(&state, &id, &session, cursor.as_deref()).await?;

    let last_emitted = replay.iter().filter_map(|env| env.counter()).max();

    let (tx, rx) = mpsc::channel(state.sse.queue_capacity.max(1));
    let mut opening = Vec::with_capacity(replay.len() + 1);
    opening.extend(reset.map(Frame::Reset));
    opening.extend(replay.into_iter().map(Frame::Update));

    tokio::spawn(pump(
        Streamer {
            session_id: id,
            state: state.clone(),
            last_emitted,
        },
        opening,
        live,
        approvals,
        tx,
    ));

    // `keep_alive` wraps the stream in a type of its own, so the handler answers an erased
    // `Response` rather than naming it.
    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(
            KeepAlive::new()
                .interval(state.sse.keepalive)
                .text("keepalive"),
        )
        .into_response())
}

/// Per-connection state the producer task carries.
struct Streamer {
    session_id: String,
    state: Arc<AppState>,
    /// Highest counter already handed to this client. Live frames at or below it were part of the
    /// replay and must not be sent twice.
    last_emitted: Option<u64>,
}

/// The producer task: replay, then live, for as long as the client reads.
///
/// Backpressure is deliberately asymmetric. The **replay** is sent with `send().await`, because it
/// is finite and the client asked for exactly it — dropping part of a replay would defeat the whole
/// resume. The **live** stream is sent with `try_send`, because it is unbounded and a phone on a
/// stalled tunnel must never be able to make the lane buffer without limit. When live delivery
/// overflows the connection's queue the client gets one `reset { slow_consumer }` and the stream
/// continues from whatever is current, which is the plan's rule.
///
/// "One" is per **episode**, not per dropped frame: once the client has been told to re-sync,
/// further drops before it has caught up are the same piece of news, and a client that is already
/// behind is the last thing that should be sent more frames. The episode ends at the next frame
/// that is actually delivered, so a connection that falls behind twice is told twice.
async fn pump(
    mut streamer: Streamer,
    opening: Vec<Frame>,
    mut live: tokio::sync::broadcast::Receiver<Notification>,
    mut approvals: tokio::sync::broadcast::Receiver<Approval>,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    use tokio::sync::broadcast::error::RecvError;

    for frame in opening {
        if tx.send(Ok(frame.into_event())).await.is_err() {
            return; // client hung up mid-replay
        }
    }

    let cancel = streamer.state.acp.cancel_token();
    // Frames were dropped and the client has not been told yet.
    let mut overflowed = false;
    // A `reset` is outstanding: the client has been told to re-sync and has not caught up.
    let mut told = false;
    loop {
        if overflowed {
            // Wait for room rather than dropping the notice too: a client that never learns it fell
            // behind would keep a silently incomplete transcript forever.
            let Ok(permit) = tx.reserve().await else {
                return;
            };
            permit.send(Ok(Frame::Reset(reason::SLOW_CONSUMER).into_event()));
            overflowed = false;
            told = true;
            continue;
        }

        let frame = tokio::select! {
            () = cancel.cancelled() => return,
            () = tx.closed() => return,
            received = live.recv() => match received {
                Ok(notification) => streamer.frame_for(&notification),
                // The shared broadcast ring wrapped past this connection: the client's view has a
                // hole, which is the same condition as a full queue and gets the same answer.
                Err(RecvError::Lagged(_)) => { overflowed = !told; continue }
                Err(RecvError::Closed) => return,
            },
            received = approvals.recv() => match received {
                Ok(approval) => streamer.approval_frame(approval),
                // An approval this connection never saw open may already be answered; a re-fetch of
                // `/approvals` is exactly what `reset` asks for.
                Err(RecvError::Lagged(_)) => { overflowed = !told; continue }
                // Unreachable while this task holds `state`, which owns the store and its sender.
                Err(RecvError::Closed) => return,
            },
        };
        let Some(frame) = frame else {
            continue;
        };
        match tx.try_send(Ok(frame.into_event())) {
            // Delivered: the client is current again, so the next drop is a new episode.
            Ok(()) => told = false,
            Err(mpsc::error::TrySendError::Full(_)) => overflowed = !told,
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}

impl Streamer {
    /// `event: approval` for this session's approvals, and nothing for anyone else's.
    ///
    /// A **state invalidation**, exactly like `event: session`: it carries the approval resource
    /// the GET routes return and deliberately no `id:` line, because an approval is not a position
    /// in the session's event history and a browser must never resume from one.
    ///
    /// Every create, submit and resolve produces one. `pending_interaction` /
    /// `interaction_resolved` do *not* produce one from the notification path — they reach the
    /// store through [`crate::state::spawn_event_pump`] and come back out here as approval changes,
    /// so a client sees one frame per change rather than two views of it.
    fn approval_frame(&self, approval: Approval) -> Option<Frame> {
        (approval.session_id == self.session_id).then(|| Frame::Approval(Arc::new(approval)))
    }

    /// The frame this notification produces for *this* session, if any.
    fn frame_for(&mut self, notification: &Notification) -> Option<Frame> {
        match notification.session_id.as_deref() {
            Some(session_id) if session_id == self.session_id => {
                let envelope = NormalizedEnvelope::from_notification(notification);
                // Already replayed. Counters are globally monotonic, so `<=` is an exact test.
                if let (Some(counter), Some(last)) = (envelope.counter(), self.last_emitted)
                    && counter <= last
                {
                    return None;
                }
                if let Some(counter) = envelope.counter() {
                    self.last_emitted = Some(counter);
                }
                Some(Frame::Update(Arc::new(envelope)))
            }
            Some(_) => None,
            // Session-less: the roster broadcast is the only one that concerns a single session,
            // and only because of the entries it carries.
            None => self.roster_frame(notification),
        }
    }

    /// `x.ai/sessions/changed` is machine-wide (`leader/server.rs` broadcasts it to every client
    /// with no `sessionId`), so this stream has to look inside it for its own session.
    fn roster_frame(&self, notification: &Notification) -> Option<Frame> {
        if notification.method != xai_grok_shell::agent::roster::SESSIONS_CHANGED_METHOD {
            return None;
        }
        let changed: RosterChanged =
            serde_json::from_value(crate::envelope::inner_params(&notification.params).clone())
                .ok()?;

        if let Some(entry) = upserted_entry(&changed, &self.session_id) {
            let summary = summarize(&self.state, entry);
            return Some(Frame::Session(serde_json::to_value(summary).ok()?));
        }
        if changed.removed.iter().any(|id| id == &self.session_id) {
            // The row is gone; the client's next GET will say so authoritatively.
            return Some(Frame::Session(
                json!({ "sessionId": self.session_id, "removed": true }),
            ));
        }
        None
    }
}

fn upserted_entry<'a>(changed: &'a RosterChanged, session_id: &str) -> Option<&'a RosterEntry> {
    changed
        .upserted
        .iter()
        .find(|entry| entry.session_id == session_id)
}

/// Work out what to send before going live. Returns an optional leading `reset` reason and the
/// frames to replay.
///
/// The four cases are the plan's, evaluated **in this order**:
///
/// 1. a malformed cursor, or one whose session prefix is another session's → `cursor_unresolvable`;
/// 2. a cursor newer than anything known (the ring's max, or the disk's `lastEventId` when the ring
///    is empty) → `cursor_unresolvable`, because resuming would mean skipping events that do not
///    exist yet;
/// 3. a cursor inside the ring → replay from memory;
/// 4. otherwise the disk, then the ring's tail, deduplicated by `eventId` — the two overlap by
///    however much of the ring is also persisted.
///
/// No cursor at all is not a reset: a fresh `EventSource` just starts live.
///
/// Cost: cases 1–3 touch no store at all. A cold resume (an empty ring, which is what a lane that
/// has just started looks like) reads the store twice — once for the tail probe in (2) and once for
/// the transcript in (4) — because the probe deliberately reads only 64 lines and the replay needs
/// the rest. That is the one-off price of the first reconnect after a leader restart.
///
/// The dedup set in (4) is belt-and-braces next to the counter cut: a frame's arrival order and its
/// counter order are not guaranteed to agree (the counter is assigned by an atomic that several
/// sessions bump concurrently), so a ring frame *can* sit past the cut with a counter the disk page
/// also carried.
async fn plan_replay(
    state: &Arc<AppState>,
    session_id: &str,
    session: &SessionSummary,
    cursor: Option<&str>,
) -> Result<(Option<&'static str>, Vec<Arc<NormalizedEnvelope>>), ApiError> {
    let Some(raw) = cursor.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok((None, Vec::new()));
    };

    // (1) Malformed, or somebody else's session.
    let Some((prefix, cursor)) = split_event_id(raw) else {
        return Ok((Some(reason::CURSOR_UNRESOLVABLE), Vec::new()));
    };
    if prefix != session_id {
        return Ok((Some(reason::CURSOR_UNRESOLVABLE), Vec::new()));
    }

    // (2) Newer than anything this leader knows about.
    let bounds = state.ring.bounds(session_id);
    let newest = match bounds {
        Some((_, newest)) => Some(newest),
        None => newest_persisted(state, session_id, &session.cwd).await?,
    };
    match newest {
        Some(newest) if cursor > newest => {
            return Ok((Some(reason::CURSOR_UNRESOLVABLE), Vec::new()));
        }
        // Nothing known at all: an empty ring over a transcript with no ids. There is no position
        // to resume from, and pretending otherwise would drop whatever came before.
        None => return Ok((Some(reason::CURSOR_UNRESOLVABLE), Vec::new())),
        Some(_) => {}
    }

    // (3) Inside the ring.
    if let Some((oldest, _)) = bounds
        && cursor >= oldest
    {
        return Ok((None, state.ring.entries_after(session_id, cursor)));
    }

    // (4) Older than the ring: disk first, then whatever the ring holds beyond it.
    let page = fetch_updates(state, session_id, &session.cwd, None, DISK_REPLAY_LIMIT).await?;
    let mut frames: Vec<Arc<NormalizedEnvelope>> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut last = cursor;
    for envelope in page.updates {
        let Some(counter) = envelope.counter() else {
            continue;
        };
        if counter <= cursor {
            continue;
        }
        last = last.max(counter);
        if let Some(id) = envelope.event_id.clone() {
            seen.insert(id);
        }
        frames.push(Arc::new(envelope));
    }
    frames.extend(
        state
            .ring
            .entries_after(session_id, last)
            .into_iter()
            .filter(|envelope| {
                envelope
                    .event_id
                    .as_ref()
                    .is_none_or(|id| !seen.contains(id))
            }),
    );
    Ok((None, frames))
}

/// The newest persisted event counter for `session_id`, or `None` when the tail carries no id.
async fn newest_persisted(
    state: &Arc<AppState>,
    session_id: &str,
    cwd: &str,
) -> Result<Option<u64>, ApiError> {
    let page = fetch_updates(state, session_id, cwd, Some(NEWEST_PROBE_TAIL), None).await?;
    Ok(page
        .last_event_id
        .as_deref()
        .and_then(crate::ring::counter_of))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `axum::response::sse::Event` cannot be rendered outside a response, so everything about the
    /// *bytes* a frame produces — the `id:` line, the event name, the reset reason — is asserted
    /// against the real HTTP body in [`crate::router_tests`]. What is unit-testable here is the
    /// decision logic that feeds it.
    fn roster_changed(upserted: &[&str], removed: &[&str]) -> RosterChanged {
        serde_json::from_value(json!({
            "upserted": upserted.iter().map(|id| json!({
                "sessionId": id,
                "cwd": "/repo",
                "isWorktree": false,
                "yolo": false,
                "activity": "working",
                "resident": true,
                "lastChangeUnixMs": 1,
                "origin": { "kind": "local" }
            })).collect::<Vec<_>>(),
            "removed": removed,
        }))
        .unwrap()
    }

    #[test]
    fn a_roster_broadcast_is_matched_on_the_entries_it_carries() {
        // `x.ai/sessions/changed` has no sessionId at all, so a stream can only tell whether it
        // concerns its own session by looking inside.
        let changed = roster_changed(&["sess-1", "sess-2"], &[]);
        assert_eq!(
            upserted_entry(&changed, "sess-2").map(|e| e.session_id.as_str()),
            Some("sess-2")
        );
        assert!(upserted_entry(&changed, "sess-9").is_none());
    }

    #[test]
    fn an_event_id_that_could_break_the_wire_is_dropped_not_attached() {
        assert!(is_sse_safe("01a0-b2-57"));
        assert!(!is_sse_safe("sess\n-1"));
        assert!(!is_sse_safe("sess\r-1"));
        assert!(!is_sse_safe("sess\0-1"));
    }
}
