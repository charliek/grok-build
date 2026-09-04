//! gx: `gx providers` — manage third-party provider presets.
//!
//! Two files, partitioned by whether stock `grok` can consume the shape:
//!
//! - **Stock-compatible** presets (GLM, OpenRouter, Meta) merge into the shared
//!   `$GROK_HOME/config.toml` so stock grok can read them.
//! - **gx-only** presets (Fireworks, openai-codex, openai-api) merge into
//!   `$GROK_HOME/providers.toml` ([`xai_grok_config::providers_layer`]), which
//!   stock grok never reads. That overlay still wins for gx, so a stock-
//!   compatible entry must not also be written there (it would shadow).
//!
//! This module owns the CLI surface for both files:
//!
//! - `install` — merge the shipped presets into the matching file with
//!   `toml_edit`, preserving comments, hand-written entries, and unknown
//!   fields. Adds what is missing; upgrades a field only while its value still
//!   equals a **shipped default** (current or older); leaves user-modified
//!   values alone unless `--force`.
//! - `set-key` / `unset-key` — write or remove `[model_providers.<id>].api_key`
//!   in the same file `install` uses for that provider. The key is never a
//!   positional argument: it comes from a no-echo prompt on a TTY, or from
//!   piped stdin.
//! - `status` — per-provider configuration, redacted key material, key source,
//!   model counts, plus the `~/.codex/auth.json` view for `openai-codex`.
//! - `login openai` — delegate to `codex login` (gx runs no OAuth flow of its
//!   own; codex owns the credential store).
//! - `token openai` — **the auth helper the `openai-codex` preset invokes**:
//!   print `{"access_token", "expires_in"}` and nothing else on stdout. See
//!   [`crate::openai_codex_auth`].
//!
//! Invariants:
//! - gx-only provider/model entries never go in `config.toml`.
//! - Both files are written atomically ([`xai_grok_config::fs_atomic`]) with
//!   mode 0600, under an exclusive advisory lock on `providers.toml.lock` so
//!   concurrent commands cannot drop each other's writes (one lock covers both
//!   files).
//! - No key material ever reaches argv, tracing, or an error message; `status`
//!   shows at most the last 4 characters of a key. That includes **parse
//!   diagnostics**: `toml`/`toml_edit` errors echo the offending source line,
//!   so nothing here ever formats one — see [`parse_position`].
//! - Every read of `providers.toml` / `config.toml` passes the same pre-read
//!   gate the runtime layer uses (regular file, size capped) before the path is
//!   opened. A `config.toml` that exists but is not valid TOML aborts rather
//!   than being overwritten.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Subcommand;

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

const PROVIDERS_AFTER_HELP: &str = "\
Examples:
  # Stock-compatible presets (GLM, OpenRouter, Meta) -> config.toml
  # gx-only presets (Fireworks, openai-codex, openai-api) -> providers.toml
  gx providers install

  # Store a key (prompted, never echoed; never pass it as an argument)
  gx providers set-key fireworks

  # Or pipe it from a secret manager
  cat key.txt | gx providers set-key fireworks

  # Fall back to the provider's env_key again
  gx providers unset-key fireworks

  # What is configured, where each key comes from (redacted)
  gx providers status";

#[derive(Debug, clap::Args, Clone)]
#[command(after_help = PROVIDERS_AFTER_HELP)]
pub struct ProvidersArgs {
    #[command(subcommand)]
    pub command: ProvidersCommand,
}

#[derive(Debug, Subcommand, Clone)]
pub enum ProvidersCommand {
    /// Install or update the shipped provider presets (stock-compatible into
    /// config.toml, gx-only into providers.toml)
    Install {
        /// Overwrite values you have edited by hand with the shipped defaults.
        #[arg(long)]
        force: bool,
    },
    /// Store an API key for a provider (prompted or piped, never an argument)
    SetKey {
        /// Provider id, e.g. `fireworks` (see `gx providers status`)
        provider: String,
    },
    /// Remove a provider's stored API key, falling back to its env_key
    UnsetKey {
        /// Provider id, e.g. `fireworks`
        provider: String,
    },
    /// Show configured providers, key sources (redacted), and model counts
    Status,
    /// Sign in to a provider (OpenAI: delegates to `codex login`)
    Login(AuthTargetArgs),
    /// Print a provider access token as the auth-helper JSON grok expects
    Token(AuthTargetArgs),
}

#[derive(Debug, clap::Args, Clone)]
pub struct AuthTargetArgs {
    #[command(subcommand)]
    pub provider: AuthTarget,
}

/// Providers with an interactive login / minted (non-static) credential.
#[derive(Debug, Subcommand, Clone, Copy, PartialEq, Eq)]
pub enum AuthTarget {
    /// OpenAI (ChatGPT/Codex plan credentials)
    Openai,
}

pub fn run(args: ProvidersArgs) -> Result<()> {
    let home = xai_grok_config::grok_home();
    match args.command {
        ProvidersCommand::Install { force } => run_install(&home, force),
        ProvidersCommand::SetKey { provider } => run_set_key(&home, &provider),
        ProvidersCommand::UnsetKey { provider } => run_unset_key(&home, &provider),
        ProvidersCommand::Status => run_status(&home),
        ProvidersCommand::Login(args) => match args.provider {
            AuthTarget::Openai => run_login_openai(),
        },
        ProvidersCommand::Token(args) => match args.provider {
            AuthTarget::Openai => crate::openai_codex_auth::run_token(),
        },
    }
}

/// `gx providers login openai`: run `codex login` and pass its exit status
/// through, so a script can branch on it exactly as if it had run codex itself.
fn run_login_openai() -> Result<()> {
    let status = crate::openai_codex_auth::run_login()?;
    if status.success() {
        eprintln!("gx: signed in. `gx providers status` shows the account and expiry.");
        return Ok(());
    }
    // `codex login` already printed its own diagnosis; do not paper over it.
    std::process::exit(status.code().unwrap_or(1))
}

/// Sessions read config at startup; there is no hot-reload (documented on the
/// providers layer), so every mutating command says so.
const RESTART_NOTICE_PROVIDERS: &str = "Restart any running gx sessions to pick up provider changes (providers.toml \
     is read at startup; there is no hot-reload).";
const RESTART_NOTICE_CONFIG: &str = "Restart any running gx or grok sessions to pick up provider changes \
     (config.toml is read at startup; there is no hot-reload).";
const RESTART_NOTICE_BOTH: &str = "Restart any running gx or grok sessions to pick up provider changes \
     (config.toml and providers.toml are read at startup; there is no hot-reload).";

fn restart_notice(config_changed: bool, providers_changed: bool) -> Option<&'static str> {
    match (config_changed, providers_changed) {
        (true, true) => Some(RESTART_NOTICE_BOTH),
        (true, false) => Some(RESTART_NOTICE_CONFIG),
        (false, true) => Some(RESTART_NOTICE_PROVIDERS),
        (false, false) => None,
    }
}

/// A stock build compiled from this source still writes stock-compatible
/// entries to config.toml (stock grok reads those) but gx-only entries in
/// providers.toml would have no effect.
fn warn_if_not_gx() {
    if !xai_grok_version::is_gx_build() {
        eprintln!(
            "warning: this is not a gx build; gx-only entries in providers.toml will \
             have no effect on this binary. Stock-compatible entries in config.toml \
             still apply."
        );
    }
}

// ---------------------------------------------------------------------------
// Preset data
//
// Catalog id -> entry, mirroring shapes proven against the live services. The
// presets carry `env_key` and NEVER `api_key`: keys are written by
// `gx providers set-key`, which is the only writer of key material here.
// ---------------------------------------------------------------------------

/// A value a preset can ship. Deliberately small — everything a
/// `[model_providers.*]` / `[model.*]` entry needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PresetValue {
    Str(&'static str),
    Int(i64),
    Bool(bool),
    StrList(&'static [&'static str]),
    /// A value only this machine can supply — the gx binary's own path, the
    /// account id in `~/.codex/auth.json`. Resolved once per `install` run
    /// through [`PresetContext`].
    Dynamic(DynamicValue),
}

/// The install-time-resolved preset values. Each one is a whole field value
/// (an inline table), not a scalar, because both are tables in the shapes gx
/// ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DynamicValue {
    /// `auth = { command = "<this gx binary>", args = [...] }` — the
    /// auth-helper seam the `openai-codex` provider mints its bearer through.
    GxTokenHelper,
    /// `extra_headers = { "chatgpt-account-id" = "<id>", originator = "gx" }`.
    /// The account id is omitted (with a warning) when `~/.codex/auth.json`
    /// carries none.
    CodexHeaders,
}

/// One key in a preset entry, plus every value gx has ever shipped for it.
///
/// `defaults[0]` is what this build writes. `defaults[1..]` are OLDER shipped
/// defaults: a value still equal to one of them was never touched by the user,
/// so `install` may upgrade it. Anything else is user-modified and is left
/// alone unless `--force`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PresetField {
    pub key: &'static str,
    pub defaults: &'static [PresetValue],
}

impl PresetField {
    const fn new(key: &'static str, defaults: &'static [PresetValue]) -> Self {
        Self { key, defaults }
    }

    fn current(&self) -> &PresetValue {
        &self.defaults[0]
    }

    fn dynamic(&self) -> Option<DynamicValue> {
        match self.current() {
            PresetValue::Dynamic(kind) => Some(*kind),
            _ => None,
        }
    }
}

/// Machine facts `install` resolves once and every [`PresetValue::Dynamic`]
/// reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PresetContext {
    /// How to invoke gx from a config file. The **current executable's**
    /// absolute path when it can be resolved, so the preset keeps working when
    /// `gx` is not on `PATH`; bare `gx` otherwise.
    pub gx_command: String,
    /// `tokens.account_id` from `~/.codex/auth.json` at install time.
    pub codex_account_id: Option<String>,
    /// Things `install` should tell the user about how the above resolved.
    pub notes: Vec<String>,
}

impl PresetContext {
    /// Resolve from this machine: the running binary and the codex store.
    pub(crate) fn detect() -> Self {
        let mut notes = Vec::new();
        let gx_command = match std::env::current_exe() {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(e) => {
                notes.push(format!(
                    "could not resolve this binary's own path ({e}); the openai-codex \
                     auth helper was written as plain `gx`, which must be on PATH."
                ));
                "gx".to_owned()
            }
        };
        let codex_account_id = crate::openai_codex_auth::codex_auth_json_path()
            .and_then(|p| crate::openai_codex_auth::read_auth_document(&p).ok())
            .and_then(|doc| doc.account_id());
        Self::new(gx_command, codex_account_id, notes)
    }

    /// The values as given, plus whatever they imply. Shared by [`detect`] and
    /// the test constructor so a note is never something only production says.
    ///
    /// [`detect`]: Self::detect
    fn new(gx_command: String, codex_account_id: Option<String>, mut notes: Vec<String>) -> Self {
        if codex_account_id.is_none() {
            notes.push(
                "no ChatGPT account id in ~/.codex/auth.json, so the openai-codex \
                 `chatgpt-account-id` header was omitted. Run `codex login` (or \
                 `gx providers login openai`), then re-run `gx providers install`."
                    .to_owned(),
            );
        }
        Self {
            gx_command,
            codex_account_id,
            notes,
        }
    }

    /// A context that reads nothing from this machine.
    #[cfg(test)]
    pub(crate) fn fixed(gx_command: &str, codex_account_id: Option<&str>) -> Self {
        Self::new(
            gx_command.to_owned(),
            codex_account_id.map(str::to_owned),
            Vec::new(),
        )
    }
}

/// The args gx's own auth helper is always invoked with. Also the signature
/// `install` recognizes when deciding whether an existing `auth` entry is a
/// gx-shipped one whose binary path may be refreshed.
pub(crate) const TOKEN_HELPER_ARGS: &[&str] = xai_grok_config::GX_TOKEN_HELPER_ARGS;

/// Budget grok gives the helper: enough for every case that ends in a token —
/// one refresh round trip, its single `invalid_grant` retry, and a wait behind
/// another gx process that is itself doing one.
///
/// It deliberately does **not** cover
/// [`crate::openai_codex_auth::LOCK_TIMEOUT`] *plus* two full round trips: that
/// combination only arises when the lock holder is itself timing out, and
/// grok's own timeout is the better outcome than a helper that hangs for
/// minutes. The lock wait's own error is what explains it.
pub(crate) const TOKEN_HELPER_TIMEOUT_SECS: i64 = 120;

/// The header naming the ChatGPT account, and the originator gx identifies as.
pub(crate) const CHATGPT_ACCOUNT_HEADER: &str = "chatgpt-account-id";
pub(crate) const GX_ORIGINATOR: &str = "gx";

/// Resolve a preset value to the concrete TOML this machine should write.
///
/// Comparison, rendering, and the `config.toml` shadow check all run on
/// [`toml::Value`], so a preset value becomes plain data exactly once, here.
fn resolve_value(v: &PresetValue, ctx: &PresetContext) -> toml::Value {
    match v {
        PresetValue::Str(s) => toml::Value::String((*s).to_owned()),
        PresetValue::Int(n) => toml::Value::Integer(*n),
        PresetValue::Bool(b) => toml::Value::Boolean(*b),
        PresetValue::StrList(items) => toml::Value::Array(
            items
                .iter()
                .map(|i| toml::Value::String((*i).to_owned()))
                .collect(),
        ),
        PresetValue::Dynamic(DynamicValue::GxTokenHelper) => {
            let mut map = toml::map::Map::new();
            map.insert(
                "command".to_owned(),
                toml::Value::String(ctx.gx_command.clone()),
            );
            map.insert(
                "args".to_owned(),
                toml::Value::Array(
                    TOKEN_HELPER_ARGS
                        .iter()
                        .map(|a| toml::Value::String((*a).to_owned()))
                        .collect(),
                ),
            );
            // Explicit, because the 30s default is too tight for the worst
            // case the helper can legitimately hit: waiting out another gx
            // process's refresh and then doing its own round trip.
            map.insert(
                "timeout_secs".to_owned(),
                toml::Value::Integer(TOKEN_HELPER_TIMEOUT_SECS),
            );
            toml::Value::Table(map)
        }
        PresetValue::Dynamic(DynamicValue::CodexHeaders) => {
            let mut map = toml::map::Map::new();
            if let Some(id) = &ctx.codex_account_id {
                map.insert(
                    CHATGPT_ACCOUNT_HEADER.to_owned(),
                    toml::Value::String(id.clone()),
                );
            }
            map.insert(
                "originator".to_owned(),
                toml::Value::String(GX_ORIGINATOR.to_owned()),
            );
            toml::Value::Table(map)
        }
    }
}

/// Whether an existing value is a **gx-shipped shape** of a dynamic field,
/// even though it does not equal what this machine would write now.
///
/// Without this, moving the gx binary (or switching ChatGPT accounts) would
/// leave the old path/account in place forever: `install` would see a value it
/// never shipped and treat it as a hand edit. The signature checks below are
/// narrow enough that a genuinely hand-written helper — different args, an
/// extra header — is still left alone.
fn is_shipped_dynamic_shape(kind: DynamicValue, existing: &toml::Value) -> bool {
    let Some(table) = existing.as_table() else {
        return false;
    };
    match kind {
        DynamicValue::GxTokenHelper => {
            let args: Vec<&str> = table
                .get("args")
                .and_then(toml::Value::as_array)
                .map(|a| a.iter().filter_map(toml::Value::as_str).collect())
                .unwrap_or_default();
            // All three, because each alone is too weak. The args say what is
            // being invoked; the command says it is *gx* invoking it — a
            // wrapper script that happens to call `gx providers token openai`
            // is the user's, and rewriting its `command` to gx's own path would
            // silently delete their wrapper from the chain. And no extra field:
            // an `env`, a `cwd`, a longer `timeout_secs` under a key gx never
            // ships is a hand edit, and replacing the whole inline table would
            // drop it.
            let command_is_gx = table
                .get("command")
                .and_then(toml::Value::as_str)
                .is_some_and(is_gx_command);
            let only_shipped_keys = table
                .keys()
                .all(|k| k == "command" || k == "args" || k == "timeout_secs");
            args == TOKEN_HELPER_ARGS && command_is_gx && only_shipped_keys
        }
        DynamicValue::CodexHeaders => {
            let originator_is_gx =
                table.get("originator").and_then(toml::Value::as_str) == Some(GX_ORIGINATOR);
            let only_shipped_keys = table
                .keys()
                .all(|k| k == CHATGPT_ACCOUNT_HEADER || k == "originator");
            originator_is_gx && only_shipped_keys
        }
    }
}

/// Whether `command` is one gx itself could have written: bare `gx` (the
/// fallback when `current_exe` fails) or an absolute path whose file name is
/// `gx` (a gx binary, wherever it was installed). A relative path, or an
/// absolute one pointing at anything else, belongs to the user.
fn is_gx_command(command: &str) -> bool {
    if command == "gx" {
        return true;
    }
    let path = Path::new(command);
    path.is_absolute() && path.file_name().is_some_and(|n| n == "gx")
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ModelPreset {
    /// Catalog id — the `[model."<id>"]` key.
    pub id: &'static str,
    pub fields: &'static [PresetField],
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProviderPreset {
    /// `[model_providers."<id>"]` key.
    pub id: &'static str,
    /// Human label for `status`.
    pub label: &'static str,
    /// Whether `gx providers install` writes this preset in this build.
    pub install: bool,
    /// `gx providers set-key` refuses this provider. Set on OAuth providers,
    /// where a static `api_key` outranks — and therefore silently disables —
    /// the auth helper that is the whole point of the entry.
    pub rejects_static_key: bool,
    /// Stock grok can consume this shape, so `install` / `set-key` /
    /// `unset-key` write it to the shared `config.toml`. gx-only presets
    /// (`false`) stay in `providers.toml`.
    pub stock_compatible: bool,
    /// Printed by `install` after the entry is written.
    pub note: Option<&'static str>,
    pub fields: &'static [PresetField],
    pub models: &'static [ModelPreset],
}

const fn s(v: &'static str) -> PresetValue {
    PresetValue::Str(v)
}
const fn i(v: i64) -> PresetValue {
    PresetValue::Int(v)
}
const fn b(v: bool) -> PresetValue {
    PresetValue::Bool(v)
}
const fn l(v: &'static [&'static str]) -> PresetValue {
    PresetValue::StrList(v)
}

const ALSO_WORKS_ON_STOCK: &str = "this shape also works in config.toml on stock grok; gx installs it \
     there so stock grok can read it.";

// -- GLM (Z.AI coding plan) --------------------------------------------------

const ZAI_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://api.z.ai/api/coding/paas/v4")]),
    PresetField::new("api_backend", &[s("chat_completions")]),
    PresetField::new("env_key", &[l(&["ZHIPU_API_KEY", "ZAI_API_KEY"])]),
];

// Both zai-coding-plan models share these fields; factored out so the
// per-model arrays below only spell out what actually varies.
const ZAI_MODEL_PROVIDER: PresetField = PresetField::new("model_provider", &[s("zai-coding-plan")]);
const ZAI_CONTEXT_WINDOW: PresetField = PresetField::new("context_window", &[i(1_000_000)]);
const ZAI_MAX_COMPLETION_TOKENS: PresetField =
    PresetField::new("max_completion_tokens", &[i(131_072)]);
const ZAI_SUPPORTS_REASONING_EFFORT: PresetField =
    PresetField::new("supports_reasoning_effort", &[b(true)]);
const ZAI_REASONING_EFFORTS: PresetField =
    PresetField::new("reasoning_efforts", &[l(&["low", "high", "max"])]);

const GLM_53_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("glm-5.3")]),
    PresetField::new("name", &[s("GLM 5.3 (Z.AI)")]),
    PresetField::new(
        "description",
        &[s("Z.AI flagship coding model. Thinking is always on.")],
    ),
    ZAI_MODEL_PROVIDER,
    ZAI_CONTEXT_WINDOW,
    ZAI_MAX_COMPLETION_TOKENS,
    ZAI_SUPPORTS_REASONING_EFFORT,
    PresetField::new("reasoning_effort", &[s("max")]),
    ZAI_REASONING_EFFORTS,
    PresetField::new("system_prompt_label", &[s("GLM 5.3")]),
];

// `glm-5.3-flash` is the fast sibling of `glm-5.3` on Z.AI's coding-plan API,
// verified reachable there on 2026-08-27. `context_window` and
// `max_completion_tokens` mirror `glm-5.3` (Z.AI documents the same limits for
// both), and `reasoning_efforts` mirrors it exactly rather than probing a
// wider menu: GLM's API silently ignores unknown effort values instead of
// rejecting them, so matching the proven sibling's shape beats guessing.
//
// Z.AI's coding-plan API also lists a `glm-5.3-highspeed` model, but it is
// tier-gated: the API returns "current subscription plan does not yet
// include access" (verified live, 2026-08-27). It is deliberately left out of
// this catalog; see `docs/gx/README.md` for how to add it by hand if the plan
// is upgraded.
const ZAI_GLM_53_FLASH_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("glm-5.3-flash")]),
    PresetField::new("name", &[s("GLM 5.3 Flash (Z.AI)")]),
    PresetField::new(
        "description",
        &[s(
            "Fast Z.AI coding-plan model, verified on the coding plan (2026-08-27).",
        )],
    ),
    ZAI_MODEL_PROVIDER,
    ZAI_CONTEXT_WINDOW,
    ZAI_MAX_COMPLETION_TOKENS,
    ZAI_SUPPORTS_REASONING_EFFORT,
    PresetField::new("reasoning_effort", &[s("high")]),
    ZAI_REASONING_EFFORTS,
];

// -- OpenRouter --------------------------------------------------------------

const OPENROUTER_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://openrouter.ai/api/v1")]),
    PresetField::new("api_backend", &[s("chat_completions")]),
    PresetField::new("env_key", &[s("OPENROUTER_API_KEY")]),
];

// `openrouter/ox-alpha` (`stealth/ox-alpha`) was OpenRouter's stealth test
// alias; it started 404ing and OpenRouter revealed it as Z.AI's GLM-5.3
// Flash. Retired 2026-08-26 in favor of `openrouter/glm-5.3-flash`, addressed
// directly by its real wire id rather than the alias. That model has now
// itself been retired (2026-08-27): the user's Z.AI coding-plan subscription
// covers `glm-5.3-flash` directly (see `ZAI_GLM_53_FLASH_FIELDS` above), so a
// metered OpenRouter duplicate of the same model is redundant. Both of these
// are a straight removal from `PRESETS`, not a replacement-in-place:
// `install` only adds/upgrades entries it finds in `PRESETS`, so a user's
// existing `[model."openrouter/ox-alpha"]` or `[model."openrouter/glm-5.3-flash"]`
// is left exactly as it was, forever — `install` never deletes an entry that
// falls out of the shipped catalog.

// Every OpenRouter model preset shares these fields; factored out so the
// per-model arrays below only spell out what actually varies. Mirrors the
// Fireworks factoring pattern.
const OPENROUTER_MODEL_PROVIDER: PresetField =
    PresetField::new("model_provider", &[s("openrouter")]);
// Kept `false`: a lenient host, but consistent with every other third-party
// preset here.
const OPENROUTER_NO_STREAM_TOOL_CALLS: PresetField =
    PresetField::new("stream_tool_calls", &[b(false)]);

// The three OpenRouter GPT-5.6 twins mirror the `openai-codex` preset's
// effort shape exactly: OpenRouter passes `reasoning_effort` straight through
// to OpenAI, so the same accepted menu applies (none/low/medium/high/xhigh,
// `minimal` rejected — see `OPENAI_CODEX_EFFORTS` above). `context_window`
// mirrors codex-rs's own value for the gpt-5.6 family.
const OPENROUTER_GPT_CONTEXT_WINDOW: PresetField =
    PresetField::new("context_window", &[i(272_000)]);
const OPENROUTER_GPT_SUPPORTS_EFFORT: PresetField =
    PresetField::new("supports_reasoning_effort", &[b(true)]);
const OPENROUTER_GPT_EFFORT: PresetField = PresetField::new("reasoning_effort", &[s("medium")]);
const OPENROUTER_GPT_EFFORTS: PresetField = PresetField::new(
    "reasoning_efforts",
    &[l(&["low", "medium", "high", "xhigh"])],
);

// Verified on OpenRouter with tools support; pricing verified 2026-08-27.
// OpenRouter reports no `reasoning_effort` support for this model, so it
// carries none of the effort fields (unlike every other model in this file).
const OPENROUTER_MINIMAX_M3_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("minimax/minimax-m3")]),
    PresetField::new("name", &[s("MiniMax M3 (OpenRouter)")]),
    PresetField::new(
        "description",
        &[s(
            "Cheap 1M-context generalist via OpenRouter ($0.30/$1.20 per M tokens, \
             verified 2026-08-27).",
        )],
    ),
    OPENROUTER_MODEL_PROVIDER,
    PresetField::new("context_window", &[i(1_048_576)]),
    OPENROUTER_NO_STREAM_TOOL_CALLS,
];

// The OpenRouter GPT-5.6 twins are a ChatGPT-plan overflow route: when the
// `openai-codex` plan-metered preset above is rate-limited or unavailable,
// these route the same models through OpenRouter's metered billing instead.
// Verified on OpenRouter with tools support; pricing verified 2026-08-27.
// Note the batch-vs-interactive pricing nuance: OpenRouter's `:batch` variants
// of these models are half-price again but async-only, so they are not
// substitutes for this preset's interactive, synchronous use.
const OPENROUTER_GPT_SOL_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("openai/gpt-5.6-sol")]),
    PresetField::new("name", &[s("GPT-5.6 Sol (OpenRouter, metered)")]),
    PresetField::new(
        "description",
        &[s(
            "ChatGPT-plan overflow route for GPT-5.6 Sol via OpenRouter, metered per \
             token; currently half of OpenAI-direct pricing ($2/$10 vs $4/$20 per M, \
             verified 2026-08-27).",
        )],
    ),
    OPENROUTER_MODEL_PROVIDER,
    OPENROUTER_GPT_CONTEXT_WINDOW,
    OPENROUTER_NO_STREAM_TOOL_CALLS,
    OPENROUTER_GPT_SUPPORTS_EFFORT,
    OPENROUTER_GPT_EFFORT,
    OPENROUTER_GPT_EFFORTS,
];

const OPENROUTER_GPT_TERRA_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("openai/gpt-5.6-terra")]),
    PresetField::new("name", &[s("GPT-5.6 Terra (OpenRouter, metered)")]),
    PresetField::new(
        "description",
        &[s(
            "ChatGPT-plan overflow route for GPT-5.6 Terra via OpenRouter, metered per \
             token; matches OpenAI-direct pricing ($2/$12 per M, verified 2026-08-27).",
        )],
    ),
    OPENROUTER_MODEL_PROVIDER,
    OPENROUTER_GPT_CONTEXT_WINDOW,
    OPENROUTER_NO_STREAM_TOOL_CALLS,
    OPENROUTER_GPT_SUPPORTS_EFFORT,
    OPENROUTER_GPT_EFFORT,
    OPENROUTER_GPT_EFFORTS,
];

const OPENROUTER_GPT_LUNA_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("openai/gpt-5.6-luna")]),
    PresetField::new("name", &[s("GPT-5.6 Luna (OpenRouter, metered)")]),
    PresetField::new(
        "description",
        &[s(
            "ChatGPT-plan overflow route for GPT-5.6 Luna via OpenRouter, metered per \
             token; the cheapest 5.6 model ($0.20/$1.20 per M, verified 2026-08-27).",
        )],
    ),
    OPENROUTER_MODEL_PROVIDER,
    OPENROUTER_GPT_CONTEXT_WINDOW,
    OPENROUTER_NO_STREAM_TOOL_CALLS,
    OPENROUTER_GPT_SUPPORTS_EFFORT,
    OPENROUTER_GPT_EFFORT,
    OPENROUTER_GPT_EFFORTS,
];

// -- Meta (Muse Spark) -------------------------------------------------------
//
// Live probe against Meta's Chat Completions API (2026-09-03) confirmed it
// validates `reasoning_effort` and 400s with the accepted list on an invalid
// value. The deserializer names `none|minimal|low|medium|high|xhigh|max`;
// Muse Spark 1.3 then rejects `none` and `max` (`Supported values: [minimal,
// low, medium, high, xhigh]`) and rejects `ultra` (`unknown variant`). The
// shipped menu is exactly those five. Default `high` matches Meta's
// coding-agents / Muse CLI baseline. `ultra` is also not a grok
// `ReasoningEffort` variant, so it would be unlistable even if the API took
// it.

const META_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://api.meta.ai/v1")]),
    PresetField::new("api_backend", &[s("chat_completions")]),
    PresetField::new("env_key", &[l(&["META_API_KEY", "MODEL_API_KEY"])]),
];

const META_MODEL_PROVIDER: PresetField = PresetField::new("model_provider", &[s("meta")]);
const META_CONTEXT_WINDOW: PresetField = PresetField::new("context_window", &[i(1_048_576)]);
const META_MAX_COMPLETION_TOKENS: PresetField =
    PresetField::new("max_completion_tokens", &[i(131_072)]);
const META_NO_STREAM_TOOL_CALLS: PresetField = PresetField::new("stream_tool_calls", &[b(false)]);
const META_SUPPORTS_REASONING_EFFORT: PresetField =
    PresetField::new("supports_reasoning_effort", &[b(true)]);
const META_REASONING_EFFORT: PresetField = PresetField::new("reasoning_effort", &[s("high")]);
const META_REASONING_EFFORTS: PresetField = PresetField::new(
    "reasoning_efforts",
    &[l(&["minimal", "low", "medium", "high", "xhigh"])],
);

const META_SPARK_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("muse-spark-1.3")]),
    PresetField::new("name", &[s("Muse Spark 1.3 (Meta)")]),
    PresetField::new(
        "description",
        &[s(
            "Muse Spark 1.3 via Meta Model API. Prompts are not used for training.",
        )],
    ),
    META_MODEL_PROVIDER,
    META_CONTEXT_WINDOW,
    META_MAX_COMPLETION_TOKENS,
    META_NO_STREAM_TOOL_CALLS,
    META_SUPPORTS_REASONING_EFFORT,
    META_REASONING_EFFORT,
    META_REASONING_EFFORTS,
];

const META_SPARK_CONTRIBUTOR_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("muse-spark-1.3-contributor")]),
    PresetField::new("name", &[s("Muse Spark 1.3 Contributor (Meta)")]),
    PresetField::new(
        "description",
        &[s(
            "Discounted Muse Spark 1.3. Your content, including inter-session messages, \
             may be used for product improvement.",
        )],
    ),
    META_MODEL_PROVIDER,
    META_CONTEXT_WINDOW,
    META_MAX_COMPLETION_TOKENS,
    META_NO_STREAM_TOOL_CALLS,
    META_SUPPORTS_REASONING_EFFORT,
    META_REASONING_EFFORT,
    META_REASONING_EFFORTS,
];

// -- Fireworks ---------------------------------------------------------------
//
// Every Fireworks model carries `stream_tool_calls = false` and an explicit
// `context_window` (grok has no catalog entry for third-party ids).
//
// `reasoning_effort` / `supports_reasoning_effort` / `reasoning_efforts`: a
// live probe against Fireworks' `chat_completions` API (2026-08-25) confirmed
// it validates `reasoning_effort` and 400s with the accepted list on an
// invalid value: low, medium, high, xhigh, max, none, adaptive. `reasoning_efforts`
// below omits `adaptive` on purpose — grok's `ReasoningEffort` enum
// (`xai-grok-sampling-types`) has no variant for it, so it is not a value gx
// can ever send.

const FIREWORKS_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://api.fireworks.ai/inference/v1")]),
    PresetField::new("api_backend", &[s("chat_completions")]),
    PresetField::new("env_key", &[s("FIREWORKS_API_KEY")]),
];

// Every Fireworks model preset shares these fields; factored out so the
// per-model arrays below only spell out what actually varies.
const FIREWORKS_MODEL_PROVIDER: PresetField = PresetField::new("model_provider", &[s("fireworks")]);
const FIREWORKS_NO_STREAM_TOOL_CALLS: PresetField =
    PresetField::new("stream_tool_calls", &[b(false)]);
const FIREWORKS_SUPPORTS_REASONING_EFFORT: PresetField =
    PresetField::new("supports_reasoning_effort", &[b(true)]);
const FIREWORKS_REASONING_EFFORT: PresetField = PresetField::new("reasoning_effort", &[s("high")]);
const FIREWORKS_REASONING_EFFORTS: PresetField = PresetField::new(
    "reasoning_efforts",
    &[l(&["low", "medium", "high", "xhigh", "max"])],
);

const FIREWORKS_KIMI_K3_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("accounts/fireworks/models/kimi-k3")]),
    PresetField::new("name", &[s("Kimi K3 (Fireworks)")]),
    PresetField::new(
        "description",
        &[s(
            "Moonshot Kimi K3 for coding and agentic work, served by Fireworks.",
        )],
    ),
    FIREWORKS_MODEL_PROVIDER,
    PresetField::new("context_window", &[i(1_048_576)]),
    FIREWORKS_NO_STREAM_TOOL_CALLS,
    FIREWORKS_SUPPORTS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORTS,
];

const FIREWORKS_QWEN_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("accounts/fireworks/models/qwen3p8-max")]),
    PresetField::new("name", &[s("Qwen3.8 Max (Fireworks)")]),
    PresetField::new(
        "description",
        &[s(
            "Qwen3.8 Max for large-context coding work, served by Fireworks.",
        )],
    ),
    FIREWORKS_MODEL_PROVIDER,
    PresetField::new("context_window", &[i(262_144)]),
    FIREWORKS_NO_STREAM_TOOL_CALLS,
    FIREWORKS_SUPPORTS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORTS,
];

const FIREWORKS_DEEPSEEK_PRO_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("accounts/fireworks/models/deepseek-v4-pro")]),
    PresetField::new("name", &[s("DeepSeek V4 Pro (Fireworks)")]),
    PresetField::new(
        "description",
        &[s(
            "DeepSeek V4 Pro for deep coding and reasoning work, served by Fireworks.",
        )],
    ),
    FIREWORKS_MODEL_PROVIDER,
    PresetField::new("context_window", &[i(1_048_576)]),
    FIREWORKS_NO_STREAM_TOOL_CALLS,
    FIREWORKS_SUPPORTS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORTS,
];

const FIREWORKS_KIMI_CODE_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("accounts/fireworks/models/kimi-k2p7-code")]),
    PresetField::new("name", &[s("Kimi K2.7 Code (Fireworks)")]),
    PresetField::new(
        "description",
        &[s("Moonshot Kimi K2.7 coding model, served by Fireworks.")],
    ),
    FIREWORKS_MODEL_PROVIDER,
    PresetField::new("context_window", &[i(262_144)]),
    FIREWORKS_NO_STREAM_TOOL_CALLS,
    FIREWORKS_SUPPORTS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORTS,
];

const FIREWORKS_DEEPSEEK_FLASH_FIELDS: &[PresetField] = &[
    PresetField::new(
        "model",
        &[s("accounts/fireworks/models/deepseek-v4-flash-0731")],
    ),
    PresetField::new("name", &[s("DeepSeek V4 Flash (Fireworks)")]),
    PresetField::new(
        "description",
        &[s(
            "Fast DeepSeek V4 Flash for high-throughput coding work, served by Fireworks.",
        )],
    ),
    FIREWORKS_MODEL_PROVIDER,
    PresetField::new("context_window", &[i(1_048_576)]),
    FIREWORKS_NO_STREAM_TOOL_CALLS,
    FIREWORKS_SUPPORTS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORT,
    FIREWORKS_REASONING_EFFORTS,
];

// -- OpenAI (ChatGPT / Codex plan) -------------------------------------------
//
// Shapes verified live against `https://chatgpt.com/backend-api/codex/responses`
// (spike 0A.1, 2026-08-25):
//
// - `api_backend = "responses"`, SSE. The endpoint's body validator is strict:
//   `store: false` is required, unknown top-level fields 400, and
//   `temperature` / `top_p` / `max_output_tokens` 400 as "Unsupported
//   parameter". `codex_compat = true` on each model is what makes grok's
//   Responses mapping emit that shape (see the sampling-types `responses.rs`).
// - NO `env_key`, and `set-key` refuses this provider: a static key or env key
//   beats the auth-provider token in credential resolution, which would shadow
//   the codex credentials this preset exists to use.
// - `auth` is the inline auth-helper seam: gx's own binary, minting from
//   `~/.codex/auth.json` (see `crate::openai_codex_auth`). Resolved to the
//   running executable's absolute path at install time so the entry keeps
//   working when `gx` is not on PATH.
// - `chatgpt-account-id` and `originator` are optional today (the spike passed
//   without each) but are sent anyway as drift insurance; both codex and
//   CLIProxyAPI send them.

const OPENAI_CODEX_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://chatgpt.com/backend-api/codex")]),
    PresetField::new("api_backend", &[s("responses")]),
    PresetField::new("auth", &[PresetValue::Dynamic(DynamicValue::GxTokenHelper)]),
    PresetField::new(
        "extra_headers",
        &[PresetValue::Dynamic(DynamicValue::CodexHeaders)],
    ),
];

/// Shared by every ChatGPT-plan model.
///
/// `context_window`: 272000, codex-rs's own value for the gpt-5.6 family
/// (`codex-rs/models-manager/models.json`). Efforts: the endpoint accepts
/// none/low/medium/high/xhigh/max and rejects `minimal`; the menu below is
/// codex's own `supported_reasoning_levels` for these models.
const OPENAI_CODEX_MODEL_PROVIDER: PresetField =
    PresetField::new("model_provider", &[s("openai-codex")]);
const OPENAI_CODEX_CONTEXT_WINDOW: PresetField = PresetField::new("context_window", &[i(272_000)]);
// gx: cosmetic stamp; existing installs infer this from codex_compat +
// the ChatGPT Codex URL (see xai_grok_shell::agent::reasoning_family).
const OPENAI_CODEX_FAMILY: PresetField = PresetField::new("model_family", &[s("openai-codex")]);
const OPENAI_CODEX_COMPAT: PresetField = PresetField::new("codex_compat", &[b(true)]);
const OPENAI_CODEX_SUPPORTS_EFFORT: PresetField =
    PresetField::new("supports_reasoning_effort", &[b(true)]);
const OPENAI_CODEX_EFFORT: PresetField = PresetField::new("reasoning_effort", &[s("medium")]);
const OPENAI_CODEX_EFFORTS: PresetField = PresetField::new(
    "reasoning_efforts",
    &[l(&["low", "medium", "high", "xhigh"])],
);

const OPENAI_SOL_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("gpt-5.6-sol")]),
    PresetField::new("name", &[s("GPT-5.6 Sol (ChatGPT)")]),
    PresetField::new(
        "description",
        &[s(
            "OpenAI's frontier agentic coding model, via your ChatGPT plan.",
        )],
    ),
    OPENAI_CODEX_MODEL_PROVIDER,
    OPENAI_CODEX_CONTEXT_WINDOW,
    OPENAI_CODEX_FAMILY,
    OPENAI_CODEX_COMPAT,
    OPENAI_CODEX_SUPPORTS_EFFORT,
    OPENAI_CODEX_EFFORT,
    OPENAI_CODEX_EFFORTS,
];

const OPENAI_TERRA_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("gpt-5.6-terra")]),
    PresetField::new("name", &[s("GPT-5.6 Terra (ChatGPT)")]),
    PresetField::new("description", &[s("GPT-5.6 Terra via your ChatGPT plan.")]),
    OPENAI_CODEX_MODEL_PROVIDER,
    OPENAI_CODEX_CONTEXT_WINDOW,
    OPENAI_CODEX_FAMILY,
    OPENAI_CODEX_COMPAT,
    OPENAI_CODEX_SUPPORTS_EFFORT,
    OPENAI_CODEX_EFFORT,
    OPENAI_CODEX_EFFORTS,
];

const OPENAI_LUNA_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("gpt-5.6-luna")]),
    PresetField::new("name", &[s("GPT-5.6 Luna (ChatGPT)")]),
    PresetField::new(
        "description",
        &[s("GPT-5.6 Luna, the faster ChatGPT-plan model.")],
    ),
    OPENAI_CODEX_MODEL_PROVIDER,
    OPENAI_CODEX_CONTEXT_WINDOW,
    OPENAI_CODEX_FAMILY,
    OPENAI_CODEX_COMPAT,
    OPENAI_CODEX_SUPPORTS_EFFORT,
    OPENAI_CODEX_EFFORT,
    OPENAI_CODEX_EFFORTS,
];

// -- OpenAI (plain API key) --------------------------------------------------
//
// A separate provider on purpose, never a dual-mode entry: this one talks to
// the public Responses API with an `sk-...` key and has nothing to do with a
// ChatGPT plan. It ships with **no models**: which OpenAI models a key can
// reach is account-specific, so a shipped catalog would be noise that goes
// stale. Users add their own `[model.<id>]` entries with
// `model_provider = "openai-api"`.

const OPENAI_API_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://api.openai.com/v1")]),
    PresetField::new("api_backend", &[s("responses")]),
    PresetField::new("env_key", &[s("OPENAI_API_KEY")]),
];

const OPENAI_API_NOTE: &str = "no models ship with this provider — the catalog is account-specific. Add your \
     own, e.g. [model.\"gpt-5.4\"] with model_provider = \"openai-api\".";

const OPENAI_CODEX_NOTE: &str = "signs in with your ChatGPT plan through `codex login`; run \
     `gx providers login openai` if `gx providers status` shows no credentials.";

/// The shipped catalog. Order is the order `install` writes and `status` prints.
pub(crate) const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "zai-coding-plan",
        label: "GLM (Z.AI coding plan)",
        install: true,
        rejects_static_key: false,
        stock_compatible: true,
        note: Some(ALSO_WORKS_ON_STOCK),
        fields: ZAI_PROVIDER_FIELDS,
        models: &[
            ModelPreset {
                id: "glm-5.3",
                fields: GLM_53_FIELDS,
            },
            ModelPreset {
                id: "glm-5.3-flash",
                fields: ZAI_GLM_53_FLASH_FIELDS,
            },
        ],
    },
    ProviderPreset {
        id: "openrouter",
        label: "OpenRouter",
        install: true,
        rejects_static_key: false,
        stock_compatible: true,
        note: Some(ALSO_WORKS_ON_STOCK),
        fields: OPENROUTER_PROVIDER_FIELDS,
        models: &[
            ModelPreset {
                id: "openrouter/minimax-m3",
                fields: OPENROUTER_MINIMAX_M3_FIELDS,
            },
            ModelPreset {
                id: "openrouter/gpt-5.6-sol",
                fields: OPENROUTER_GPT_SOL_FIELDS,
            },
            ModelPreset {
                id: "openrouter/gpt-5.6-terra",
                fields: OPENROUTER_GPT_TERRA_FIELDS,
            },
            ModelPreset {
                id: "openrouter/gpt-5.6-luna",
                fields: OPENROUTER_GPT_LUNA_FIELDS,
            },
        ],
    },
    ProviderPreset {
        id: "meta",
        label: "Meta Model API",
        install: true,
        rejects_static_key: false,
        stock_compatible: true,
        note: Some(ALSO_WORKS_ON_STOCK),
        fields: META_PROVIDER_FIELDS,
        models: &[
            ModelPreset {
                id: "muse-spark-1.3",
                fields: META_SPARK_FIELDS,
            },
            ModelPreset {
                id: "muse-spark-1.3-contributor",
                fields: META_SPARK_CONTRIBUTOR_FIELDS,
            },
        ],
    },
    ProviderPreset {
        id: "fireworks",
        label: "Fireworks",
        install: true,
        rejects_static_key: false,
        stock_compatible: false,
        note: None,
        fields: FIREWORKS_PROVIDER_FIELDS,
        models: &[
            ModelPreset {
                id: "fireworks/kimi-k3",
                fields: FIREWORKS_KIMI_K3_FIELDS,
            },
            ModelPreset {
                id: "fireworks/qwen3p8-max",
                fields: FIREWORKS_QWEN_FIELDS,
            },
            ModelPreset {
                id: "fireworks/deepseek-v4-pro",
                fields: FIREWORKS_DEEPSEEK_PRO_FIELDS,
            },
            ModelPreset {
                id: "fireworks/kimi-k2p7-code",
                fields: FIREWORKS_KIMI_CODE_FIELDS,
            },
            ModelPreset {
                id: "fireworks/deepseek-v4-flash",
                fields: FIREWORKS_DEEPSEEK_FLASH_FIELDS,
            },
        ],
    },
    ProviderPreset {
        id: "openai-codex",
        label: "OpenAI (ChatGPT/Codex plan)",
        install: true,
        rejects_static_key: true,
        stock_compatible: false,
        note: Some(OPENAI_CODEX_NOTE),
        fields: OPENAI_CODEX_PROVIDER_FIELDS,
        models: &[
            ModelPreset {
                id: "gpt-5.6-sol",
                fields: OPENAI_SOL_FIELDS,
            },
            ModelPreset {
                id: "gpt-5.6-terra",
                fields: OPENAI_TERRA_FIELDS,
            },
            ModelPreset {
                id: "gpt-5.6-luna",
                fields: OPENAI_LUNA_FIELDS,
            },
        ],
    },
    ProviderPreset {
        id: "openai-api",
        label: "OpenAI (API key)",
        install: true,
        rejects_static_key: false,
        stock_compatible: false,
        note: Some(OPENAI_API_NOTE),
        fields: OPENAI_API_PROVIDER_FIELDS,
        models: &[],
    },
];

fn preset_for(id: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|p| p.id == id)
}

// ---------------------------------------------------------------------------
// TOML value plumbing
// ---------------------------------------------------------------------------

/// Render a resolved preset value as the `toml_edit` value `install` writes.
/// Tables become **inline** tables: they are one field of an entry
/// (`auth = { ... }`), not a section of their own.
fn to_edit_value(v: &toml::Value) -> toml_edit::Value {
    match v {
        toml::Value::String(s) => toml_edit::Value::from(s.as_str()),
        toml::Value::Integer(n) => toml_edit::Value::from(*n),
        toml::Value::Float(f) => toml_edit::Value::from(*f),
        toml::Value::Boolean(b) => toml_edit::Value::from(*b),
        toml::Value::Datetime(d) => toml_edit::Value::from(d.to_string()),
        toml::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(to_edit_value(item));
            }
            toml_edit::Value::Array(array)
        }
        toml::Value::Table(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, v) in map {
                table.insert(k, to_edit_value(v));
            }
            toml_edit::Value::InlineTable(table)
        }
    }
}

/// Both comparisons go through [`edit_item_to_value`] rather than matching on
/// the `toml_edit` shape: one definition of equality for scalars, arrays, and
/// tables alike, and no way for an inline table and a section to disagree.
fn edit_item_matches(item: &toml_edit::Item, want: &toml::Value) -> bool {
    edit_item_to_value(item).as_ref() == Some(want)
}

/// Convert a `toml_edit` item to a plain [`toml::Value`], so an entry gx just
/// wrote can be compared field-by-field against the same entry in
/// `config.toml`. `None` for `Item::None` or a value that cannot round-trip.
fn edit_item_to_value(item: &toml_edit::Item) -> Option<toml::Value> {
    match item {
        toml_edit::Item::None => None,
        toml_edit::Item::Value(v) => edit_value_to_value(v),
        toml_edit::Item::Table(t) => {
            let mut map = toml::map::Map::new();
            for (k, v) in t.iter() {
                if let Some(v) = edit_item_to_value(v) {
                    map.insert(k.to_owned(), v);
                }
            }
            Some(toml::Value::Table(map))
        }
        toml_edit::Item::ArrayOfTables(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for t in arr.iter() {
                out.push(edit_item_to_value(&toml_edit::Item::Table(t.clone()))?);
            }
            Some(toml::Value::Array(out))
        }
    }
}

fn edit_value_to_value(v: &toml_edit::Value) -> Option<toml::Value> {
    Some(match v {
        toml_edit::Value::String(s) => toml::Value::String(s.value().clone()),
        toml_edit::Value::Integer(n) => toml::Value::Integer(*n.value()),
        toml_edit::Value::Float(f) => toml::Value::Float(*f.value()),
        toml_edit::Value::Boolean(b) => toml::Value::Boolean(*b.value()),
        // `toml` and `toml_edit` pull different `toml_datetime` majors, so the
        // hop goes through the (identical) RFC 3339 text form.
        toml_edit::Value::Datetime(d) => toml::Value::Datetime(d.value().to_string().parse().ok()?),
        toml_edit::Value::Array(a) => toml::Value::Array(
            a.iter()
                .map(edit_value_to_value)
                .collect::<Option<Vec<_>>>()?,
        ),
        toml_edit::Value::InlineTable(t) => {
            let mut map = toml::map::Map::new();
            for (k, v) in t.iter() {
                map.insert(k.to_owned(), edit_value_to_value(v)?);
            }
            toml::Value::Table(map)
        }
    })
}

/// Assign `value` to `key`, **keeping the formatting around it**.
///
/// `Table::insert` re-formats the key (`entry.key_mut().fmt()`), which drops
/// any comment lines sitting in the key's decor prefix, and a freshly built
/// value carries no decor, which drops the trailing `# comment` after the old
/// value. So an existing field is replaced in place and inherits the old
/// value's decor; only a genuinely new field goes through `insert`.
fn set_value_preserving_decor(table: &mut toml_edit::Table, key: &str, value: toml_edit::Value) {
    let mut value = value;
    if let Some(slot) = table.get_mut(key) {
        if let Some(old) = slot.as_value() {
            *value.decor_mut() = old.decor().clone();
        }
        *slot = toml_edit::Item::Value(value);
    } else {
        table.insert(key, toml_edit::Item::Value(value));
    }
}

/// TOML path rendering for messages: `model_providers.fireworks`,
/// `model."glm-5.3"`.
fn quoted_path(parent: &str, child: &str) -> String {
    let bare = !child.is_empty()
        && child
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare {
        format!("{parent}.{child}")
    } else {
        format!("{parent}.\"{child}\"")
    }
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

/// One entry that exists in **both** files with at least one differing field.
///
/// `fields` names the differing keys only — never their values. `api_key` is
/// one of the keys compared, so printing a value here would print a secret.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ShadowConflict {
    /// Rendered TOML path, e.g. `model_providers.fireworks`.
    pub path: String,
    /// Sorted names of the fields that differ between the two files.
    pub fields: Vec<String>,
}

/// What one `install` run did. Every field holds rendered TOML paths.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct InstallReport {
    /// Entries created from scratch.
    pub added_entries: Vec<String>,
    /// Fields added to an entry that already existed.
    pub added_fields: Vec<String>,
    /// Fields upgraded from an older shipped default.
    pub upgraded_fields: Vec<String>,
    /// Machine-derived fields (the gx binary path, the ChatGPT account header)
    /// rewritten because this machine now resolves them differently.
    pub refreshed_fields: Vec<String>,
    /// How the machine-derived values resolved, when that is worth saying.
    pub context_notes: Vec<String>,
    /// User-modified fields left as they are.
    pub kept_fields: Vec<String>,
    /// User-modified fields overwritten because of `--force`.
    pub forced_fields: Vec<String>,
    /// Entries not written because config.toml already has an identical one.
    pub skipped_identical_in_config: Vec<String>,
    /// Entries present in both files whose **full** tables differ.
    pub shadows_config: Vec<ShadowConflict>,
    /// Stock-compatible entries relocated out of providers.toml so they cannot
    /// shadow the config.toml copy gx now installs. Paths only — never values
    /// (`api_key` may have been copied).
    pub migrated_from_providers: Vec<String>,
    /// Whether either file's rendered document differs from what was on disk.
    pub changed: bool,
    /// Whether config.toml's rendered document differed from what was on disk.
    pub config_changed: bool,
    /// Whether providers.toml's rendered document differed from what was on disk.
    pub providers_changed: bool,
    /// The over-wide mode `install` found on providers.toml and clamped back
    /// to 0600. `Some` is a warning: the keys in it were readable by others.
    pub reclamped_from: Option<u32>,
    /// Same as [`Self::reclamped_from`], for config.toml.
    pub config_reclamped_from: Option<u32>,
    /// Rendered size, when it exceeds the runtime cap and the whole layer will
    /// therefore be skipped at startup.
    pub exceeds_runtime_cap: Option<u64>,
}

impl InstallReport {
    fn merge_apply(&mut self, other: Self) {
        self.added_entries.extend(other.added_entries);
        self.added_fields.extend(other.added_fields);
        self.upgraded_fields.extend(other.upgraded_fields);
        self.refreshed_fields.extend(other.refreshed_fields);
        self.context_notes.extend(other.context_notes);
        self.kept_fields.extend(other.kept_fields);
        self.forced_fields.extend(other.forced_fields);
        self.skipped_identical_in_config
            .extend(other.skipped_identical_in_config);
        self.shadows_config.extend(other.shadows_config);
        self.migrated_from_providers
            .extend(other.migrated_from_providers);
        if other.exceeds_runtime_cap.is_some() {
            self.exceeds_runtime_cap = other.exceeds_runtime_cap;
        }
    }
}

/// Header written once, when `providers.toml` is created.
const NEW_FILE_HEADER: &str = "\
# gx providers layer — managed by `gx providers install`.
#
# gx-only: stock `grok` never reads this file. Its [model], [model_providers],
# and [auth_provider] tables merge OVER config.toml inside the user tier; any
# other top-level table here is ignored with a warning.
#
# Hand edits are preserved: `install` only adds what is missing and upgrades
# values still equal to an older shipped default (`--force` overrides that).
# API keys belong under [model_providers.<id>].api_key — write them with
# `gx providers set-key <id>`, never by hand on a command line. Mode 0600.

";

/// How a preset entry compares to the same entry in `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigState {
    /// `config.toml` has no such entry.
    Absent,
    /// `config.toml` has it and every shipped default matches.
    Identical,
    /// `config.toml` has it, but it differs (or is not a table).
    Differs,
}

fn config_state(
    config: Option<&toml::Value>,
    parent: &str,
    child: &str,
    fields: &[PresetField],
    ctx: &PresetContext,
) -> ConfigState {
    let Some(entry) = config
        .and_then(|c| c.get(parent))
        .and_then(|t| t.get(child))
    else {
        return ConfigState::Absent;
    };
    let Some(table) = entry.as_table() else {
        return ConfigState::Differs;
    };
    let identical = fields.iter().all(|f| {
        table
            .get(f.key)
            .is_some_and(|v| *v == resolve_value(f.current(), ctx))
    });
    if identical {
        ConfigState::Identical
    } else {
        ConfigState::Differs
    }
}

/// Get (creating when missing) `[<parent>.<child>]`, reporting whether it was
/// created here.
fn entry_table<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    parent: &str,
    child: &str,
    file_label: &str,
) -> Result<(&'a mut toml_edit::Table, bool)> {
    if doc.get(parent).is_none() {
        let mut table = toml_edit::Table::new();
        // Implicit: render `[model_providers.fireworks]`, not a bare
        // `[model_providers]` header with nothing under it.
        table.set_implicit(true);
        doc.insert(parent, toml_edit::Item::Table(table));
    }
    let parent_table = doc
        .get_mut(parent)
        .and_then(toml_edit::Item::as_table_mut)
        .with_context(|| {
            format!("`{parent}` in {file_label} is not a table; refusing to modify it")
        })?;
    let created = !parent_table.contains_key(child);
    if created {
        parent_table.insert(child, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let table = parent_table
        .get_mut(child)
        .and_then(toml_edit::Item::as_table_mut)
        .with_context(|| {
            format!(
                "`{}` in {file_label} is not a table; refusing to modify it",
                quoted_path(parent, child)
            )
        })?;
    Ok((table, created))
}

/// Merge one entry's fields, recording every decision in `report`.
fn apply_fields(
    table: &mut toml_edit::Table,
    fields: &[PresetField],
    path: &str,
    entry_created: bool,
    force: bool,
    ctx: &PresetContext,
    report: &mut InstallReport,
) {
    for field in fields {
        let current = resolve_value(field.current(), ctx);
        match table.get(field.key) {
            None => {
                set_value_preserving_decor(table, field.key, to_edit_value(&current));
                if !entry_created {
                    report.added_fields.push(format!("{path}.{}", field.key));
                }
            }
            Some(item) if edit_item_matches(item, &current) => {}
            Some(item)
                if field.defaults[1..]
                    .iter()
                    .any(|older| edit_item_matches(item, &resolve_value(older, ctx))) =>
            {
                set_value_preserving_decor(table, field.key, to_edit_value(&current));
                report.upgraded_fields.push(format!("{path}.{}", field.key));
            }
            // A dynamic field still carrying a gx-shipped *shape* is stale, not
            // hand-edited: the binary moved, or the ChatGPT account changed.
            // Refresh it, and say so, rather than requiring `--force`.
            Some(item)
                if field.dynamic().is_some_and(|kind| {
                    edit_item_to_value(item).is_some_and(|v| is_shipped_dynamic_shape(kind, &v))
                }) =>
            {
                set_value_preserving_decor(table, field.key, to_edit_value(&current));
                report
                    .refreshed_fields
                    .push(format!("{path}.{}", field.key));
            }
            Some(_) if force => {
                set_value_preserving_decor(table, field.key, to_edit_value(&current));
                report.forced_fields.push(format!("{path}.{}", field.key));
            }
            Some(_) => {
                report.kept_fields.push(format!("{path}.{}", field.key));
            }
        }
    }
}

/// Compare the entry gx wrote into `providers.toml` with the same entry in
/// `config.toml`, **field by field over the full tables** — not just over the
/// fields a preset ships. A conflict in `api_key`, `extra_headers`, or any
/// other hand-written field is exactly as load-bearing as one in `base_url`.
///
/// `None` when the entry is absent from either file or the tables agree.
fn shadow_conflict(
    doc: &toml_edit::DocumentMut,
    config: Option<&toml::Value>,
    parent: &str,
    child: &str,
) -> Option<ShadowConflict> {
    let path = quoted_path(parent, child);
    let ours = doc.get(parent).and_then(|t| t.get(child))?;
    let theirs = config
        .and_then(|c| c.get(parent))
        .and_then(|t| t.get(child))?;
    let ours = edit_item_to_value(ours)?;
    let (Some(ours), Some(theirs)) = (ours.as_table(), theirs.as_table()) else {
        return Some(ShadowConflict {
            path,
            fields: vec!["(one side is not a table)".to_owned()],
        });
    };
    let mut fields: Vec<String> = Vec::new();
    for key in ours.keys().chain(theirs.keys()) {
        if fields.iter().any(|f| f == key) {
            continue;
        }
        if ours.get(key) != theirs.get(key) {
            fields.push(key.clone());
        }
    }
    if fields.is_empty() {
        return None;
    }
    fields.sort();
    Some(ShadowConflict { path, fields })
}

/// Merge one entry (provider or model) into `doc`.
fn apply_entry(
    doc: &mut toml_edit::DocumentMut,
    other: Option<&toml::Value>,
    parent: &str,
    child: &str,
    fields: &[PresetField],
    force: bool,
    ctx: &PresetContext,
    skip_identical: bool,
    file_label: &str,
    report: &mut InstallReport,
) -> Result<()> {
    let path = quoted_path(parent, child);

    // Type check FIRST, before any short-circuit. A non-table sitting where
    // install manages a table (`model_providers = "oops"`, or
    // `fireworks = "oops"` under it) must be reported, never stepped over: the
    // identical-in-config path below used to return `Ok` and leave the bogus
    // value in place, so a value gx refuses to touch could survive an install
    // that reported success.
    if let Some(parent_item) = doc.get(parent) {
        if !parent_item.is_table() {
            bail!("`{parent}` in {file_label} is not a table; refusing to modify it");
        }
        if let Some(existing) = parent_item.get(child)
            && !existing.is_table()
        {
            bail!(
                "`{path}` in {file_label} is not a table; refusing to modify it \
                 (an inline table must be rewritten as a `[{path}]` section)"
            );
        }
    }

    let state = config_state(other, parent, child, fields, ctx);
    let present = doc
        .get(parent)
        .and_then(|t| t.get(child))
        .is_some_and(toml_edit::Item::is_table);

    // For gx-only writes into providers.toml: config.toml already carries
    // exactly this entry, so writing a byte-identical copy would only add a
    // shadow to reason about. Stock-compatible writes into config.toml must
    // *not* skip just because providers.toml already has a copy — that overlay
    // would keep shadowing, and stock grok never sees it.
    if skip_identical && !present && state == ConfigState::Identical {
        report.skipped_identical_in_config.push(path);
        return Ok(());
    }

    let (table, created) = entry_table(doc, parent, child, file_label)?;
    apply_fields(table, fields, &path, created, force, ctx, report);
    if created {
        report.added_entries.push(path);
    }
    Ok(())
}

/// Merge `presets` into `doc`. Pure: no I/O, so every merge rule is unit
/// testable against a synthetic preset table.
///
/// `other` is the sibling file (`config.toml` when writing providers.toml, and
/// vice versa) used for skip-identical and shadow detection. `skip_identical`
/// is true only for gx-only writes into providers.toml.
pub(crate) fn apply_presets(
    doc: &mut toml_edit::DocumentMut,
    other: Option<&toml::Value>,
    presets: &[ProviderPreset],
    force: bool,
    ctx: &PresetContext,
    skip_identical: bool,
    file_label: &str,
) -> Result<InstallReport> {
    let mut report = InstallReport::default();
    for preset in presets.iter().filter(|p| p.install) {
        apply_entry(
            doc,
            other,
            "model_providers",
            preset.id,
            preset.fields,
            force,
            ctx,
            skip_identical,
            file_label,
            &mut report,
        )?;
        for model in preset.models {
            apply_entry(
                doc,
                other,
                "model",
                model.id,
                model.fields,
                force,
                ctx,
                skip_identical,
                file_label,
                &mut report,
            )?;
        }
    }
    // Only mention how the machine-derived values resolved when a preset that
    // actually uses one was installed.
    if presets
        .iter()
        .filter(|p| p.install)
        .any(|p| p.fields.iter().any(|f| f.dynamic().is_some()))
    {
        report.context_notes = ctx.notes.clone();
    }
    // Shadow detection runs over the *finished* document so it can compare the
    // full tables, catching conflicts in fields no preset ships. `other` is
    // whichever file we are *not* writing; the warning text always describes
    // providers.toml overlaying config.toml, which is the runtime order.
    for preset in presets.iter().filter(|p| p.install) {
        report
            .shadows_config
            .extend(shadow_conflict(doc, other, "model_providers", preset.id));
        for model in preset.models {
            report
                .shadows_config
                .extend(shadow_conflict(doc, other, "model", model.id));
        }
    }
    Ok(report)
}

fn overlay_entry_present(doc: &toml_edit::DocumentMut, parent: &str, child: &str) -> bool {
    doc.get(parent).and_then(|t| t.get(child)).is_some()
}

fn remove_overlay_entry(doc: &mut toml_edit::DocumentMut, parent: &str, child: &str) {
    let empty = if let Some(table) = doc.get_mut(parent).and_then(toml_edit::Item::as_table_mut) {
        table.remove(child);
        table.is_empty()
    } else {
        false
    };
    if empty {
        doc.remove(parent);
    }
}

/// Values on a providers.toml overlay table, cloned so they can be written
/// into config.toml without holding a borrow. Nested non-value items are
/// skipped (shipped presets only ever store values / inline tables).
fn overlay_values(
    doc: &toml_edit::DocumentMut,
    parent: &str,
    child: &str,
) -> Vec<(String, toml_edit::Value)> {
    let Some(item) = doc.get(parent).and_then(|t| t.get(child)) else {
        return Vec::new();
    };
    if let Some(table) = item.as_table() {
        return table
            .iter()
            .filter_map(|(k, v)| v.as_value().cloned().map(|val| (k.to_string(), val)))
            .collect();
    }
    if let Some(table) = item.as_inline_table() {
        return table
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
    }
    Vec::new()
}

fn copy_overlay_into_config(
    providers_doc: &toml_edit::DocumentMut,
    config_doc: &mut toml_edit::DocumentMut,
    parent: &str,
    child: &str,
) -> Result<()> {
    let fields = overlay_values(providers_doc, parent, child);
    if fields.is_empty() {
        return Ok(());
    }
    let (table, _) = entry_table(config_doc, parent, child, "config.toml")?;
    // Overlay wins for every key it carries: that is what gx was using at
    // runtime before the partition move (providers.toml shadows config.toml).
    for (key, value) in fields {
        set_value_preserving_decor(table, &key, value);
    }
    Ok(())
}

/// Relocate shipped stock-compatible tables out of providers.toml so they
/// cannot shadow the config.toml copy. Copies every overlay field (not just
/// `api_key`) so a hand-edited `base_url` / `extra_headers` / unknown key
/// survives. An explicit exception to "install never deletes": this is a
/// partition move, not a catalog removal.
fn migrate_stock_overlay(
    providers_doc: &mut toml_edit::DocumentMut,
    config_doc: &mut toml_edit::DocumentMut,
    presets: &[ProviderPreset],
) -> Result<Vec<String>> {
    let mut migrated = Vec::new();
    for preset in presets.iter().filter(|p| p.install && p.stock_compatible) {
        if overlay_entry_present(providers_doc, "model_providers", preset.id) {
            copy_overlay_into_config(
                providers_doc,
                config_doc,
                "model_providers",
                preset.id,
            )?;
            remove_overlay_entry(providers_doc, "model_providers", preset.id);
            migrated.push(quoted_path("model_providers", preset.id));
        }
        for model in preset.models {
            if overlay_entry_present(providers_doc, "model", model.id) {
                copy_overlay_into_config(providers_doc, config_doc, "model", model.id)?;
                remove_overlay_entry(providers_doc, "model", model.id);
                migrated.push(quoted_path("model", model.id));
            }
        }
    }
    Ok(migrated)
}

/// Path-injectable core of `gx providers install`.
///
/// Stock-compatible presets merge into `config.toml`; gx-only presets merge
/// into `providers.toml`. Both documents are applied in memory first; only
/// then is either file written, so a config.toml apply failure leaves
/// providers.toml unchanged. One lock covers both files so `set-key` cannot
/// interleave.
pub(crate) fn install_at(
    home: &Path,
    presets: &[ProviderPreset],
    force: bool,
    ctx: &PresetContext,
) -> Result<InstallReport> {
    let providers = providers_path(home);
    let config = config_path(home);
    // Everything below is a read-modify-write; hold the cross-process lock for
    // all of it so a concurrent `set-key` cannot be rendered away.
    let _lock = lock_providers(home)?;

    let providers_previous = read_file_gated(&providers, MAX_PROVIDERS_BYTES, "providers.toml")?;
    let mut providers_doc = parse_document(&providers_previous, &providers)?;
    let config_previous = read_file_gated(&config, MAX_CONFIG_BYTES, "config.toml")?;
    let mut config_doc = parse_document(&config_previous, &config)?;

    let config_value = toml_value_of(&config_previous);
    let providers_value = toml_value_of(&providers_previous);

    let gx_only: Vec<ProviderPreset> = presets
        .iter()
        .copied()
        .filter(|p| p.install && !p.stock_compatible)
        .collect();
    let stock: Vec<ProviderPreset> = presets
        .iter()
        .copied()
        .filter(|p| p.install && p.stock_compatible)
        .collect();

    let mut report = InstallReport::default();

    if !gx_only.is_empty() {
        let applied = apply_presets(
            &mut providers_doc,
            config_value.as_ref(),
            &gx_only,
            force,
            ctx,
            true,
            "providers.toml",
        )?;
        report.merge_apply(applied);
    }

    if !stock.is_empty() {
        let applied = apply_presets(
            &mut config_doc,
            providers_value.as_ref(),
            &stock,
            force,
            ctx,
            false,
            "config.toml",
        )?;
        report.merge_apply(applied);
        let migrated = migrate_stock_overlay(&mut providers_doc, &mut config_doc, &stock)?;
        report
            .shadows_config
            .retain(|s| !migrated.iter().any(|p| p == &s.path));
        report.migrated_from_providers.extend(migrated);
    }

    // Writes happen only after both applies (and any overlay migration)
    // succeeded. Commit config.toml (the destination of migrated keys) *before*
    // providers.toml (which drops the overlay). A config write failure then
    // leaves the overlay in place; a later providers write failure leaves a
    // dual copy that the next install will migrate again — never a lost key.
    if !stock.is_empty() {
        // Never stamp the providers.toml header onto config.toml: that file is
        // shared with stock grok and may already carry [cli]/[ui]/[plugins].
        let rendered = config_doc.to_string();
        let outcome = write_toml_file(&config, &rendered, &config_previous)?;
        report.config_changed = outcome.changed;
        report.config_reclamped_from = outcome.reclamped_from;
    }

    if !gx_only.is_empty() || !report.migrated_from_providers.is_empty() {
        let mut rendered = providers_doc.to_string();
        if providers_previous.trim().is_empty() {
            rendered = format!("{NEW_FILE_HEADER}{rendered}");
        }
        if rendered.len() as u64 > MAX_PROVIDERS_BYTES {
            report.exceeds_runtime_cap = Some(rendered.len() as u64);
        }
        let outcome = write_toml_file(&providers, &rendered, &providers_previous)?;
        report.providers_changed = outcome.changed;
        report.reclamped_from = outcome.reclamped_from;
    }

    report.changed = report.providers_changed || report.config_changed;
    Ok(report)
}

fn run_install(home: &Path, force: bool) -> Result<()> {
    warn_if_not_gx();
    let providers = providers_path(home);
    let config = config_path(home);
    let report = install_at(home, PRESETS, force, &PresetContext::detect())?;

    println!("config.toml:    {}", config.display());
    println!("providers.toml: {}", providers.display());
    if report.added_entries.is_empty()
        && report.added_fields.is_empty()
        && report.upgraded_fields.is_empty()
        && report.refreshed_fields.is_empty()
        && report.forced_fields.is_empty()
        && report.migrated_from_providers.is_empty()
    {
        println!("  up to date — nothing to add or upgrade");
    }
    for entry in &report.added_entries {
        println!("  added     {entry}");
    }
    for field in &report.added_fields {
        println!("  added     {field}");
    }
    for field in &report.upgraded_fields {
        println!("  upgraded  {field} (was an older shipped default)");
    }
    for field in &report.refreshed_fields {
        println!("  refreshed {field} (re-resolved for this machine)");
    }
    for field in &report.forced_fields {
        println!("  forced    {field} (--force replaced your value)");
    }
    for field in &report.kept_fields {
        println!("  kept      {field} (edited by you; --force overrides)");
    }
    for entry in &report.skipped_identical_in_config {
        println!("  skipped   {entry} — config.toml already has an identical entry");
    }
    for entry in &report.migrated_from_providers {
        println!("  migrated  {entry}  (providers.toml → config.toml)");
    }
    for shadow in &report.shadows_config {
        // Field NAMES only: `api_key` is one of the compared fields.
        eprintln!(
            "warning: providers.toml {} shadows a different entry of the same \
             name in config.toml; the providers.toml value wins for gx. \
             Differing fields: {}",
            shadow.path,
            shadow.fields.join(", ")
        );
    }
    for note in &report.context_notes {
        eprintln!("warning: {note}");
    }
    warn_if_reclamped(&providers, report.reclamped_from);
    warn_if_reclamped(&config, report.config_reclamped_from);
    if let Some(len) = report.exceeds_runtime_cap {
        eprintln!(
            "warning: {} is {len} bytes, over the {MAX_PROVIDERS_BYTES}-byte runtime cap — \
             gx will SKIP the entire providers layer at startup until it is trimmed.",
            providers.display()
        );
    }
    for preset in PRESETS.iter().filter(|p| p.install) {
        if let Some(note) = preset.note {
            println!("  note      {}: {note}", preset.id);
        }
    }
    println!();
    println!("Next: `gx providers set-key <provider>` to store a key, or export the");
    println!("provider's env_key. `gx providers status` shows what resolved.");
    if let Some(notice) = restart_notice(report.config_changed, report.providers_changed) {
        println!();
        println!("{notice}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// set-key / unset-key
// ---------------------------------------------------------------------------

/// Path-injectable core of `gx providers set-key`. `key` is never logged.
/// Returns the file the key was written to (`config.toml` or `providers.toml`).
pub(crate) fn set_key_at(home: &Path, provider: &str, key: &str) -> Result<PathBuf> {
    let key = key.trim();
    if key.is_empty() {
        bail!("empty key: nothing written");
    }
    // Checked before anything is opened, and regardless of whether the entry is
    // already in the target file: on an OAuth provider a static key is not a
    // "second credential", it is one that *wins* over the auth helper.
    if preset_for(provider).is_some_and(|p| p.rejects_static_key) {
        bail!(
            "`{provider}` does not take an API key: it signs in with your ChatGPT \
             account through `codex login`, and a stored api_key would shadow that. \
             Run `gx providers login openai`, or use the `openai-api` provider with \
             $OPENAI_API_KEY for a plain API key."
        );
    }
    let _lock = lock_providers(home)?;
    let target = resolve_key_file(home, provider)?;
    let previous = read_file_gated(&target.path, target.max_bytes, target.label)?;
    let mut doc = parse_document(&previous, &target.path)?;

    let known_here = provider_table_exists(&doc, provider);
    let preset = preset_for(provider);
    if !known_here {
        match preset {
            // A preset this build never installs: writing a lone api_key under
            // it would leave a half-configured provider.
            Some(preset) if !preset.install => bail!(
                "`{provider}` is not installed by this build; nothing written. \
                 Run `gx providers status` to see what is configured."
            ),
            // A preset the user has not installed yet: install just that one so
            // the key lands on a complete, usable entry. Skip-identical is off:
            // we need the table in *this* file to hold the key.
            Some(preset) => {
                apply_presets(
                    &mut doc,
                    None,
                    std::slice::from_ref(preset),
                    false,
                    &PresetContext::detect(),
                    false,
                    target.label,
                )?;
            }
            None => bail!(
                "unknown provider `{provider}`: not in {} and not a shipped \
                 preset. Known presets: {}",
                target.label,
                PRESETS.iter().map(|p| p.id).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    // A leftover stock-compatible overlay in providers.toml would shadow the
    // api_key we are about to write into config.toml. Relocate (or drop) it
    // under the same lock, before the config.toml write.
    if let Some(preset) = preset.filter(|p| p.stock_compatible) {
        let providers = providers_path(home);
        let providers_previous =
            read_file_gated(&providers, MAX_PROVIDERS_BYTES, "providers.toml")?;
        let mut providers_doc = parse_document(&providers_previous, &providers)?;
        let migrated =
            migrate_stock_overlay(&mut providers_doc, &mut doc, std::slice::from_ref(preset))?;
        if !migrated.is_empty() {
            let outcome =
                write_toml_file(&providers, &providers_doc.to_string(), &providers_previous)?;
            warn_if_reclamped(&providers, outcome.reclamped_from);
        }
    }

    let (table, _) = entry_table(&mut doc, "model_providers", provider, target.label)?;
    // Decor-preserving: a hand-written `api_key = "old" # vault-managed` keeps
    // its comment (and any comment lines above it) across a rotation.
    set_value_preserving_decor(table, "api_key", toml_edit::Value::from(key));

    let mut rendered = doc.to_string();
    if previous.trim().is_empty()
        && let Some(header) = target.new_file_header
    {
        rendered = format!("{header}{rendered}");
    }
    let outcome = write_toml_file(&target.path, &rendered, &previous)?;
    warn_if_reclamped(&target.path, outcome.reclamped_from);
    Ok(target.path)
}

/// Path-injectable core of `gx providers unset-key`. `Ok((false, path))` means
/// there was no key to remove; `path` is the file that was considered.
pub(crate) fn unset_key_at(home: &Path, provider: &str) -> Result<(bool, PathBuf)> {
    let _lock = lock_providers(home)?;
    let target = resolve_key_file(home, provider)?;
    let previous = read_file_gated(&target.path, target.max_bytes, target.label)?;
    if previous.is_empty() && !target.path.exists() {
        bail!(
            "no {} at {} — nothing to unset",
            target.label,
            target.path.display()
        );
    }
    let mut doc = parse_document(&previous, &target.path)?;
    let Some(table) = doc
        .get_mut("model_providers")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|t| t.get_mut(provider))
        .and_then(toml_edit::Item::as_table_mut)
    else {
        bail!(
            "provider `{provider}` is not defined in {}",
            target.path.display()
        );
    };
    let removed = table.remove("api_key").is_some();
    if removed {
        let outcome = write_toml_file(&target.path, &doc.to_string(), &previous)?;
        warn_if_reclamped(&target.path, outcome.reclamped_from);
    }
    Ok((removed, target.path))
}

/// A providers.toml found wider than 0600 is a disclosure, not a nit: say so
/// on stderr rather than clamping it back in silence.
fn warn_if_reclamped(path: &Path, reclamped_from: Option<u32>) {
    if let Some(mode) = reclamped_from {
        eprintln!(
            "warning: {} was mode {mode:04o}, not 0600 — it may hold API keys and was \
             readable beyond its owner. Reclamped to 0600; rotate any key stored in it.",
            path.display()
        );
    }
}

fn run_set_key(home: &Path, provider: &str) -> Result<()> {
    warn_if_not_gx();
    let key = read_secret(&format!("API key for `{provider}` (input hidden): "))?;
    // Redact before anything else can touch the value.
    let shown = redact_tail(&key, 4);
    let path = set_key_at(home, provider, &key)?;
    println!(
        "stored api_key for `{provider}` in {} ({shown})",
        path.display()
    );
    let config_changed = path.file_name().is_some_and(|n| n == "config.toml");
    if let Some(notice) = restart_notice(config_changed, !config_changed) {
        println!("{notice}");
    }
    Ok(())
}

fn run_unset_key(home: &Path, provider: &str) -> Result<()> {
    warn_if_not_gx();
    let (removed, path) = unset_key_at(home, provider)?;
    let label = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("the provider file");
    if removed {
        println!("removed api_key for `{provider}` from {}", path.display());
        println!("`{provider}` now resolves through its env_key, if one is set.");
        let config_changed = label == "config.toml";
        if let Some(notice) = restart_notice(config_changed, !config_changed) {
            println!("{notice}");
        }
    } else {
        println!("`{provider}` has no api_key in {label}; nothing to remove.");
    }
    Ok(())
}

/// Read a secret without echoing it: piped stdin when stdin is not a TTY,
/// otherwise a no-echo prompt. Never a command-line argument.
fn read_secret(prompt: &str) -> Result<String> {
    use std::io::IsTerminal as _;
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return read_secret_from_pipe(stdin.lock());
    }
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let result = read_secret_from_tty();
    eprintln!();
    result
}

/// Hard cap on a piped key. Real keys are well under 200 bytes; 8 KiB is
/// generous headroom and still bounded.
pub(crate) const MAX_PIPED_KEY_BYTES: u64 = 8 * 1024;

/// Piped path: the **first line** of stdin, trimmed. Split out so tests can
/// drive it with any reader.
///
/// The reader is `take()`-limited to [`MAX_PIPED_KEY_BYTES`] and consumed by a
/// single `read_line`, so `gx providers set-key` never buffers an unbounded
/// stream: `cat /dev/zero | gx providers set-key x` stops after 8 KiB instead
/// of growing a `String` until the OOM killer arrives.
pub(crate) fn read_secret_from_pipe(reader: impl std::io::Read) -> Result<String> {
    use std::io::{BufRead as _, Read as _};
    let mut limited = std::io::BufReader::new(reader.take(MAX_PIPED_KEY_BYTES));
    let mut line = String::new();
    let read = limited
        .read_line(&mut line)
        .context("failed to read the key from stdin")?;
    // Hit the cap without ever seeing a newline: the "key" is not a key.
    if read as u64 >= MAX_PIPED_KEY_BYTES && !line.ends_with('\n') {
        bail!("the key on stdin exceeds {MAX_PIPED_KEY_BYTES} bytes: nothing written");
    }
    let key = line.trim().to_owned();
    if key.is_empty() {
        bail!("no key on stdin: nothing written");
    }
    Ok(key)
}

/// RAII terminal raw mode: `Drop` restores echo on **every** path out of
/// [`read_secret_from_tty`] — the `?`s, the panics, the early `break`s — and
/// says so on stderr if the restore itself fails, rather than leaving the user
/// with a silently echo-less terminal.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("failed to disable terminal echo")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(e) = crossterm::terminal::disable_raw_mode() {
            eprintln!(
                "warning: failed to restore the terminal after reading the key: {e}. \
                 Run `stty sane` if your shell stops echoing."
            );
        }
    }
}

/// TTY path: raw mode, so the terminal never echoes what is typed.
fn read_secret_from_tty() -> Result<String> {
    use std::io::Read as _;
    let _raw = RawModeGuard::enable()?;
    let mut bytes: Vec<u8> = Vec::new();
    let mut stdin = std::io::stdin();
    let mut one = [0u8; 1];
    let mut aborted = false;
    loop {
        match stdin.read(&mut one) {
            Ok(0) => break,
            Ok(_) => match one[0] {
                b'\r' | b'\n' => break,
                0x03 => {
                    aborted = true;
                    break;
                }
                0x04 => break,
                0x7f | 0x08 => {
                    bytes.pop();
                }
                c if c < 0x20 => {}
                // Same bound as the piped path: a wedged terminal cannot grow
                // this buffer without limit.
                _ if bytes.len() as u64 >= MAX_PIPED_KEY_BYTES => {}
                c => bytes.push(c),
            },
            // `_raw` restores the terminal on the way out.
            Err(e) => return Err(e).context("failed to read the key"),
        }
    }
    if aborted {
        bail!("aborted: nothing written");
    }
    let key = String::from_utf8_lossy(&bytes).trim().to_owned();
    if key.is_empty() {
        bail!("empty key: nothing written");
    }
    Ok(key)
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// Where a provider's credential comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeySource {
    /// `api_key` in providers.toml (redacted tail).
    ProvidersFile(String),
    /// `api_key` in config.toml (redacted tail).
    ConfigFile(String),
    /// A set environment variable named by `env_key` (redacted tail).
    Env { var: String, redacted: String },
    /// `env_key` is declared but none of its variables are set.
    EnvUnset { vars: Vec<String> },
    /// No key and no env_key.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderStatus {
    pub id: String,
    pub label: Option<&'static str>,
    pub in_providers: bool,
    pub in_config: bool,
    pub base_url: Option<String>,
    pub env_keys: Vec<String>,
    pub key: KeySource,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StatusReport {
    pub providers_path: PathBuf,
    pub providers_present: bool,
    pub providers_error: Option<String>,
    pub config_error: Option<String>,
    pub providers: Vec<ProviderStatus>,
    pub codex: Option<CodexAuthStatus>,
}

/// Environment lookup seam, so `status` is testable without touching process
/// env (which is global and racy under a threaded test runner).
pub(crate) type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

fn env_key_names(entry: Option<&toml::Value>) -> Vec<String> {
    let Some(v) = entry.and_then(|e| e.get("env_key")) else {
        return Vec::new();
    };
    match v {
        toml::Value::String(s) => vec![s.clone()],
        toml::Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn table_of<'a>(root: Option<&'a toml::Value>, key: &str) -> Option<&'a toml::Table> {
    root.and_then(|r| r.get(key)).and_then(|t| t.as_table())
}

/// The **runtime** view of one entry: `config.toml`'s table with
/// `providers.toml`'s merged over it, field by field.
///
/// This is [`xai_grok_config::deep_merge_toml`], the exact call
/// `load_user_tier_for` makes, so `status` cannot disagree with what a session
/// will actually see. Whole-entry replacement (the shape `status` used before)
/// is wrong: a providers.toml entry that only overrides `base_url` leaves
/// config.toml's `env_key` in force at runtime, and `status` must say so.
fn merged_entry(
    from_config: Option<&toml::Value>,
    from_providers: Option<&toml::Value>,
) -> Option<toml::Value> {
    match (from_config, from_providers) {
        (Some(config), Some(providers)) => {
            let mut merged = config.clone();
            xai_grok_config::deep_merge_toml(&mut merged, providers);
            Some(merged)
        }
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (None, None) => None,
    }
}

/// Build the status view. Pure over its inputs.
pub(crate) fn status_report(
    home: &Path,
    codex_auth_path: Option<&Path>,
    env: EnvLookup<'_>,
) -> StatusReport {
    let path = providers_path(home);
    let (providers_doc, providers_error) =
        read_toml_file(&path, MAX_PROVIDERS_BYTES, "providers.toml");
    // A config.toml that trips the gate is *warned about and treated as
    // absent* here: `status` is read-only and must still render.
    let (config_doc, config_error) =
        read_toml_file(&home.join("config.toml"), MAX_CONFIG_BYTES, "config.toml");

    let mut ids: Vec<String> = Vec::new();
    let mut push_id = |ids: &mut Vec<String>, id: &str| {
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_owned());
        }
    };
    for preset in PRESETS {
        push_id(&mut ids, preset.id);
    }
    for source in [providers_doc.as_ref(), config_doc.as_ref()] {
        if let Some(table) = table_of(source, "model_providers") {
            for id in table.keys() {
                push_id(&mut ids, id);
            }
        }
    }

    let providers = ids
        .iter()
        .map(|id| provider_status(id, providers_doc.as_ref(), config_doc.as_ref(), env))
        .collect();

    let providers_present = path.exists();
    let codex = codex_auth_path.map(|p| {
        let mut status = read_codex_auth(p);
        let installed = installed_openai_codex_entry(providers_doc.as_ref(), config_doc.as_ref());
        annotate_codex_account(home, &mut status, installed.as_ref());
        annotate_token_helper(&mut status, installed.as_ref());
        status
    });
    StatusReport {
        providers_path: path,
        providers_present,
        providers_error,
        config_error,
        providers,
        codex,
    }
}

/// The `openai-codex` entry as the **runtime** will see it: providers.toml
/// deep-merged over config.toml, exactly as `load_user_tier_for` resolves it.
fn installed_openai_codex_entry(
    providers_doc: Option<&toml::Value>,
    config_doc: Option<&toml::Value>,
) -> Option<toml::Value> {
    merged_entry(
        table_of(config_doc, "model_providers").and_then(|t| t.get("openai-codex")),
        table_of(providers_doc, "model_providers").and_then(|t| t.get("openai-codex")),
    )
}

/// Flag an installed `auth.command` that is an absolute path to nothing.
///
/// The preset writes gx's own absolute path so the helper keeps working when
/// `gx` is not on `PATH` — which means moving, reinstalling or `cargo clean`ing
/// the binary turns the entry into a dangling reference. grok's failure there
/// is a helper that will not spawn, several layers away from this file; saying
/// so here is the difference between "re-run install" and a debugging session.
/// Only absolute paths are checked: a bare `gx` (or any other relative command)
/// is resolved against `PATH` at spawn time, which is not this function's to
/// second-guess.
fn annotate_token_helper(status: &mut CodexAuthStatus, installed: Option<&toml::Value>) {
    let Some(command) = installed
        .and_then(|entry| entry.get("auth"))
        .and_then(|auth| auth.get("command"))
        .and_then(toml::Value::as_str)
    else {
        return;
    };
    let path = Path::new(command);
    if path.is_absolute() && !path.exists() {
        status.helper_path_missing = Some(command.to_owned());
    }
}

/// Compare the account in `auth.json` against (a) the one gx last saw and (b)
/// the one baked into the installed `chatgpt-account-id` header, then refresh
/// gx's cache.
///
/// A ChatGPT account switch is invisible otherwise: the tokens keep working,
/// but requests carry the previous account's header until `install` is re-run.
fn annotate_codex_account(
    home: &Path,
    status: &mut CodexAuthStatus,
    installed: Option<&toml::Value>,
) {
    let Some(current) = status.account_id.clone() else {
        return;
    };
    let state_path =
        crate::openai_codex_auth::CodexPaths::for_auth_json(status.path.clone(), home).state;
    let cached = crate::openai_codex_auth::read_state(&state_path).account_id;
    let check = crate::openai_codex_auth::account_check(Some(current.clone()), cached);
    if check.changed {
        status.account_changed_from = check.cached.as_deref().map(|id| redact_tail(id, 6));
    }
    // Cache the account we just saw, so the *next* switch is the one reported.
    crate::openai_codex_auth::record_account(&state_path, Some(&current));

    // The header that will actually be sent.
    let header = installed.and_then(|entry| {
        entry
            .get("extra_headers")
            .and_then(|h| h.get(crate::providers_cmd::CHATGPT_ACCOUNT_HEADER))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    });
    if let Some(header) = header
        && header != current
    {
        status.installed_header_mismatch = Some(redact_tail(&header, 6));
    }
}

/// Build one provider's [`ProviderStatus`] from the merged providers.toml /
/// config.toml view. Split out of [`status_report`] so that function reads as
/// "collect ids, then map each to a status".
fn provider_status(
    id: &str,
    providers_doc: Option<&toml::Value>,
    config_doc: Option<&toml::Value>,
    env: EnvLookup<'_>,
) -> ProviderStatus {
    let from_providers = table_of(providers_doc, "model_providers").and_then(|t| t.get(id));
    let from_config = table_of(config_doc, "model_providers").and_then(|t| t.get(id));
    // providers.toml deep-merges over config.toml inside the user tier, so
    // every derived fact below reads off the merged entry, not off whichever
    // file happened to define the provider.
    let effective = merged_entry(from_config, from_providers);
    let effective = effective.as_ref();

    let env_keys: Vec<String> = env_key_names(effective);

    let providers_key = from_providers
        .and_then(|e| e.get("api_key"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let config_key = from_config
        .and_then(|e| e.get("api_key"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let key = if let Some(k) = providers_key {
        KeySource::ProvidersFile(redact_tail(k, 4))
    } else if let Some(k) = config_key {
        KeySource::ConfigFile(redact_tail(k, 4))
    } else if let Some((var, value)) = env_keys
        .iter()
        .find_map(|v| env(v).filter(|s| !s.trim().is_empty()).map(|s| (v, s)))
    {
        KeySource::Env {
            var: var.clone(),
            redacted: redact_tail(&value, 4),
        }
    } else if env_keys.is_empty() {
        KeySource::None
    } else {
        KeySource::EnvUnset {
            vars: env_keys.clone(),
        }
    };

    // Model ids come from the merged view: a model defined in both files is
    // counted once, and its `model_provider` is read off the field-level merge
    // (a providers.toml entry that only overrides `context_window` keeps
    // config.toml's `model_provider`).
    let providers_models = table_of(providers_doc, "model");
    let config_models = table_of(config_doc, "model");
    let mut model_ids: Vec<String> = Vec::new();
    for table in [config_models, providers_models].into_iter().flatten() {
        for model_id in table.keys() {
            if !model_ids.iter().any(|s| s == model_id) {
                model_ids.push(model_id.clone());
            }
        }
    }
    let mut models: Vec<String> = Vec::new();
    for model_id in model_ids {
        let merged = merged_entry(
            config_models.and_then(|t| t.get(&model_id)),
            providers_models.and_then(|t| t.get(&model_id)),
        );
        if merged
            .as_ref()
            .and_then(|e| e.get("model_provider"))
            .and_then(|v| v.as_str())
            == Some(id)
        {
            models.push(model_id);
        }
    }
    models.sort();

    ProviderStatus {
        id: id.to_owned(),
        label: preset_for(id).map(|p| p.label),
        in_providers: from_providers.is_some(),
        in_config: from_config.is_some(),
        base_url: effective
            .and_then(|e| e.get("base_url"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        env_keys,
        key,
        models,
    }
}

/// Render a [`StatusReport`]. `now_unix` keeps expiry text deterministic in
/// tests.
pub(crate) fn render_status(report: &StatusReport, now_unix: i64) -> String {
    let mut out = String::new();
    let present = if report.providers_present {
        ""
    } else {
        "  (not created yet — run `gx providers install`)"
    };
    out.push_str(&format!(
        "providers file: {}{present}\n",
        report.providers_path.display()
    ));
    if let Some(err) = &report.providers_error {
        out.push_str(&format!("  WARNING: providers.toml did not parse: {err}\n"));
        out.push_str("  gx skips the whole layer while this is broken.\n");
    }
    if let Some(err) = &report.config_error {
        out.push_str(&format!("  WARNING: config.toml did not parse: {err}\n"));
    }
    out.push('\n');

    for p in &report.providers {
        let label = p.label.map(|l| format!("  — {l}")).unwrap_or_default();
        out.push_str(&format!("{}{label}\n", p.id));
        let configured = match (p.in_providers, p.in_config) {
            (true, true) => "yes  (providers.toml, shadowing config.toml)",
            (true, false) => "yes  (providers.toml)",
            (false, true) => "yes  (config.toml)",
            (false, false) => "no   (run `gx providers install`)",
        };
        out.push_str(&format!("  configured   {configured}\n"));
        if let Some(url) = &p.base_url {
            out.push_str(&format!("  base_url     {url}\n"));
        }
        let key = match &p.key {
            KeySource::ProvidersFile(red) => {
                format!("yes  {red}  (providers.toml api_key)")
            }
            KeySource::ConfigFile(red) => format!("yes  {red}  (config.toml api_key)"),
            KeySource::Env { var, redacted } => format!("yes  {redacted}  (env {var})"),
            KeySource::EnvUnset { vars } => {
                format!("no   (env_key {} not set)", vars.join(", "))
            }
            KeySource::None => "no   (no api_key, no env_key)".to_owned(),
        };
        out.push_str(&format!("  key          {key}\n"));
        if !p.env_keys.is_empty() {
            out.push_str(&format!("  env_key      {}\n", p.env_keys.join(", ")));
        }
        if p.models.is_empty() {
            out.push_str("  models       0\n");
        } else {
            out.push_str(&format!(
                "  models       {}  ({})\n",
                p.models.len(),
                p.models.join(", ")
            ));
        }
        out.push('\n');
    }

    if let Some(codex) = &report.codex {
        out.push_str(&render_codex_status(codex, now_unix));
    }
    out
}

fn run_status(home: &Path) -> Result<()> {
    let env = |name: &str| std::env::var(name).ok();
    let codex_path = codex_auth_path();
    let report = status_report(home, codex_path.as_deref(), &env);
    let now = chrono::Utc::now().timestamp();
    print!("{}", render_status(&report, now));
    Ok(())
}

// ---------------------------------------------------------------------------
// codex credentials (read-only, never written or refreshed here)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CodexAuthStatus {
    pub path: PathBuf,
    pub present: bool,
    pub error: Option<String>,
    /// `exp` from the access token's JWT payload (signature NOT verified).
    pub access_token_exp: Option<i64>,
    /// Account id in full. Compared against gx's cache and against the
    /// installed header; **never rendered** — `account_id_redacted` is.
    pub account_id: Option<String>,
    /// Account id, redacted to its last 6 characters.
    pub account_id_redacted: Option<String>,
    /// `chatgpt_plan_type`, when the token carries one.
    pub plan: Option<String>,
    pub has_api_key: bool,
    pub has_refresh_token: bool,
    /// `last_refresh`, as a unix timestamp. Held (rather than a verdict) so the
    /// refresh rule stays the credential module's single definition and
    /// rendering stays a pure function of `(status, now)`.
    pub last_refresh_unix: Option<i64>,
    /// Whether the file carries a `tokens` object at all.
    pub has_tokens: bool,
    /// The account gx saw last time, redacted. `Some` only when it differs
    /// from the current one.
    pub account_changed_from: Option<String>,
    /// The `chatgpt-account-id` written into providers.toml at install time,
    /// redacted. `Some` only when it no longer matches `auth.json`.
    pub installed_header_mismatch: Option<String>,
    /// The installed `auth.command`, when it is an absolute path that no longer
    /// exists — a moved or removed gx binary. Not a secret: it is a path the
    /// user wrote (indirectly) and has to fix.
    pub helper_path_missing: Option<String>,
}

/// `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`.
fn codex_auth_path() -> Option<PathBuf> {
    crate::openai_codex_auth::codex_auth_json_path()
}

/// Decode a JWT payload **without verifying the signature**. gx only reads
/// claims for display; the server is the only thing that validates the token.
pub(crate) fn decode_jwt_claims_unverified(token: &str) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Read codex's `auth.json` read-only: never written, never refreshed, and
/// unknown fields are simply not touched.
///
/// The parsing and the staleness rule are [`crate::openai_codex_auth`]'s, not a
/// second copy: what `status` reports about expiry is exactly what
/// `gx providers token openai` will act on.
pub(crate) fn read_codex_auth(path: &Path) -> CodexAuthStatus {
    let mut status = CodexAuthStatus {
        path: path.to_path_buf(),
        ..Default::default()
    };
    // `read_auth_document` reports a missing file as an error; `status` renders
    // absence differently from unreadability, so it is distinguished here.
    match std::fs::metadata(path) {
        Ok(_) => status.present = true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return status,
        Err(e) => {
            status.error = Some(e.to_string());
            return status;
        }
    }
    let doc = match crate::openai_codex_auth::read_auth_document(path) {
        Ok(doc) => doc,
        Err(e) => {
            // The module's messages carry the path and a position, never file
            // content — this file is nothing but credentials.
            status.error = Some(format!("{e:#}"));
            return status;
        }
    };
    status.has_api_key = doc.has_api_key();
    status.has_refresh_token = doc.has_refresh_token();
    status.access_token_exp = doc.access_token_exp();
    status.plan = doc.plan();
    status.account_id = doc.account_id();
    status.account_id_redacted = status.account_id.as_deref().map(|id| redact_tail(id, 6));
    status.last_refresh_unix = doc.last_refresh_unix();
    status.has_tokens = doc.has_tokens();
    status
}

fn render_codex_status(codex: &CodexAuthStatus, now_unix: i64) -> String {
    let mut out = String::new();
    out.push_str("openai-codex credentials (codex CLI, read-only)\n");
    out.push_str(&format!("  auth.json    {}\n", codex.path.display()));
    if !codex.present {
        out.push_str("  status       not found — run `codex login` to create it\n\n");
        return out;
    }
    if let Some(err) = &codex.error {
        out.push_str(&format!("  status       unreadable: {err}\n\n"));
        return out;
    }
    out.push_str("  status       present\n");
    match codex.access_token_exp {
        Some(exp) => {
            let when = chrono::DateTime::from_timestamp(exp, 0)
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_else(|| exp.to_string());
            // `exp` is attacker-influenced JSON: it can be i64::MIN or
            // i64::MAX, where `exp - now` and `-delta` both overflow and panic
            // in a debug build. Saturating throughout.
            let delta = exp.saturating_sub(now_unix);
            let rel = if delta <= 0 {
                format!("EXPIRED {}", human_duration(delta.saturating_neg()))
            } else {
                format!("expires in {}", human_duration(delta))
            };
            out.push_str(&format!("  access token {when}  ({rel})\n"));
        }
        None => {
            out.push_str("  access token no parsable JWT exp claim\n");
        }
    }
    if codex.has_tokens {
        use crate::openai_codex_auth::Freshness;
        let verdict = crate::openai_codex_auth::freshness_from(
            codex.access_token_exp,
            codex.last_refresh_unix,
            now_unix,
        );
        let line = match verdict {
            Freshness::FreshByJwt { .. } => "fresh (JWT exp)",
            Freshness::FreshByLastRefresh { .. } => "fresh (last_refresh, no parsable JWT)",
            Freshness::StaleByJwt => "REFRESH DUE — `gx providers token openai` will refresh it",
            Freshness::StaleByLastRefresh => {
                "REFRESH DUE — last_refresh is over 7 days old, no parsable JWT"
            }
            Freshness::StaleUnknown => {
                "UNKNOWN — no JWT exp and no last_refresh; gx will try to refresh"
            }
        };
        out.push_str(&format!("  refresh      {line}\n"));
    }
    if let Some(account) = &codex.account_id_redacted {
        out.push_str(&format!("  account      {account}\n"));
    }
    if let Some(plan) = &codex.plan {
        out.push_str(&format!("  plan         {plan}\n"));
    }
    if let Some(previous) = &codex.account_changed_from {
        out.push_str(&format!(
            "  ACCOUNT CHANGED  was {previous}; gx's cached account has been updated\n"
        ));
    }
    if let Some(installed) = &codex.installed_header_mismatch {
        out.push_str(&format!(
            "  STALE HEADER     providers.toml sends chatgpt-account-id {installed}, which no \
             longer matches auth.json — re-run `gx providers install`\n"
        ));
    }
    if let Some(command) = &codex.helper_path_missing {
        out.push_str(&format!(
            "  HELPER MISSING   {command}: helper path missing (binary moved?) — gx falls \
             back to this binary at runtime; rerun `gx providers install` to persist\n"
        ));
    }
    if codex.has_api_key {
        out.push_str("  OPENAI_API_KEY present in auth.json (api-key mode)\n");
    }
    if codex.has_tokens && !codex.has_refresh_token {
        out.push_str("  note         no refresh_token; `codex login` again when it expires\n");
    }
    out.push('\n');
    out
}

fn human_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 48 {
        return format!("{hours}h{}m", mins % 60);
    }
    format!("{}d", hours / 24)
}

// ---------------------------------------------------------------------------
// shared file plumbing
// ---------------------------------------------------------------------------

fn providers_path(home: &Path) -> PathBuf {
    xai_grok_config::providers_layer_path(home)
}

fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

fn toml_value_of(content: &str) -> Option<toml::Value> {
    if content.trim().is_empty() {
        None
    } else {
        toml::from_str(content).ok()
    }
}

fn provider_table_exists(doc: &toml_edit::DocumentMut, provider: &str) -> bool {
    doc.get("model_providers")
        .and_then(|t| t.get(provider))
        .is_some_and(toml_edit::Item::is_table)
}

/// Which file `set-key` / `unset-key` mutates for a provider.
struct KeyFile {
    path: PathBuf,
    label: &'static str,
    max_bytes: u64,
    new_file_header: Option<&'static str>,
}

fn providers_key_file(home: &Path) -> KeyFile {
    KeyFile {
        path: providers_path(home),
        label: "providers.toml",
        max_bytes: MAX_PROVIDERS_BYTES,
        new_file_header: Some(NEW_FILE_HEADER),
    }
}

fn config_key_file(home: &Path) -> KeyFile {
    KeyFile {
        path: config_path(home),
        label: "config.toml",
        max_bytes: MAX_CONFIG_BYTES,
        new_file_header: None,
    }
}

/// Stock-compatible presets (and unknown providers that exist only in
/// config.toml) mutate config.toml; everything else mutates providers.toml.
fn resolve_key_file(home: &Path, provider: &str) -> Result<KeyFile> {
    if let Some(preset) = preset_for(provider) {
        return Ok(if preset.stock_compatible {
            config_key_file(home)
        } else {
            providers_key_file(home)
        });
    }

    let providers = providers_key_file(home);
    let config = config_key_file(home);
    let in_providers = {
        let previous = read_file_gated(&providers.path, providers.max_bytes, providers.label)?;
        provider_table_exists(&parse_document(&previous, &providers.path)?, provider)
    };
    let in_config = {
        let previous = read_file_gated(&config.path, config.max_bytes, config.label)?;
        provider_table_exists(&parse_document(&previous, &config.path)?, provider)
    };
    if in_config && !in_providers {
        Ok(config)
    } else {
        Ok(providers)
    }
}

/// Cap on `providers.toml`, mirroring the runtime layer's own
/// `MAX_PROVIDERS_LAYER_BYTES`. A larger file is skipped wholesale at startup,
/// so reading (or growing past) one here would be pointless as well as unsafe.
pub(crate) const MAX_PROVIDERS_BYTES: u64 = 1024 * 1024;

/// Cap on `config.toml`. Roomier than the providers cap — config.toml is stock
/// grok's file and may legitimately be large — but still bounded, so a runaway
/// file or a device node dropped in its place can never be read into memory
/// here. Stock-compatible presets are written to this file.
pub(crate) const MAX_CONFIG_BYTES: u64 = 10 * 1024 * 1024;

/// Pre-read gate, the same shape [`xai_grok_config::providers_layer`] applies
/// at startup: one [`std::fs::metadata`] call **before** the path is opened, so
/// a fifo can never block and `/dev/zero` can never be slurped.
///
/// `Ok(false)` = absent, treat as empty. `Ok(true)` = safe to read. `Err` =
/// refuse. The message carries only the path and sizes, never file content.
pub(crate) fn gate_file(path: &Path, max_bytes: u64, what: &str) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if !meta.is_file() {
                bail!(
                    "{} is not a regular file (fifo, device, directory, or similar); \
                     refusing to read it",
                    path.display()
                );
            }
            if meta.len() > max_bytes {
                bail!(
                    "{} is {} bytes, over the {max_bytes}-byte {what} limit; refusing to read it",
                    path.display(),
                    meta.len()
                );
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn read_file_gated(path: &Path, max_bytes: u64, what: &str) -> Result<String> {
    if !gate_file(path, max_bytes, what)? {
        return Ok(String::new());
    }
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// 1-based line/column of a byte offset in `src`.
fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let (mut line, mut col) = (1usize, 1usize);
    for (i, ch) in src.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Where a parse failed, and **nothing else**.
///
/// Both `toml::de::Error` and `toml_edit::TomlError` render the offending
/// source line in their `Display` impls. In these two files that line is
/// routinely `api_key = "sk-live-…"`, so a stray `{e}` in an error chain leaks
/// the key to stdout/stderr, to a shell's scrollback, and to any log that
/// captures it. Nothing in this module formats those errors; every parse
/// failure is described by this function's output instead.
fn parse_position(src: &str, span: Option<std::ops::Range<usize>>) -> String {
    match span {
        Some(span) => {
            let (line, col) = line_col(src, span.start);
            format!("malformed TOML at line {line}, column {col}")
        }
        None => "malformed TOML".to_owned(),
    }
}

fn parse_document(content: &str, path: &Path) -> Result<toml_edit::DocumentMut> {
    if content.trim().is_empty() {
        return Ok(toml_edit::DocumentMut::new());
    }
    content.parse::<toml_edit::DocumentMut>().map_err(|e| {
        // Deliberately NOT `.context(e)`: that would put the library error —
        // and the source line it echoes — into the `{e:#}` chain.
        anyhow!(
            "{} is not valid TOML ({}); refusing to overwrite it — fix or move it, then re-run",
            path.display(),
            parse_position(content, e.span())
        )
    })
}

/// Read + parse a TOML file for the read-only `status` view. The returned error
/// string is always sanitized — position only, never file content.
fn read_toml_file(
    path: &Path,
    max_bytes: u64,
    what: &str,
) -> (Option<toml::Value>, Option<String>) {
    match gate_file(path, max_bytes, what) {
        Ok(false) => return (None, None),
        Ok(true) => {}
        Err(e) => return (None, Some(format!("{e:#}"))),
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (None, None),
        Err(e) => return (None, Some(format!("unreadable: {e}"))),
    };
    // `toml::from_str` (not `str::parse`) — the FromStr impl parses a single
    // TOML *value*, not a document.
    match toml::from_str::<toml::Value>(&raw) {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(parse_position(&raw, e.span()))),
    }
}

// ---------------------------------------------------------------------------
// cross-process lock
//
// `install` / `set-key` / `unset-key` are read-modify-rename cycles. Two of
// them racing (a script storing three keys in parallel, or an `install` running
// while a `set-key` lands) would have the later rename drop whatever the other
// wrote. An exclusive advisory flock on a sibling lockfile serializes them.
// ---------------------------------------------------------------------------

/// How long a mutating command waits for another one before giving up. Short:
/// these commands take milliseconds, so anything longer is a stuck process.
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// `<providers.toml>.lock`, alongside the file it guards.
pub(crate) fn providers_lock_path(home: &Path) -> PathBuf {
    let path = providers_path(home);
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(".lock");
    path.with_file_name(name)
}

/// Held for the whole read-modify-write; `Drop` releases the flock.
#[derive(Debug)]
pub(crate) struct ProvidersLock {
    #[cfg_attr(not(unix), allow(dead_code))]
    file: std::fs::File,
}

impl Drop for ProvidersLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd as _;
            // SAFETY: `self.file` is open for the whole call; closing it would
            // release the lock anyway, this just makes the release explicit.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// Take the exclusive advisory lock guarding `$GROK_HOME/providers.toml`.
pub(crate) fn lock_providers(home: &Path) -> Result<ProvidersLock> {
    lock_providers_at(&providers_lock_path(home), LOCK_TIMEOUT)
}

/// Path- and timeout-injectable core, so the lock is unit-testable.
pub(crate) fn lock_providers_at(
    lock_path: &Path,
    timeout: std::time::Duration,
) -> Result<ProvidersLock> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(false).write(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(PROVIDERS_MODE);
    }
    let file = opts
        .open(lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd as _;
        // `mode()` above only applies at creation; clamp a lockfile that
        // predates it too (best effort — it holds no key material itself).
        let _ = enforce_mode(lock_path);
        let fd = file.as_raw_fd();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // SAFETY: `fd` belongs to `file`, which is alive for this call.
            if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => {}
                _ => {
                    return Err(err)
                        .with_context(|| format!("failed to lock {}", lock_path.display()));
                }
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "another `gx providers` command is holding {} (waited {:?}); \
                     nothing was written — retry once it finishes, or remove the \
                     lockfile if no gx process is running",
                    lock_path.display(),
                    timeout
                );
            }
            std::thread::sleep(LOCK_POLL);
        }
    }
    Ok(ProvidersLock { file })
}

/// Mode for `providers.toml`: it can hold API keys, so owner-only.
const PROVIDERS_MODE: u32 = 0o600;

/// The result of one [`write_toml_file`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WriteOutcome {
    /// The rendered document differed from what was on disk.
    pub changed: bool,
    /// The over-wide mode found on the file and clamped back. `Some` means the
    /// file was readable beyond its owner and must be reported, not shrugged at.
    pub reclamped_from: Option<u32>,
}

/// Write only when the rendered document differs, so a no-op `install` leaves
/// the file byte-identical. Permissions are re-asserted either way — including
/// on the byte-identical early return, which is precisely the path a repeated
/// `gx providers install` takes over a file someone chmod'ed to 0644.
///
/// Used for both `providers.toml` and `config.toml` (stock-compatible presets
/// put keys in the latter).
fn write_toml_file(path: &Path, rendered: &str, previous: &str) -> Result<WriteOutcome> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if rendered == previous && path.exists() {
        return Ok(WriteOutcome {
            changed: false,
            reclamped_from: enforce_mode(path)?,
        });
    }
    xai_grok_config::fs_atomic::write_atomically(path, rendered, Some(PROVIDERS_MODE))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(WriteOutcome {
        changed: true,
        reclamped_from: enforce_mode(path)?,
    })
}

/// `write_atomically` sets the mode at creation, but a pre-existing file (or
/// one a user chmod'ed) keeps its own; keys live here, so clamp every time.
///
/// Returns the previous mode when bits outside 0600 had to be dropped — the
/// caller warns about that. A clamp that *fails* is a hard error: leaving a
/// world-readable key file behind while exiting 0 is exactly the outcome this
/// function exists to prevent. A mode narrower than 0600 (0400, say) is left
/// alone; only extra bits are removed.
fn enforce_mode(path: &Path) -> Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::metadata(path)
            .with_context(|| format!("failed to stat {} to check its mode", path.display()))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & !PROVIDERS_MODE == 0 {
            return Ok(None);
        }
        let clamped = mode & PROVIDERS_MODE;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(clamped)).with_context(
            || {
                format!(
                    "{} is mode {mode:04o} but must not be readable beyond its owner \
                     (it can hold API keys), and the permission change failed",
                    path.display()
                )
            },
        )?;
        Ok(Some(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// At most `keep` trailing characters of a secret, never more. Short values
/// render as `…` with no characters at all.
pub(crate) fn redact_tail(secret: &str, keep: usize) -> String {
    let n = secret.chars().count();
    if n == 0 {
        return "(empty)".to_owned();
    }
    if n <= keep * 2 {
        return "…".to_owned();
    }
    let tail: String = secret.chars().skip(n - keep).collect();
    format!("…{tail}")
}

#[cfg(test)]
#[path = "providers_cmd_tests.rs"]
mod tests;
