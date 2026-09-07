//! gx: `gx remote` — find, probe and start the loopback remote lane.
//!
//! The lane itself is `gx-remote-api`, hosted inside the leader process (see
//! [`crate::gx_remote_lane`] and `docs/gx/REMOTE_API.md`). This module is the operator's view of
//! it: two subcommands, both of which end in the same status report.
//!
//! - `gx remote status [--json]` — read every discovery record under `$GROK_HOME`, check that each
//!   record's leader pid is alive, and probe each lane's `GET /v1/healthz`. Exit 1 when there is
//!   no lane at all, so a script can branch on it.
//! - `gx remote up` — make sure a leader exists (which is what starts a lane), then the same
//!   report. There is no `--foreground`: the lane has no life of its own outside a leader.
//!
//! # What this never prints
//!
//! **The token.** `status` reports the token *file's path* and nothing about its contents, and the
//! health probe is deliberately the one route that takes no credential — so `gx remote status`
//! never reads the token file at all, and nothing it writes to a terminal, a log, or a CI
//! transcript can leak it. A client that wants the token reads the file itself
//! (`docs/gx/REMOTE_API.md`).
//!
//! # Why the health probe is hand-rolled
//!
//! One unauthenticated GET against a loopback port, from two synchronous callers (`gx doctor` runs
//! before any runtime exists). A full HTTP client would be a new dependency on the pager for a
//! request whose entire surface is a status line and a small JSON object; [`probe_healthz`] writes
//! the request and [`parse_healthz_response`] reads the answer, both refusing anything that is not
//! loopback.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use gx_remote_api::discovery::DiscoveryRecord;
use serde::Serialize;

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

const REMOTE_AFTER_HELP: &str = "\
Examples:
  # What is listening, and is it healthy?
  gx remote status

  # Machine-readable, for a script or an SSH one-liner
  gx remote status --json

  # Start a leader if none is running, then report
  gx remote up

The token is never printed. Read it yourself from the tokenFile path the
report names; see docs/gx/REMOTE_API.md.";

#[derive(Debug, clap::Args, Clone)]
#[command(after_help = REMOTE_AFTER_HELP)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(Debug, clap::Subcommand, Clone)]
pub enum RemoteCommand {
    /// Show every remote lane discovered under $GROK_HOME
    Status {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Ensure a leader (and therefore a lane) is running, then show status
    Up,
}

/// Per-probe budget for connect, write and read. A loopback round trip is sub-millisecond; this is
/// only long enough to distinguish "nothing there" from "busy".
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `up` waits for a freshly spawned leader to publish a reachable lane.
const UP_WAIT: Duration = Duration::from_secs(20);
const UP_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub async fn run(args: RemoteArgs) -> Result<()> {
    match args.command {
        RemoteCommand::Status { json } => report(json),
        RemoteCommand::Up => {
            ensure_leader().await?;
            wait_for_a_reachable_lane(UP_WAIT).await;
            report(false)
        }
    }
}

/// Collect, render and exit-code the status report.
fn report(json: bool) -> Result<()> {
    let home = xai_dirs::grok_home();
    let lanes = collect_lanes(&home);
    if json {
        println!("{}", render_json(&lanes)?);
    } else {
        print!("{}", render_human(&lanes, &home));
    }
    if lanes.iter().any(LaneStatus::reachable) {
        return Ok(());
    }
    if lanes.is_empty() {
        bail!(
            "no gx remote lane found under {}.\n\
             A gx build starts one alongside its leader; run `gx remote up` to start a leader, \
             or check `gx doctor` if you disabled the leader ([cli] use_leader, --no-leader) or \
             set GX_REMOTE_DISABLE.",
            home.display()
        );
    }
    bail!(
        "found {} gx remote discovery record(s) under {}, none of them reachable. \
         A record outlives a crash; `gx remote up` starts a fresh leader.",
        lanes.len(),
        home.display()
    );
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// One discovery record, plus what probing it revealed.
#[derive(Debug)]
pub(crate) struct LaneStatus {
    pub record_path: PathBuf,
    pub record: DiscoveryRecord,
    /// Whether the record's leader pid still names a live process.
    pub pid_alive: bool,
    /// `GET /v1/healthz`, or why it failed.
    pub health: std::result::Result<Health, String>,
}

impl LaneStatus {
    /// Reachable means more than "the port answered".
    ///
    /// A loopback port outlives the process that bound it and gets recycled, so a stale record can
    /// name a port some unrelated local program now owns. The `instanceId` is what ties the answer
    /// back to *this* record — it is the same check `docs/gx/REMOTE_API.md` requires of every
    /// client before it sends a bearer token.
    pub(crate) fn reachable(&self) -> bool {
        self.health
            .as_ref()
            .is_ok_and(|h| h.ok && h.instance_id == self.record.instance_id)
    }

    fn health_note(&self) -> String {
        match &self.health {
            Ok(health) if self.reachable() => format!("ok ({})", health.version),
            Ok(health) => format!(
                "answered, but instanceId {} is not this record's — a different process owns the port",
                short_id(&health.instance_id)
            ),
            Err(err) => format!("unreachable ({err})"),
        }
    }
}

/// Every `gx-remote*.json` directly under `grok_home`, sorted by name.
///
/// Deliberately a directory listing rather than "the default record plus guesses": a leader with a
/// `GROK_LEADER_SOCKET` override or a non-default relay URL publishes a *hashed* record name that
/// nothing outside `gx-remote-api` can reconstruct, and those are exactly the leaders an operator
/// is most likely to have lost track of.
///
/// Staging files are skipped by construction: `write_record` names them `.gx-remote.json.<pid>…`,
/// with a leading dot, so they never match the prefix.
pub(crate) fn discover_record_paths(grok_home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(grok_home) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with("gx-remote") && name.ends_with(".json"))
        })
        .collect();
    found.sort();
    found
}

/// Read and probe every record under `grok_home`. Unparseable records are skipped, not fatal.
pub(crate) fn collect_lanes(grok_home: &Path) -> Vec<LaneStatus> {
    discover_record_paths(grok_home)
        .into_iter()
        .filter_map(|record_path| {
            let record = gx_remote_api::discovery::read_record(&record_path)
                .map_err(|err| {
                    tracing::debug!(path = %record_path.display(), %err, "gx remote: skipping an unreadable discovery record");
                })
                .ok()?;
            let pid_alive = xai_grok_shell::util::is_process_alive(record.pid);
            let health = probe_healthz(&record.url, PROBE_TIMEOUT).map_err(|err| format!("{err:#}"));
            Some(LaneStatus {
                record_path,
                record,
                pid_alive,
                health,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The health probe
// ---------------------------------------------------------------------------

/// `GET /v1/healthz`'s body — the only route that needs no token.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Health {
    pub ok: bool,
    pub version: String,
    pub leader_pid: u32,
    pub instance_id: String,
    pub build: String,
}

/// Probe `base_url`'s health route. Synchronous, so `gx doctor` (which runs before any runtime
/// exists) and `gx remote` can share one implementation.
pub(crate) fn probe_healthz(base_url: &str, timeout: Duration) -> Result<Health> {
    let (addr, host_header) = loopback_target(base_url)?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("connecting to {addr}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    // HTTP/1.1 with `Connection: close`, so the server frames the body by closing and a plain
    // read-to-end is the whole response. No token: healthz is the unauthenticated route.
    write!(
        stream,
        "GET /v1/healthz HTTP/1.1\r\nHost: {host_header}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )
    .context("sending the health request")?;
    stream.flush().ok();

    let mut raw = Vec::new();
    // Capped: a stale record can point at a port some other program owns, and that program is
    // under no obligation to stop talking.
    std::io::Read::by_ref(&mut stream)
        .take(64 * 1024)
        .read_to_end(&mut raw)
        .context("reading the health response")?;
    parse_healthz_response(&raw)
}

/// The socket address to probe, and the `Host` header to send.
///
/// Refuses anything that is not plain `http` on a loopback address. The lane binds nowhere else
/// (`gx_remote_api::bind_loopback`), so a record naming another host is either corrupt or hostile,
/// and following it would turn `gx remote status` into an outbound request generator.
fn loopback_target(base_url: &str) -> Result<(SocketAddr, String)> {
    let url = url::Url::parse(base_url).with_context(|| format!("parsing {base_url:?}"))?;
    if url.scheme() != "http" {
        bail!("{base_url:?} is not an http:// URL");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("{base_url:?} has no host"))?;
    let port = url
        .port()
        .ok_or_else(|| anyhow!("{base_url:?} has no port"))?;
    let addr = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}:{port}"))?
        .find(|addr| addr.ip().is_loopback())
        .ok_or_else(|| anyhow!("{base_url:?} does not resolve to a loopback address"))?;
    Ok((addr, format!("{host}:{port}")))
}

/// Pull the health object out of a raw HTTP response.
///
/// Tolerant about framing on purpose: the status line is checked, then the JSON object is taken
/// from the first `{` to the last `}` in the body. That reads a `Content-Length` body and a
/// chunked one identically, which matters because the framing is the server's choice and this
/// probe has exactly one thing to learn from the answer.
fn parse_healthz_response(raw: &[u8]) -> Result<Health> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .ok_or_else(|| anyhow!("the response had no header/body boundary"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("the response had no status line"))?;
    if status != "200" {
        bail!("/v1/healthz answered HTTP {status}");
    }
    let start = body
        .find('{')
        .ok_or_else(|| anyhow!("/v1/healthz returned no JSON object"))?;
    let end = body
        .rfind('}')
        .ok_or_else(|| anyhow!("/v1/healthz returned a truncated JSON object"))?;
    serde_json::from_str(&body[start..=end]).context("parsing the health object")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The JSON shape of one lane. Additive over the discovery record; the token value is not in it,
/// and neither is anything derived from it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LaneJson<'a> {
    pub url: &'a str,
    pub pid: u32,
    pub pid_alive: bool,
    pub instance_id: &'a str,
    pub socket_path: &'a str,
    /// Path only. Never the token.
    pub token_file: &'a str,
    pub version: &'a str,
    pub started_at: u64,
    pub record: String,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<&'a Health>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
}

pub(crate) fn lane_json(lane: &LaneStatus) -> LaneJson<'_> {
    LaneJson {
        url: &lane.record.url,
        pid: lane.record.pid,
        pid_alive: lane.pid_alive,
        instance_id: &lane.record.instance_id,
        socket_path: &lane.record.socket_path,
        token_file: &lane.record.token_file,
        version: &lane.record.version,
        started_at: lane.record.started_at,
        record: lane.record_path.display().to_string(),
        reachable: lane.reachable(),
        health: lane.health.as_ref().ok(),
        error: lane.health.as_ref().err().map(String::as_str),
    }
}

pub(crate) fn render_json(lanes: &[LaneStatus]) -> Result<String> {
    let rows: Vec<LaneJson<'_>> = lanes.iter().map(lane_json).collect();
    serde_json::to_string_pretty(&rows).context("serializing the remote status")
}

pub(crate) fn render_human(lanes: &[LaneStatus], grok_home: &Path) -> String {
    if lanes.is_empty() {
        return String::new();
    }
    let mut out = format!("gx remote lanes under {}\n", grok_home.display());
    for lane in lanes {
        out.push('\n');
        out.push_str(&format!("  {}\n", lane.record.url));
        row(
            &mut out,
            "leader pid",
            &pid_note(lane.record.pid, lane.pid_alive),
        );
        row(&mut out, "leader socket", &lane.record.socket_path);
        row(&mut out, "instance", &short_id(&lane.record.instance_id));
        row(&mut out, "version", &lane.record.version);
        row(&mut out, "healthz", &lane.health_note());
        row(&mut out, "token file", &lane.record.token_file);
        row(&mut out, "record", &lane.record_path.display().to_string());
    }
    out
}

fn row(out: &mut String, label: &str, value: &str) {
    out.push_str(&format!("    · {label:<16} {value}\n"));
}

fn pid_note(pid: u32, alive: bool) -> String {
    format!("{pid} ({})", if alive { "alive" } else { "not running" })
}

/// Enough of an instance id to tell two lanes apart in a terminal, never the whole 128 bits.
fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

// ---------------------------------------------------------------------------
// `up`
// ---------------------------------------------------------------------------

/// Connect to a leader, spawning one if there is none, then let the connection go.
///
/// Exactly the shape `main.rs` uses for `grok workspace`: the connection's only job is to make a
/// leader exist. Dropping it immediately is deliberate — an idle client would count against a
/// manually started leader's exit-on-disconnect bookkeeping for no reason.
async fn ensure_leader() -> Result<()> {
    let agent_config = xai_grok_shell::config::load_agent_config_disk_only()
        .map_err(|e| anyhow!("failed to load the agent config: {e}"))?;
    let env_urls = xai_grok_shell::leader::LeaderEnvUrls::from(&agent_config.grok_com_config);
    let capabilities = xai_grok_shell::leader::ClientCapabilities {
        client_version: Some(crate::client_identity::PAGER_CLIENT_VERSION.to_string()),
        ..Default::default()
    };
    let conn = xai_grok_shell::leader::connect_or_spawn(
        "gx-remote-cli",
        xai_grok_shell::leader::ClientMode::Stdio,
        &env_urls,
        capabilities,
    )
    .await
    .map_err(|e| anyhow!("failed to start or connect to a leader: {e}"))?;
    drop(conn);
    Ok(())
}

/// Poll until some lane answers, or `budget` expires.
///
/// A leader is reachable before its lane is: the lane waits for the leader lock's pid, then
/// registers over the socket, then binds. Reporting "no lane" in that window would be true and
/// useless.
async fn wait_for_a_reachable_lane(budget: Duration) {
    let home = xai_dirs::grok_home();
    let deadline = std::time::Instant::now() + budget;
    loop {
        if collect_lanes(&home).iter().any(LaneStatus::reachable) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(UP_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
#[path = "remote_cmd_tests.rs"]
mod tests;
