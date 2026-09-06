// Slot names are process-global, so every test uses a unique name and needs no #[serial]
// No test mutates the process env: the scrub test sets its leak values on the child command instead

use super::test_counting_provider as counting_provider;
use super::*;

#[tokio::test]
async fn provider_token_is_cached_while_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let provider = counting_provider("test-cache", dir.path());
    assert_eq!(
        provider.cached_token(),
        None,
        "cache-only read must miss on a cold cache without running the command"
    );
    let first = provider.ensure_fresh_token(None).await.rotated().unwrap();
    let second = provider.ensure_fresh_token(None).await.rotated().unwrap();
    assert_eq!(first, "tok-1");
    assert_eq!(second, "tok-1", "fresh token must be served from cache");
    assert_eq!(
        provider.cached_token().as_deref(),
        Some("tok-1"),
        "sync cache-only read must serve the warm cache"
    );
}

#[tokio::test]
async fn provider_token_reminted_when_expired() {
    let dir = tempfile::tempdir().unwrap();
    let provider = counting_provider("test-expiry", dir.path());
    assert_eq!(
        provider.ensure_fresh_token(None).await.rotated().unwrap(),
        "tok-1"
    );
    test_expire_provider_token("test-expiry");
    assert_eq!(
        provider.cached_token(),
        None,
        "cache-only read must not serve a stale token"
    );
    assert_eq!(
        provider.ensure_fresh_token(None).await.rotated().unwrap(),
        "tok-2",
        "expired token must be re-minted"
    );
}

#[tokio::test]
async fn provider_pre_turn_refresh_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let provider = counting_provider("test-stale", dir.path());
    let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

    assert_eq!(
        provider.ensure_fresh_token(Some(&token)).await,
        ProviderRefreshOutcome::Unchanged,
        "fresh matching token must not be re-minted pre-turn"
    );
    assert_eq!(
        provider
            .ensure_fresh_token(Some("lagging-chat-state-key"))
            .await
            .rotated()
            .as_deref(),
        Some("tok-1"),
        "chat-state lagging behind a rotation adopts the fresh cached token"
    );
    test_expire_provider_token("test-stale");
    assert_eq!(
        provider
            .ensure_fresh_token(Some(&token))
            .await
            .rotated()
            .as_deref(),
        Some("tok-2"),
        "stale token must be re-minted pre-turn"
    );
}

#[tokio::test]
async fn provider_401_recovery_has_fresh_mint_guard() {
    let dir = tempfile::tempdir().unwrap();
    let provider = counting_provider("test-401", dir.path());
    let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

    assert_eq!(
        provider.recover_rejected_token(&token).await,
        None,
        "a token minted moments ago must not be re-minted on 401 (loop guard)"
    );

    test_backdate_provider_mint("test-401", std::time::Duration::from_secs(60));
    assert_eq!(
        provider.recover_rejected_token(&token).await.as_deref(),
        Some("tok-2"),
        "an aged rejected token is re-minted once"
    );

    assert_eq!(
        provider.recover_rejected_token(&token).await.as_deref(),
        Some("tok-2"),
        "a rejection of the already-replaced key adopts the fresh token without a re-run"
    );
}

/// Regression: a warm cache must not outlive the provider's config.
#[tokio::test]
async fn provider_removed_from_config_drops_cached_token() {
    let dir = tempfile::tempdir().unwrap();
    let provider = counting_provider("test-removed", dir.path());
    let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

    let removed = AuthProviderRef::new("test-removed".to_owned(), AuthProviderConfig::default());
    assert_eq!(
        removed.cached_token(),
        None,
        "empty command must fail closed even with a warm cache"
    );
    assert_eq!(
        removed.ensure_fresh_token(Some(&token)).await,
        ProviderRefreshOutcome::Unusable
    );
    let restored = counting_provider("test-removed", dir.path());
    assert_eq!(
        restored
            .ensure_fresh_token(Some(&token))
            .await
            .rotated()
            .as_deref(),
        Some("tok-2"),
        "the removed provider's token must not survive in the slot"
    );
}

#[tokio::test]
async fn provider_config_edit_invalidates_cached_token() {
    let dir = tempfile::tempdir().unwrap();
    let old = counting_provider("test-freshen", dir.path());
    assert_eq!(
        old.ensure_fresh_token(None).await.rotated().unwrap(),
        "tok-1"
    );

    let edited = AuthProviderRef::new(
        "test-freshen".to_owned(),
        AuthProviderConfig {
            command: "printf edited-token".to_owned(),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: None,
        },
    );
    assert_eq!(
        edited.cached_token(),
        None,
        "the unexpired old token must not be served under the edited table"
    );
    assert_eq!(
        edited
            .ensure_fresh_token(Some("tok-1"))
            .await
            .rotated()
            .as_deref(),
        Some("edited-token"),
        "refresh must run the edited command without waiting for expiry"
    );
}

/// The fresh-mint guard applies per table version.
#[tokio::test]
async fn provider_401_recovery_reminted_under_edited_config() {
    let dir = tempfile::tempdir().unwrap();
    let old = counting_provider("test-401-edited", dir.path());
    let token = old.ensure_fresh_token(None).await.rotated().unwrap();

    let edited = AuthProviderRef::new(
        "test-401-edited".to_owned(),
        AuthProviderConfig {
            command: "printf new-config-token".to_owned(),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: None,
        },
    );
    assert_eq!(
        edited.recover_rejected_token(&token).await.as_deref(),
        Some("new-config-token"),
        "recovery must run the edited command, not adopt the old-table token"
    );
}

/// Editing only `timeout_secs` keeps the token; it is not part of `token_identity`.
#[tokio::test]
async fn provider_timeout_edit_does_not_invalidate_token() {
    let dir = tempfile::tempdir().unwrap();
    let provider = counting_provider("test-timeout-edit", dir.path());
    provider.ensure_fresh_token(None).await.rotated().unwrap();

    let retimed = AuthProviderRef::new(
        "test-timeout-edit".to_owned(),
        AuthProviderConfig {
            command: provider.config.command.clone(),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: Some(30),
            cwd: None,
        },
    );
    assert_eq!(
        retimed.cached_token().as_deref(),
        Some("tok-1"),
        "a timeout-only edit must not invalidate the cached token"
    );
}

/// `cwd` is part of `token_identity`, so editing it invalidates the cache: the same helper in a different directory can mint a different token.
#[tokio::test]
async fn provider_cwd_edit_invalidates_cached_token() {
    let dir = tempfile::tempdir().unwrap();
    let provider = counting_provider("test-cwd-edit", dir.path());
    provider.ensure_fresh_token(None).await.rotated().unwrap();

    let moved = AuthProviderRef::new(
        "test-cwd-edit".to_owned(),
        AuthProviderConfig {
            command: provider.config.command.clone(),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: Some("/some/other/dir".to_owned()),
        },
    );
    assert_eq!(
        moved.cached_token(),
        None,
        "a cwd edit must invalidate the cached token"
    );
}

#[tokio::test]
async fn attach_trusted_config_lets_a_revived_ref_mint() {
    let dir = tempfile::tempdir().unwrap();
    let template = counting_provider("test-attach", dir.path());
    let mut revived: AuthProviderRef = serde_json::from_str(r#"{"name": "test-attach"}"#).unwrap();
    assert_eq!(
        revived.ensure_fresh_token(None).await,
        ProviderRefreshOutcome::Unusable
    );
    revived.attach_trusted_config(Some(&template.config));
    assert_eq!(
        revived.ensure_fresh_token(None).await.rotated().as_deref(),
        Some("tok-1"),
        "a re-attached ref must be able to mint"
    );
}

/// A ref revived from bytes never mutates the shared slot: a mutating call fails closed and leaves a resolved ref's token intact.
#[tokio::test]
async fn deserialized_ref_never_drops_the_shared_token() {
    let dir = tempfile::tempdir().unwrap();
    let resolved = counting_provider("test-unresolved", dir.path());
    resolved.ensure_fresh_token(None).await.rotated().unwrap();

    let revived: AuthProviderRef = serde_json::from_str(r#"{"name": "test-unresolved"}"#).unwrap();
    assert_eq!(
        revived.ensure_fresh_token(None).await,
        ProviderRefreshOutcome::Unusable
    );
    assert_eq!(revived.recover_rejected_token("tok-1").await, None);
    assert_eq!(
        resolved.cached_token().as_deref(),
        Some("tok-1"),
        "the resolved ref's token must survive a mutating call on the stub"
    );
}

/// A ref serializes to its name only: the revived ref carries no command and fails closed until re-attached.
/// The shared slot still serves resolved refs of the same name.
#[tokio::test]
async fn provider_ref_serializes_name_only_and_drops_config() {
    let dir = tempfile::tempdir().unwrap();
    let provider = counting_provider("test-serde", dir.path());
    provider.ensure_fresh_token(None).await.rotated().unwrap();

    let bytes = serde_json::to_string(&provider).unwrap();
    assert!(bytes.contains("test-serde"));
    assert!(
        !bytes.contains("tok-%s") && !bytes.contains("command"),
        "the serialized form must carry the name only: {bytes}"
    );
    let revived: AuthProviderRef = serde_json::from_str(&bytes).unwrap();
    assert_eq!(revived.name, "test-serde");
    assert_eq!(
        revived.config,
        AuthProviderConfig::default(),
        "a serialized command must not survive deserialization"
    );
    assert_eq!(
        revived.cached_token(),
        None,
        "an unresolved ref fails closed"
    );
    let same_name = counting_provider("test-serde", dir.path());
    assert_eq!(
        same_name.cached_token().as_deref(),
        Some("tok-1"),
        "the shared slot still serves refs constructed with the real config"
    );
}

#[tokio::test]
async fn provider_refresh_sets_expired_env() {
    let provider = AuthProviderRef::new(
        "test-expired-env".to_owned(),
        AuthProviderConfig {
            command: "printf 'tok-%s' \"${GROK_AUTH_EXPIRED:-0}\"".to_owned(),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: None,
        },
    );
    assert_eq!(
        provider.ensure_fresh_token(None).await.rotated().as_deref(),
        Some("tok-0"),
        "first mint runs without GROK_AUTH_EXPIRED"
    );
    test_expire_provider_token("test-expired-env");
    assert_eq!(
        provider.ensure_fresh_token(None).await.rotated().as_deref(),
        Some("tok-1"),
        "re-mints run with GROK_AUTH_EXPIRED=1"
    );
}

#[tokio::test]
async fn provider_concurrent_mints_single_flight() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("count");
    let provider = AuthProviderRef::new(
        "test-single-flight".to_owned(),
        AuthProviderConfig {
            command: format!(
                "sleep 0.3; echo run >> {c}; printf 'tok-%s' \"$(wc -l < {c} | tr -d ' ')\"",
                c = counter.display()
            ),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: None,
        },
    );
    let (a, b) = tokio::join!(
        provider.ensure_fresh_token(None),
        provider.ensure_fresh_token(None)
    );
    assert_eq!(a.rotated().as_deref(), Some("tok-1"));
    assert_eq!(
        b.rotated().as_deref(),
        Some("tok-1"),
        "second caller adopts, never re-runs"
    );
    let runs = std::fs::read_to_string(&counter).unwrap().lines().count();
    assert_eq!(runs, 1, "the command must run exactly once");
}

/// The winning expiry source is proven by staleness: an expiry inside the 60s skew re-mints, a distant one serves from cache.
#[tokio::test]
async fn provider_expiry_source_precedence() {
    fn short_jwt() -> String {
        // The exp lands inside the skew window, so the token is immediately stale if the claim is consumed
        jwt_with_exp(chrono::Utc::now().timestamp() + 30)
    }
    fn long_jwt() -> String {
        jwt_with_exp(chrono::Utc::now().timestamp() + 7200)
    }
    fn jwt_with_exp(exp: i64) -> String {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &serde_json::json!({ "exp": exp }),
            &jsonwebtoken::EncodingKey::from_secret(b"test"),
        )
        .unwrap()
    }
    async fn mints_after_first(
        name: &str,
        command: String,
        token_ttl_secs: Option<u64>,
        counter: &std::path::Path,
    ) -> usize {
        let provider = AuthProviderRef::new(
            name.to_owned(),
            AuthProviderConfig {
                command,
                args: None,
                token_ttl_secs,
                timeout_secs: None,
                cwd: None,
            },
        );
        let first = provider
            .ensure_fresh_token(None)
            .await
            .rotated()
            .expect("first mint");
        let _ = provider.ensure_fresh_token(Some(&first)).await;
        std::fs::read_to_string(counter).unwrap().lines().count()
    }

    let dir = tempfile::tempdir().unwrap();

    // expires_in=10 (stale) wins over token_ttl_secs=3600 (fresh), so the second call re-mints
    let c1 = dir.path().join("c1");
    let cmd1 = format!(
        "echo run >> {}; printf '{{\"access_token\":\"t1\",\"expires_in\":10}}'",
        c1.display()
    );
    assert_eq!(
        mints_after_first("test-exp-expires-in", cmd1, Some(3600), &c1).await,
        2,
        "expires_in must win over token_ttl_secs"
    );

    // token_ttl_secs=1 (stale) wins over a 2h JWT exp (fresh), so the second call re-mints
    let c2 = dir.path().join("c2");
    let cmd2 = format!("echo run >> {}; printf '{}'", c2.display(), long_jwt());
    assert_eq!(
        mints_after_first("test-exp-ttl", cmd2, Some(1), &c2).await,
        2,
        "token_ttl_secs must win over the JWT exp claim"
    );

    // With the JWT exp alone, a near-expiry claim (inside the skew) re-mints, proving the claim is consumed when nothing else is configured
    let c3 = dir.path().join("c3");
    let cmd3 = format!("echo run >> {}; printf '{}'", c3.display(), short_jwt());
    assert_eq!(
        mints_after_first("test-exp-jwt", cmd3, None, &c3).await,
        2,
        "the JWT exp claim must apply when expires_in and token_ttl_secs are absent"
    );
}

#[tokio::test]
async fn provider_unusable_expiry_still_mints() {
    let provider = AuthProviderRef::new(
        "test-overflow".to_owned(),
        AuthProviderConfig {
            command: format!(
                "printf '{{\"access_token\":\"t\",\"expires_in\":{}}}'",
                u64::MAX
            ),
            args: None,
            token_ttl_secs: Some(u64::MAX),
            timeout_secs: None,
            cwd: None,
        },
    );
    assert_eq!(
        provider.ensure_fresh_token(None).await.rotated().as_deref(),
        Some("t"),
        "an unusable expiry still mints; the token just has no expiry"
    );
    assert_eq!(
        provider.ensure_fresh_token(Some("t")).await,
        ProviderRefreshOutcome::Unchanged,
        "no expiry source: never proactively re-minted"
    );
}

#[tokio::test]
async fn provider_args_run_without_a_shell() {
    let provider = AuthProviderRef::new(
        "test-args".to_owned(),
        AuthProviderConfig {
            command: "printf".to_owned(),
            // Shell metacharacters stay literal under direct exec.
            args: Some(vec!["tok-$HOME;42".to_owned()]),
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: None,
        },
    );
    assert_eq!(
        provider.ensure_fresh_token(None).await.rotated().as_deref(),
        Some("tok-$HOME;42"),
    );
}

#[tokio::test]
async fn provider_command_times_out() {
    let provider = AuthProviderRef::new(
        "test-timeout".to_owned(),
        AuthProviderConfig {
            command: "sleep 20; printf never".to_owned(),
            args: None,
            token_ttl_secs: None,
            timeout_secs: Some(1),
            cwd: None,
        },
    );
    let start = std::time::Instant::now();
    assert_eq!(
        provider.ensure_fresh_token(None).await,
        ProviderRefreshOutcome::MintFailed
    );
    assert!(
        start.elapsed().as_secs() < 5,
        "1s timeout_secs must bound the mint (took {}s)",
        start.elapsed().as_secs()
    );
}

#[tokio::test]
async fn provider_zero_timeout_clamps_to_one_second() {
    // `timeout_secs = 0` clamps up to the 1s floor, so an instant helper mints rather than failing immediately
    let fast = AuthProviderRef::new(
        "test-zero-timeout-fast".to_owned(),
        AuthProviderConfig {
            command: "printf tok".to_owned(),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: Some(0),
            cwd: None,
        },
    );
    assert_eq!(
        fast.ensure_fresh_token(None).await.rotated().as_deref(),
        Some("tok")
    );

    // ...and clamps down from the 30s default: a helper that runs past 1s times out, proving the effective bound is the clamp, not the default
    let slow = AuthProviderRef::new(
        "test-zero-timeout-slow".to_owned(),
        AuthProviderConfig {
            command: "sleep 5; printf tok".to_owned(),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: Some(0),
            cwd: None,
        },
    );
    assert!(
        matches!(
            slow.ensure_fresh_token(None).await,
            ProviderRefreshOutcome::MintFailed
        ),
        "a >1s helper under timeout_secs=0 must time out at the 1s clamp"
    );
}

/// Each mint-failure mode (timeout, spawn failure, ran but printed no token) produces a distinct, greppable error message so operators can triage.
#[tokio::test]
async fn mint_error_messages_distinguish_failure_modes() {
    let timed_out = AuthProviderRef::new(
        "test-classify-timeout".to_owned(),
        AuthProviderConfig {
            command: "sleep 20".to_owned(),
            args: None,
            token_ttl_secs: None,
            timeout_secs: Some(1),
            cwd: None,
        },
    );
    let err = mint_provider_token(&timed_out, false, None)
        .await
        .err()
        .expect("timeout must fail the mint");
    assert!(err.to_string().contains("timed out"), "got: {err}");

    let missing = AuthProviderRef::new(
        "test-classify-spawn".to_owned(),
        AuthProviderConfig {
            command: "/nonexistent/provider-binary".to_owned(),
            args: Some(vec![]),
            token_ttl_secs: None,
            timeout_secs: Some(30),
            cwd: None,
        },
    );
    let err = mint_provider_token(&missing, false, None)
        .await
        .err()
        .expect("spawn failure must fail the mint");
    assert!(err.to_string().contains("failed to start"), "got: {err}");

    let empty_output = AuthProviderRef::new(
        "test-classify-permanent".to_owned(),
        AuthProviderConfig {
            command: "printf ''".to_owned(),
            args: None,
            token_ttl_secs: None,
            timeout_secs: Some(30),
            cwd: None,
        },
    );
    let err = mint_provider_token(&empty_output, false, None)
        .await
        .err()
        .expect("empty output must fail the mint");
    assert!(err.to_string().contains("no output"), "got: {err}");
}

/// On an in-session re-mint, the prior credential is handed back to the command via `GROK_AUTH_PROVIDER_*`.
/// A command holding a refresh grant can then refresh instead of re-authenticating.
/// Nothing is written to disk.
#[tokio::test]
async fn re_mint_hands_the_prior_token_back_to_the_command() {
    let provider = AuthProviderRef::new(
        "test-handback".to_owned(),
        AuthProviderConfig {
            command: "printf 'seen-%s' \"${GROK_AUTH_PROVIDER_ACCESS_TOKEN:-none}\"".to_owned(),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: None,
        },
    );

    let first = provider.ensure_fresh_token(None).await.rotated().unwrap();
    assert_eq!(first, "seen-none", "the first mint has no prior credential");
    test_expire_provider_token("test-handback");
    assert_eq!(
        provider
            .ensure_fresh_token(Some(&first))
            .await
            .rotated()
            .as_deref(),
        Some("seen-seen-none"),
        "the re-mint must receive the prior access token via env"
    );
}

/// A 401 whose re-mint fails invalidates the rejected token, so it is not re-served next turn (fail closed) even while still locally unexpired.
#[tokio::test]
async fn failed_401_remint_invalidates_the_cached_token() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("count");
    // Mints tok-1 on the first run, then exits non-zero on every later run.
    let provider = AuthProviderRef::new(
        "test-401-invalidate".to_owned(),
        AuthProviderConfig {
            command: format!(
                "echo run >> {c}; n=$(wc -l < {c} | tr -d ' '); \
                 [ \"$n\" = 1 ] && printf 'tok-1' || exit 1",
                c = counter.display()
            ),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: None,
        },
    );

    let token = provider.ensure_fresh_token(None).await.rotated().unwrap();
    assert_eq!(token, "tok-1");
    // Age past the fresh-mint guard so recovery attempts a re-mint.
    test_backdate_provider_mint("test-401-invalidate", PROVIDER_TOKEN_FRESH_MINT_GUARD * 2);

    assert_eq!(
        provider.recover_rejected_token(&token).await,
        None,
        "a failed re-mint surfaces the 401"
    );
    assert_eq!(
        provider.cached_token(),
        None,
        "a rejected token whose re-mint failed must not be re-served"
    );
}

/// A pre-turn re-mint that fails over a now-stale cached token leaves nothing servable: the stale token is never handed to the wire.
/// This mirrors the 401 recovery path.
#[tokio::test]
async fn failed_pre_turn_mint_does_not_serve_the_stale_token() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("count");
    let provider = AuthProviderRef::new(
        "test-pre-turn-stale".to_owned(),
        AuthProviderConfig {
            command: format!(
                "echo run >> {c}; n=$(wc -l < {c} | tr -d ' '); \
                 [ \"$n\" = 1 ] && printf 'tok-1' || exit 1",
                c = counter.display()
            ),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: None,
        },
    );

    let token = provider.ensure_fresh_token(None).await.rotated().unwrap();
    assert_eq!(token, "tok-1");
    // Make the cached token stale so the next pre-turn call re-mints (and fails).
    test_expire_provider_token("test-pre-turn-stale");

    assert!(matches!(
        provider.ensure_fresh_token(Some(token.as_str())).await,
        ProviderRefreshOutcome::MintFailed
    ));
    assert_eq!(
        provider.cached_token(),
        None,
        "a stale token whose pre-turn re-mint failed must not be served"
    );
}

/// A helper that writes past the stdout cap fails closed (permanent), so a runaway command can't exhaust memory or put a huge token on the wire.
#[tokio::test]
async fn provider_output_over_cap_fails_closed() {
    let over = PROVIDER_STDOUT_CAP_BYTES + 4096;
    let provider = AuthProviderRef::new(
        "test-stdout-cap".to_owned(),
        AuthProviderConfig {
            command: format!("head -c {over} /dev/zero"),
            args: None,
            token_ttl_secs: None,
            timeout_secs: Some(30),
            cwd: None,
        },
    );
    let err = mint_provider_token(&provider, false, None)
        .await
        .err()
        .expect("over-cap output must fail the mint");
    assert!(
        err.to_string().contains("more than"),
        "an over-cap write must be reported as such, got: {err}"
    );
    assert_eq!(
        provider.ensure_fresh_token(None).await,
        ProviderRefreshOutcome::MintFailed
    );
}

/// A BYOK helper must never inherit a first-party credential. `EXPECTED` is an
/// independently audited copy of the scrub list, so the assert is not tautological.
#[tokio::test]
async fn provider_helper_env_scrubs_first_party_credentials() {
    const EXPECTED: &[&str] = &[
        "GROK_AUTH",
        "GROK_AUTH_PATH",
        "XAI_API_KEY",
        "GROK_DEPLOYMENT_KEY",
        "GROK_CODE_XAI_API_KEY",
        "GROK_EXTRA_AUTH_KEY",
        "GROK_TRACE_UPLOAD_CREDENTIALS_FILE",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "GROK_INTERNAL_OTLP_HEADERS",
    ];
    assert_eq!(
        xai_grok_env::FIRST_PARTY_CREDENTIAL_ENV_VARS,
        EXPECTED,
        "the scrub list changed: re-audit that every entry is a first-party \
         credential a BYOK helper must not inherit, then update EXPECTED"
    );

    let echo = EXPECTED
        .iter()
        .map(|v| format!("${{{v}-}}"))
        .collect::<Vec<_>>()
        .join("");
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", &format!("printf 'tok[%s]' \"{echo}\"")]);
    for var in EXPECTED {
        cmd.env(var, "first-party-leak");
    }
    super::scrub_first_party_credentials(&mut cmd);

    let output = cmd.output().await.expect("helper spawns");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "tok[]",
        "no first-party credential may survive into the helper env"
    );
}

/// `resolve_program` branches: a bare name resolves via `PATH`, an absolute path is used as-is, a relative one resolves against `cwd`.
#[test]
fn resolve_program_resolves_against_cwd() {
    let cwd = std::path::Path::new("/work");
    assert_eq!(
        super::resolve_program("token-helper", Some(cwd)),
        std::path::PathBuf::from("token-helper")
    );
    let abs = if cfg!(windows) {
        r"C:\bin\helper.exe"
    } else {
        "/usr/local/bin/helper"
    };
    assert_eq!(
        super::resolve_program(abs, Some(cwd)),
        std::path::PathBuf::from(abs)
    );
    assert_eq!(
        super::resolve_program("bin/helper", Some(cwd)),
        cwd.join("bin/helper")
    );
    assert_eq!(
        super::resolve_program("bin/helper", None),
        std::path::PathBuf::from("bin/helper"),
        "with no cwd a relative path is left to the process cwd"
    );
}

/// gx: a mise upgrade deletes the versioned directory baked into
/// `auth.command`. Spawn must fall back rather than 401 ChatGPT turns.
#[test]
fn resolve_auth_program_falls_back_when_baked_gx_is_gone() {
    let args: Vec<String> = xai_grok_config::GX_TOKEN_HELPER_ARGS
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let missing = "/no/such/gx-install/gx";
    assert_eq!(
        super::resolve_auth_program(missing, Some(&args), None),
        std::path::PathBuf::from(xai_grok_config::gx_helper_replacement()),
    );

    let dir = tempfile::tempdir().unwrap();
    let present = dir.path().join("gx");
    std::fs::write(&present, "#!/bin/sh\n").unwrap();
    assert_eq!(
        super::resolve_auth_program(&present.to_string_lossy(), Some(&args), None),
        present,
        "an existing helper must still be used"
    );
    assert_eq!(
        super::resolve_auth_program(missing, None, None),
        std::path::PathBuf::from(missing),
        "shell form (no args) is not the shipped helper"
    );
}

/// gx: cwd-joining a relative `bin/gx` must not look like a baked absolute
/// helper. The predicate runs on the raw command first.
#[test]
fn resolve_auth_program_does_not_rewrite_relative_bin_gx() {
    let args: Vec<String> = xai_grok_config::GX_TOKEN_HELPER_ARGS
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let cwd = std::path::Path::new("/opt/tools");
    assert_eq!(
        super::resolve_auth_program("bin/gx", Some(&args), Some(cwd)),
        cwd.join("bin/gx"),
        "a relative helper is the user's, even if cwd-join would be a missing /opt/tools/bin/gx"
    );
}

/// The `args` form (the portable, no-shell shape a desktop/Windows helper
/// should use) resolves a relative program against the provider's `cwd`.
#[cfg(unix)]
#[tokio::test]
async fn provider_resolves_relative_program_against_cwd() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("token.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf 'cwd-tok'\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let provider = AuthProviderRef::new(
        "test-cwd-relative".to_owned(),
        AuthProviderConfig {
            command: "./token.sh".to_owned(),
            args: Some(vec![]),
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: Some(dir.path().to_string_lossy().into_owned()),
        },
    );
    assert_eq!(
        provider.ensure_fresh_token(None).await.rotated().as_deref(),
        Some("cwd-tok")
    );
}

/// `cwd` is the command's runtime directory: reading a file by relative name only succeeds if `current_dir` took effect (here via the shell form).
#[cfg(unix)]
#[tokio::test]
async fn provider_command_runs_in_cwd() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token.txt"), "file-tok").unwrap();

    let provider = AuthProviderRef::new(
        "test-cwd-shell".to_owned(),
        AuthProviderConfig {
            command: "cat token.txt".to_owned(),
            args: None,
            token_ttl_secs: Some(3600),
            timeout_secs: None,
            cwd: Some(dir.path().to_string_lossy().into_owned()),
        },
    );
    assert_eq!(
        provider.ensure_fresh_token(None).await.rotated().as_deref(),
        Some("file-tok")
    );
}

// ---------------------------------------------------------------------------
// gx: shipped openai-codex helper is minted in-process, never PATH-exec'd
// ---------------------------------------------------------------------------

fn helper_args() -> Vec<String> {
    xai_grok_config::GX_TOKEN_HELPER_ARGS
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

fn wall_clock_fresh_jwt() -> String {
    wall_clock_jwt(3_600, None)
}

fn wall_clock_jwt(ttl_secs: i64, nonce: Option<&str>) -> String {
    use base64::Engine as _;
    let exp = chrono::Utc::now().timestamp() + ttl_secs;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let mut claims = serde_json::json!({ "exp": exp });
    if let Some(nonce) = nonce {
        claims["nonce"] = serde_json::Value::String(nonce.to_owned());
    }
    let payload = engine.encode(serde_json::to_vec(&claims).expect("payload"));
    format!("{header}.{payload}.c2ln")
}

fn fixture_auth_json(access_token: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "tokens": {
            "access_token": access_token,
            "refresh_token": "rt-fixture",
        }
    }))
    .expect("fixture")
}

struct CodexStore {
    _codex: tempfile::TempDir,
    _grok: tempfile::TempDir,
    paths: crate::gx_openai_codex::CodexPaths,
}

impl CodexStore {
    fn with_body(body: &str) -> Self {
        let codex = tempfile::tempdir().expect("codex home");
        let grok = tempfile::tempdir().expect("grok home");
        let auth_json = codex.path().join("auth.json");
        std::fs::write(&auth_json, body).expect("write fixture");
        let paths = crate::gx_openai_codex::CodexPaths::for_auth_json(auth_json, grok.path());
        Self {
            _codex: codex,
            _grok: grok,
            paths,
        }
    }

    fn fresh() -> (Self, String) {
        let token = wall_clock_fresh_jwt();
        (Self::with_body(&fixture_auth_json(&token)), token)
    }

    fn missing() -> Self {
        let codex = tempfile::tempdir().expect("codex home");
        let grok = tempfile::tempdir().expect("grok home");
        let paths = crate::gx_openai_codex::CodexPaths::for_auth_json(
            codex.path().join("auth.json"),
            grok.path(),
        );
        Self {
            _codex: codex,
            _grok: grok,
            paths,
        }
    }
}

fn shipped_provider(name: &str, command: &str, cwd: Option<String>) -> AuthProviderRef {
    AuthProviderRef::new(
        name.to_owned(),
        AuthProviderConfig {
            command: command.to_owned(),
            args: Some(helper_args()),
            token_ttl_secs: None,
            timeout_secs: Some(30),
            cwd,
        },
    )
}

#[cfg(unix)]
fn evil_helper(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let marker = dir.join(format!("{name}.spawned"));
    let script = dir.join(name);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf spawned > '{}'\nprintf 'EVIL\\n'\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    (script, marker)
}

#[cfg(unix)]
async fn mint_at(
    provider: &AuthProviderRef,
    is_gx: bool,
    mark_expired: bool,
    store: &CodexStore,
    token_endpoint_url: Option<String>,
) -> anyhow::Result<MintedProviderToken> {
    super::mint_provider_token_for_at(
        provider,
        mark_expired,
        None,
        is_gx,
        store.paths.clone(),
        token_endpoint_url,
    )
    .await
}

/// gx: a shipped helper (absolute path named gx + helper args, empty cwd)
/// mints from this process. An evil file at that path must not run.
#[cfg(unix)]
#[tokio::test]
async fn shipped_gx_helper_intercepts_even_when_the_baked_path_exists() {
    let dir = tempfile::tempdir().unwrap();
    let (evil, marker) = evil_helper(dir.path(), "gx");
    let (store, fixture) = CodexStore::fresh();
    let provider = shipped_provider(
        "shipped-gx-helper-intercepts",
        &evil.to_string_lossy(),
        None,
    );

    let minted = mint_at(&provider, true, false, &store, None)
        .await
        .expect("in-process mint");

    assert_eq!(minted.token, fixture);
    assert!(!marker.exists(), "evil helper must not have been spawned");
}

/// gx: stock grok (`is_gx=false`) still spawns the baked helper.
#[cfg(unix)]
#[tokio::test]
async fn shipped_gx_helper_spawns_when_is_gx_is_false() {
    let dir = tempfile::tempdir().unwrap();
    let (evil, marker) = evil_helper(dir.path(), "gx");
    let (store, _) = CodexStore::fresh();
    let provider = shipped_provider(
        "shipped-gx-helper-stock-spawns",
        &evil.to_string_lossy(),
        None,
    );
    assert_eq!(
        super::resolve_auth_program(&evil.to_string_lossy(), Some(&helper_args()), None),
        evil,
        "stock spawn must exec the fixture script, not PATH gx"
    );
    let direct = std::process::Command::new(&evil)
        .args(helper_args())
        .output()
        .expect("direct fixture exec");
    assert!(
        direct.status.success(),
        "fixture script must exit 0 when run directly; stderr={}",
        String::from_utf8_lossy(&direct.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&direct.stdout).trim(), "EVIL");

    let minted = mint_at(&provider, false, false, &store, None)
        .await
        .expect("spawned mint");

    assert_eq!(minted.token, "EVIL");
    assert!(marker.exists(), "stock build must still spawn the helper");
}

/// gx: a wrapper, different args, or a non-empty cwd is the user's — still spawn.
#[cfg(unix)]
#[tokio::test]
async fn shipped_gx_helper_spawns_wrapper_wrong_args_and_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let (wrapper, wrapper_marker) = evil_helper(dir.path(), "wrapper");
    let (gx, gx_marker) = evil_helper(dir.path(), "gx");
    let (store, _) = CodexStore::fresh();

    let wrapper_provider = shipped_provider(
        "shipped-gx-helper-wrapper-spawns",
        &wrapper.to_string_lossy(),
        None,
    );
    let minted = mint_at(&wrapper_provider, true, false, &store, None)
        .await
        .expect("wrapper spawn");
    assert_eq!(minted.token, "EVIL");
    assert!(wrapper_marker.exists());

    let mut wrong_args = helper_args();
    wrong_args.push("--json".into());
    let wrong_args_provider = AuthProviderRef::new(
        "shipped-gx-helper-wrong-args-spawns".to_owned(),
        AuthProviderConfig {
            command: gx.to_string_lossy().into_owned(),
            args: Some(wrong_args),
            token_ttl_secs: None,
            timeout_secs: Some(30),
            cwd: None,
        },
    );
    let minted = mint_at(&wrong_args_provider, true, false, &store, None)
        .await
        .expect("wrong-args spawn");
    assert_eq!(minted.token, "EVIL");
    assert!(gx_marker.exists());
    std::fs::remove_file(&gx_marker).ok();

    let cwd_provider = shipped_provider(
        "shipped-gx-helper-cwd-spawns",
        &gx.to_string_lossy(),
        Some(dir.path().to_string_lossy().into_owned()),
    );
    let minted = mint_at(&cwd_provider, true, false, &store, None)
        .await
        .expect("cwd spawn");
    assert_eq!(minted.token, "EVIL");
    assert!(gx_marker.exists());
    std::fs::remove_file(&gx_marker).ok();

    // A set cwd, even whitespace-only, is a hand edit — spawn, do not intercept.
    let blank_cwd = shipped_provider(
        "shipped-gx-helper-blank-cwd-spawns",
        &gx.to_string_lossy(),
        Some("   ".to_owned()),
    );
    let minted = mint_at(&blank_cwd, true, false, &store, None)
        .await
        .expect("blank-cwd spawn");
    assert_eq!(minted.token, "EVIL");
    assert!(gx_marker.exists());
}

struct PathGuard {
    prev: Option<std::ffi::OsString>,
}

impl PathGuard {
    fn prepend(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("PATH");
        let mut new_path = dir.as_os_str().to_os_string();
        if let Some(ref p) = prev {
            new_path.push(":");
            new_path.push(p);
        }
        unsafe { std::env::set_var("PATH", &new_path) };
        Self { prev }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(p) => unsafe { std::env::set_var("PATH", p) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

/// gx: sentinel `command = "gx"` means this process, never `which gx`.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(codex_env)]
async fn shipped_gx_helper_sentinel_does_not_path_exec() {
    let dir = tempfile::tempdir().unwrap();
    let (_evil, marker) = evil_helper(dir.path(), "gx");
    let _path = PathGuard::prepend(dir.path());
    let (store, fixture) = CodexStore::fresh();
    let provider = shipped_provider("shipped-gx-helper-path-poison", "gx", None);

    let minted = mint_at(&provider, true, false, &store, None)
        .await
        .expect("in-process mint");

    assert_eq!(minted.token, fixture);
    assert!(!marker.exists(), "PATH gx must not have been spawned");
}

/// gx: a mint error must not fall back to spawning the helper.
#[cfg(unix)]
#[tokio::test]
async fn shipped_gx_helper_mint_error_does_not_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let (evil, marker) = evil_helper(dir.path(), "gx");
    let store = CodexStore::missing();
    let provider = shipped_provider(
        "shipped-gx-helper-mint-err-no-fallback",
        &evil.to_string_lossy(),
        None,
    );

    let err = match mint_at(&provider, true, false, &store, None).await {
        Ok(_) => panic!("missing auth.json must fail closed"),
        Err(err) => err,
    };

    let msg = format!("{err:#}");
    assert!(
        msg.contains("no codex credentials") || msg.contains("auth.json"),
        "{msg}"
    );
    assert!(!marker.exists(), "failed in-process mint must not spawn");
}

/// gx: a control-character access token fails closed and does not spawn.
#[cfg(unix)]
#[tokio::test]
async fn shipped_gx_helper_rejects_control_char_token_without_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let (evil, marker) = evil_helper(dir.path(), "gx");
    let last_refresh = chrono::Utc::now().to_rfc3339();
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "tokens": {
            "access_token": "tok\ninjected",
            "refresh_token": "rt-fixture",
        },
        "last_refresh": last_refresh,
    }))
    .unwrap();
    let store = CodexStore::with_body(&body);
    let provider = shipped_provider(
        "shipped-gx-helper-control-char",
        &evil.to_string_lossy(),
        None,
    );

    let err = match mint_at(&provider, true, false, &store, None).await {
        Ok(_) => panic!("control-char token must fail closed"),
        Err(err) => err,
    };

    let msg = format!("{err:#}");
    assert!(msg.contains("control characters"), "{msg}");
    assert!(!marker.exists(), "failed validation must not spawn");
}

fn spawn_token_server(status_line: &'static str, body: String) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{BufRead as _, Read as _, Write as _};
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

/// gx: `mark_expired=true` reaches `force_refresh` (injected endpoint; no live OpenAI).
#[cfg(unix)]
#[tokio::test]
async fn shipped_gx_helper_mark_expired_force_refreshes() {
    let dir = tempfile::tempdir().unwrap();
    let (evil, marker) = evil_helper(dir.path(), "gx");
    let (store, fixture) = CodexStore::fresh();
    let rotated = wall_clock_jwt(7_200, Some("rotated"));
    let body = serde_json::json!({
        "access_token": rotated,
        "refresh_token": "rt-rotated",
        "expires_in": 3600,
    })
    .to_string();
    let (url, server) = spawn_token_server("HTTP/1.1 200 OK", body);
    let provider = shipped_provider(
        "shipped-gx-helper-force-refresh",
        &evil.to_string_lossy(),
        None,
    );

    let minted = mint_at(&provider, true, true, &store, Some(url))
        .await
        .expect("forced refresh");

    let sent = server.join().expect("server accepted a POST");
    assert!(sent.contains("refresh_token"), "{sent}");
    assert_eq!(minted.token, rotated);
    assert_ne!(minted.token, fixture);
    assert!(!marker.exists(), "force-refresh must stay in-process");
}
