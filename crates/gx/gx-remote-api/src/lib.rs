//! gx: a loopback HTTP façade over the grok **leader**, so a phone on an SSH port-forward can read
//! (and, from C5/C6, steer) the sessions running in a TUI on this machine.
//!
//! This crate exists only in the gx fork. Nothing upstream depends on it; it depends on
//! `xai-grok-shell` for the leader client and the roster wire types, never the other way round.
//!
//! # Shape
//!
//! The lane is **one more ACP client of the leader** — not a route bolted onto the leader's own
//! server, which does not exist (the leader is a Unix-socket JSON-RPC multiplexer, not an HTTP
//! server). It registers as [`ClientMode::Stdio`] with `client_type = "gx-remote-api"` and the gx
//! `observer` capability, which makes it inert in the leader's routing: never a session driver,
//! never the last-active client, ignored by exit-on-disconnect, identity-only meta injection. It is
//! still a full *subscriber*, so it receives session fan-out and keeps a session resident — which
//! is exactly the handoff the Roost Pivot needs.
//!
//! ```text
//!   phone ──http──> 127.0.0.1:2421 ─┐
//!                                   │  (this crate)
//!                      AcpClient ───┤  id assignment, correlation, fan-out
//!                     LeaderLink ───┘  raw JSON-RPC payload strings
//!                          │
//!                    LeaderClient ── framed IPC ──> $GROK_HOME/gx-leader.sock
//! ```
//!
//! # What is here
//!
//! **C4** built the core and the read-only half: `/v1/healthz`, `/v1/sessions`,
//! `/v1/sessions/{id}`, `/v1/sessions/{id}/history`, lazy attach, the token and the discovery
//! record.
//!
//! **C5** adds the live half. `GET /v1/sessions/{id}/events` streams normalized frames over SSE and
//! resumes from a `Last-Event-ID` cursor against a bounded in-memory [`ring`] (falling back to the
//! persisted transcript when the cursor predates it); `POST /v1/sessions`,
//! `POST /v1/sessions/{id}/messages` and `POST /v1/sessions/{id}/cancel` are the write verbs, each
//! admitted by the session-state table in [`policy`].
//!
//! **C6** brings approvals. An approval is an agent→client **reverse-request** that must be
//! answered on a live connection, which is precisely what a phone does not have; [`approvals`]
//! keeps the request and its JSON-RPC id as addressable state, `GET`/`POST
//! /v1/sessions/{id}/approvals[/{toolCallId}]` read and answer it over whatever connection the
//! client has now, and the change shows up as `event: approval` on the stream. A pending entry also
//! overrides the roster's activity in [`policy`], because the roster lags and an approval does not.
//!
//! # Accepted risks
//!
//! Known, deliberate, and written down here so the next reader does not have to rediscover them:
//!
//! - **A well-formed `Authorization: Bearer <wrong>` shadows a valid `?token=` and returns 401.**
//!   Deliberate: one credential per request, header first. A client that sends both and gets a 401
//!   has a wrong header, not a fallback to fix.
//! - **Unknown paths and wrong methods answer 404/405 without a token.** The token layer is a
//!   `route_layer`, so it runs only on a matched route. No protected handler is reachable this way;
//!   the only thing that leaks is whether a path exists.
//! - **`?token=` puts the secret in the request URI**, where shell history, proxy logs and browser
//!   history can keep it. Deliberate, for SSE clients (`EventSource`) that cannot set headers. The
//!   `Authorization` header is the documented default, and the lane never logs a full URI.
//! - **A stale discovery record can name a port a different local process now owns.** The record
//!   outlives a crash, and loopback ports get recycled. The **client contract** is therefore: call
//!   the token-free `GET /v1/healthz` first and match its `instanceId` against the record's, and
//!   only then send the bearer token. A client that skips that check can hand the token to whatever
//!   bound the port next.
//!
//! [`ClientMode::Stdio`]: xai_grok_shell::leader::ClientMode::Stdio

pub mod acp_client;
pub mod approvals;
pub mod auth;
pub mod discovery;
pub mod envelope;
pub mod error;
pub mod link;
pub mod policy;
pub mod ring;
pub mod routes;
pub mod state;

#[cfg(test)]
mod router_tests;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use xai_grok_shell::leader::{ClientCapabilities, ClientMode, LeaderClient};

use crate::acp_client::{AcpClient, DEFAULT_REQUEST_TIMEOUT};
use crate::approvals::ApprovalStore;
use crate::link::ChannelLink;
use crate::ring::EventRing;
use crate::state::{AppState, Attachments, HealthInfo, SseSettings, spawn_event_pump};

/// How the leader labels this client in `gx leader info` and in its logs.
pub const CLIENT_TYPE: &str = "gx-remote-api";

/// Default loopback port for the default-suffix leader. Overridden by `GX_REMOTE_PORT`.
pub const DEFAULT_PORT: u16 = 2421;

/// Env var overriding [`DEFAULT_PORT`].
pub const PORT_ENV: &str = "GX_REMOTE_PORT";

/// Retry budget for the initial leader connect.
///
/// A stale socket file can outlive the process that bound it, and `write_pid` happens before bind,
/// so no file-based predicate proves the listener is up. The only proof is a completed `Register`
/// handshake — so we retry the whole `LeaderClient::connect` until it succeeds or the cap expires.
const CONNECT_BACKOFF: Duration = Duration::from_millis(250);
const CONNECT_DEADLINE: Duration = Duration::from_secs(30);

/// The capabilities this lane registers with.
///
/// `observer: true` is load-bearing, not decorative: without it the lane's first `session/load`
/// would claim the driver slot of a session a TUI is driving, and the leader would start routing
/// that session's terminal and filesystem tool calls at a phone that cannot execute them. Every
/// other per-client flag stays `false` for the same reason — the leader injects them into the
/// session's shared meta.
pub fn lane_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        observer: true,
        client_version: Some(xai_grok_version::VERSION.to_string()),
        ..Default::default()
    }
}

/// Everything `serve` needs that is not derivable from the environment at call time.
#[derive(Debug, Clone)]
pub struct Config {
    /// Leader socket to attach to. Also the discovery record's identity.
    pub socket_path: PathBuf,
    /// Where the token and the discovery record live.
    pub grok_home: PathBuf,
    /// Preferred loopback port; `0` binds ephemeral outright.
    pub port: u16,
    /// PID reported by `/v1/healthz`. The leader process, which is this process when hosted.
    pub leader_pid: u32,
    /// Version string reported by `/v1/healthz` and written to the discovery record.
    pub version: String,
    /// Per-request ceiling on the leader round trip.
    pub request_timeout: Duration,
    /// Keepalive gap and per-connection queue bound for `/v1/sessions/{id}/events`.
    pub sse: SseSettings,
}

impl Config {
    /// A config for `socket_path`, with the port taken from `GX_REMOTE_PORT` and everything else
    /// from this process.
    ///
    /// `$GROK_HOME` comes from [`xai_dirs::grok_home`], the same resolver the rest of the tree
    /// uses, so the lane never disagrees with the leader about where home is.
    pub fn for_socket(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            grok_home: xai_dirs::grok_home(),
            port: port_from_env(),
            leader_pid: std::process::id(),
            version: xai_grok_version::VERSION.to_string(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            sse: SseSettings::default(),
        }
    }
}

/// `GX_REMOTE_PORT`, or [`DEFAULT_PORT`]. An unparseable value warns and falls back rather than
/// refusing to start: the lane is a convenience, not something worth failing a leader over.
fn port_from_env() -> u16 {
    match std::env::var(PORT_ENV) {
        Ok(raw) => match raw.trim().parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                warn!(value = %raw, "gx-remote-api: ignoring an unparseable {PORT_ENV}");
                DEFAULT_PORT
            }
        },
        Err(_) => DEFAULT_PORT,
    }
}

/// Cancels its token when it goes out of scope, however it goes out of scope.
///
/// [`serve`] holds one from the moment [`AcpClient::spawn`] puts a task on the runtime. That task
/// owns the leader link and lives until the token is cancelled, so **every** exit from `serve`
/// after the spawn has to cancel — an early `?` included, or a failed `write_record` would leave an
/// orphan ACP client registered with the leader for as long as the process lives, holding sessions
/// resident for a lane that no longer has a listener. A guard rather than a `cancel.cancel()` on
/// each fallible step, so that a new early return added later cannot forget.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        // Idempotent: the normal shutdown path has usually cancelled already.
        self.0.cancel();
    }
}

/// Attach to the leader, bind loopback, publish the discovery record, serve until `cancel`.
///
/// Shuts down when `cancel` fires **or** when the leader link closes, whichever comes first; the
/// discovery record is then removed if it is still ours. There is no reconnect: a replacement
/// leader brings its own lane (plan D3).
pub async fn serve(config: Config, cancel: CancellationToken) -> anyhow::Result<()> {
    let token = auth::load_or_create_token(&config.grok_home)?;
    let leader = connect_to_leader(&config.socket_path, &cancel).await?;

    // Both exist before the link task does: an interaction reverse-request can arrive on the first
    // millisecond of the connection, and the store is what decides whether to hold it or refuse it.
    let attachments = Arc::new(Attachments::default());
    let approvals = Arc::new(ApprovalStore::new(attachments.clone()));

    // Sharing (not cloning) the token means a closed link cancels the caller's token too, which is
    // how the hosting task learns the lane is finished.
    let acp = AcpClient::spawn(
        ChannelLink::from_leader_client(leader),
        cancel.clone(),
        config.request_timeout,
        approvals.clone(),
    );
    // From here on, every return path — `?`, panic-unwind, or the normal one — takes the ACP task
    // down with it. See [`CancelOnDrop`].
    let _acp_task = CancelOnDrop(cancel.clone());

    acp.initialize()
        .await
        .context("ACP initialize against the leader")?;

    let listener = bind_loopback(config.port).await?;
    let addr = listener.local_addr().context("reading the bound address")?;
    let url = format!("http://{addr}");

    let instance_id = discovery::new_instance_id();
    let state = Arc::new(AppState {
        acp: acp.clone(),
        token,
        health: HealthInfo {
            version: config.version.clone(),
            leader_pid: config.leader_pid,
            instance_id: instance_id.clone(),
        },
        attachments,
        approvals,
        ring: EventRing::new(),
        sse: config.sse.clone(),
    });
    // Start filling the ring now, not on the first SSE connection: what makes a reconnect cheap is
    // the frames that arrived while nobody was connected.
    spawn_event_pump(state.clone(), cancel.clone());

    let record_path = discovery::record_path(&config.grok_home, &config.socket_path);
    discovery::write_record(
        &record_path,
        &discovery::DiscoveryRecord {
            url: url.clone(),
            pid: config.leader_pid,
            instance_id: instance_id.clone(),
            socket_path: config.socket_path.display().to_string(),
            token_file: auth::token_path(&config.grok_home).display().to_string(),
            version: config.version.clone(),
            started_at: unix_millis(),
        },
    )?;
    info!(%url, socket = %config.socket_path.display(), "gx-remote-api: lane is up");

    let shutdown = cancel.clone();
    let result = axum::serve(listener, routes::router(state))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .context("serving the remote lane");

    discovery::remove_if_ours(&record_path, &instance_id);
    info!("gx-remote-api: lane is down");
    result
}

/// `LeaderClient::connect` with bounded retry until the `Register` handshake succeeds.
async fn connect_to_leader(
    socket_path: &std::path::Path,
    cancel: &CancellationToken,
) -> anyhow::Result<LeaderClient> {
    let deadline = Instant::now() + CONNECT_DEADLINE;
    let last_error;
    loop {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled before the leader link was established");
        }
        match LeaderClient::connect(
            socket_path.to_path_buf(),
            CLIENT_TYPE,
            ClientMode::Stdio,
            lane_capabilities(),
        )
        .await
        {
            Ok(client) => return Ok(client),
            Err(err) => {
                if Instant::now() >= deadline {
                    last_error = err;
                    break;
                }
                tokio::select! {
                    () = cancel.cancelled() => anyhow::bail!("cancelled while connecting to the leader"),
                    () = tokio::time::sleep(CONNECT_BACKOFF) => {}
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "could not register with the leader at {} within {CONNECT_DEADLINE:?}: {last_error}",
        socket_path.display(),
    ))
}

/// Bind `127.0.0.1:port`, falling back to an ephemeral port if it is taken.
///
/// The address is **not** configurable. A non-loopback listener would put every session on the
/// machine behind a single bearer token on the LAN; reaching the lane from elsewhere is SSH's job.
async fn bind_loopback(port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    let wanted = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match tokio::net::TcpListener::bind(wanted).await {
        Ok(listener) => Ok(listener),
        Err(err) if port != 0 => {
            warn!(
                %err,
                port,
                "gx-remote-api: preferred port is busy; binding an ephemeral one (read the discovery record for the real URL)"
            );
            tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .await
                .context("binding an ephemeral loopback port")
        }
        Err(err) => Err(err).context("binding the loopback listener"),
    }
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
    fn the_lane_registers_as_an_observer() {
        let caps = lane_capabilities();
        assert!(
            caps.observer,
            "without observer the lane would claim the driver slot of a TUI's session"
        );
        assert_eq!(
            caps.client_version.as_deref(),
            Some(xai_grok_version::VERSION)
        );
    }

    #[test]
    fn the_lane_claims_no_other_capability() {
        // Every one of these makes the leader inject per-client meta into the shared session; an
        // observer must change nothing about how a TUI's session is routed.
        let caps = lane_capabilities();
        assert!(!caps.yolo_mode);
        assert!(!caps.auto_mode);
        assert!(!caps.code_nav_enabled);
        assert!(!caps.terminal);
        assert!(!caps.fs_read);
        assert!(!caps.fs_write);
        assert!(!caps.status_line);
        assert_eq!(caps.default_model, None);
    }

    #[test]
    fn the_client_type_is_what_the_leader_tests_expect() {
        // `leader/server_gx_tests.rs` registers its observer under this exact name.
        assert_eq!(CLIENT_TYPE, "gx-remote-api");
    }

    #[tokio::test]
    async fn a_busy_preferred_port_falls_back_to_an_ephemeral_one() {
        let squatter = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let taken = squatter.local_addr().unwrap().port();

        let listener = bind_loopback(taken).await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_ne!(addr.port(), taken);
        assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST, "loopback only, always");
    }

    #[tokio::test]
    async fn an_early_return_after_the_acp_spawn_stops_the_acp_task() {
        // Stands in for `serve`'s post-spawn failure paths (a failed `initialize`, a busy port that
        // cannot fall back, a `write_record` that hits a read-only `$GROK_HOME`). Each is a `?`, and
        // each must leave the ACP task cancelled rather than registered with the leader forever.
        let cancel = CancellationToken::new();
        let (link, _handle) = crate::link::FakeLink::new();
        let acp = AcpClient::spawn(
            link,
            cancel.clone(),
            Duration::from_secs(5),
            Arc::new(ApprovalStore::new(Arc::new(Attachments::default()))),
        );

        async fn fails_after_the_spawn(cancel: CancellationToken) -> anyhow::Result<()> {
            let _acp_task = CancelOnDrop(cancel);
            anyhow::bail!("write_record failed");
        }
        assert!(fails_after_the_spawn(cancel.clone()).await.is_err());

        assert!(
            cancel.is_cancelled(),
            "an orphan ACP client would keep sessions resident for a lane with no listener"
        );
        // And the client itself now fails fast rather than parking callers until their timeout.
        assert!(matches!(
            acp.request("x.ai/sessions/list", serde_json::json!({}))
                .await,
            Err(crate::acp_client::AcpError::Closed)
        ));
    }

    #[tokio::test]
    async fn the_listener_is_never_bound_off_loopback() {
        let listener = bind_loopback(0).await.unwrap();
        assert_eq!(listener.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
    }
}
