//! gx: tests for the OpenAI (ChatGPT/Codex-plan) credential module.
//!
//! **Nothing here reads or writes the real `~/.codex/auth.json`, and nothing
//! here performs a live refresh.** Every case builds a fixture store in a temp
//! directory, drives an injected clock, and refreshes against either a scripted
//! [`TokenEndpoint`] or a local `TcpListener` — never `auth.openai.com`. A real
//! refresh rotates the user's refresh token server-side and would log them out
//! of codex.

use super::*;

use std::io::{BufRead as _, Read as _, Write as _};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

const NOW: i64 = 1_800_000_000;

/// A syntactically valid, cryptographically meaningless JWT. The module only
/// ever decodes the payload without verifying, which is the whole point: gx
/// reads claims for scheduling, the server is what validates the token.
fn jwt(exp: i64, account_id: Option<&str>) -> String {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let mut auth = serde_json::Map::new();
    if let Some(id) = account_id {
        auth.insert("chatgpt_account_id".into(), id.into());
    }
    auth.insert("chatgpt_plan_type".into(), "plus".into());
    let payload = serde_json::json!({
        "exp": exp,
        "https://api.openai.com/auth": auth,
    });
    let payload = engine.encode(serde_json::to_vec(&payload).expect("payload"));
    format!("{header}.{payload}.c2ln")
}

/// A store shaped like a real `auth.json`, including fields gx does not model
/// (`auth_mode`, `agent_identity`, a nested unknown object) so every test can
/// assert they survive.
fn fixture_json(access_token: &str, refresh_token: &str, last_refresh: Option<&str>) -> String {
    let mut doc = serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "id-token-value",
            "access_token": access_token,
            "refresh_token": refresh_token,
            "account_id": "acct_alpha",
            "unknown_token_field": {"nested": [1, 2, {"deep": true}]}
        },
        "agent_identity": {"kind": "cli", "trusted": false, "meta": {"a": 1}},
        "bedrock_region": "us-east-1",
        "some_future_field": [1, "two", null]
    });
    if let Some(lr) = last_refresh {
        doc["last_refresh"] = serde_json::Value::String(lr.to_owned());
    }
    serde_json::to_string_pretty(&doc).expect("fixture json")
}

struct Store {
    _dir: TempDir,
    _home: TempDir,
    paths: CodexPaths,
}

impl Store {
    fn new(body: &str) -> Self {
        let dir = tempfile::tempdir().expect("codex home");
        let home = tempfile::tempdir().expect("grok home");
        let auth_json = dir.path().join("auth.json");
        std::fs::write(&auth_json, body).expect("write fixture");
        let paths = CodexPaths::for_auth_json(auth_json, home.path());
        Self {
            _dir: dir,
            _home: home,
            paths,
        }
    }

    fn empty() -> Self {
        let dir = tempfile::tempdir().expect("codex home");
        let home = tempfile::tempdir().expect("grok home");
        let paths = CodexPaths::for_auth_json(dir.path().join("auth.json"), home.path());
        Self {
            _dir: dir,
            _home: home,
            paths,
        }
    }

    fn raw(&self) -> String {
        std::fs::read_to_string(&self.paths.auth_json).expect("read back")
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.raw()).expect("valid json")
    }

    fn write(&self, body: &str) {
        std::fs::write(&self.paths.auth_json, body).expect("rewrite fixture");
    }

    /// The rotation journal, if one is on disk.
    fn journal(&self) -> Option<RecoveryJournal> {
        read_recovery_journal(&self.paths.journal)
    }

    /// Plant a journal, as a process that crashed between the server's answer
    /// and its own write would have left behind.
    fn plant_journal(&self, refresh_token: &str) {
        let journal = RecoveryJournal {
            access_token: Some(jwt(NOW + 7200, None)),
            refresh_token: Some(refresh_token.to_owned()),
            id_token: None,
            received_at: rfc3339(NOW - 5),
        };
        std::fs::write(
            &self.paths.journal,
            serde_json::to_string_pretty(&journal).expect("render journal"),
        )
        .expect("plant journal");
    }
}

/// A scripted refresh endpoint that counts its calls. Never touches a socket.
#[derive(Default)]
struct MockEndpoint {
    calls: AtomicUsize,
    /// Answers, consumed in order; the last one repeats.
    script: Mutex<Vec<std::result::Result<RefreshResponse, String>>>,
    /// Refresh tokens the endpoint was handed, in order.
    seen: Mutex<Vec<String>>,
}

impl MockEndpoint {
    fn ok(resp: RefreshResponse) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(vec![Ok(resp)]),
            ..Default::default()
        })
    }

    fn scripted(script: Vec<std::result::Result<RefreshResponse, String>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            ..Default::default()
        })
    }

    /// Always fails with `invalid_grant`.
    fn invalid_grant() -> Arc<Self> {
        Self::scripted(vec![Err("invalid_grant".to_owned())])
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }
}

impl TokenEndpoint for MockEndpoint {
    fn refresh(&self, refresh_token: &str) -> std::result::Result<RefreshResponse, RefreshError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .expect("seen")
            .push(refresh_token.to_owned());
        let mut script = self.script.lock().expect("script");
        let answer = if script.len() > 1 {
            script.remove(0)
        } else {
            script.first().cloned_result()
        };
        answer.map_err(|code| {
            let msg = format!("token refresh failed: HTTP 400 ({code})");
            if code == "invalid_grant" {
                RefreshError::InvalidGrant(msg)
            } else {
                RefreshError::Other(msg)
            }
        })
    }
}

/// `Result<RefreshResponse, String>` is not `Clone` through `Option`, and the
/// last scripted answer must repeat; this keeps the borrow tidy.
trait ClonedResult {
    fn cloned_result(self) -> std::result::Result<RefreshResponse, String>;
}

impl ClonedResult for Option<&std::result::Result<RefreshResponse, String>> {
    fn cloned_result(self) -> std::result::Result<RefreshResponse, String> {
        match self {
            Some(Ok(r)) => Ok(r.clone()),
            Some(Err(e)) => Err(e.clone()),
            None => Err("exhausted".to_owned()),
        }
    }
}

fn refreshed(exp: i64) -> RefreshResponse {
    RefreshResponse {
        access_token: Some(jwt(exp, Some("acct_alpha"))),
        refresh_token: Some("rt-rotated".to_owned()),
        id_token: Some("id-token-new".to_owned()),
        expires_in: Some(exp - NOW),
    }
}

fn mint(store: &Store, now: i64, endpoint: &dyn TokenEndpoint) -> Result<MintedToken> {
    let clock = FixedClock(now);
    let mut opts = MintOptions::new(&store.paths, &clock, endpoint);
    opts.lock_timeout = Duration::from_secs(10);
    mint_token(&opts)
}

// ---------------------------------------------------------------------------
// reading and staleness
// ---------------------------------------------------------------------------

#[test]
fn cold_read_of_a_fresh_store_mints_without_refreshing() {
    let store = Store::new(&fixture_json(
        &jwt(NOW + 3600, Some("acct_alpha")),
        "rt-1",
        None,
    ));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    let token = mint(&store, NOW, endpoint.as_ref()).expect("mints");

    assert_eq!(token.expires_in, 3600);
    assert!(token.access_token.starts_with("eyJ"), "a JWT was emitted");
    assert_eq!(endpoint.calls(), 0, "a fresh token must not be refreshed");
    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-1"),
        "a fresh read must not rewrite the file"
    );
}

#[test]
fn the_seam_payload_is_exactly_two_fields() {
    let token = MintedToken {
        access_token: "tok".to_owned(),
        expires_in: 42,
    };
    assert_eq!(
        serde_json::to_string(&token).expect("render"),
        r#"{"access_token":"tok","expires_in":42}"#
    );
}

#[test]
fn a_missing_store_names_codex_login() {
    let store = Store::empty();
    let endpoint = MockEndpoint::ok(refreshed(NOW + 3600));

    let err = mint(&store, NOW, endpoint.as_ref()).expect_err("no file");
    let msg = format!("{err:#}");

    assert!(msg.contains("no codex credentials"), "{msg}");
    assert!(msg.contains("codex login"), "{msg}");
    assert_eq!(endpoint.calls(), 0);
}

#[test]
fn an_api_key_only_store_points_at_the_openai_api_provider() {
    let store = Store::new(r#"{"OPENAI_API_KEY": "sk-test-abc", "auth_mode": "apikey"}"#);
    let endpoint = MockEndpoint::ok(refreshed(NOW + 3600));

    let err = mint(&store, NOW, endpoint.as_ref()).expect_err("api-key only");
    let msg = format!("{err:#}");

    assert!(msg.contains("openai-api"), "{msg}");
    assert!(!msg.contains("sk-test-abc"), "the key must not be echoed");
    assert_eq!(endpoint.calls(), 0);
}

#[test]
fn freshness_follows_the_jwt_when_it_parses() {
    let doc: AuthDocument =
        serde_json::from_str(&fixture_json(&jwt(NOW + 600, None), "rt", None)).expect("parse");

    // 600s of life, 300s of skew: fresh now, stale once inside the window.
    assert_eq!(
        freshness(&doc, NOW),
        Freshness::FreshByJwt { expires_in: 600 }
    );
    assert_eq!(
        freshness(&doc, NOW + 299),
        Freshness::FreshByJwt { expires_in: 301 }
    );
    assert_eq!(freshness(&doc, NOW + 300), Freshness::StaleByJwt);
    assert_eq!(freshness(&doc, NOW + 10_000), Freshness::StaleByJwt);
}

#[test]
fn an_unparsable_jwt_falls_back_to_last_refresh_in_both_directions() {
    let recent = chrono::DateTime::from_timestamp(NOW - 60, 0)
        .expect("ts")
        .to_rfc3339();
    let ancient = chrono::DateTime::from_timestamp(NOW - LAST_REFRESH_FALLBACK_SECS - 60, 0)
        .expect("ts")
        .to_rfc3339();

    let fresh: AuthDocument =
        serde_json::from_str(&fixture_json("not-a-jwt", "rt", Some(&recent))).expect("parse");
    assert!(
        matches!(freshness(&fresh, NOW), Freshness::FreshByLastRefresh { .. }),
        "{:?}",
        freshness(&fresh, NOW)
    );

    let stale: AuthDocument =
        serde_json::from_str(&fixture_json("not-a-jwt", "rt", Some(&ancient))).expect("parse");
    assert_eq!(freshness(&stale, NOW), Freshness::StaleByLastRefresh);
}

#[test]
fn a_jwt_without_an_exp_claim_uses_last_refresh() {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let no_exp = format!(
        "{}.{}.sig",
        engine.encode(br#"{"alg":"none"}"#),
        engine.encode(br#"{"sub":"user"}"#)
    );
    let recent = chrono::DateTime::from_timestamp(NOW - 60, 0)
        .expect("ts")
        .to_rfc3339();

    let doc: AuthDocument =
        serde_json::from_str(&fixture_json(&no_exp, "rt", Some(&recent))).expect("parse");

    assert!(matches!(
        freshness(&doc, NOW),
        Freshness::FreshByLastRefresh { .. }
    ));
}

#[test]
fn neither_an_exp_nor_a_last_refresh_is_treated_as_stale() {
    let doc: AuthDocument =
        serde_json::from_str(&fixture_json("not-a-jwt", "rt", None)).expect("parse");
    assert_eq!(freshness(&doc, NOW), Freshness::StaleUnknown);
}

#[test]
fn an_absurd_exp_claim_does_not_overflow() {
    for exp in [i64::MIN, i64::MAX] {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let token = format!(
            "{}.{}.sig",
            engine.encode(br#"{"alg":"none"}"#),
            engine.encode(serde_json::json!({"exp": exp}).to_string())
        );
        let doc: AuthDocument =
            serde_json::from_str(&fixture_json(&token, "rt", None)).expect("parse");
        // The assertion is that this does not panic in a debug build.
        let _ = freshness(&doc, NOW);
    }
}

// ---------------------------------------------------------------------------
// refresh
// ---------------------------------------------------------------------------

#[test]
fn a_stale_store_refreshes_and_persists_the_rotated_token() {
    let before = fixture_json(&jwt(NOW - 10, Some("acct_alpha")), "rt-old", None);
    let store = Store::new(&before);
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    let token = mint(&store, NOW, endpoint.as_ref()).expect("mints");

    assert_eq!(endpoint.calls(), 1);
    assert_eq!(endpoint.seen(), vec!["rt-old".to_owned()]);
    assert_eq!(token.expires_in, 7200);

    let after = store.json();
    assert_eq!(
        after["tokens"]["refresh_token"],
        serde_json::json!("rt-rotated")
    );
    assert_eq!(
        after["tokens"]["id_token"],
        serde_json::json!("id-token-new")
    );
    assert_eq!(
        after["tokens"]["access_token"],
        serde_json::json!(token.access_token)
    );
    assert!(
        after["last_refresh"]
            .as_str()
            .is_some_and(|s| s.starts_with("2027-")),
        "last_refresh was stamped: {:?}",
        after["last_refresh"]
    );
}

#[test]
fn a_refresh_preserves_every_field_gx_does_not_model() {
    let before: serde_json::Value = serde_json::from_str(&fixture_json(
        &jwt(NOW - 10, Some("acct_alpha")),
        "rt-old",
        None,
    ))
    .expect("parse");
    let store = Store::new(&serde_json::to_string_pretty(&before).expect("render"));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    mint(&store, NOW, endpoint.as_ref()).expect("mints");
    let after = store.json();

    // Everything outside the three fields a refresh is allowed to touch must
    // be deep-equal, nested unknown objects included.
    for key in [
        "auth_mode",
        "agent_identity",
        "bedrock_region",
        "some_future_field",
    ] {
        assert_eq!(after[key], before[key], "top-level `{key}` changed");
    }
    assert_eq!(
        after["tokens"]["unknown_token_field"], before["tokens"]["unknown_token_field"],
        "a nested unknown object under `tokens` changed"
    );
    assert_eq!(
        after["tokens"]["account_id"], before["tokens"]["account_id"],
        "account_id is not a refresh output"
    );
}

#[test]
fn a_refresh_without_a_rotated_token_keeps_the_old_one() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    let endpoint = MockEndpoint::ok(RefreshResponse {
        access_token: Some(jwt(NOW + 3600, None)),
        refresh_token: None,
        id_token: None,
        expires_in: Some(3600),
    });

    mint(&store, NOW, endpoint.as_ref()).expect("mints");

    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-old"),
        "an absent refresh_token means the server kept ours alive"
    );
}

#[test]
fn a_store_without_a_refresh_token_says_run_codex_login() {
    let mut doc: serde_json::Value =
        serde_json::from_str(&fixture_json(&jwt(NOW - 10, None), "rt", None)).expect("parse");
    doc["tokens"]
        .as_object_mut()
        .expect("tokens")
        .remove("refresh_token");
    let store = Store::new(&serde_json::to_string(&doc).expect("render"));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 3600));

    let err = mint(&store, NOW, endpoint.as_ref()).expect_err("nothing to refresh with");

    assert!(format!("{err:#}").contains("codex login"), "{err:#}");
    assert_eq!(endpoint.calls(), 0);
}

#[test]
fn invalid_grant_reloads_once_and_then_gives_up() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    let endpoint = MockEndpoint::invalid_grant();

    let err = mint(&store, NOW, endpoint.as_ref()).expect_err("dead refresh token");
    let msg = format!("{err:#}");

    assert!(msg.contains("invalid_grant"), "{msg}");
    assert!(msg.contains("codex login"), "{msg}");
    // One POST. The reload found the same dead token, so there was no second
    // attempt and — critically — no retry loop.
    assert_eq!(endpoint.calls(), 1);
}

#[test]
fn invalid_grant_recovers_when_the_reload_finds_a_fresh_token() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    let fresh = fixture_json(&jwt(NOW + 3600, None), "rt-new", None);
    // The endpoint rewrites the file the way a racing codex would, then fails
    // the way the server would once codex had rotated the token.
    struct Racing {
        path: PathBuf,
        body: String,
        calls: AtomicUsize,
    }
    impl TokenEndpoint for Racing {
        fn refresh(&self, _rt: &str) -> std::result::Result<RefreshResponse, RefreshError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(&self.path, &self.body).expect("racing write");
            Err(RefreshError::InvalidGrant("invalid_grant".to_owned()))
        }
    }
    let endpoint = Racing {
        path: store.paths.auth_json.clone(),
        body: fresh,
        calls: AtomicUsize::new(0),
    };

    let token = mint(&store, NOW, &endpoint).expect("recovers from the reload");

    assert_eq!(token.expires_in, 3600);
    assert_eq!(
        endpoint.calls.load(Ordering::SeqCst),
        1,
        "the reload short-circuits before a second POST"
    );
}

#[test]
fn invalid_grant_retries_once_with_a_token_another_process_rotated_in() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    // Still expired, but carrying a different refresh token: exactly the state
    // a codex refresh that rotated-but-did-not-mint would leave behind.
    let rotated = fixture_json(&jwt(NOW - 10, None), "rt-someone-elses", None);
    let endpoint = MockEndpoint::scripted(vec![
        Err("invalid_grant".to_owned()),
        Ok(refreshed(NOW + 3600)),
    ]);

    // Rewrite the file the moment the first POST fails.
    struct Sequencer {
        inner: Arc<MockEndpoint>,
        path: PathBuf,
        body: String,
    }
    impl TokenEndpoint for Sequencer {
        fn refresh(&self, rt: &str) -> std::result::Result<RefreshResponse, RefreshError> {
            let out = self.inner.refresh(rt);
            if out.is_err() {
                std::fs::write(&self.path, &self.body).expect("rewrite");
            }
            out
        }
    }
    let seq = Sequencer {
        inner: Arc::clone(&endpoint),
        path: store.paths.auth_json.clone(),
        body: rotated,
    };

    let token = mint(&store, NOW, &seq).expect("second attempt succeeds");

    assert_eq!(token.expires_in, 3600);
    assert_eq!(endpoint.calls(), 2, "exactly one retry, never a loop");
    assert_eq!(
        endpoint.seen(),
        vec!["rt-old".to_owned(), "rt-someone-elses".to_owned()]
    );
}

// ---------------------------------------------------------------------------
// the lock
// ---------------------------------------------------------------------------

#[test]
fn a_store_refreshed_while_we_waited_for_the_lock_is_not_refreshed_again() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    let endpoint = Arc::new(MockEndpoint {
        script: Mutex::new(vec![Ok(refreshed(NOW + 7200))]),
        ..Default::default()
    });

    // Hold the lock, as a sibling gx process mid-refresh would.
    let held = crate::providers_cmd::lock_providers_at(&store.paths.lock, Duration::from_secs(5))
        .expect("lock");

    let paths = store.paths.clone();
    let ep = Arc::clone(&endpoint);
    let worker = std::thread::spawn(move || {
        let clock = FixedClock(NOW);
        let mut opts = MintOptions::new(&paths, &clock, ep.as_ref());
        opts.lock_timeout = Duration::from_secs(10);
        mint_token(&opts)
    });

    // Give the worker time to read (stale), then block on the lock.
    std::thread::sleep(Duration::from_millis(150));
    // Now do what the lock holder would have done.
    store.write(&fixture_json(&jwt(NOW + 3600, None), "rt-fresh", None));
    drop(held);

    let token = worker.join().expect("thread").expect("mints");

    assert_eq!(token.expires_in, 3600);
    assert_eq!(
        endpoint.calls(),
        0,
        "the re-read under the lock must cancel the refresh"
    );
    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-fresh"),
        "the other writer's rotated token survives"
    );
}

#[test]
fn concurrent_mints_serialize_and_rotate_exactly_once() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    let workers: Vec<_> = (0..6)
        .map(|_| {
            let paths = store.paths.clone();
            let ep = Arc::clone(&endpoint);
            std::thread::spawn(move || {
                let clock = FixedClock(NOW);
                let mut opts = MintOptions::new(&paths, &clock, ep.as_ref());
                opts.lock_timeout = Duration::from_secs(20);
                mint_token(&opts)
            })
        })
        .collect();

    let tokens: Vec<MintedToken> = workers
        .into_iter()
        .map(|w| w.join().expect("thread").expect("mints"))
        .collect();

    assert_eq!(
        endpoint.calls(),
        1,
        "the flock must let exactly one process rotate"
    );
    let first = &tokens[0].access_token;
    assert!(
        tokens.iter().all(|t| &t.access_token == first),
        "every caller gets the same token"
    );
    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-rotated")
    );
}

// ---------------------------------------------------------------------------
// the real HTTP path, against a local listener
// ---------------------------------------------------------------------------

/// One-shot HTTP/1.1 server. Returns its URL and a handle yielding the request
/// body it received.
fn spawn_token_server(
    status_line: &'static str,
    body: String,
) -> (String, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!(
        "http://{}/oauth/token",
        listener.local_addr().expect("addr")
    );
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut reader = std::io::BufReader::new(sock.try_clone().expect("clone"));
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("read header") == 0 {
                break;
            }
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).expect("read body");
        let response = format!(
            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(response.as_bytes()).expect("write");
        sock.flush().ok();
        String::from_utf8_lossy(&buf).into_owned()
    });
    (url, handle)
}

#[test]
fn the_http_endpoint_posts_the_documented_json_body() {
    let body = serde_json::json!({
        "access_token": jwt(NOW + 3600, None),
        "refresh_token": "rt-rotated",
        "id_token": "id-new",
        "expires_in": 3600,
    })
    .to_string();
    let (url, server) = spawn_token_server("HTTP/1.1 200 OK", body);

    let endpoint = HttpTokenEndpoint {
        url,
        client_id: CODEX_CLIENT_ID.to_owned(),
        timeout: Duration::from_secs(10),
    };
    let resp = endpoint.refresh("rt-old").expect("refreshes");

    let sent: serde_json::Value =
        serde_json::from_str(&server.join().expect("server")).expect("request body is JSON");
    assert_eq!(sent["client_id"], serde_json::json!(CODEX_CLIENT_ID));
    assert_eq!(sent["grant_type"], serde_json::json!("refresh_token"));
    assert_eq!(sent["refresh_token"], serde_json::json!("rt-old"));
    assert_eq!(resp.refresh_token.as_deref(), Some("rt-rotated"));
}

#[test]
fn the_http_endpoint_classifies_invalid_grant() {
    let (url, server) = spawn_token_server(
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"invalid_grant","error_description":"expired"}"#.to_owned(),
    );
    let endpoint = HttpTokenEndpoint {
        url,
        client_id: CODEX_CLIENT_ID.to_owned(),
        timeout: Duration::from_secs(10),
    };

    let err = endpoint.refresh("rt-old").expect_err("400");
    let _ = server.join();

    assert!(matches!(err, RefreshError::InvalidGrant(_)), "{err}");
}

#[test]
fn a_full_mint_drives_the_real_http_path_end_to_end() {
    let store = Store::new(&fixture_json(
        &jwt(NOW - 10, Some("acct_alpha")),
        "rt-old",
        None,
    ));
    let body = serde_json::json!({
        "access_token": jwt(NOW + 7200, Some("acct_alpha")),
        "refresh_token": "rt-rotated",
        "expires_in": 7200,
    })
    .to_string();
    let (url, server) = spawn_token_server("HTTP/1.1 200 OK", body);
    let endpoint = HttpTokenEndpoint {
        url,
        client_id: CODEX_CLIENT_ID.to_owned(),
        timeout: Duration::from_secs(10),
    };

    let token = mint(&store, NOW, &endpoint).expect("mints");
    let _ = server.join();

    assert_eq!(token.expires_in, 7200);
    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-rotated")
    );
}

// ---------------------------------------------------------------------------
// account-change detection
// ---------------------------------------------------------------------------

#[test]
fn a_first_sighting_is_not_an_account_change() {
    let check = account_check(Some("acct_alpha".to_owned()), None);
    assert!(!check.changed);
}

#[test]
fn a_different_account_id_is_reported_as_changed() {
    let check = account_check(Some("acct_beta".to_owned()), Some("acct_alpha".to_owned()));
    assert!(check.changed);
    assert_eq!(check.cached.as_deref(), Some("acct_alpha"));
}

#[test]
fn the_account_cache_round_trips() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("nested").join("state.json");

    assert_eq!(read_state(&path), GxCodexState::default());
    record_account(&path, Some("acct_alpha"));
    assert_eq!(read_state(&path).account_id.as_deref(), Some("acct_alpha"));

    record_account(&path, Some("acct_beta"));
    assert_eq!(read_state(&path).account_id.as_deref(), Some("acct_beta"));
}

#[test]
fn the_account_id_falls_back_to_the_jwt_claim() {
    let mut doc: serde_json::Value = serde_json::from_str(&fixture_json(
        &jwt(NOW + 600, Some("acct_from_jwt")),
        "rt",
        None,
    ))
    .expect("parse");
    doc["tokens"]
        .as_object_mut()
        .expect("tokens")
        .remove("account_id");
    let parsed: AuthDocument = serde_json::from_value(doc).expect("typed");

    assert_eq!(parsed.account_id().as_deref(), Some("acct_from_jwt"));
    assert_eq!(parsed.plan().as_deref(), Some("plus"));
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

#[test]
fn the_lockfile_sits_beside_auth_json_and_never_inside_it() {
    let paths = CodexPaths::for_auth_json(
        PathBuf::from("/tmp/codexhome/auth.json"),
        Path::new("/tmp/grokhome"),
    );
    assert_eq!(paths.lock, PathBuf::from("/tmp/codexhome/.gx-auth.lock"));
    assert_eq!(
        paths.state,
        PathBuf::from("/tmp/grokhome/openai-codex-state.json"),
        "gx state lives in $GROK_HOME, not in codex's directory"
    );
}

#[cfg(unix)]
#[test]
fn a_rewritten_store_stays_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    std::fs::set_permissions(
        &store.paths.auth_json,
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod");
    let endpoint = MockEndpoint::ok(refreshed(NOW + 3600));

    mint(&store, NOW, endpoint.as_ref()).expect("mints");

    let mode = std::fs::metadata(&store.paths.auth_json)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "a refresh must not leave the store world-readable"
    );
}

// ---------------------------------------------------------------------------
// forced refresh after a 401
// ---------------------------------------------------------------------------

fn mint_forced(store: &Store, now: i64, endpoint: &dyn TokenEndpoint) -> Result<MintedToken> {
    let clock = FixedClock(now);
    let mut opts = MintOptions::new(&store.paths, &clock, endpoint);
    opts.lock_timeout = Duration::from_secs(10);
    opts.force_refresh = true;
    mint_token(&opts)
}

#[test]
fn a_rejected_token_is_refreshed_even_though_its_exp_says_it_is_fine() {
    // The 401 case: the server disowned a token whose `exp` is hours away.
    // Handing the same one back would fail the turn a second time.
    let store = Store::new(&fixture_json(&jwt(NOW + 3600, None), "rt-old", None));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    let token = mint_forced(&store, NOW, endpoint.as_ref()).expect("mints");

    assert_eq!(endpoint.calls(), 1);
    assert_eq!(token.expires_in, 7200);
    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-rotated")
    );
}

#[test]
fn a_forced_refresh_yields_to_a_token_this_process_never_saw() {
    // Two sessions 401 on the same dead token. The first rotates; the second
    // must take that result rather than rotating again — otherwise every
    // concurrent 401 burns another refresh token.
    let store = Store::new(&fixture_json(&jwt(NOW + 3600, None), "rt-old", None));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    let held = crate::providers_cmd::lock_providers_at(&store.paths.lock, Duration::from_secs(5))
        .expect("lock");
    let paths = store.paths.clone();
    let ep = Arc::clone(&endpoint);
    let worker = std::thread::spawn(move || {
        let clock = FixedClock(NOW);
        let mut opts = MintOptions::new(&paths, &clock, ep.as_ref());
        opts.lock_timeout = Duration::from_secs(10);
        opts.force_refresh = true;
        mint_token(&opts)
    });
    std::thread::sleep(Duration::from_millis(150));
    store.write(&fixture_json(
        &jwt(NOW + 9000, None),
        "rt-someone-elses",
        None,
    ));
    drop(held);

    let token = worker.join().expect("thread").expect("mints");

    assert_eq!(token.expires_in, 9000);
    assert_eq!(endpoint.calls(), 0, "no second rotation");
    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-someone-elses")
    );
}

#[test]
fn the_expired_env_var_is_the_one_grok_sets() {
    // Named here so a rename in the shell's auth-provider path breaks a test
    // rather than silently disabling 401 recovery.
    assert_eq!(AUTH_EXPIRED_ENV, "GROK_AUTH_EXPIRED");
}

#[test]
fn an_explicit_null_survives_a_refresh() {
    // The real store carries `"OPENAI_API_KEY": null`. Modelling it as an
    // `Option<String>` would erase the key on the way back out — indisting-
    // uishable from "absent" once deserialized.
    let mut doc: serde_json::Value =
        serde_json::from_str(&fixture_json(&jwt(NOW - 10, None), "rt-old", None)).expect("parse");
    doc["OPENAI_API_KEY"] = serde_json::Value::Null;
    doc["tokens"]["some_null_field"] = serde_json::Value::Null;
    let store = Store::new(&serde_json::to_string_pretty(&doc).expect("render"));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 3600));

    mint(&store, NOW, endpoint.as_ref()).expect("mints");

    let after = store.json();
    assert!(
        after
            .as_object()
            .expect("object")
            .contains_key("OPENAI_API_KEY"),
        "an explicit null must not be deleted: {after}"
    );
    assert_eq!(after["OPENAI_API_KEY"], serde_json::Value::Null);
    assert_eq!(after["tokens"]["some_null_field"], serde_json::Value::Null);
}

#[test]
fn a_null_api_key_is_not_an_api_key_only_store() {
    // `"OPENAI_API_KEY": null` alongside real tokens is the ordinary ChatGPT
    // login, not the API-key provider's credential.
    let mut doc: serde_json::Value =
        serde_json::from_str(&fixture_json(&jwt(NOW + 3600, None), "rt", None)).expect("parse");
    doc["OPENAI_API_KEY"] = serde_json::Value::Null;
    let store = Store::new(&serde_json::to_string(&doc).expect("render"));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    let token = mint(&store, NOW, endpoint.as_ref()).expect("mints");

    assert_eq!(token.expires_in, 3600);
    assert_eq!(endpoint.calls(), 0);
}

// ---------------------------------------------------------------------------
// the rotation-recovery journal
//
// The window this file exists to close: the server has issued R2 and killed R1,
// and R2 is still only in this process's memory. Everything below either walks
// through that window or simulates having died inside it.
// ---------------------------------------------------------------------------

#[test]
fn a_successful_refresh_leaves_no_recovery_journal_behind() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    mint(&store, NOW, endpoint.as_ref()).expect("mints");

    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-rotated")
    );
    assert!(
        !store.paths.journal.exists(),
        "the journal is deleted once auth.json holds the rotation"
    );
}

#[test]
fn a_rotation_that_cannot_be_persisted_is_left_in_the_journal() {
    // The crash window, made deterministic: the store becomes unwritable
    // between the server's answer and gx's write. Whatever happens next, the
    // rotated refresh token must be on disk somewhere — otherwise the user is
    // locked out of codex, not just out of gx.
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));

    struct Sabotage {
        path: PathBuf,
    }
    impl TokenEndpoint for Sabotage {
        fn refresh(&self, _rt: &str) -> std::result::Result<RefreshResponse, RefreshError> {
            // A directory where auth.json was: the rename that publishes the
            // new document cannot land.
            std::fs::remove_file(&self.path).expect("remove store");
            std::fs::create_dir(&self.path).expect("block the path");
            Ok(refreshed(NOW + 7200))
        }
    }
    let endpoint = Sabotage {
        path: store.paths.auth_json.clone(),
    };

    let err = mint(&store, NOW, &endpoint).expect_err("the write cannot land");
    assert!(format!("{err:#}").contains("failed to write"), "{err:#}");

    let journal = store.journal().expect("the journal survives the failure");
    assert_eq!(journal.refresh_token.as_deref(), Some("rt-rotated"));
    assert!(!journal.received_at.is_empty(), "{journal:?}");
}

#[test]
fn a_journalled_rotation_recovers_a_store_whose_token_is_already_dead() {
    // The full crash-and-recover story: a previous run rotated to
    // `rt-journalled`, died before writing, and left the store holding the
    // now-dead `rt-dead`. This mint must find its way out without `codex login`.
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-dead", None));
    store.plant_journal("rt-journalled");
    let endpoint = MockEndpoint::scripted(vec![
        Err("invalid_grant".to_owned()),
        Ok(refreshed(NOW + 7200)),
    ]);

    let token = mint(&store, NOW, endpoint.as_ref()).expect("recovers");

    assert_eq!(token.expires_in, 7200);
    assert_eq!(
        endpoint.seen(),
        vec!["rt-dead".to_owned(), "rt-journalled".to_owned()],
        "the journalled token is tried exactly once, after the store's"
    );
    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-rotated")
    );
    assert!(
        !store.paths.journal.exists(),
        "the recovered rotation is persisted, so the journal is cleared"
    );
}

#[test]
fn a_journalled_token_the_server_also_rejects_names_the_recovery_file() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-dead", None));
    store.plant_journal("rt-journalled");
    let endpoint = MockEndpoint::invalid_grant();

    let err = mint(&store, NOW, endpoint.as_ref()).expect_err("both tokens are dead");
    let msg = format!("{err:#}");

    assert!(msg.contains("codex login"), "{msg}");
    assert!(
        msg.contains(".gx-auth-recovery.json"),
        "the recovery file is named so the user can see what was tried: {msg}"
    );
    assert_eq!(endpoint.calls(), 2, "the store's token, then the journal's");
    assert!(
        store.paths.journal.exists(),
        "a rejected journal is kept, never deleted — it is still the only copy"
    );
}

#[test]
fn a_journal_matching_the_store_is_not_retried() {
    // Nothing to recover: the journal holds what the store already has, and
    // re-POSTing it would just be a second identical failure.
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    store.plant_journal("rt-old");
    let endpoint = MockEndpoint::invalid_grant();

    let err = mint(&store, NOW, endpoint.as_ref()).expect_err("dead token");

    assert_eq!(endpoint.calls(), 1, "no pointless second POST");
    assert!(format!("{err:#}").contains("codex login"), "{err:#}");
}

// ---------------------------------------------------------------------------
// a 2xx that rotates but cannot be minted from
// ---------------------------------------------------------------------------

#[test]
fn a_2xx_with_a_rotation_but_no_access_token_still_persists_the_rotation() {
    // The server has already killed `rt-old` by issuing `rt-rotated`. Bailing
    // with the replacement only in memory would be the lockout this module
    // exists to prevent, so it is written *before* the error is returned.
    let before = fixture_json(&jwt(NOW - 10, None), "rt-old", None);
    let store = Store::new(&before);
    let endpoint = MockEndpoint::ok(RefreshResponse {
        access_token: None,
        refresh_token: Some("rt-rotated".to_owned()),
        id_token: None,
        expires_in: Some(3600),
    });

    let err = mint(&store, NOW, endpoint.as_ref()).expect_err("nothing to mint");
    assert!(format!("{err:#}").contains("no access_token"), "{err:#}");

    let after = store.json();
    assert_eq!(
        after["tokens"]["refresh_token"],
        serde_json::json!("rt-rotated"),
        "the rotation is persisted even though the mint failed"
    );
    let before: serde_json::Value = serde_json::from_str(&before).expect("parse");
    assert_eq!(
        after["tokens"]["access_token"], before["tokens"]["access_token"],
        "the old access token is kept — the response carried no replacement"
    );
    assert_eq!(
        after.get("last_refresh"),
        None,
        "last_refresh is not stamped: the store is still stale and must refresh again"
    );
    assert_eq!(
        after["agent_identity"], before["agent_identity"],
        "the partial write is still a full-document merge"
    );
    assert!(
        !store.paths.journal.exists(),
        "the rotation reached auth.json, so the journal is cleared"
    );
}

#[test]
fn a_2xx_with_neither_a_token_nor_a_rotation_leaves_nothing_behind() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    let endpoint = MockEndpoint::ok(RefreshResponse::default());

    let err = mint(&store, NOW, endpoint.as_ref()).expect_err("an empty 2xx");

    assert!(format!("{err:#}").contains("no access_token"), "{err:#}");
    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-old"),
        "nothing was rotated, so nothing is replaced"
    );
    assert!(
        !store.paths.journal.exists(),
        "a journal with nothing to recover is not left lying around"
    );
}

// ---------------------------------------------------------------------------
// forced refresh: the generation rule
// ---------------------------------------------------------------------------

#[test]
fn a_forced_refresh_that_hits_invalid_grant_never_returns_the_rejected_token() {
    // `exp` says this token is good for another hour; the server said
    // otherwise, which is why `force_refresh` is set. When the refresh fails
    // and the reload finds the *same* credential, handing that token back would
    // fail the turn a second time and hide the real problem.
    let store = Store::new(&fixture_json(&jwt(NOW + 3600, None), "rt-old", None));
    let endpoint = MockEndpoint::invalid_grant();

    let err = mint_forced(&store, NOW, endpoint.as_ref()).expect_err("must not mint");
    let msg = format!("{err:#}");

    assert!(msg.contains("invalid_grant"), "{msg}");
    assert!(msg.contains("codex login"), "{msg}");
    assert_eq!(endpoint.calls(), 1, "one POST, no retry loop");
}

#[test]
fn a_forced_refresh_falls_through_to_the_journal_before_giving_up() {
    // Same as above, but a previous run left a rotation behind: the fall-
    // through must reach it rather than stopping at the rejected token.
    let store = Store::new(&fixture_json(&jwt(NOW + 3600, None), "rt-dead", None));
    store.plant_journal("rt-journalled");
    let endpoint = MockEndpoint::scripted(vec![
        Err("invalid_grant".to_owned()),
        Ok(refreshed(NOW + 7200)),
    ]);

    let token = mint_forced(&store, NOW, endpoint.as_ref()).expect("recovers");

    assert_eq!(token.expires_in, 7200);
    assert_eq!(
        endpoint.seen(),
        vec!["rt-dead".to_owned(), "rt-journalled".to_owned()]
    );
}

#[test]
fn a_forced_refresh_compares_the_whole_credential_not_just_the_access_token() {
    // Two sessions 401 on the same credential. The first rotates, and the
    // server hands back an access-token string identical to the old one —
    // nothing forbids that. The second session must still see a *new*
    // generation (the refresh token changed) and use it, because rotating again
    // would burn the token the first session just stored.
    let access = jwt(NOW + 3600, None);
    let store = Store::new(&fixture_json(&access, "rt-old", None));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    let held = crate::providers_cmd::lock_providers_at(&store.paths.lock, Duration::from_secs(5))
        .expect("lock");
    let paths = store.paths.clone();
    let ep = Arc::clone(&endpoint);
    let worker = std::thread::spawn(move || {
        let clock = FixedClock(NOW);
        let mut opts = MintOptions::new(&paths, &clock, ep.as_ref());
        opts.lock_timeout = Duration::from_secs(10);
        opts.force_refresh = true;
        mint_token(&opts)
    });
    std::thread::sleep(Duration::from_millis(150));
    // The other session's result: same access token string, rotated refresh.
    store.write(&fixture_json(&access, "rt-someone-elses", None));
    drop(held);

    let token = worker.join().expect("thread").expect("mints");

    assert_eq!(token.access_token, access);
    assert_eq!(
        endpoint.calls(),
        0,
        "the refresh token changed, so this session must not rotate again"
    );
    assert_eq!(
        store.json()["tokens"]["refresh_token"],
        serde_json::json!("rt-someone-elses")
    );
}

// ---------------------------------------------------------------------------
// symlinked stores
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_symlinked_store_is_updated_through_the_link() {
    // `~/.codex/auth.json` is routinely a symlink into a dotfile repo. A
    // tmp+rename over the link would replace it with a regular file: the target
    // would keep the old, now-dead refresh token, and everything reading
    // through the real path would be logged out.
    let dir = tempfile::tempdir().expect("codex home");
    let home = tempfile::tempdir().expect("grok home");
    let target = dir.path().join("real-auth.json");
    let link = dir.path().join("auth.json");
    std::fs::write(&target, fixture_json(&jwt(NOW - 10, None), "rt-old", None)).expect("write");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let store = Store {
        _dir: dir,
        _home: home,
        paths: CodexPaths::for_auth_json(link.clone(), Path::new("/nonexistent-grok-home")),
    };
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    mint(&store, NOW, endpoint.as_ref()).expect("mints");

    assert!(
        std::fs::symlink_metadata(&link)
            .expect("stat")
            .file_type()
            .is_symlink(),
        "the link itself must survive the write"
    );
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&target).expect("read target"))
            .expect("json");
    assert_eq!(
        written["tokens"]["refresh_token"],
        serde_json::json!("rt-rotated"),
        "the rotation landed in the link's target"
    );
}

#[cfg(unix)]
#[test]
fn the_lock_and_journal_follow_the_link_to_the_real_directory() {
    // Two gx processes reaching the same store by different paths must take the
    // same lock, which only holds if the lock is placed beside the *resolved*
    // file.
    let dir = tempfile::tempdir().expect("codex home");
    let real_dir = dir.path().join("real");
    let link_dir = dir.path().join("link");
    std::fs::create_dir(&real_dir).expect("mkdir");
    std::os::unix::fs::symlink(&real_dir, &link_dir).expect("symlink dir");
    std::fs::write(
        real_dir.join("auth.json"),
        fixture_json(&jwt(NOW - 10, None), "rt-old", None),
    )
    .expect("write");

    let paths = CodexPaths::for_auth_json(
        link_dir.join("auth.json"),
        Path::new("/nonexistent-grok-home"),
    );
    let resolved = paths.canonicalized().expect("canonicalize");

    assert_eq!(
        resolved.lock.parent().map(std::path::Path::to_path_buf),
        Some(std::fs::canonicalize(&real_dir).expect("canonical dir")),
        "the lock sits beside the real file"
    );
    assert_eq!(
        resolved.journal.file_name().and_then(|n| n.to_str()),
        Some(".gx-auth-recovery.json")
    );
    assert_eq!(
        resolved.auth_json,
        std::fs::canonicalize(real_dir.join("auth.json")).expect("canonical")
    );
}

#[test]
fn a_store_that_does_not_exist_yet_keeps_its_literal_path() {
    // Canonicalization is for stores gx is about to *write*; a missing one must
    // still produce the "run codex login" message, not a resolution error.
    let store = Store::empty();
    let resolved = store.paths.canonicalized().expect("no file, no resolution");
    assert_eq!(resolved.auth_json, store.paths.auth_json);
}

// ---------------------------------------------------------------------------
// the durable writer
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn the_durable_writer_creates_the_file_owner_only_and_leaves_no_temp() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("secret.json");

    write_secret_atomically(&path, "{\"refresh_token\":\"rt\"}\n").expect("writes");

    assert_eq!(
        std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
        0o600,
        "the mode is set at creation, before any secret byte is written"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "{\"refresh_token\":\"rt\"}\n"
    );

    // Replacing a loosened file re-tightens it, and nothing is left over.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    write_secret_atomically(&path, "second\n").expect("rewrites");
    assert_eq!(
        std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "second\n");

    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("readdir")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn the_durable_writer_reports_a_directory_it_cannot_write_into() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("nope").join("secret.json");

    let err = write_secret_atomically(&path, "x").expect_err("no such directory");

    assert!(format!("{err:#}").contains("failed to write"), "{err:#}");
}

// ---------------------------------------------------------------------------
// timeouts and error rendering
// ---------------------------------------------------------------------------

#[test]
fn the_credential_lock_waits_long_enough_for_a_holder_that_is_working() {
    // A holder can legitimately spend two 30s round trips (the POST plus its
    // one retry) inside the lock. A shorter wait would time out on a process
    // that is making progress and send the user to `codex login` for nothing.
    assert_eq!(LOCK_TIMEOUT, Duration::from_secs(90));
    assert!(
        LOCK_TIMEOUT >= REFRESH_TIMEOUT * 2,
        "the wait must cover the worst holder that still succeeds"
    );
}

#[test]
fn only_known_oauth_error_codes_are_ever_printed() {
    for code in KNOWN_OAUTH_ERRORS {
        assert_eq!(describe(code), format!(" ({code})"));
    }
    assert_eq!(describe(""), "");
    assert_eq!(
        describe("bearer sk-live-CANARY"),
        " (unrecognized OAuth error)"
    );
}

#[test]
fn an_unrecognized_oauth_error_is_never_echoed_to_the_user() {
    // `error` is server text. A body that answers with a page of HTML, or with
    // the token it was just sent, must not end up in stderr or in scrollback.
    const CANARY: &str = "sk-live-CANARY-9f3b";
    let (url, server) = spawn_token_server(
        "HTTP/1.1 400 Bad Request",
        serde_json::json!({
            "error": CANARY,
            "error_description": format!("{CANARY} is not a grant type"),
        })
        .to_string(),
    );
    let endpoint = HttpTokenEndpoint {
        url,
        client_id: CODEX_CLIENT_ID.to_owned(),
        timeout: Duration::from_secs(10),
    };

    let err = endpoint.refresh("rt-old").expect_err("400");
    let _ = server.join();
    let msg = err.to_string();

    assert!(matches!(err, RefreshError::Other(_)), "{msg}");
    assert!(msg.contains("unrecognized OAuth error"), "{msg}");
    assert!(!msg.contains("CANARY"), "server text was echoed: {msg}");
    assert!(!msg.contains("sk-live"), "server text was echoed: {msg}");
}

#[test]
fn a_recognized_oauth_error_code_still_reaches_the_message() {
    let (url, server) = spawn_token_server(
        "HTTP/1.1 429 Too Many Requests",
        r#"{"error":"rate_limited"}"#.to_owned(),
    );
    let endpoint = HttpTokenEndpoint {
        url,
        client_id: CODEX_CLIENT_ID.to_owned(),
        timeout: Duration::from_secs(10),
    };

    let err = endpoint.refresh("rt-old").expect_err("429");
    let _ = server.join();

    assert!(err.to_string().contains("(rate_limited)"), "{err}");
}

// ---------------------------------------------------------------------------
// platform support
// ---------------------------------------------------------------------------

#[cfg(not(unix))]
#[test]
fn refreshing_is_refused_where_gx_cannot_persist_a_rotation() {
    let store = Store::new(&fixture_json(&jwt(NOW - 10, None), "rt-old", None));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    let err = mint(&store, NOW, endpoint.as_ref()).expect_err("unsupported platform");

    assert!(
        format!("{err:#}").contains("unsupported on this platform"),
        "{err:#}"
    );
    assert_eq!(endpoint.calls(), 0, "the refusal comes before any POST");
}

#[cfg(not(unix))]
#[test]
fn a_fresh_token_is_still_readable_where_refreshing_is_refused() {
    let store = Store::new(&fixture_json(&jwt(NOW + 3600, None), "rt-1", None));
    let endpoint = MockEndpoint::ok(refreshed(NOW + 7200));

    let token = mint(&store, NOW, endpoint.as_ref()).expect("reads");

    assert_eq!(token.expires_in, 3600);
}
