//! gx: tests for the `gx doctor` leader + lane section ([`super`]).
//!
//! Rendering is pure in [`GxFacts`], so all of this runs in a stock-flavoured test binary — which
//! is what `cargo test` builds (no `GROK_VERSION` is stamped, so `is_gx_build()` is false). The one
//! thing that *cannot* be exercised here is [`collect`] returning `Some`; see
//! [`the_whole_section_is_absent_on_a_stock_build`] for the half that matters.

use super::*;

fn facts() -> GxFacts {
    GxFacts {
        version: "1.0.16+gx.11".into(),
        leader_default_on: true,
        config_use_leader: None,
        config_error: None,
        effective_use_leader: true,
        socket_path: "/home/u/.grok/gx-leader.sock".into(),
        lock_path: "/home/u/.grok/gx-leader.lock".into(),
        socket_exists: true,
        lock_pid: Some(4242),
        lock_pid_alive: true,
        lane_disabled_by_env: false,
        token_path: "/home/u/.grok/gx-remote.token".into(),
        token: TokenFact::Present { mode: 0o600 },
        lanes: Vec::new(),
    }
}

fn lane(reachable: bool) -> LaneStatus {
    let instance = "c".repeat(32);
    LaneStatus {
        record_path: "/home/u/.grok/gx-remote.json".into(),
        record: gx_remote_api::discovery::DiscoveryRecord {
            url: "http://127.0.0.1:2421".into(),
            pid: 4242,
            instance_id: instance.clone(),
            socket_path: "/home/u/.grok/gx-leader.sock".into(),
            token_file: "/home/u/.grok/gx-remote.token".into(),
            version: "1.0.16+gx.11".into(),
            started_at: 1_700_000_000_000,
        },
        pid_alive: true,
        health: if reachable {
            Ok(crate::remote_cmd::Health {
                ok: true,
                version: "1.0.16+gx.11".into(),
                leader_pid: 4242,
                instance_id: instance,
                build: "gx".into(),
            })
        } else {
            Err("connection refused".into())
        },
    }
}

/// The property that keeps upstream's exact-output doctor fixtures passing: on a stock build there
/// is no gx section at all, in either format.
#[test]
fn the_whole_section_is_absent_on_a_stock_build() {
    assert!(
        !xai_grok_version::is_gx_build(),
        "this test binary is expected to be stock-flavoured (no GROK_VERSION stamped)"
    );
    assert!(
        collect().is_none(),
        "a stock build has no gx leader and no lane to report"
    );
}

#[test]
fn the_human_section_reports_the_decision_inputs_and_never_the_token() {
    let text = human_section(&facts());
    assert!(text.contains("gx leader\n"), "{text}");
    assert!(
        text.contains("· build                        gx (1.0.16+gx.11)"),
        "{text}"
    );
    assert!(
        text.contains("· default                      on (compiled in for gx builds)"),
        "{text}"
    );
    assert!(
        text.contains("· [cli] use_leader             unset"),
        "{text}"
    );
    assert!(text.contains("· effective (flags + config)   on"), "{text}");
    // The one input doctor refuses to guess at.
    assert!(
        text.contains("? remote leader_mode           not evaluated (needs a network fetch)"),
        "{text}"
    );
    assert!(
        text.contains("/home/u/.grok/gx-leader.sock (present)"),
        "{text}"
    );
    assert!(
        text.contains("· lock pid (default relay)     4242 (alive)"),
        "{text}"
    );
    assert!(text.contains("gx remote lane\n"), "{text}");
    assert!(
        text.contains("/home/u/.grok/gx-remote.token (mode 0600)"),
        "{text}"
    );
    assert!(
        text.contains("? lanes                        none discovered"),
        "{text}"
    );
}

/// The finding this guards against: `collect()` resolves the socket/lock/lock-pid facts with
/// `LeaderLock::new("")` — the default relay URL — so on a machine with a non-default
/// `grok_ws_url` those rows are not that machine's leader. The report must say so rather than
/// print them as if they were universal.
#[test]
fn the_socket_and_lock_rows_are_labelled_default_relay_only() {
    let text = human_section(&facts());
    assert!(
        text.contains("socket (default relay)"),
        "the socket row must say it is default-relay-only: {text}"
    );
    assert!(
        text.contains("lock (default relay)"),
        "the lock row must say it is default-relay-only: {text}"
    );
    assert!(
        text.contains("lock pid (default relay)"),
        "the lock pid row must say it is default-relay-only: {text}"
    );
    assert!(
        text.contains("other relays") && text.contains("gx remote lane"),
        "a reader must be pointed at the lane/discovery section for a leader on a non-default \
         relay URL: {text}"
    );
}

#[test]
fn config_and_lock_states_each_get_their_own_row() {
    let mut off = facts();
    off.config_use_leader = Some(false);
    off.effective_use_leader = false;
    let text = human_section(&off);
    assert!(
        text.contains("· [cli] use_leader             off"),
        "{text}"
    );
    assert!(
        text.contains("· effective (flags + config)   off"),
        "{text}"
    );

    let mut broken = facts();
    broken.config_error = Some("config.toml: expected a table".into());
    assert!(
        human_section(&broken).contains("? [cli] use_leader             unreadable: config.toml"),
        "an unreadable config must not be silently reported as `unset`"
    );

    let mut no_leader = facts();
    no_leader.lock_pid = None;
    no_leader.lock_pid_alive = false;
    no_leader.socket_exists = false;
    let text = human_section(&no_leader);
    assert!(
        text.contains("· lock pid (default relay)     none (no leader running)"),
        "{text}"
    );
    assert!(text.contains("gx-leader.sock (absent)"), "{text}");

    let mut stale = facts();
    stale.lock_pid_alive = false;
    assert!(human_section(&stale).contains("4242 (not running)"));
}

#[test]
fn a_wrong_token_mode_is_called_out_and_the_value_never_appears() {
    let mut loose = facts();
    loose.token = TokenFact::Present { mode: 0o644 };
    let text = human_section(&loose);
    assert!(text.contains("mode 0644 — must be 0600"), "{text}");

    let mut absent = facts();
    absent.token = TokenFact::Absent;
    assert!(human_section(&absent).contains("(absent; created on first lane start)"));

    let mut disabled = facts();
    disabled.lane_disabled_by_env = true;
    assert!(
        human_section(&disabled).contains("set (this build starts no lane)"),
        "GX_REMOTE_DISABLE has to be visible or its effect looks like a bug"
    );
}

#[test]
fn discovered_lanes_are_marked_by_whether_they_answer() {
    let mut healthy = facts();
    healthy.lanes = vec![lane(true)];
    let text = human_section(&healthy);
    assert!(text.contains("· http://127.0.0.1:2421"), "{text}");
    assert!(text.contains("pid 4242 (alive), healthz ok"), "{text}");

    let mut broken = facts();
    broken.lanes = vec![lane(false)];
    let text = human_section(&broken);
    assert!(
        text.contains("! http://127.0.0.1:2421"),
        "an unreachable lane is a finding: {text}"
    );
    assert!(
        text.contains("healthz unreachable (connection refused)"),
        "{text}"
    );
}

#[test]
fn the_json_object_is_additive_and_token_free() {
    let mut with_lane = facts();
    with_lane.lanes = vec![lane(true)];
    let value = serde_json::to_value(json_section(&with_lane)).unwrap();

    assert_eq!(value["build"], "gx");
    assert_eq!(value["version"], "1.0.16+gx.11");
    assert_eq!(value["leader"]["defaultOn"], true);
    assert_eq!(value["leader"]["configUseLeader"], serde_json::Value::Null);
    assert_eq!(value["leader"]["effectiveUseLeader"], true);
    assert_eq!(value["leader"]["remoteLeaderMode"], "not_evaluated");
    // The JSON must not silently imply this is the only leader: a non-default-relay leader is
    // resolved nowhere in `leader`, only via `lane.lanes` (see the module docs).
    assert_eq!(value["leader"]["leaderResolvedFor"], "default_relay");
    assert_eq!(value["leader"]["lockPid"], 4242);
    assert_eq!(value["leader"]["lockPidAlive"], true);
    assert_eq!(value["lane"]["tokenFile"], "/home/u/.grok/gx-remote.token");
    assert_eq!(value["lane"]["tokenPresent"], true);
    assert_eq!(value["lane"]["tokenMode"], "0600");
    assert_eq!(value["lane"]["lanes"][0]["url"], "http://127.0.0.1:2421");
    assert_eq!(value["lane"]["lanes"][0]["reachable"], true);

    // Nothing anywhere in the object is the token itself.
    let text = serde_json::to_string(&value).unwrap();
    assert!(!text.contains("\"token\":"), "{text}");

    let mut absent = facts();
    absent.token = TokenFact::Absent;
    let value = serde_json::to_value(json_section(&absent)).unwrap();
    assert_eq!(value["lane"]["tokenPresent"], false);
    assert_eq!(value["lane"]["tokenMode"], serde_json::Value::Null);
    assert_eq!(value["lane"]["lanes"], serde_json::json!([]));
    // `configError` is omitted rather than nulled when there is nothing to report.
    assert!(value["leader"].get("configError").is_none());
}
