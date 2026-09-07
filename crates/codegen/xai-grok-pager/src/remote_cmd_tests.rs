//! gx: tests for `gx remote` ([`crate::remote_cmd`]).

use std::net::{Ipv4Addr, TcpListener};

use super::*;

fn record(url: &str, instance: &str) -> DiscoveryRecord {
    DiscoveryRecord {
        url: url.to_string(),
        pid: std::process::id(),
        instance_id: instance.to_string(),
        socket_path: "/home/u/.grok/gx-leader.sock".into(),
        token_file: "/home/u/.grok/gx-remote.token".into(),
        version: "1.0.16+gx.11".into(),
        started_at: 1_700_000_000_000,
    }
}

fn lane(url: &str, instance: &str, health: std::result::Result<Health, String>) -> LaneStatus {
    LaneStatus {
        record_path: PathBuf::from("/home/u/.grok/gx-remote.json"),
        record: record(url, instance),
        pid_alive: true,
        health,
    }
}

fn health(instance: &str) -> Health {
    Health {
        ok: true,
        version: "1.0.16+gx.11".into(),
        leader_pid: std::process::id(),
        instance_id: instance.to_string(),
        build: "gx".into(),
    }
}

#[test]
fn discovery_finds_every_record_and_no_staging_file() {
    let home = tempfile::tempdir().unwrap();
    for name in [
        "gx-remote.json",
        "gx-remote-00ff00ff00ff00ff.json",
        // Not ours.
        "config.toml",
        "gx-leader.sock",
        "gx-remote.token",
        "providers.toml",
        // `write_record`'s staging name: same prefix, but hidden, and mid-write.
        ".gx-remote.json.4242.00000000deadbeef.tmp",
        // The name has to be *anchored*: these two contain "gx-remote" and end in ".json", and
        // neither is a record this lane wrote.
        ".gx-remote.json",
        "old-gx-remote.json",
    ] {
        std::fs::write(home.path().join(name), b"{}").unwrap();
    }
    // A directory that happens to match the pattern is not a record.
    std::fs::create_dir(home.path().join("gx-remote-dir.json")).unwrap();

    let found: Vec<String> = discover_record_paths(home.path())
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        found,
        vec!["gx-remote-00ff00ff00ff00ff.json", "gx-remote.json"],
        "sorted, records only"
    );
}

#[test]
fn an_unreadable_record_is_skipped_rather_than_fatal() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("gx-remote.json"), b"not json at all").unwrap();
    assert!(collect_lanes(home.path()).is_empty());
    assert!(collect_lanes(&home.path().join("nope")).is_empty());
}

#[test]
fn a_lane_is_reachable_only_when_healthz_names_the_records_instance() {
    // The stale-port guard: the record's port answers, but a different process owns it now.
    let mismatched = lane("http://127.0.0.1:2421", "aaaa", Ok(health("bbbb")));
    assert!(!mismatched.reachable());
    assert!(
        mismatched.health_note().contains("not this record's"),
        "{}",
        mismatched.health_note()
    );

    let matched = lane("http://127.0.0.1:2421", "aaaa", Ok(health("aaaa")));
    assert!(matched.reachable());

    let mut not_ok = health("aaaa");
    not_ok.ok = false;
    assert!(!lane("http://127.0.0.1:2421", "aaaa", Ok(not_ok)).reachable());

    let dead = lane(
        "http://127.0.0.1:2421",
        "aaaa",
        Err("connection refused".into()),
    );
    assert!(!dead.reachable());
    assert!(dead.health_note().contains("unreachable"));
}

#[test]
fn the_probe_refuses_anything_that_is_not_loopback_http() {
    for bad in [
        "https://127.0.0.1:2421",
        "http://127.0.0.1",
        "ftp://127.0.0.1:2421",
        "not a url",
    ] {
        assert!(loopback_target(bad).is_err(), "{bad} must be refused");
    }
    // A public address resolves fine and must still be refused: the lane binds loopback only.
    assert!(loopback_target("http://93.184.216.34:2421").is_err());

    let (addr, host) = loopback_target("http://127.0.0.1:2421").unwrap();
    assert!(addr.ip().is_loopback());
    assert_eq!(addr.port(), 2421);
    assert_eq!(host, "127.0.0.1:2421");
}

#[test]
fn the_response_parser_reads_both_framings_and_rejects_the_rest() {
    let body =
        r#"{"ok":true,"version":"1.0.16+gx.11","leaderPid":7,"instanceId":"abc","build":"gx"}"#;

    let content_length = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let parsed = parse_healthz_response(content_length.as_bytes()).unwrap();
    assert_eq!(parsed.instance_id, "abc");
    assert_eq!(parsed.leader_pid, 7);
    assert!(parsed.ok);

    // Chunked framing: the size prefix and terminator are outside the braces, so first-`{`-to-
    // last-`}` reads the same object.
    let chunked = format!(
        "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
        body.len()
    );
    assert_eq!(parse_healthz_response(chunked.as_bytes()).unwrap(), parsed);

    for bad in [
        "HTTP/1.1 401 Unauthorized\r\n\r\n{\"error\":\"unauthorized\"}",
        "HTTP/1.1 200 OK\r\n\r\nnot json",
        "HTTP/1.1 200 OK\r\n\r\n",
        "no boundary here",
    ] {
        assert!(
            parse_healthz_response(bad.as_bytes()).is_err(),
            "{bad:?} must not parse"
        );
    }
}

#[test]
fn probing_a_port_nobody_owns_is_an_error_not_a_panic() {
    // Bind, read the port, drop: the port is now almost certainly free, and either way the probe
    // must return an `Err` rather than hang or panic.
    let squatter = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = squatter.local_addr().unwrap().port();
    drop(squatter);
    let err = probe_healthz(
        &format!("http://127.0.0.1:{port}"),
        Duration::from_millis(500),
    )
    .expect_err("nothing is listening");
    assert!(!format!("{err:#}").is_empty());
}

#[test]
fn the_json_report_carries_the_token_path_and_never_a_token() {
    let lanes = vec![lane("http://127.0.0.1:2421", "aaaa", Ok(health("aaaa")))];
    let text = render_json(&lanes).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert!(parsed.is_array(), "--json prints an array");
    assert_eq!(parsed[0]["url"], "http://127.0.0.1:2421");
    assert_eq!(parsed[0]["reachable"], true);
    assert_eq!(parsed[0]["tokenFile"], "/home/u/.grok/gx-remote.token");
    assert_eq!(parsed[0]["health"]["build"], "gx");
    assert!(parsed[0].get("token").is_none());
    assert!(parsed[0].get("error").is_none());

    // An empty report is still a well-formed array, not `null`.
    assert_eq!(render_json(&[]).unwrap(), "[]");
}

#[test]
fn the_human_report_names_the_token_file_and_shows_the_instance_short() {
    let lanes = vec![lane(
        "http://127.0.0.1:2421",
        &"a".repeat(32),
        Ok(health(&"a".repeat(32))),
    )];
    let text = render_human(&lanes, Path::new("/home/u/.grok"));
    assert!(text.contains("http://127.0.0.1:2421"), "{text}");
    assert!(
        text.contains("token file       /home/u/.grok/gx-remote.token"),
        "{text}"
    );
    assert!(
        text.contains(&format!("instance         {}", "a".repeat(12))),
        "{text}"
    );
    assert!(
        !text.contains(&"a".repeat(32)),
        "the full instance id is noise: {text}"
    );
    assert!(
        text.contains("healthz          ok (1.0.16+gx.11)"),
        "{text}"
    );

    assert_eq!(render_human(&[], Path::new("/home/u/.grok")), "");
}

// ---------------------------------------------------------------------------
// clap surface
// ---------------------------------------------------------------------------

fn parse(argv: &[&str]) -> RemoteCommand {
    use clap::Parser as _;
    let args = crate::app::PagerArgs::try_parse_from(argv).expect("args should parse");
    match args.command {
        Some(crate::app::Command::Remote(RemoteArgs { command })) => command,
        other => panic!("expected remote, got {other:?}"),
    }
}

#[test]
fn clap_parses_every_remote_subcommand() {
    assert!(matches!(
        parse(&["gx", "remote", "status"]),
        RemoteCommand::Status { json: false }
    ));
    assert!(matches!(
        parse(&["gx", "remote", "status", "--json"]),
        RemoteCommand::Status { json: true }
    ));
    assert!(matches!(parse(&["gx", "remote", "up"]), RemoteCommand::Up));

    // Pinned: there is no `--foreground`, and no way to ask for a lane without a leader.
    use clap::Parser as _;
    assert!(crate::app::PagerArgs::try_parse_from(["gx", "remote", "up", "--foreground"]).is_err());
    assert!(crate::app::PagerArgs::try_parse_from(["gx", "remote", "serve"]).is_err());
    assert!(crate::app::PagerArgs::try_parse_from(["gx", "remote"]).is_err());
}

#[test]
fn a_dead_leader_pid_is_reported_as_such() {
    let mut lane = lane("http://127.0.0.1:2421", "aaaa", Err("refused".into()));
    lane.pid_alive = false;
    let text = render_human(std::slice::from_ref(&lane), Path::new("/home/u/.grok"));
    assert!(text.contains("(not running)"), "{text}");
    assert!(!lane_json(&lane).pid_alive, "the leader pid is not alive");
    assert_eq!(lane_json(&lane).error, Some("refused"));
}
