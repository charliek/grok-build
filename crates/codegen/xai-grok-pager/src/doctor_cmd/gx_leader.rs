//! gx: the `gx doctor` leader + remote-lane section.
//!
//! A gx build starts a leader by default (unlike stock grok), and that leader hosts a loopback
//! HTTP lane (`docs/gx/REMOTE_API.md`). Both are invisible until something goes wrong, and both
//! are things a user may want to turn off — so `doctor` is where they get explained.
//!
//! # Why the whole section disappears on a stock build
//!
//! [`collect`] returns `None` unless [`xai_grok_version::is_gx_build`], and both renderers take a
//! collected `GxFacts`. A stock-flavoured binary therefore emits a report that is byte-for-byte
//! upstream's — no extra human rows, no extra JSON key — which is exactly right (it has no gx
//! leader and no lane to report) and is also what keeps upstream's exact-output doctor fixtures
//! passing unchanged.
//!
//! # What it can and cannot answer
//!
//! The leader decision has one input `doctor` cannot evaluate: the release-dist **remote**
//! `leader_mode`, which needs a network fetch the TUI does at startup. That row says
//! `not evaluated` rather than guessing. Everything else — the compiled-in default, `[cli]
//! use_leader`, the socket and lock paths, who holds the lock — is on disk.
//!
//! One more limitation: the socket/lock/lock-pid rows are resolved for the **default relay URL**
//! only (`LeaderLock::new("")`, since finding a configured `grok_ws_url` would mean loading the
//! agent config, which this synchronous section deliberately does not do) — a leader on a
//! non-default relay is not named there, though it still surfaces in the "gx remote lane" rows
//! below, which discover every leader by scanning `$GROK_HOME` rather than deriving a path.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::remote_cmd::LaneStatus;

/// Everything the section reports. Rendering is pure in this, so both output formats are testable
/// without a leader, a lane, or a gx-flavoured test binary.
#[derive(Debug)]
pub(super) struct GxFacts {
    /// The compiled-in version string, which is also what makes this a gx build.
    pub version: String,
    /// The build-flavour default for `[cli] use_leader` when nothing else decides.
    pub leader_default_on: bool,
    /// `[cli] use_leader` as written in the effective config, if it is set at all.
    pub config_use_leader: Option<bool>,
    /// Why the config could not be read, when it could not be.
    pub config_error: Option<String>,
    /// The decision from flags and config alone (see the module docs).
    pub effective_use_leader: bool,
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    /// Whether the socket *file* exists. Not proof of a listener: a killed leader leaves it
    /// behind, which is why the lane waits on the lock pid instead (`crate::gx_remote_lane`).
    pub socket_exists: bool,
    pub lock_pid: Option<u32>,
    pub lock_pid_alive: bool,
    /// `GX_REMOTE_DISABLE` is set to something non-empty, so this build starts no lane.
    pub lane_disabled_by_env: bool,
    pub token_path: PathBuf,
    pub token: TokenFact,
    /// Every discovery record found under `$GROK_HOME`, probed.
    pub lanes: Vec<LaneStatus>,
}

/// The bearer token file, as far as `doctor` is willing to look at it.
///
/// Never its contents — not in the human report, not in the JSON, not in a debug log. The mode is
/// reported because a token readable by other local users defeats the only thing guarding the
/// lane's port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TokenFact {
    Absent,
    /// The low 9 permission bits.
    Present {
        mode: u32,
    },
    /// The file is there but could not be stat-ed.
    Unreadable,
}

pub(super) fn collect() -> Option<GxFacts> {
    if !xai_grok_version::is_gx_build() {
        return None;
    }

    let (raw_config, config_error) = match xai_grok_shell::config::load_effective_config() {
        Ok(value) => (value, None),
        Err(err) => (
            toml::Value::Table(toml::map::Map::new()),
            Some(err.to_string()),
        ),
    };
    let leader_default_on = xai_grok_version::is_gx_build();
    // Flags and config only. `eligible = true` and no confinement profile stand in for "a normal
    // interactive terminal"; the release-dist remote setting is not passed because fetching it is
    // a network round trip `doctor` does not make. See the module docs.
    let effective = crate::app::resolve_leader_mode_with_default(
        false,
        false,
        &raw_config,
        None,
        true,
        None,
        leader_default_on,
    );

    let lock = xai_grok_shell::leader::LeaderLock::new("");
    let socket_path = lock.socket_path().clone();
    let lock_path = lock.lock_path().clone();
    let lock_pid = lock.read_pid();

    let grok_home = xai_dirs::grok_home();
    let token_path = grok_home.join(gx_remote_api::auth::TOKEN_FILE_NAME);

    Some(GxFacts {
        version: xai_grok_version::VERSION.to_string(),
        leader_default_on,
        config_use_leader: xai_grok_shell::util::config::use_leader_from_toml_opt(&raw_config),
        config_error,
        effective_use_leader: effective.use_leader,
        socket_exists: socket_path.exists(),
        socket_path,
        lock_path,
        lock_pid,
        lock_pid_alive: lock_pid.is_some_and(xai_grok_shell::util::is_process_alive),
        lane_disabled_by_env: std::env::var(crate::gx_remote_lane::DISABLE_ENV)
            .is_ok_and(|v| !v.is_empty()),
        token: token_fact(&token_path),
        token_path,
        lanes: crate::remote_cmd::collect_lanes(&grok_home),
    })
}

fn token_fact(path: &Path) -> TokenFact {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                TokenFact::Present {
                    mode: meta.permissions().mode() & 0o777,
                }
            }
            #[cfg(not(unix))]
            {
                let _ = meta;
                TokenFact::Present { mode: 0o600 }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => TokenFact::Absent,
        Err(_) => TokenFact::Unreadable,
    }
}

// ---------------------------------------------------------------------------
// Human
// ---------------------------------------------------------------------------

/// Same `  {marker} {label:<28} {value}` geometry as the rest of the report, kept here rather than
/// borrowed from `human.rs` so that file keeps exactly one gx hunk.
fn row(out: &mut String, marker: &str, label: &str, value: &str) {
    out.push_str(&format!("  {marker} {label:<28} {value}\n"));
}

fn fact(out: &mut String, label: &str, value: &str) {
    row(out, "·", label, value);
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

pub(super) fn human_section(facts: &GxFacts) -> String {
    let mut out = String::from("\ngx leader\n");
    fact(&mut out, "build", &format!("gx ({})", facts.version));
    fact(
        &mut out,
        "default",
        &format!(
            "{} (compiled in for gx builds)",
            on_off(facts.leader_default_on)
        ),
    );
    match (facts.config_use_leader, &facts.config_error) {
        (_, Some(err)) => row(
            &mut out,
            "?",
            "[cli] use_leader",
            &format!("unreadable: {err}"),
        ),
        (Some(value), None) => fact(&mut out, "[cli] use_leader", on_off(value)),
        (None, None) => fact(&mut out, "[cli] use_leader", "unset"),
    }
    fact(
        &mut out,
        "effective (flags + config)",
        on_off(facts.effective_use_leader),
    );
    row(
        &mut out,
        "?",
        "remote leader_mode",
        "not evaluated (needs a network fetch)",
    );
    fact(
        &mut out,
        "socket (default relay)",
        &format!(
            "{} ({})",
            facts.socket_path.display(),
            if facts.socket_exists {
                "present"
            } else {
                "absent"
            }
        ),
    );
    fact(
        &mut out,
        "lock (default relay)",
        &facts.lock_path.display().to_string(),
    );
    match facts.lock_pid {
        Some(pid) => fact(
            &mut out,
            "lock pid (default relay)",
            &format!(
                "{pid} ({})",
                if facts.lock_pid_alive {
                    "alive"
                } else {
                    "not running"
                }
            ),
        ),
        None => fact(
            &mut out,
            "lock pid (default relay)",
            "none (no leader running)",
        ),
    }
    row(
        &mut out,
        "?",
        "other relays",
        "not resolved here (needs the agent config); see \"gx remote lane\" below, which finds \
         every leader by scanning $GROK_HOME instead",
    );

    out.push_str("\ngx remote lane\n");
    if facts.lane_disabled_by_env {
        fact(
            &mut out,
            "GX_REMOTE_DISABLE",
            "set (this build starts no lane)",
        );
    }
    fact(&mut out, "token file", &token_row(facts));
    if facts.lanes.is_empty() {
        row(&mut out, "?", "lanes", "none discovered");
    }
    for lane in &facts.lanes {
        let marker = if lane.reachable() { "·" } else { "!" };
        row(&mut out, marker, &lane.record.url, &lane_row(lane));
    }
    out
}

fn token_row(facts: &GxFacts) -> String {
    match facts.token {
        TokenFact::Absent => format!(
            "{} (absent; created on first lane start)",
            facts.token_path.display()
        ),
        TokenFact::Present { mode: 0o600 } => {
            format!("{} (mode 0600)", facts.token_path.display())
        }
        TokenFact::Present { mode } => format!(
            "{} (mode {mode:04o} — must be 0600; the lane refuses to read it otherwise)",
            facts.token_path.display()
        ),
        TokenFact::Unreadable => format!("{} (present, not stat-able)", facts.token_path.display()),
    }
}

fn lane_row(lane: &LaneStatus) -> String {
    let health = match (&lane.health, lane.reachable()) {
        (Ok(_), true) => "healthz ok".to_owned(),
        (Ok(_), false) => "healthz answered by a different instance".to_owned(),
        (Err(err), _) => format!("healthz unreachable ({err})"),
    };
    format!(
        "pid {} ({}), {health}",
        lane.record.pid,
        if lane.pid_alive {
            "alive"
        } else {
            "not running"
        }
    )
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// The additive `gx` object on the doctor report.
///
/// `SCHEMA_VERSION` stays `"1"`: this is purely additive (a new optional top-level key that is
/// absent on a stock build), and every existing key keeps its name, place and meaning. A consumer
/// written against schema 1 parses this report unchanged.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GxJson {
    pub build: &'static str,
    pub version: String,
    pub leader: GxLeaderJson,
    pub lane: GxLaneJson,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GxLeaderJson {
    pub default_on: bool,
    pub config_use_leader: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
    pub effective_use_leader: bool,
    /// Always `"not_evaluated"`: see the module docs.
    pub remote_leader_mode: &'static str,
    /// Always `"default_relay"`: `socket_path`/`lock_path`/`lock_pid` below are resolved for the
    /// default relay URL only (see the module docs) — they name nothing when the running leader
    /// (if any) is on a non-default `grok_ws_url`. That leader is not absent from this report, only
    /// from this object: it still appears in `lane.lanes`, found independently by scanning
    /// `$GROK_HOME` for discovery records.
    pub leader_resolved_for: &'static str,
    pub socket_path: String,
    pub socket_exists: bool,
    pub lock_path: String,
    pub lock_pid: Option<u32>,
    pub lock_pid_alive: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GxLaneJson {
    pub disabled_by_env: bool,
    /// Path only, never the token.
    pub token_file: String,
    pub token_present: bool,
    /// `null` when the file is absent or not stat-able.
    pub token_mode: Option<String>,
    pub lanes: Vec<serde_json::Value>,
}

pub(super) fn json_section(facts: &GxFacts) -> GxJson {
    GxJson {
        build: "gx",
        version: facts.version.clone(),
        leader: GxLeaderJson {
            default_on: facts.leader_default_on,
            config_use_leader: facts.config_use_leader,
            config_error: facts.config_error.clone(),
            effective_use_leader: facts.effective_use_leader,
            remote_leader_mode: "not_evaluated",
            leader_resolved_for: "default_relay",
            socket_path: facts.socket_path.display().to_string(),
            socket_exists: facts.socket_exists,
            lock_path: facts.lock_path.display().to_string(),
            lock_pid: facts.lock_pid,
            lock_pid_alive: facts.lock_pid_alive,
        },
        lane: GxLaneJson {
            disabled_by_env: facts.lane_disabled_by_env,
            token_file: facts.token_path.display().to_string(),
            token_present: !matches!(facts.token, TokenFact::Absent),
            token_mode: match facts.token {
                TokenFact::Present { mode } => Some(format!("{mode:04o}")),
                TokenFact::Absent | TokenFact::Unreadable => None,
            },
            lanes: facts
                .lanes
                .iter()
                .map(|lane| {
                    serde_json::to_value(crate::remote_cmd::lane_json(lane))
                        .unwrap_or(serde_json::Value::Null)
                })
                .collect(),
        },
    }
}

#[cfg(test)]
#[path = "gx_leader_tests.rs"]
mod tests;
