//! gx: start the `gx-remote-api` lane inside this process's leader.
//!
//! The lane is an HTTP/SSE façade over the leader, attached to it as an ordinary — but *observer*
//! — ACP client (see `crates/gx/gx-remote-api/src/lib.rs` and `docs/gx/REMOTE_API.md`). It runs in
//! the leader's own process, so something has to start it there; this module is that something,
//! and `main.rs`'s `AgentCmd::Leader` arm carries only the call.
//!
//! # Why the readiness wait exists
//!
//! `run_leader` does its lock acquisition, its `write_pid` and its socket bind **itself**, and
//! offers no post-bind hook, so the lane has to be spawned *before* `run_leader` is awaited and
//! then wait for the leader to catch up. Two things make that wait non-obvious:
//!
//! - On Unix `leader::listener_is_ready` is only `path.exists()`, and a killed predecessor leaves
//!   its socket file behind, so a file check can be satisfied by a socket nobody is listening on.
//! - `write_pid` happens *before* the bind, so even the PID file does not prove the listener is up.
//!
//! So the wait is in two stages, and neither is a file-existence check:
//!
//! 1. poll the leader lock's PID until it is **ours** (100 ms, capped at 30 s). That is what says
//!    "this process won the lock and is the leader" — a stale socket file cannot fake it, and a
//!    *different* live leader's pid is the signal to stand down entirely (it brings its own lane).
//! 2. hand off to [`gx_remote_api::serve`], whose own bounded connect-retry (250 ms, 30 s) covers
//!    the remaining gap: only a completed `Register` handshake proves the listener accepts.
//!
//! # Failure is never fatal
//!
//! Every failure path here warns and returns. The lane is a convenience; a leader that cannot
//! start one is still a perfectly good leader, and killing it (or panicking on its runtime) over a
//! busy port or an unwritable `$GROK_HOME` would be a strictly worse outcome for the TUI in front
//! of the user.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use xai_grok_shell::leader::LeaderLock;

/// Set to any non-empty value to skip the lane entirely on a gx build.
///
/// The escape hatch for "I want the shared leader but not a loopback listener". `gx --no-leader`
/// and `[cli] use_leader = false` are the other two, and they turn off the leader as well.
pub const DISABLE_ENV: &str = "GX_REMOTE_DISABLE";

/// How often the leader-lock PID is re-read while waiting for this process to become the leader.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long to wait for that before giving up on the lane.
const LOCK_POLL_DEADLINE: Duration = Duration::from_secs(30);
/// How often the discovery record is re-read while waiting for the lane to publish its URL.
const RECORD_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long to wait for the record before giving up on the hook stamp (the lane itself is fine).
const RECORD_POLL_DEADLINE: Duration = Duration::from_secs(60);

/// A running lane, cancelled when this value is dropped.
///
/// Held by `main.rs` across the `run_leader(...).await`, so the lane goes down with the leader —
/// including when `run_leader` returns an error or the future is dropped on a signal — without
/// `main.rs` needing an explicit teardown call.
pub struct LaneHandle {
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl LaneHandle {
    /// A handle for a lane that was never started (stock build, or `GX_REMOTE_DISABLE`).
    fn inert() -> Self {
        Self {
            cancel: CancellationToken::new(),
            task: None,
        }
    }

    /// Whether a lane task is actually running behind this handle. Tests and diagnostics only.
    pub fn is_running(&self) -> bool {
        self.task.is_some()
    }
}

impl Drop for LaneHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Deliberately not awaited: `Drop` cannot await, and the leader is on its way out anyway.
        // Cancelling is what closes the listener and removes the discovery record; dropping the
        // handle detaches the task, which observes the cancellation on its next poll.
        self.task = None;
    }
}

/// Whether this build should host a lane at all, and why not when it should not.
///
/// Pure in its inputs so both gates are unit-testable in one process — `is_gx_build()` is compiled
/// in (see [`xai_grok_version::is_gx_build`]), so a test that called the real thing would only
/// ever exercise whichever flavour the test binary happens to be.
pub fn lane_skip_reason(is_gx: bool, disable_env: Option<&str>) -> Option<&'static str> {
    if !is_gx {
        return Some("this is not a gx build");
    }
    if disable_env.is_some_and(|v| !v.is_empty()) {
        return Some("GX_REMOTE_DISABLE is set");
    }
    None
}

/// Start the lane for the leader this process is about to become.
///
/// `ws_url` must be the same relay URL `run_leader` will use, because it is what selects the
/// leader socket/lock pair (`LeaderLock::new`) and therefore the lane's identity.
///
/// Returns immediately. The returned handle cancels the lane when dropped.
pub fn spawn(ws_url: &str) -> LaneHandle {
    if let Some(reason) = lane_skip_reason(
        xai_grok_version::is_gx_build(),
        std::env::var(DISABLE_ENV).ok().as_deref(),
    ) {
        debug!("gx remote lane: not starting ({reason})");
        return LaneHandle::inert();
    }

    let cancel = CancellationToken::new();
    let task = tokio::spawn(run(ws_url.to_string(), cancel.clone()));
    LaneHandle {
        cancel,
        task: Some(task),
    }
}

/// Wait for this process to be the leader, then serve until cancelled.
async fn run(ws_url: String, cancel: CancellationToken) {
    // `LeaderLock::new` only computes paths — it acquires nothing, and its `Drop` is a no-op
    // unless `try_acquire` succeeded — so constructing one here cannot disturb `run_leader`'s.
    let lock = LeaderLock::new(&ws_url);
    let socket_path = lock.socket_path().clone();

    if !wait_until_we_are_the_leader(&lock, &cancel).await {
        return;
    }

    let config = gx_remote_api::Config::for_socket(socket_path.clone());
    let record_path = gx_remote_api::discovery::record_path(&config.grok_home, &config.socket_path);
    // Anything the lane writes is stamped at bind time, so a record older than this instant is a
    // leftover — from a crashed predecessor, or from a previous process that happened to be
    // assigned the same pid. Captured before `serve` can possibly write, never after.
    let not_before = unix_millis();

    let announce = announce_when_published(
        record_path,
        not_before,
        cancel.clone(),
        RECORD_POLL_DEADLINE,
    );
    let serve = serve_then_cancel(gx_remote_api::serve(config, cancel.clone()), cancel.clone());

    let (result, ()) = tokio::join!(serve, announce);
    match result {
        Ok(()) => info!("gx remote lane: stopped"),
        // Never fatal: see the module docs.
        Err(err) => warn!("gx remote lane: not serving ({err:#})"),
    }
}

/// Run `serve`, then cancel `cancel` regardless of the outcome.
///
/// Without this, an early `serve` failure (a busy port, an unwritable `$GROK_HOME`) leaves
/// [`announce_when_published`] polling for a discovery record that will now never appear, all the
/// way out to its own `RECORD_POLL_DEADLINE` — `tokio::join!` only completes once *both* futures
/// have, so that lingering poll is what `run`'s caller actually waits on. Cancelling here is safe
/// precisely because `cancel` is this lane's own token (created in [`spawn`]): nothing outside this
/// module observes it being cancelled a little early, before `run` itself returns.
async fn serve_then_cancel<F, T>(serve: F, cancel: CancellationToken) -> T
where
    F: std::future::Future<Output = T>,
{
    let result = serve.await;
    cancel.cancel();
    result
}

/// Poll the leader lock until its PID is this process's.
///
/// `false` means "do not start a lane": cancelled, another live leader won the lock, or the
/// deadline expired.
async fn wait_until_we_are_the_leader(lock: &LeaderLock, cancel: &CancellationToken) -> bool {
    let me = std::process::id();
    let deadline = Instant::now() + LOCK_POLL_DEADLINE;
    loop {
        match lock.read_pid() {
            Some(pid) if pid == me => return true,
            // Someone else holds it. `run_leader` is about to exit with "another leader already
            // holds the lock", and that leader hosts its own lane; a second one here would fight
            // it for the port and publish a competing discovery record.
            Some(pid) if pid != me && xai_grok_shell::util::is_process_alive(pid) => {
                debug!(
                    lock = %lock.lock_path().display(),
                    other_pid = pid,
                    "gx remote lane: another live leader holds the lock; standing down"
                );
                return false;
            }
            // A dead pid, or no lock file yet: `run_leader` has not written ours yet. Keep waiting.
            _ => {}
        }
        if Instant::now() >= deadline {
            warn!(
                lock = %lock.lock_path().display(),
                "gx remote lane: this process never became the leader within {LOCK_POLL_DEADLINE:?}; no lane"
            );
            return false;
        }
        tokio::select! {
            () = cancel.cancelled() => return false,
            () = tokio::time::sleep(LOCK_POLL_INTERVAL) => {}
        }
    }
}

/// Wait for the lane's own discovery record, then publish its URL to the hook payloads.
///
/// The record is the lane's single source of truth for the URL it actually bound (the preferred
/// port may have been busy), and it is written immediately after the bind — so watching for it is
/// how the host learns the URL without `gx-remote-api` having to depend on `xai-grok-hooks`.
///
/// Gives up quietly: a missing stamp costs a hook consumer one discovery step, nothing more.
///
/// `deadline` is a parameter (rather than hardcoding [`RECORD_POLL_DEADLINE`]) so tests can shrink
/// it instead of waiting out the real one.
async fn announce_when_published(
    record_path: std::path::PathBuf,
    not_before: u64,
    cancel: CancellationToken,
    deadline: Duration,
) {
    let deadline = Instant::now() + deadline;
    loop {
        if let Some(url) = our_record_url(&record_path, not_before) {
            info!(%url, "gx remote lane: listening");
            xai_grok_hooks::gx_remote::announce(&url);
            return;
        }
        if Instant::now() >= deadline {
            debug!(
                record = %record_path.display(),
                "gx remote lane: no discovery record to announce"
            );
            return;
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(RECORD_POLL_INTERVAL) => {}
        }
    }
}

/// The URL from `record_path`, but only if the record is **this process's, from this run**.
fn our_record_url(record_path: &Path, not_before: u64) -> Option<String> {
    let record = gx_remote_api::discovery::read_record(record_path).ok()?;
    (record.pid == std::process::id() && record.started_at >= not_before).then_some(record.url)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stock_build_never_starts_a_lane() {
        // The one property that must hold for coexistence: a stock-flavoured binary binds no
        // loopback port and writes no discovery record, whatever the environment says.
        assert_eq!(
            lane_skip_reason(false, None),
            Some("this is not a gx build")
        );
        assert_eq!(
            lane_skip_reason(false, Some("")),
            Some("this is not a gx build")
        );
    }

    #[test]
    fn the_disable_env_switches_the_lane_off_for_a_gx_build() {
        assert_eq!(lane_skip_reason(true, None), None);
        assert_eq!(
            lane_skip_reason(true, Some("1")),
            Some("GX_REMOTE_DISABLE is set")
        );
        // Exported-but-empty is how a shell spells "unset" often enough that treating it as "on"
        // would be a trap: `GX_REMOTE_DISABLE= gx` must still get a lane.
        assert_eq!(lane_skip_reason(true, Some("")), None);
    }

    #[test]
    fn an_inert_handle_runs_nothing_and_drops_cleanly() {
        let handle = LaneHandle::inert();
        assert!(!handle.is_running());
        drop(handle);
    }

    #[test]
    fn a_record_from_another_process_or_an_earlier_run_is_not_ours_to_announce() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gx-remote.json");
        let record = |pid: u32, started_at: u64| gx_remote_api::discovery::DiscoveryRecord {
            url: "http://127.0.0.1:2421".into(),
            pid,
            instance_id: "f".repeat(32),
            socket_path: "/tmp/gx-leader.sock".into(),
            token_file: "/tmp/gx-remote.token".into(),
            version: "1.0.0+gx.1".into(),
            started_at,
        };

        // A crashed predecessor's record, left behind at the same path. `u32::MAX` is not a pid
        // any kernel assigns, so it can never accidentally equal ours.
        gx_remote_api::discovery::write_record(&path, &record(u32::MAX, 5_000)).unwrap();
        assert_eq!(
            our_record_url(&path, 1_000),
            None,
            "another process's record"
        );

        // Our own pid, but stamped before this run started: a recycled pid, not our lane.
        gx_remote_api::discovery::write_record(&path, &record(std::process::id(), 999)).unwrap();
        assert_eq!(
            our_record_url(&path, 1_000),
            None,
            "an earlier run's record"
        );

        gx_remote_api::discovery::write_record(&path, &record(std::process::id(), 1_000)).unwrap();
        assert_eq!(
            our_record_url(&path, 1_000).as_deref(),
            Some("http://127.0.0.1:2421")
        );

        // No record at all is the common case while the lane is still binding.
        assert_eq!(our_record_url(&dir.path().join("absent.json"), 0), None);
    }

    /// The bug this guards against: `announce_when_published` polling all the way to its own
    /// deadline after `serve` has already failed, because nothing told it to stop. `serve_then_cancel`
    /// is what wires "serve is done" to "the announce poll should give up too" (see `run`); this
    /// test exercises that wiring directly, with a `serve` future that resolves instantly and an
    /// announce deadline long enough that a passing run only happens by being cancelled, never by
    /// outrunning the clock.
    #[tokio::test]
    async fn announce_poll_stops_promptly_when_serve_errors_instead_of_running_to_deadline() {
        let cancel = CancellationToken::new();
        let dir = tempfile::tempdir().unwrap();
        // Never written: were the poll not cancelled, it would run for the full deadline below.
        let record_path = dir.path().join("absent.json");

        let serve = serve_then_cancel(
            async { Err::<(), anyhow::Error>(anyhow::anyhow!("bind failed")) },
            cancel.clone(),
        );
        let announce =
            announce_when_published(record_path, 0, cancel.clone(), Duration::from_secs(20));

        let started = Instant::now();
        let (result, ()) = tokio::join!(serve, announce);
        let elapsed = started.elapsed();

        assert!(result.is_err(), "the fake serve future is set up to fail");
        assert!(
            elapsed < Duration::from_secs(5),
            "announce should have been cancelled as soon as serve failed, \
             not run out its 20s deadline; took {elapsed:?}"
        );
    }
}
