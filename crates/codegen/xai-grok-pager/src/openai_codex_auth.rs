//! gx: OpenAI (ChatGPT / Codex-plan) credentials, read from the codex CLI's
//! `auth.json` and minted for grok's auth-provider helper seam.
//!
//! `gx providers token openai` is the helper command the `openai-codex` preset
//! invokes before every turn (and again after a 401). It prints exactly
//! `{"access_token": "...", "expires_in": <secs>}` on stdout and nothing else;
//! everything diagnostic goes to stderr.
//!
//! # What this module owns
//!
//! - Reading `~/.codex/auth.json` **whole**: typed known fields plus a
//!   `serde_json::Map` passthrough for everything else, so a rewrite never
//!   drops `auth_mode`, `agent_identity`, `bedrock_*`, or any field a newer
//!   codex adds ([`AuthDocument`]).
//! - Deciding staleness: the access token's unverified JWT `exp` minus
//!   [`EXPIRY_SKEW_SECS`], with `last_refresh + 7d` as the fallback when the
//!   JWT does not parse ([`freshness`]).
//! - Refreshing, exactly once, under a gx-owned cross-process lock, and
//!   persisting the **rotated** refresh token atomically ([`mint_token`]).
//! - Noticing that the signed-in account changed ([`account_check`]).
//!
//! # Rotation safety, and the race gx cannot close
//!
//! `POST https://auth.openai.com/oauth/token` **rotates** the refresh token:
//! the old one dies the moment the new one is issued. Losing the new one — by
//! crashing before the write, or by having a concurrent writer clobber it —
//! logs the user out of codex, not just out of gx. So every refresh here:
//!
//! 1. takes an exclusive `flock` on [`lock_path`] (`.gx-auth.lock`, mode 0600,
//!    **beside** `auth.json`, never inside it),
//! 2. **re-reads** `auth.json` under that lock — another gx process may have
//!    refreshed while this one waited — and returns early if it is now fresh,
//!    so a queue of blocked processes performs exactly one rotation between
//!    them,
//! 3. POSTs, then persists the full document (rotated `refresh_token`, new
//!    `access_token`/`id_token`, `last_refresh = now`) with tmp+rename in the
//!    same directory,
//! 4. releases.
//!
//! **Residual race:** codex-rs takes no lock on this file and truncate-rewrites
//! it, so it cannot be made to respect ours. The window is small by
//! construction: gx refreshes only when the token is *actually* expired (no
//! eager rotation), which is a few seconds once every ~12 hours per machine,
//! and codex only refreshes on the same condition.
//!
//! # The rotation-recovery journal
//!
//! Between "the server has issued R2 and killed R1" and "R2 is on disk" there
//! is a window in which the only copy of a live refresh token is in this
//! process's memory. A crash there — or a codex-rs truncate-rewrite landing on
//! top of the write ([`write_auth_document`] renames, but codex's rewrite can
//! land after it) — used to mean a lockout: the store holds R1, R1 is dead, and
//! `codex login` is the only way out.
//!
//! So the moment a 2xx parses, and **before** anything is merged into
//! `auth.json`, the raw token response is written to
//! [`journal_path`] (`.gx-auth-recovery.json`, mode 0600, beside the store),
//! and it is deleted only once the `auth.json` rename has succeeded. A later
//! mint that hits `invalid_grant`, after its one reload has failed, tries the
//! journalled refresh token exactly once before telling the user to run
//! `codex login`. Both the crash and the documented clobber race are therefore
//! recoverable rather than a lockout.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::providers_cmd::decode_jwt_claims_unverified;

// ---------------------------------------------------------------------------
// constants
// ---------------------------------------------------------------------------

/// Refresh once the access token is within this much of its `exp`. Matches
/// codex-rs's own 5-minute window, so gx and codex agree on when a token is
/// "about to expire" and neither rotates a token the other still considers
/// fresh.
pub(crate) const EXPIRY_SKEW_SECS: i64 = 300;

/// Fallback lifetime when the access token is not a parsable JWT: refresh when
/// `last_refresh` is older than this. Only a backstop — the JWT path is what
/// normally decides.
pub(crate) const LAST_REFRESH_FALLBACK_SECS: i64 = 7 * 24 * 60 * 60;

/// The codex CLI's OAuth client id. gx mints against the same client because it
/// is refreshing codex's own credential, not obtaining one of its own.
pub(crate) const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Production token endpoint. Injectable so no test can ever reach it.
pub(crate) const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";

/// Ceiling on one refresh round trip.
pub(crate) const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for another gx process's refresh before giving up. Longer
/// than the providers.toml lock: the holder may be doing network round trips.
///
/// The worst legitimate holder does two [`REFRESH_TIMEOUT`] round trips (the
/// POST plus the single `invalid_grant` retry) around a few file writes, so a
/// wait shorter than ~60s can time out on a holder that is making progress.
pub(crate) const LOCK_TIMEOUT: Duration = Duration::from_secs(90);

/// `auth.json` is a small credential document; anything larger is not one.
const MAX_AUTH_JSON_BYTES: u64 = 1024 * 1024;

/// Owner-only: this file holds a refresh token.
const AUTH_MODE: u32 = 0o600;

/// Emitted when neither the JWT nor the refresh response says otherwise.
const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

/// Every path the credential flow touches. Injectable as one unit so a test
/// can never fall back to a real one by forgetting a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPaths {
    /// codex's credential store.
    pub auth_json: PathBuf,
    /// gx-owned lockfile beside it.
    pub lock: PathBuf,
    /// gx-owned rotation journal beside it: the raw token response, held from
    /// the moment the server answers until the store has been rewritten. See
    /// the module docs.
    pub journal: PathBuf,
    /// gx-owned state (last seen account id). Lives in `$GROK_HOME`, not in
    /// `~/.codex`: it is gx's memory, not codex's data.
    pub state: PathBuf,
}

impl CodexPaths {
    /// `$CODEX_HOME/auth.json` (else `~/.codex/auth.json`) plus the sibling
    /// lockfile and the gx state file under `$GROK_HOME`.
    pub(crate) fn from_env() -> Option<Self> {
        let auth_json = codex_auth_json_path()?;
        Some(Self::for_auth_json(
            auth_json,
            &xai_grok_config::grok_home(),
        ))
    }

    /// Derive the lock, journal and state paths from an `auth.json` path and a
    /// gx home.
    pub(crate) fn for_auth_json(auth_json: PathBuf, grok_home: &Path) -> Self {
        let lock = lock_path(&auth_json);
        let journal = journal_path(&auth_json);
        Self {
            auth_json,
            lock,
            journal,
            state: grok_home.join("openai-codex-state.json"),
        }
    }

    /// The same paths, rooted at `auth.json`'s **canonical** path.
    ///
    /// `~/.codex/auth.json` is routinely a symlink (a dotfile repo, a shared
    /// volume, `$CODEX_HOME` itself pointing through one). Every write here is
    /// a tmp+rename over the path it is given, and renaming over a symlink
    /// *replaces the link with a regular file* — the target would silently stop
    /// receiving updates, and anything else reading through it would keep
    /// serving the old, now-dead refresh token. Resolving up front makes the
    /// read, the lock, the temp+rename and the journal all address the real
    /// file, which also means two gx processes reaching the same store by
    /// different paths take the *same* lock.
    ///
    /// A path that exists but cannot be canonicalized is an error, never a
    /// guess: gx does not write credentials to a path it could not resolve.
    fn canonicalized(&self) -> Result<Self> {
        if !self.auth_json.exists() {
            return Ok(self.clone());
        }
        let auth_json = std::fs::canonicalize(&self.auth_json).with_context(|| {
            format!(
                "failed to resolve {} to a real path",
                self.auth_json.display()
            )
        })?;
        Ok(Self {
            lock: lock_path(&auth_json),
            journal: journal_path(&auth_json),
            auth_json,
            state: self.state.clone(),
        })
    }
}

/// `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`.
pub(crate) fn codex_auth_json_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home).join("auth.json"));
    }
    // gx: home-anchored `~/.codex` must go through `xai_dirs::home_dir` (upstream
    // renamed `xai-grok-home` and banned `dirs::home_dir` in clippy.toml).
    xai_dirs::home_dir().map(|h| h.join(".codex").join("auth.json"))
}

/// `.gx-auth.lock` beside `auth.json`. A separate file on purpose: a lock byte
/// inside `auth.json` would be a write to codex's document.
pub(crate) fn lock_path(auth_json: &Path) -> PathBuf {
    auth_json.with_file_name(".gx-auth.lock")
}

/// `.gx-auth-recovery.json` beside `auth.json`: the rotation journal. Beside
/// rather than inside for the same reason as the lock — `auth.json` is codex's
/// document, and this one holds a token codex has never seen.
pub(crate) fn journal_path(auth_json: &Path) -> PathBuf {
    auth_json.with_file_name(".gx-auth-recovery.json")
}

// ---------------------------------------------------------------------------
// the document
// ---------------------------------------------------------------------------

/// `auth.json`, in full.
///
/// Known fields are typed; **everything else round-trips** through `extra`.
/// codex writes fields gx has never heard of (and will add more), and gx
/// rewrites this file on refresh, so anything not modelled here must survive
/// verbatim.
/// Every modelled field is an `Option<serde_json::Value>` read through
/// [`present`], not an `Option<String>`. The real store carries
/// `"OPENAI_API_KEY": null`, and serde's own `Option` impl folds an explicit
/// `null` into `None` — indistinguishable from an absent key, so a rewrite
/// would silently delete it. `present` keeps the distinction: absent stays
/// `None`, `null` becomes `Some(Value::Null)`, and both round-trip.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct AuthDocument {
    #[serde(
        rename = "OPENAI_API_KEY",
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub openai_api_key: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<AuthTokens>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_refresh: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The `tokens` object. Same passthrough discipline as [`AuthDocument`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct AuthTokens {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub id_token: Option<serde_json::Value>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub access_token: Option<serde_json::Value>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub refresh_token: Option<serde_json::Value>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Deserialize a key that **is** in the document, whatever its value, as
/// `Some`. Paired with `#[serde(default)]`, which supplies `None` for a key
/// that is absent — the distinction serde's stock `Option` impl throws away.
fn present<'de, D>(deserializer: D) -> std::result::Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// A JSON value read as a non-empty string, or `None`.
fn text(value: Option<&serde_json::Value>) -> Option<&str> {
    value
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

impl AuthDocument {
    fn access_token(&self) -> Option<&str> {
        text(self.tokens.as_ref()?.access_token.as_ref())
    }

    fn refresh_token(&self) -> Option<&str> {
        text(self.tokens.as_ref()?.refresh_token.as_ref())
    }

    /// Whether the store carries a usable plain API key — the `openai-api`
    /// provider's credential, not this one's.
    pub(crate) fn has_api_key(&self) -> bool {
        text(self.openai_api_key.as_ref()).is_some()
    }

    pub(crate) fn has_refresh_token(&self) -> bool {
        self.refresh_token().is_some()
    }

    pub(crate) fn has_tokens(&self) -> bool {
        self.tokens.is_some()
    }

    /// `tokens.account_id`, else the JWT's `chatgpt_account_id` claim.
    pub(crate) fn account_id(&self) -> Option<String> {
        if let Some(id) = self
            .tokens
            .as_ref()
            .and_then(|t| text(t.account_id.as_ref()))
        {
            return Some(id.to_owned());
        }
        self.auth_claims()?
            .get("chatgpt_account_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    /// `chatgpt_plan_type` from the access token, when it carries one.
    pub(crate) fn plan(&self) -> Option<String> {
        self.auth_claims()?
            .get("chatgpt_plan_type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    fn auth_claims(&self) -> Option<serde_json::Value> {
        let claims = decode_jwt_claims_unverified(self.access_token()?)?;
        claims.get("https://api.openai.com/auth").cloned()
    }

    /// `exp` from the access token's unverified JWT payload.
    pub(crate) fn access_token_exp(&self) -> Option<i64> {
        let claims = decode_jwt_claims_unverified(self.access_token()?)?;
        claims.get("exp").and_then(serde_json::Value::as_i64)
    }

    pub(crate) fn last_refresh_unix(&self) -> Option<i64> {
        chrono::DateTime::parse_from_rfc3339(text(self.last_refresh.as_ref())?)
            .ok()
            .map(|t| t.timestamp())
    }

    /// True when the file carries an API key and no OAuth tokens at all. That
    /// is the `openai-api` provider's credential, not this one's.
    fn is_api_key_only(&self) -> bool {
        self.access_token().is_none() && self.refresh_token().is_none() && self.has_api_key()
    }
}

// ---------------------------------------------------------------------------
// clock
// ---------------------------------------------------------------------------

/// Injectable clock: staleness is a time comparison, and a test must be able to
/// stand on either side of it without sleeping or touching a real token.
pub(crate) trait Clock: Send + Sync {
    fn now_unix(&self) -> i64;
}

/// Wall clock.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

/// A clock frozen at a chosen instant.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// staleness
// ---------------------------------------------------------------------------

/// Why the credential is considered fresh or stale — reported as-is by
/// `status`, so the user can see which rule fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Freshness {
    /// The JWT parsed and `exp - EXPIRY_SKEW_SECS` is still in the future.
    FreshByJwt { expires_in: i64 },
    /// No parsable JWT, but `last_refresh + 7d` is still in the future.
    FreshByLastRefresh { expires_in: i64 },
    /// `exp - EXPIRY_SKEW_SECS` has passed.
    StaleByJwt,
    /// No parsable JWT and `last_refresh + 7d` has passed.
    StaleByLastRefresh,
    /// Neither an `exp` claim nor a parsable `last_refresh`: nothing says the
    /// token is good, so treat it as stale rather than hand out a token that
    /// may already be dead.
    StaleUnknown,
}

impl Freshness {
    pub(crate) fn is_fresh(self) -> bool {
        matches!(
            self,
            Freshness::FreshByJwt { .. } | Freshness::FreshByLastRefresh { .. }
        )
    }

    /// Seconds of remaining life to report on the seam, clamped at zero.
    fn expires_in(self) -> Option<i64> {
        match self {
            Freshness::FreshByJwt { expires_in } | Freshness::FreshByLastRefresh { expires_in } => {
                Some(expires_in.max(0))
            }
            _ => None,
        }
    }
}

/// The refresh rule, in one place: JWT `exp` when it parses, `last_refresh +
/// 7d` when it does not, stale when neither is available.
///
/// All arithmetic saturates: `exp` is attacker-influenced JSON and can be
/// `i64::MIN`/`i64::MAX`, where a plain `exp - now` panics in a debug build.
pub(crate) fn freshness(doc: &AuthDocument, now: i64) -> Freshness {
    freshness_from(doc.access_token_exp(), doc.last_refresh_unix(), now)
}

/// [`freshness`] over the two facts it actually needs, so `gx providers status`
/// can render the same verdict from a struct that holds no token material.
pub(crate) fn freshness_from(
    access_token_exp: Option<i64>,
    last_refresh_unix: Option<i64>,
    now: i64,
) -> Freshness {
    if let Some(exp) = access_token_exp {
        let deadline = exp.saturating_sub(EXPIRY_SKEW_SECS);
        return if now < deadline {
            Freshness::FreshByJwt {
                expires_in: exp.saturating_sub(now).max(0),
            }
        } else {
            Freshness::StaleByJwt
        };
    }
    match last_refresh_unix {
        Some(last) => {
            let deadline = last.saturating_add(LAST_REFRESH_FALLBACK_SECS);
            if now < deadline {
                Freshness::FreshByLastRefresh {
                    expires_in: deadline.saturating_sub(now).max(0),
                }
            } else {
                Freshness::StaleByLastRefresh
            }
        }
        None => Freshness::StaleUnknown,
    }
}

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

/// Read and parse `auth.json`, refusing anything that is not a bounded regular
/// file (a fifo would block forever; a device node would never end).
pub(crate) fn read_auth_document(path: &Path) -> Result<AuthDocument> {
    if !crate::providers_cmd::gate_file(path, MAX_AUTH_JSON_BYTES, "auth.json")? {
        bail!(
            "no codex credentials at {} — run `codex login` (or `gx providers login openai`) first",
            path.display()
        );
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    // Deliberately not `.context(e)`: serde's message quotes the offending
    // source, which here is a file full of tokens.
    serde_json::from_str::<AuthDocument>(&raw)
        .map_err(|e| anyhow::anyhow!("{} is not valid JSON (line {})", path.display(), e.line()))
}

/// [`read_auth_document`] plus the "this is the other provider's credential"
/// check, which every caller that wants a *bearer* must make.
fn read_oauth_document(path: &Path) -> Result<AuthDocument> {
    let doc = read_auth_document(path)?;
    if doc.is_api_key_only() {
        bail!(
            "{} holds an OPENAI_API_KEY and no ChatGPT OAuth tokens. That is the \
             `openai-api` provider's credential, not `openai-codex`: use the \
             `openai-api` models (they read $OPENAI_API_KEY), or run `codex login` \
             to sign in with a ChatGPT plan.",
            path.display()
        );
    }
    if doc.tokens.is_none() {
        bail!(
            "{} has no `tokens` object — run `codex login` to sign in",
            path.display()
        );
    }
    Ok(doc)
}

// ---------------------------------------------------------------------------
// the token endpoint
// ---------------------------------------------------------------------------

/// What the refresh endpoint answered.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct RefreshResponse {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<i64>,
}

/// Refresh failures gx distinguishes. `InvalidGrant` is the only one worth a
/// reload-and-retry; everything else is transport or server trouble.
#[derive(Debug)]
pub(crate) enum RefreshError {
    /// The refresh token is dead — rotated out from under us, or revoked.
    InvalidGrant(String),
    /// Any other non-2xx, or a body that did not parse.
    Other(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::InvalidGrant(m) | RefreshError::Other(m) => f.write_str(m),
        }
    }
}

/// The refresh round trip, behind a trait so every test drives a local socket
/// (or no socket at all) instead of `auth.openai.com`.
pub(crate) trait TokenEndpoint {
    fn refresh(&self, refresh_token: &str) -> std::result::Result<RefreshResponse, RefreshError>;
}

/// The real endpoint. `url` is a field, not a constant, so an integration test
/// can point the *production* code path at a local listener.
#[derive(Debug, Clone)]
pub(crate) struct HttpTokenEndpoint {
    pub url: String,
    pub client_id: String,
    pub timeout: Duration,
}

impl Default for HttpTokenEndpoint {
    fn default() -> Self {
        Self {
            url: TOKEN_ENDPOINT.to_owned(),
            client_id: CODEX_CLIENT_ID.to_owned(),
            timeout: REFRESH_TIMEOUT,
        }
    }
}

impl TokenEndpoint for HttpTokenEndpoint {
    fn refresh(&self, refresh_token: &str) -> std::result::Result<RefreshResponse, RefreshError> {
        let body = serde_json::json!({
            "client_id": self.client_id,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        });
        let (status, text) = post_json(&self.url, &body, self.timeout)
            .map_err(|e| RefreshError::Other(format!("token refresh request failed: {e}")))?;
        if !(200..300).contains(&status) {
            // `error` is *supposed* to be an OAuth code, but it is server text:
            // only the codes gx knows are ever printed (see [`describe`]), and
            // the token we sent is never echoed.
            let code = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            let msg = format!("token refresh failed: HTTP {status}{}", describe(&code));
            return Err(if code == "invalid_grant" {
                RefreshError::InvalidGrant(msg)
            } else {
                RefreshError::Other(msg)
            });
        }
        serde_json::from_str::<RefreshResponse>(&text)
            .map_err(|_| RefreshError::Other("token refresh returned an unreadable body".into()))
    }
}

/// The OAuth error codes gx is willing to repeat verbatim: RFC 6749's set plus
/// the two the endpoint actually adds. Anything else is unbounded server text.
pub(crate) const KNOWN_OAUTH_ERRORS: &[&str] = &[
    "invalid_grant",
    "invalid_request",
    "invalid_client",
    "unauthorized_client",
    "unsupported_grant_type",
    "server_error",
    "temporarily_unavailable",
    "rate_limited",
];

/// Render the `error` field of a failed token response.
///
/// **Never interpolates arbitrary server text.** The field is attacker- (or
/// merely bug-) influenced and this message goes to stderr, to scrollback, and
/// into whatever captures gx's output: a body that answered
/// `{"error": "<a page of HTML>"}` — or an echo of the token that was sent —
/// must not be reprinted. Only a code on the allowlist appears; everything else
/// collapses to a fixed phrase.
fn describe(code: &str) -> String {
    if code.is_empty() {
        String::new()
    } else if KNOWN_OAUTH_ERRORS.contains(&code) {
        format!(" ({code})")
    } else {
        " (unrecognized OAuth error)".to_owned()
    }
}

/// One blocking JSON POST, run on a dedicated thread.
///
/// `gx providers token openai` is dispatched from inside the pager's tokio
/// runtime, and a blocking reqwest client must not be driven from a runtime
/// worker thread; the scoped thread keeps it off one.
fn post_json(url: &str, body: &serde_json::Value, timeout: Duration) -> Result<(u16, String)> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| -> Result<(u16, String)> {
                let client =
                    xai_grok_extra_ca::build_blocking_reqwest_client(|b| b.timeout(timeout))
                        .context("failed to build the HTTP client")?;
                let resp = client
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .json(body)
                    .send()
                    .context("request failed")?;
                let status = resp.status().as_u16();
                let text = resp.text().unwrap_or_default();
                Ok((status, text))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("the refresh thread panicked"))?
    })
}

// ---------------------------------------------------------------------------
// minting
// ---------------------------------------------------------------------------

/// What `gx providers token openai` prints. Field names and types are grok's
/// auth-provider seam contract; nothing else may appear on stdout.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct MintedToken {
    pub access_token: String,
    pub expires_in: i64,
}

/// Everything [`mint_token`] needs, so no code path can silently fall back to a
/// real path, the real clock, or the real token endpoint.
pub(crate) struct MintOptions<'a> {
    pub paths: &'a CodexPaths,
    pub clock: &'a dyn Clock,
    pub endpoint: &'a dyn TokenEndpoint,
    pub lock_timeout: Duration,
    /// The token in the store was rejected on the wire, so refresh it even
    /// though its `exp` still says it is fine. grok sets `GROK_AUTH_EXPIRED=1`
    /// when it re-runs the helper after a 401; without honoring that, gx would
    /// hand back the same rejected token and the turn would fail twice.
    ///
    /// It does **not** force a second rotation when another process has already
    /// replaced the credential: [`mint_token`] only forces past the
    /// *generation* — the `(access_token, refresh_token)` pair — it has itself
    /// seen (see [`generation`]).
    pub force_refresh: bool,
}

impl<'a> MintOptions<'a> {
    pub(crate) fn new(
        paths: &'a CodexPaths,
        clock: &'a dyn Clock,
        endpoint: &'a dyn TokenEndpoint,
    ) -> Self {
        Self {
            paths,
            clock,
            endpoint,
            lock_timeout: LOCK_TIMEOUT,
            force_refresh: false,
        }
    }
}

/// The environment variable grok sets when re-running an auth helper after a
/// 401. See [`MintOptions::force_refresh`].
pub(crate) const AUTH_EXPIRED_ENV: &str = "GROK_AUTH_EXPIRED";

/// Mint a bearer for the codex endpoint, refreshing only if the current one is
/// actually stale. See the module docs for the lock protocol.
pub(crate) fn mint_token(opts: &MintOptions<'_>) -> Result<MintedToken> {
    // Resolve first: a symlinked store must be updated through the link, and
    // the lock and journal must sit beside the *real* file.
    let paths = opts.paths.canonicalized()?;
    let now = opts.clock.now_unix();
    let doc = read_oauth_document(&paths.auth_json)?;
    let rejected = opts.force_refresh.then(|| generation(&doc)).flatten();
    if rejected.is_none()
        && let Some(token) = fresh_token(&doc, now)
    {
        return Ok(token);
    }

    // Stale on the unlocked read. Everything from here on happens under the
    // lock, including a second read: a sibling gx process may have refreshed
    // while this one queued, and rotating again would throw its token away.
    let _lock = crate::providers_cmd::lock_providers_at(&paths.lock, opts.lock_timeout)?;
    let doc = read_oauth_document(&paths.auth_json)?;
    let now = opts.clock.now_unix();
    // A forced refresh gives way to a credential this process has not seen:
    // someone else already replaced the one the server rejected.
    let still_rejected = rejected.is_some() && rejected == generation(&doc);
    if !still_rejected && let Some(token) = fresh_token(&doc, now) {
        return Ok(token);
    }

    let Some(refresh_token) = doc.refresh_token() else {
        bail!(
            "{} has no refresh_token and its access token has expired — run `codex login`",
            paths.auth_json.display()
        );
    };
    // Before any POST: a platform gx cannot safely persist a rotation on must
    // never ask the server to perform one.
    ensure_refresh_supported()?;

    match opts.endpoint.refresh(refresh_token) {
        Ok(resp) => persist_and_mint(&paths, doc, resp, now),
        Err(RefreshError::InvalidGrant(msg)) => {
            eprintln!("gx: {msg}");
            // Exactly one reload — never a retry loop. The usual cause is that
            // codex refreshed (and rotated) between our read and our POST, in
            // which case the file now holds a token that works.
            let reloaded = read_oauth_document(&paths.auth_json)?;
            let now = opts.clock.now_unix();
            // ...but an *unchanged* store is not a recovery. Under
            // `force_refresh` the access token there is the one the server just
            // rejected on the wire; handing it back would fail the turn a
            // second time and hide the real problem.
            let unchanged = rejected.is_some() && rejected == generation(&reloaded);
            if !unchanged && let Some(token) = fresh_token(&reloaded, now) {
                eprintln!("gx: another process had already refreshed; using its token.");
                return Ok(token);
            }
            let retry = reloaded
                .refresh_token()
                .filter(|rt| *rt != refresh_token)
                .map(str::to_owned);
            if let Some(rt) = retry
                && let Ok(resp) = opts.endpoint.refresh(&rt)
            {
                return persist_and_mint(&paths, reloaded, resp, now);
            }
            recover_from_journal(opts, &paths, reloaded, refresh_token, now)
        }
        Err(RefreshError::Other(msg)) => bail!("{msg}"),
    }
}

/// The credential *generation*: the `(access_token, refresh_token)` pair.
///
/// The unit a rotation replaces is the pair, so the pair is what "has this
/// store changed since the server rejected it?" must compare. Comparing access
/// tokens alone is not enough: nothing stops a server from issuing the same
/// access-token string twice, and a match would then read as "still the
/// rejected credential" and rotate a live refresh token away.
fn generation(doc: &AuthDocument) -> Option<(String, Option<String>)> {
    Some((
        doc.access_token()?.to_owned(),
        doc.refresh_token().map(str::to_owned),
    ))
}

/// Last resort after `invalid_grant`: a rotation some earlier process obtained
/// but never managed to persist, recorded in the journal (see the module docs).
///
/// Tried **once**, and only when it is a token neither the store nor this
/// attempt already carries. On failure nothing is deleted — the journal is the
/// only remaining copy, and a later `codex login` is what clears it.
fn recover_from_journal(
    opts: &MintOptions<'_>,
    paths: &CodexPaths,
    doc: AuthDocument,
    tried: &str,
    now: i64,
) -> Result<MintedToken> {
    let journalled = read_recovery_journal(&paths.journal)
        .and_then(|journal| journal.refresh_token)
        .map(|rt| rt.trim().to_owned())
        .filter(|rt| !rt.is_empty())
        .filter(|rt| Some(rt.as_str()) != doc.refresh_token())
        .filter(|rt| rt != tried);
    let Some(rt) = journalled else {
        bail!(
            "the OpenAI refresh token in {} is no longer valid (invalid_grant). \
             Run `codex login` to sign in again.",
            paths.auth_json.display()
        );
    };
    eprintln!(
        "gx: trying the rotation recorded in {} — a refresh that was never written back.",
        paths.journal.display()
    );
    if let Ok(resp) = opts.endpoint.refresh(&rt) {
        return persist_and_mint(paths, doc, resp, now);
    }
    bail!(
        "the OpenAI refresh token in {} is no longer valid (invalid_grant), and the \
         unpersisted rotation recorded in {} was rejected too. Run `codex login` to sign \
         in again; the recovery file is left in place.",
        paths.auth_json.display(),
        paths.journal.display()
    );
}

/// Refusal for platforms where gx cannot make a rotation safe.
///
/// Windows is a declared non-goal here: [`crate::providers_cmd::lock_providers_at`]'s
/// `flock` is Unix-only (so nothing serializes two gx processes), and the
/// tmp+rename that stores the result cannot be relied on to replace a file
/// another process holds open. A refresh would then take the old refresh token
/// off the server and fail to store the new one — stranding the *only* live
/// credential in a temp file and logging the user out of codex as well as gx.
/// Reading a still-fresh token keeps working; only the rotation refuses.
#[cfg(unix)]
fn ensure_refresh_supported() -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn ensure_refresh_supported() -> Result<()> {
    bail!(
        "gx OpenAI token refresh is unsupported on this platform: gx cannot lock or \
         safely replace codex's auth.json here, and a refresh would strand the rotated \
         token. Run `codex login` to refresh the credential, or use the `openai-api` \
         provider instead."
    )
}

/// The seam payload for a document that needs no refresh.
fn fresh_token(doc: &AuthDocument, now: i64) -> Option<MintedToken> {
    let expires_in = freshness(doc, now).expires_in()?;
    Some(MintedToken {
        access_token: doc.access_token()?.to_owned(),
        expires_in,
    })
}

/// Fold a successful refresh into the full document, write it durably, and
/// return the seam payload. Called with the lock held.
fn persist_and_mint(
    paths: &CodexPaths,
    mut doc: AuthDocument,
    resp: RefreshResponse,
    now: i64,
) -> Result<MintedToken> {
    // The journal comes first, before a single field is merged: from the
    // instant the server answered until the rename below lands, this response
    // is the only copy of a live refresh token anywhere. See the module docs.
    write_recovery_journal(&paths.journal, &resp, now);

    let rotated = resp.refresh_token.clone().filter(|s| !s.trim().is_empty());
    let Some(access_token) = resp
        .access_token
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
    else {
        // A 2xx gx cannot mint from, but the rotation it carried is real and
        // the old token is already dead. Persist that much *before* reporting
        // the failure — see [`persist_rotation_only`].
        persist_rotation_only(paths, doc, rotated);
        bail!("the token endpoint returned no access_token");
    };
    let tokens = doc.tokens.get_or_insert_with(empty_tokens);
    tokens.access_token = Some(access_token.clone().into());
    if let Some(id_token) = resp.id_token.filter(|s| !s.trim().is_empty()) {
        tokens.id_token = Some(id_token.into());
    }
    // The rotation. Absent means the server kept the old one valid — not every
    // OAuth server rotates on every refresh, and codex-rs makes the same
    // assumption — so keeping ours is correct, and overwriting it with `None`
    // would be a logout.
    if let Some(rotated) = rotated {
        tokens.refresh_token = Some(rotated.into());
    }
    doc.last_refresh = Some(rfc3339(now).into());

    write_auth_document(&paths.auth_json, &doc)?;
    // Only now: the new refresh token exists in two places, so the journal has
    // nothing left to protect.
    clear_recovery_journal(&paths.journal);

    let expires_in = doc
        .access_token_exp()
        .map(|exp| exp.saturating_sub(now).max(0))
        .or(resp.expires_in)
        .unwrap_or(DEFAULT_EXPIRES_IN_SECS)
        .max(0);
    Ok(MintedToken {
        access_token,
        expires_in,
    })
}

/// Persist a rotation from a 2xx that carried nothing else usable.
///
/// The server has already killed the refresh token in `auth.json` by issuing
/// this one, so returning the error with the replacement still in memory would
/// be the lockout this module exists to prevent. The write is a full-document
/// merge (every unmodelled field survives) that touches *only*
/// `tokens.refresh_token`: the old access token stays, and `last_refresh` is
/// deliberately not stamped, so the store still reads as stale and the next
/// mint refreshes again — this time with a token the server will accept.
fn persist_rotation_only(paths: &CodexPaths, mut doc: AuthDocument, rotated: Option<String>) {
    let Some(rotated) = rotated else {
        // Nothing was rotated, so nothing is at risk and the journal is noise.
        clear_recovery_journal(&paths.journal);
        return;
    };
    doc.tokens.get_or_insert_with(empty_tokens).refresh_token = Some(rotated.into());
    match write_auth_document(&paths.auth_json, &doc) {
        Ok(()) => clear_recovery_journal(&paths.journal),
        Err(e) => eprintln!(
            "gx: could not store the rotated refresh token ({e:#}); it is recorded in {}",
            paths.journal.display()
        ),
    }
}

fn empty_tokens() -> AuthTokens {
    AuthTokens {
        id_token: None,
        access_token: None,
        refresh_token: None,
        account_id: None,
        extra: serde_json::Map::new(),
    }
}

fn rfc3339(now: i64) -> String {
    chrono::DateTime::from_timestamp(now, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Durable tmp+rename in `auth.json`'s own directory, mode 0600.
pub(crate) fn write_auth_document(path: &Path, doc: &AuthDocument) -> Result<()> {
    let rendered =
        serde_json::to_string_pretty(doc).context("failed to serialize the codex auth document")?;
    write_secret_atomically(path, &format!("{rendered}\n"))
}

// ---------------------------------------------------------------------------
// the rotation journal
// ---------------------------------------------------------------------------

/// The raw token response, as received. Written before `auth.json` is touched
/// and deleted once it has been rewritten; see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RecoveryJournal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// RFC 3339, from the same clock that stamps `last_refresh`.
    pub received_at: String,
}

/// Record a rotation before persisting it.
///
/// **Never fatal.** The server has already rotated by the time this runs;
/// failing the mint because the safety net could not be written would throw
/// away the very token the net exists to protect.
fn write_recovery_journal(path: &Path, resp: &RefreshResponse, now: i64) {
    let journal = RecoveryJournal {
        access_token: resp.access_token.clone(),
        refresh_token: resp.refresh_token.clone(),
        id_token: resp.id_token.clone(),
        received_at: rfc3339(now),
    };
    let Ok(rendered) = serde_json::to_string_pretty(&journal) else {
        return;
    };
    if let Err(e) = write_secret_atomically(path, &format!("{rendered}\n")) {
        eprintln!(
            "gx: could not write the rotation journal {} ({e:#}); continuing.",
            path.display()
        );
    }
}

/// The journal, when there is a readable one. Same pre-open gate as
/// `auth.json`: this path is a sibling of a file gx does not own, so it can be
/// anything.
pub(crate) fn read_recovery_journal(path: &Path) -> Option<RecoveryJournal> {
    if !crate::providers_cmd::gate_file(path, MAX_AUTH_JSON_BYTES, "auth recovery journal")
        .ok()
        .unwrap_or(false)
    {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Drop the journal. Only ever called once the rotation it holds is on disk in
/// `auth.json`.
fn clear_recovery_journal(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "gx: could not remove the rotation journal {} ({e}); it is stale but harmless.",
            path.display()
        ),
    }
}

// ---------------------------------------------------------------------------
// durable writes
// ---------------------------------------------------------------------------

/// Owner-only, atomic **and durable** replacement of a credential file.
///
/// Deliberately gx-local rather than [`xai_grok_config::fs_atomic`]: that
/// helper is shared with callers whose files are regenerable (cache markers,
/// id sidecars) and it does not fsync, so a crash just after the rename can
/// leave the new name pointing at unwritten blocks. Here the "new blocks" are a
/// refresh token that exists nowhere else, so:
///
/// 1. the temp file is created in the **same directory** as the target (a
///    rename across filesystems is not atomic) and `create_new`, so two writers
///    never share one,
/// 2. it is created mode 0600 — *before* any secret byte is written, not
///    chmod'ed afterwards, so the token is never readable by anyone else even
///    for an instant,
/// 3. `sync_all` puts the bytes on the medium **before** the rename publishes
///    them, and
/// 4. the parent directory is synced afterwards (best effort) so the rename
///    itself survives a power loss.
pub(crate) fn write_secret_atomically(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE: AtomicU64 = AtomicU64::new(0);

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auth".to_owned());
    let tmp = dir.join(format!(
        ".{name}.gx{}.{}.tmp",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(AUTH_MODE);
    }

    let written = (|| -> std::io::Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to write {}", path.display()));
    }

    // The contents are durable either way; this only makes the *name* durable.
    // Not every filesystem allows opening a directory, hence best effort.
    #[cfg(unix)]
    {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// account-change detection
// ---------------------------------------------------------------------------

/// gx's memory of the codex account it last saw.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct GxCodexState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// What `status` reports about the account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AccountCheck {
    /// Account id gx recorded the last time it looked.
    pub cached: Option<String>,
    /// The two differ and gx had actually seen one before.
    pub changed: bool,
}

pub(crate) fn read_state(path: &Path) -> GxCodexState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Compare `auth.json`'s account against the cached one. Pure over its inputs.
pub(crate) fn account_check(current: Option<String>, cached: Option<String>) -> AccountCheck {
    let changed = match (current.as_deref(), cached.as_deref()) {
        (Some(now), Some(before)) => now != before,
        // First sighting is not a change.
        _ => false,
    };
    AccountCheck { cached, changed }
}

/// Record the account gx just saw, so the next `status` can notice a switch.
/// Best effort: a state file gx cannot write is a missing note, not an error.
pub(crate) fn record_account(path: &Path, account_id: Option<&str>) {
    let state = GxCodexState {
        account_id: account_id.map(str::to_owned),
    };
    let Ok(rendered) = serde_json::to_string_pretty(&state) else {
        return;
    };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let _ = xai_grok_config::fs_atomic::write_atomically(path, &rendered, Some(AUTH_MODE));
}

// ---------------------------------------------------------------------------
// command entry points
// ---------------------------------------------------------------------------

/// `gx providers token openai`: the auth-provider helper the preset invokes.
///
/// stdout carries the seam JSON and nothing else — no banner, no warning, no
/// trailing note — because grok parses the whole of it.
pub(crate) fn run_token() -> Result<()> {
    let Some(paths) = CodexPaths::from_env() else {
        bail!("cannot locate a home directory to find ~/.codex/auth.json");
    };
    let clock = SystemClock;
    let endpoint = HttpTokenEndpoint::default();
    let mut opts = MintOptions::new(&paths, &clock, &endpoint);
    opts.force_refresh = std::env::var(AUTH_EXPIRED_ENV).is_ok_and(|v| v == "1");
    let token = mint_token(&opts)?;
    // `to_string`, not `to_string_pretty`: one line, one object.
    println!(
        "{}",
        serde_json::to_string(&token).context("failed to render the token payload")?
    );
    Ok(())
}

/// `gx providers login openai`: a thin, honest wrapper around `codex login`.
/// gx does not implement its own OAuth flow; codex owns the credential store.
pub(crate) fn run_login() -> Result<std::process::ExitStatus> {
    let Some(codex) = crate::diagnostics::find_on_path("codex") else {
        bail!(
            "the `codex` CLI is not on PATH. gx signs in to OpenAI by delegating to it: \
             install codex (https://github.com/openai/codex), run `codex login`, then \
             re-run `gx providers status`."
        );
    };
    eprintln!("gx: running `{} login`", codex.display());
    std::process::Command::new(&codex)
        .arg("login")
        .status()
        .with_context(|| format!("failed to run {}", codex.display()))
}

#[cfg(test)]
#[path = "openai_codex_auth_tests.rs"]
mod tests;
