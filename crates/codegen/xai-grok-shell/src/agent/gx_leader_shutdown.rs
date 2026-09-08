//! gx: the process-global seam a signal handler uses to end a leader's sessions gracefully.
//!
//! A leader that is terminated by a signal used to skip its sessions entirely: the pager's signal
//! task calls `shutdown_and_flush_telemetry`, which `std::process::exit`s, so no session actor was
//! ever asked to shut down and no `SessionEnd` hook ran. roost releases a tab's ownership only on
//! `SessionEnd`, so every tab a killed leader owned stayed owned by a dead session (issue #14).
//!
//! [`register`] hands this module the leader's root cancellation token and its [`AgentActivity`];
//! [`request`] is what a signal handler calls to spend the flush grace before the process exits.
//! `gx leader kill` does not go through here — it asks over the leader socket
//! (`ControlCommand::Shutdown`) — but both paths end in the same two steps, in the same order.
//!
//! # The order is the whole point
//!
//! [`AgentActivity::flush_all_sessions`] sends `SessionCommand::Shutdown` to every live session
//! actor and waits for those actors to exit. The actors are `spawn_local` tasks on the `LocalSet`
//! that `run_leader` drives with `run_until(select! { .., cancel.cancelled() })`. Once the root
//! token is cancelled, that `LocalSet` is never polled again — so a flush issued *after* the cancel
//! would burn its whole grace and still run nothing. Flush first, cancel second; the leader
//! server's relaunch drain (`spawn_relaunch_drain`) has always done it in that order, and
//! [`AgentActivity::flush_all_sessions`]'s own docs state the constraint.
//!
//! # Why a process-global
//!
//! The signal task is spawned in `main` before any mode has been chosen, and never gets a handle to
//! the leader's internals. Only `run_leader` registers; every other mode (TUI, stdio, headless,
//! one-shot CLI) leaves the cell empty and [`request`] returns `false` immediately, so nothing but
//! a leader ever pays a flush deadline on the way out.
//!
//! # The flush flag
//!
//! Flushing first only helps if nothing *else* cancels the root token while the flush runs, and the
//! leader has other paths that do exactly that — most sharply the server's exit-on-disconnect check,
//! which ends `run_leader_server` (and so drops the root token's guard) the moment the last
//! non-observer client goes away. [`is_flushing`] is the flag those paths consult; every shutdown
//! path that flushes runs its flush *and* the cancel that follows it inside [`during_flush`], so
//! there is exactly one place the flag is set and cleared.

use std::future::Future;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use super::activity::{AgentActivity, SESSION_FLUSH_GRACE};

/// gx: set while some path is flushing a leader's sessions on the way out.
///
/// A plain `bool` rather than a counter, deliberately: two overlapping flushes would let the first
/// one to finish clear the flag early, but by then that flush has already cancelled the root token,
/// so the `LocalSet` the flag exists to protect is gone anyway and there is nothing left to guard.
static FLUSHING: AtomicBool = AtomicBool::new(false);

/// Is a leader shutdown flushing its sessions right now?
///
/// Read by any lifecycle path that would otherwise cancel the leader's root `CancellationToken`:
/// doing that mid-flush stops the `LocalSet` the session actors run on, abandoning the `SessionEnd`
/// hooks the flush exists to run. See the module docs.
pub fn is_flushing() -> bool {
    FLUSHING.load(Ordering::SeqCst)
}

/// Clears [`FLUSHING`] on drop, so a cancelled or panicking flush cannot leave the flag stuck on —
/// which would pin a leader alive past its last client forever.
struct FlushMarker;

impl Drop for FlushMarker {
    fn drop(&mut self) {
        FLUSHING.store(false, Ordering::SeqCst);
    }
}

/// gx: run a leader shutdown sequence with [`is_flushing`] reporting `true`.
///
/// `f` must cover the whole sequence — the session flush *and* the cancel that follows it — because
/// the window that needs protecting ends only once the token has been cancelled by this path. Used
/// by both flushing shutdown paths: [`LeaderShutdown::run`] (signals) and the leader server's
/// `spawn_gx_shutdown_drain` (`ControlCommand::Shutdown`).
pub async fn during_flush<F: Future>(f: F) -> F::Output {
    FLUSHING.store(true, Ordering::SeqCst);
    let _marker = FlushMarker;
    f.await
}

/// gx: serializes every test that sets or observes [`FLUSHING`].
///
/// [`FLUSHING`] is process-global and `cargo test` runs this crate's tests in parallel threads of
/// one binary, so a flush in one test is visible to every other — including
/// `leader::server::gx_tests::exit_on_disconnect_ignores_observers`, whose whole point is that the
/// leader *does* exit. Any test that runs a flush, and any test that depends on no flush running,
/// holds this lock. `tokio::sync::Mutex` rather than `std`'s: it is not poisoned by a failing
/// assertion, so one failure does not cascade into unrelated ones.
#[cfg(test)]
pub(crate) fn flush_flag_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// What [`request`] needs to end a leader gracefully: the same two values `run_leader` hands the
/// leader server.
struct LeaderShutdown {
    cancel: CancellationToken,
    activity: AgentActivity,
}

impl LeaderShutdown {
    /// Flush every session actor (running its `SessionEnd` hooks), then cancel the root token.
    /// See the module docs for why this order is not interchangeable.
    ///
    /// Both steps run under [`during_flush`] so no other lifecycle path cancels the token first.
    async fn run(&self) {
        during_flush(async {
            self.activity.flush_all_sessions(SESSION_FLUSH_GRACE).await;
            self.cancel.cancel();
        })
        .await;
    }
}

static LEADER_SHUTDOWN: OnceLock<LeaderShutdown> = OnceLock::new();

/// Publish the running leader's shutdown handles. Called once, from `run_leader`.
///
/// First call wins. A process only ever runs one leader, so a second call means a caller retried
/// after a failed start; keeping the first registration avoids cancelling the token of a leader that
/// is no longer the one running.
pub fn register(cancel: CancellationToken, activity: AgentActivity) {
    if LEADER_SHUTDOWN
        .set(LeaderShutdown { cancel, activity })
        .is_err()
    {
        tracing::debug!("gx: leader shutdown handles already registered; keeping the first pair");
    }
}

/// Ask the registered leader to flush its sessions and stop.
///
/// Returns `true` once the flush has completed and the root token has been cancelled, `false`
/// immediately when this process is not a leader (nothing registered). Bounded by
/// [`SESSION_FLUSH_GRACE`]; callers that must exit regardless should still impose their own
/// timeout, since a wedged actor is only *abandoned* at the grace, not killed.
pub async fn request() -> bool {
    let Some(state) = LEADER_SHUTDOWN.get() else {
        return false;
    };
    state.run().await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionCommand, ShutdownKind};
    use std::time::Duration;

    /// The ordering contract, asserted directly and without touching the process-global.
    ///
    /// A fake session actor (the pattern `activity.rs`'s own tests use) samples the token when it
    /// receives `Shutdown` and again while it is still "running its `SessionEnd` hooks". Both
    /// samples must see an *uncancelled* token: the root token gates the `LocalSet` those actors
    /// live on, so cancelling before or during the flush would abandon exactly the hooks this
    /// module exists to run.
    #[tokio::test]
    async fn flush_completes_before_the_token_is_cancelled() {
        let _serialized = flush_flag_test_lock().lock().await;
        let activity = AgentActivity::default();
        let cancel = CancellationToken::new();
        let (mut rx, _prompt_id, _pending) = activity.register_for_test("s1");

        let actor_cancel = cancel.clone();
        let actor = tokio::spawn(async move {
            let cmd = rx.recv().await.expect("actor must receive a command");
            assert!(
                matches!(cmd, SessionCommand::Shutdown(ShutdownKind::Graceful)),
                "the flush must ask the actor for a graceful shutdown"
            );
            let at_shutdown = actor_cancel.is_cancelled();
            // Stand in for the SessionEnd hooks: work the actor does while the flush waits.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let while_working = actor_cancel.is_cancelled();
            // `rx` drops with this future, which is what tells the flush the actor has exited.
            (at_shutdown, while_working)
        });

        LeaderShutdown {
            cancel: cancel.clone(),
            activity,
        }
        .run()
        .await;

        let (at_shutdown, while_working) = actor.await.unwrap();
        assert!(
            !at_shutdown,
            "the token was already cancelled when the actor received Shutdown: the flush must run \
             BEFORE the cancel, or the LocalSet is dead and no SessionEnd hook runs"
        );
        assert!(
            !while_working,
            "the token was cancelled while a session actor was still running its SessionEnd hooks"
        );
        assert!(
            cancel.is_cancelled(),
            "the token must be cancelled once the flush has completed"
        );
    }

    /// Both halves of the global's contract in **one** test, deliberately.
    ///
    /// [`LEADER_SHUTDOWN`] is a process-global `OnceLock`, so "not registered" is a state this test
    /// binary passes through exactly once. Splitting the assertions would make the unregistered
    /// half depend on cargo's scheduling. Nothing else in this crate calls [`register`].
    #[tokio::test]
    async fn request_is_a_noop_until_registered_then_flushes_and_cancels() {
        let _serialized = flush_flag_test_lock().lock().await;
        assert!(
            !request().await,
            "a process that never registered (every non-leader mode) must not pay a flush deadline"
        );

        let activity = AgentActivity::default();
        let cancel = CancellationToken::new();
        let (mut rx, _prompt_id, _pending) = activity.register_for_test("s1");
        let actor =
            tokio::spawn(
                async move { matches!(rx.recv().await, Some(SessionCommand::Shutdown(_))) },
            );

        register(cancel.clone(), activity.clone());

        // First registration wins: a retried leader start must not swap in a second token.
        register(CancellationToken::new(), AgentActivity::default());

        assert!(request().await, "a registered leader must report the flush");
        assert!(actor.await.unwrap(), "the session actor must be shut down");
        assert!(
            cancel.is_cancelled(),
            "the FIRST registration's token is the one that gets cancelled"
        );
    }

    /// [`is_flushing`] is the signal every *other* lifecycle path reads before it cancels the root
    /// token, so it has to be true for exactly the window the flush occupies — no wider (a leader
    /// that never clears it can never exit on its last client) and no narrower (a leader that
    /// clears it early gets its `LocalSet` pulled out mid-hook).
    ///
    /// "During" is sampled from inside the fake session actor, i.e. from the exact place a real
    /// `SessionEnd` hook would run.
    #[tokio::test]
    async fn is_flushing_is_true_only_while_a_flush_is_running() {
        let _serialized = flush_flag_test_lock().lock().await;
        assert!(
            !is_flushing(),
            "an idle process must not claim to be flushing a leader"
        );

        let activity = AgentActivity::default();
        let cancel = CancellationToken::new();
        let (mut rx, _prompt_id, _pending) = activity.register_for_test("s1");
        let actor = tokio::spawn(async move {
            let cmd = rx.recv().await.expect("actor must receive a command");
            assert!(matches!(cmd, SessionCommand::Shutdown(_)));
            // Sampled where a SessionEnd hook runs: the flag must still be up here, because this is
            // the work the exit-on-disconnect check must not cancel out from under.
            let while_working = is_flushing();
            tokio::time::sleep(Duration::from_millis(50)).await;
            while_working
        });

        LeaderShutdown { cancel, activity }.run().await;

        assert!(
            actor.await.unwrap(),
            "is_flushing() must be true while a session actor is running its SessionEnd hooks"
        );
        assert!(
            !is_flushing(),
            "is_flushing() must be cleared once the flush has finished and the token is cancelled"
        );
    }
}
