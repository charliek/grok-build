//! gx: `gx providers` — manage the gx-only providers layer,
//! `$GROK_HOME/providers.toml`.
//!
//! Stock `grok` never reads `providers.toml`
//! ([`xai_grok_config::providers_layer`]), so every gx-only model/provider
//! entry lives here instead of in the `config.toml` both binaries share. This
//! module owns the CLI surface for that file:
//!
//! - `install` — merge the shipped presets into `providers.toml` with
//!   `toml_edit`, preserving comments, hand-written entries, and unknown
//!   fields. Adds what is missing; upgrades a field only while its value still
//!   equals a **shipped default** (current or older); leaves user-modified
//!   values alone unless `--force`.
//! - `set-key` / `unset-key` — write or remove `[model_providers.<id>].api_key`.
//!   The key is never a positional argument: it comes from a no-echo prompt on
//!   a TTY, or from piped stdin.
//! - `status` — per-provider configuration, redacted key material, key source,
//!   model counts, plus the `~/.codex/auth.json` view for `openai-codex`.
//! - `login openai` / `token openai` — parsed but not implemented in this
//!   build (Phase 2); they exit 2 so the clap surface stays stable.
//!
//! Invariants:
//! - `config.toml` is **never** written by this module. Not one code path.
//! - `providers.toml` is written atomically ([`xai_grok_config::fs_atomic`])
//!   with mode 0600, under an exclusive advisory lock on
//!   `providers.toml.lock` so concurrent commands cannot drop each other's
//!   writes.
//! - No key material ever reaches argv, tracing, or an error message; `status`
//!   shows at most the last 4 characters of a key. That includes **parse
//!   diagnostics**: `toml`/`toml_edit` errors echo the offending source line,
//!   so nothing here ever formats one — see [`parse_position`].
//! - Every read of `providers.toml` / `config.toml` passes the same pre-read
//!   gate the runtime layer uses (regular file, size capped) before the path is
//!   opened.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Subcommand;

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

const PROVIDERS_AFTER_HELP: &str = "\
Examples:
  # Write the shipped presets into ~/.grok/providers.toml (never config.toml)
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
    /// Install or update the shipped provider presets in providers.toml
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
    /// Sign in to a provider (not yet available in this build)
    Login(Phase2Args),
    /// Print a provider access token (not yet available in this build)
    Token(Phase2Args),
}

#[derive(Debug, clap::Args, Clone)]
pub struct Phase2Args {
    #[command(subcommand)]
    pub provider: Phase2Provider,
}

/// Providers the Phase-2 auth commands will support. Parsed today so the CLI
/// surface does not change when the implementation lands.
#[derive(Debug, Subcommand, Clone, Copy, PartialEq, Eq)]
pub enum Phase2Provider {
    /// OpenAI (ChatGPT/Codex plan credentials)
    Openai,
}

impl Phase2Provider {
    fn id(self) -> &'static str {
        match self {
            Phase2Provider::Openai => "openai",
        }
    }
}

/// Exit code for a subcommand that parses but is not implemented yet.
pub const PHASE2_EXIT_CODE: i32 = 2;

pub fn run(args: ProvidersArgs) -> Result<()> {
    let home = xai_grok_config::grok_home();
    match args.command {
        ProvidersCommand::Install { force } => run_install(&home, force),
        ProvidersCommand::SetKey { provider } => run_set_key(&home, &provider),
        ProvidersCommand::UnsetKey { provider } => run_unset_key(&home, &provider),
        ProvidersCommand::Status => run_status(&home),
        ProvidersCommand::Login(args) => phase2_unavailable("login", args.provider),
        ProvidersCommand::Token(args) => phase2_unavailable("token", args.provider),
    }
}

/// The message a Phase-2 placeholder prints before exiting [`PHASE2_EXIT_CODE`].
fn phase2_unavailable_message(command: &str, provider: Phase2Provider) -> String {
    format!(
        "gx providers {command} {}: not yet available in this build",
        provider.id()
    )
}

fn phase2_unavailable(command: &str, provider: Phase2Provider) -> ! {
    eprintln!("{}", phase2_unavailable_message(command, provider));
    eprintln!(
        "  This lands with the gx OpenAI phase. Until then, sign in with the \
         codex CLI (`codex login`) and check `gx providers status`."
    );
    std::process::exit(PHASE2_EXIT_CODE)
}

/// Sessions read `providers.toml` once at startup; there is no hot-reload
/// (documented on the providers layer), so every mutating command says so.
const RESTART_NOTICE: &str = "Restart any running gx sessions to pick up provider changes (providers.toml \
     is read at startup; there is no hot-reload).";

/// A stock build compiled from this source would write a file nothing reads.
fn warn_if_not_gx() {
    if !xai_grok_version::is_gx_build() {
        eprintln!(
            "warning: this is not a gx build; only gx reads providers.toml, so these \
             entries will have no effect on this binary."
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

/// A scalar a preset can ship. Deliberately small — everything a
/// `[model_providers.*]` / `[model.*]` entry needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PresetValue {
    Str(&'static str),
    Int(i64),
    Bool(bool),
    StrList(&'static [&'static str]),
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

const ALSO_WORKS_ON_STOCK: &str = "this shape also works in config.toml on stock grok; gx installs it to \
     providers.toml so the shared config.toml stays untouched.";

// -- GLM (Z.AI coding plan) --------------------------------------------------

const ZAI_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://api.z.ai/api/coding/paas/v4")]),
    PresetField::new("api_backend", &[s("chat_completions")]),
    PresetField::new("env_key", &[l(&["ZHIPU_API_KEY", "ZAI_API_KEY"])]),
];

const GLM_53_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("glm-5.3")]),
    PresetField::new("name", &[s("GLM 5.3 (Z.AI)")]),
    PresetField::new(
        "description",
        &[s("Z.AI flagship coding model. Thinking is always on.")],
    ),
    PresetField::new("model_provider", &[s("zai-coding-plan")]),
    PresetField::new("context_window", &[i(1_000_000)]),
    PresetField::new("max_completion_tokens", &[i(131_072)]),
    PresetField::new("supports_reasoning_effort", &[b(true)]),
    PresetField::new("reasoning_effort", &[s("max")]),
    PresetField::new("reasoning_efforts", &[l(&["low", "high", "max"])]),
    PresetField::new("system_prompt_label", &[s("GLM 5.3")]),
];

// -- OpenRouter --------------------------------------------------------------

const OPENROUTER_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://openrouter.ai/api/v1")]),
    PresetField::new("api_backend", &[s("chat_completions")]),
    PresetField::new("env_key", &[s("OPENROUTER_API_KEY")]),
];

const OX_ALPHA_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("stealth/ox-alpha")]),
    PresetField::new("name", &[s("Ox Alpha (OpenRouter)")]),
    PresetField::new(
        "description",
        &[s("Stealth reasoning model via OpenRouter.")],
    ),
    PresetField::new("model_provider", &[s("openrouter")]),
    PresetField::new("context_window", &[i(200_000)]),
    PresetField::new("stream_tool_calls", &[b(false)]),
];

// -- Fireworks ---------------------------------------------------------------
//
// Every Fireworks model carries `stream_tool_calls = false` and an explicit
// `context_window` (grok has no catalog entry for third-party ids).
//
// NOTE: no `reasoning_effort` / `supports_reasoning_effort` / `reasoning_efforts`
// here on purpose. Whether these models accept grok's reasoning-effort wire
// fields is a live-probe question; adding a guessed default now would make the
// probe's answer a "user-modified value" for anyone who installed early. Add
// the fields (as new `defaults[0]` entries) once the probe lands.

const FIREWORKS_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://api.fireworks.ai/inference/v1")]),
    PresetField::new("api_backend", &[s("chat_completions")]),
    PresetField::new("env_key", &[s("FIREWORKS_API_KEY")]),
];

// Every Fireworks model preset shares these two fields; factored out so the
// per-model arrays below only spell out what actually varies.
const FIREWORKS_MODEL_PROVIDER: PresetField = PresetField::new("model_provider", &[s("fireworks")]);
const FIREWORKS_NO_STREAM_TOOL_CALLS: PresetField =
    PresetField::new("stream_tool_calls", &[b(false)]);

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
];

// -- OpenAI (Phase 2 skeletons; NOT installed by this build) -----------------
//
// Shapes are placeholders until the ChatGPT request-contract spike fixes them.
// `openai-codex` deliberately carries NO `env_key`: a static key/env key beats
// the auth-provider token in credential resolution, which would shadow the
// codex credentials this preset exists to use.

const OPENAI_CODEX_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://chatgpt.com/backend-api/codex")]),
    PresetField::new("api_backend", &[s("responses")]),
];

const OPENAI_API_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://api.openai.com/v1")]),
    PresetField::new("api_backend", &[s("responses")]),
    PresetField::new("env_key", &[s("OPENAI_API_KEY")]),
];

/// The shipped catalog. Order is the order `install` writes and `status` prints.
pub(crate) const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "zai-coding-plan",
        label: "GLM (Z.AI coding plan)",
        install: true,
        note: Some(ALSO_WORKS_ON_STOCK),
        fields: ZAI_PROVIDER_FIELDS,
        models: &[ModelPreset {
            id: "glm-5.3",
            fields: GLM_53_FIELDS,
        }],
    },
    ProviderPreset {
        id: "openrouter",
        label: "OpenRouter",
        install: true,
        note: Some(ALSO_WORKS_ON_STOCK),
        fields: OPENROUTER_PROVIDER_FIELDS,
        models: &[ModelPreset {
            id: "openrouter/ox-alpha",
            fields: OX_ALPHA_FIELDS,
        }],
    },
    ProviderPreset {
        id: "fireworks",
        label: "Fireworks",
        install: true,
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
        install: false,
        note: None,
        fields: OPENAI_CODEX_PROVIDER_FIELDS,
        models: &[],
    },
    ProviderPreset {
        id: "openai-api",
        label: "OpenAI (API key)",
        install: false,
        note: None,
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

fn to_edit_value(v: &PresetValue) -> toml_edit::Value {
    match v {
        PresetValue::Str(s) => toml_edit::Value::from(*s),
        PresetValue::Int(n) => toml_edit::Value::from(*n),
        PresetValue::Bool(b) => toml_edit::Value::from(*b),
        PresetValue::StrList(items) => {
            let mut array = toml_edit::Array::new();
            for item in *items {
                array.push(*item);
            }
            toml_edit::Value::Array(array)
        }
    }
}

fn edit_item_matches(item: &toml_edit::Item, v: &PresetValue) -> bool {
    match v {
        PresetValue::Str(s) => item.as_str() == Some(*s),
        PresetValue::Int(n) => item.as_integer() == Some(*n),
        PresetValue::Bool(b) => item.as_bool() == Some(*b),
        PresetValue::StrList(items) => item.as_array().is_some_and(|a| {
            a.len() == items.len()
                && a.iter()
                    .zip(items.iter())
                    .all(|(got, want)| got.as_str() == Some(*want))
        }),
    }
}

fn toml_value_matches(value: &toml::Value, v: &PresetValue) -> bool {
    match v {
        PresetValue::Str(s) => value.as_str() == Some(*s),
        PresetValue::Int(n) => value.as_integer() == Some(*n),
        PresetValue::Bool(b) => value.as_bool() == Some(*b),
        PresetValue::StrList(items) => value.as_array().is_some_and(|a| {
            a.len() == items.len()
                && a.iter()
                    .zip(items.iter())
                    .all(|(got, want)| got.as_str() == Some(*want))
        }),
    }
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
    /// User-modified fields left as they are.
    pub kept_fields: Vec<String>,
    /// User-modified fields overwritten because of `--force`.
    pub forced_fields: Vec<String>,
    /// Entries not written because config.toml already has an identical one.
    pub skipped_identical_in_config: Vec<String>,
    /// Entries present in both files whose **full** tables differ.
    pub shadows_config: Vec<ShadowConflict>,
    /// Whether the rendered document differs from what was on disk.
    pub changed: bool,
    /// The over-wide mode `install` found on providers.toml and clamped back
    /// to 0600. `Some` is a warning: the keys in it were readable by others.
    pub reclamped_from: Option<u32>,
    /// Rendered size, when it exceeds the runtime cap and the whole layer will
    /// therefore be skipped at startup.
    pub exceeds_runtime_cap: Option<u64>,
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
            .is_some_and(|v| toml_value_matches(v, f.current()))
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
            format!("`{parent}` in providers.toml is not a table; refusing to modify it")
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
                "`{}` in providers.toml is not a table; refusing to modify it",
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
    report: &mut InstallReport,
) {
    for field in fields {
        let current = field.current();
        match table.get(field.key) {
            None => {
                set_value_preserving_decor(table, field.key, to_edit_value(current));
                if !entry_created {
                    report.added_fields.push(format!("{path}.{}", field.key));
                }
            }
            Some(item) if edit_item_matches(item, current) => {}
            Some(item)
                if field.defaults[1..]
                    .iter()
                    .any(|older| edit_item_matches(item, older)) =>
            {
                set_value_preserving_decor(table, field.key, to_edit_value(current));
                report.upgraded_fields.push(format!("{path}.{}", field.key));
            }
            Some(_) if force => {
                set_value_preserving_decor(table, field.key, to_edit_value(current));
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
    config: Option<&toml::Value>,
    parent: &str,
    child: &str,
    fields: &[PresetField],
    force: bool,
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
            bail!("`{parent}` in providers.toml is not a table; refusing to modify it");
        }
        if let Some(existing) = parent_item.get(child)
            && !existing.is_table()
        {
            bail!(
                "`{path}` in providers.toml is not a table; refusing to modify it \
                 (an inline table must be rewritten as a `[{path}]` section)"
            );
        }
    }

    let state = config_state(config, parent, child, fields);
    let present = doc
        .get(parent)
        .and_then(|t| t.get(child))
        .is_some_and(toml_edit::Item::is_table);

    // config.toml already carries exactly this entry: writing a byte-identical
    // copy into providers.toml would only add a shadow to reason about.
    if !present && state == ConfigState::Identical {
        report.skipped_identical_in_config.push(path);
        return Ok(());
    }

    let (table, created) = entry_table(doc, parent, child)?;
    apply_fields(table, fields, &path, created, force, report);
    if created {
        report.added_entries.push(path);
    }
    Ok(())
}

/// Merge `presets` into `doc`. Pure: no I/O, so every merge rule is unit
/// testable against a synthetic preset table.
pub(crate) fn apply_presets(
    doc: &mut toml_edit::DocumentMut,
    config: Option<&toml::Value>,
    presets: &[ProviderPreset],
    force: bool,
) -> Result<InstallReport> {
    let mut report = InstallReport::default();
    for preset in presets.iter().filter(|p| p.install) {
        apply_entry(
            doc,
            config,
            "model_providers",
            preset.id,
            preset.fields,
            force,
            &mut report,
        )?;
        for model in preset.models {
            apply_entry(
                doc,
                config,
                "model",
                model.id,
                model.fields,
                force,
                &mut report,
            )?;
        }
    }
    // Shadow detection runs over the *finished* document so it can compare the
    // full tables, catching conflicts in fields no preset ships.
    for preset in presets.iter().filter(|p| p.install) {
        report
            .shadows_config
            .extend(shadow_conflict(doc, config, "model_providers", preset.id));
        for model in preset.models {
            report
                .shadows_config
                .extend(shadow_conflict(doc, config, "model", model.id));
        }
    }
    Ok(report)
}

/// Path-injectable core of `gx providers install`.
pub(crate) fn install_at(
    home: &Path,
    presets: &[ProviderPreset],
    force: bool,
) -> Result<InstallReport> {
    let path = providers_path(home);
    // Everything below is a read-modify-write; hold the cross-process lock for
    // all of it so a concurrent `set-key` cannot be rendered away.
    let _lock = lock_providers(home)?;
    let previous = read_existing(&path)?;
    let mut doc = parse_document(&previous, &path)?;
    let config = read_config_for_install(home)?;
    let mut report = apply_presets(&mut doc, config.as_ref(), presets, force)?;

    let mut rendered = doc.to_string();
    if previous.trim().is_empty() {
        rendered = format!("{NEW_FILE_HEADER}{rendered}");
    }
    if rendered.len() as u64 > MAX_PROVIDERS_BYTES {
        report.exceeds_runtime_cap = Some(rendered.len() as u64);
    }
    let outcome = write_providers_toml(&path, &rendered, &previous)?;
    report.changed = outcome.changed;
    report.reclamped_from = outcome.reclamped_from;
    Ok(report)
}

fn run_install(home: &Path, force: bool) -> Result<()> {
    warn_if_not_gx();
    let path = providers_path(home);
    let report = install_at(home, PRESETS, force)?;

    println!("{}", path.display());
    if report.added_entries.is_empty()
        && report.added_fields.is_empty()
        && report.upgraded_fields.is_empty()
        && report.forced_fields.is_empty()
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
    for field in &report.forced_fields {
        println!("  forced    {field} (--force replaced your value)");
    }
    for field in &report.kept_fields {
        println!("  kept      {field} (edited by you; --force overrides)");
    }
    for entry in &report.skipped_identical_in_config {
        println!("  skipped   {entry} — config.toml already has an identical entry");
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
    warn_if_reclamped(&path, report.reclamped_from);
    if let Some(len) = report.exceeds_runtime_cap {
        eprintln!(
            "warning: {} is {len} bytes, over the {MAX_PROVIDERS_BYTES}-byte runtime cap — \
             gx will SKIP the entire providers layer at startup until it is trimmed.",
            path.display()
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
    if report.changed {
        println!();
        println!("{RESTART_NOTICE}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// set-key / unset-key
// ---------------------------------------------------------------------------

/// Path-injectable core of `gx providers set-key`. `key` is never logged.
pub(crate) fn set_key_at(home: &Path, provider: &str, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("empty key: nothing written");
    }
    let path = providers_path(home);
    let _lock = lock_providers(home)?;
    let previous = read_existing(&path)?;
    let mut doc = parse_document(&previous, &path)?;

    let known_here = doc
        .get("model_providers")
        .and_then(|t| t.get(provider))
        .is_some_and(toml_edit::Item::is_table);
    let preset = preset_for(provider);
    if !known_here {
        match preset {
            // A Phase-2 skeleton: writing a lone api_key under an entry this
            // build never installs would leave a half-configured provider.
            Some(preset) if !preset.install => bail!(
                "`{provider}` is not available in this build yet; nothing written. \
                 Run `gx providers status` to see what is configured."
            ),
            // A preset the user has not installed yet: install just that one so
            // the key lands on a complete, usable entry.
            Some(preset) => {
                apply_presets(
                    &mut doc,
                    read_config_for_install(home)?.as_ref(),
                    std::slice::from_ref(preset),
                    false,
                )?;
            }
            None => bail!(
                "unknown provider `{provider}`: not in providers.toml and not a shipped \
                 preset. Known presets: {}",
                PRESETS.iter().map(|p| p.id).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    let (table, _) = entry_table(&mut doc, "model_providers", provider)?;
    // Decor-preserving: a hand-written `api_key = "old" # vault-managed` keeps
    // its comment (and any comment lines above it) across a rotation.
    set_value_preserving_decor(table, "api_key", toml_edit::Value::from(key));

    let mut rendered = doc.to_string();
    if previous.trim().is_empty() {
        rendered = format!("{NEW_FILE_HEADER}{rendered}");
    }
    let outcome = write_providers_toml(&path, &rendered, &previous)?;
    warn_if_reclamped(&path, outcome.reclamped_from);
    Ok(())
}

/// Path-injectable core of `gx providers unset-key`. `Ok(false)` means there
/// was no key to remove.
pub(crate) fn unset_key_at(home: &Path, provider: &str) -> Result<bool> {
    let path = providers_path(home);
    let _lock = lock_providers(home)?;
    let previous = read_existing(&path)?;
    if previous.is_empty() && !path.exists() {
        bail!("no providers.toml at {} — nothing to unset", path.display());
    }
    let mut doc = parse_document(&previous, &path)?;
    let Some(table) = doc
        .get_mut("model_providers")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|t| t.get_mut(provider))
        .and_then(toml_edit::Item::as_table_mut)
    else {
        bail!("provider `{provider}` is not defined in {}", path.display());
    };
    let removed = table.remove("api_key").is_some();
    if removed {
        let outcome = write_providers_toml(&path, &doc.to_string(), &previous)?;
        warn_if_reclamped(&path, outcome.reclamped_from);
    }
    Ok(removed)
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
    set_key_at(home, provider, &key)?;
    println!(
        "stored api_key for `{provider}` in {} ({shown})",
        providers_path(home).display()
    );
    println!("{RESTART_NOTICE}");
    Ok(())
}

fn run_unset_key(home: &Path, provider: &str) -> Result<()> {
    warn_if_not_gx();
    if unset_key_at(home, provider)? {
        println!(
            "removed api_key for `{provider}` from {}",
            providers_path(home).display()
        );
        println!("`{provider}` now resolves through its env_key, if one is set.");
        println!("{RESTART_NOTICE}");
    } else {
        println!("`{provider}` has no api_key in providers.toml; nothing to remove.");
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
    StatusReport {
        providers_path: path,
        providers_present,
        providers_error,
        config_error,
        providers,
        codex: codex_auth_path.map(read_codex_auth),
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
    /// Account id, redacted to its last 6 characters.
    pub account_id_redacted: Option<String>,
    /// `chatgpt_plan_type`, when the token carries one.
    pub plan: Option<String>,
    pub has_api_key: bool,
    pub has_refresh_token: bool,
}

/// `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`.
fn codex_auth_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home).join("auth.json"));
    }
    dirs::home_dir().map(|h| h.join(".codex").join("auth.json"))
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
pub(crate) fn read_codex_auth(path: &Path) -> CodexAuthStatus {
    let mut status = CodexAuthStatus {
        path: path.to_path_buf(),
        ..Default::default()
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return status,
        Err(e) => {
            status.error = Some(e.to_string());
            return status;
        }
    };
    status.present = true;
    let json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            status.error = Some(e.to_string());
            return status;
        }
    };
    status.has_api_key = json
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    let tokens = json.get("tokens");
    status.has_refresh_token = tokens
        .and_then(|t| t.get("refresh_token"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());

    let mut account_id = tokens
        .and_then(|t| t.get("account_id"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    if let Some(access) = tokens
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        && let Some(claims) = decode_jwt_claims_unverified(access)
    {
        status.access_token_exp = claims.get("exp").and_then(serde_json::Value::as_i64);
        let auth = claims.get("https://api.openai.com/auth");
        if account_id.is_none() {
            account_id = auth
                .and_then(|a| a.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
        status.plan = auth
            .and_then(|a| a.get("chatgpt_plan_type"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
    }
    status.account_id_redacted = account_id.as_deref().map(|id| redact_tail(id, 6));
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
    if let Some(account) = &codex.account_id_redacted {
        out.push_str(&format!("  account      {account}\n"));
    }
    if let Some(plan) = &codex.plan {
        out.push_str(&format!("  plan         {plan}\n"));
    }
    if codex.has_api_key {
        out.push_str("  OPENAI_API_KEY present in auth.json (api-key mode)\n");
    }
    if !codex.has_refresh_token {
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

/// Cap on `providers.toml`, mirroring the runtime layer's own
/// `MAX_PROVIDERS_LAYER_BYTES`. A larger file is skipped wholesale at startup,
/// so reading (or growing past) one here would be pointless as well as unsafe.
pub(crate) const MAX_PROVIDERS_BYTES: u64 = 1024 * 1024;

/// Cap on `config.toml`, which this module only ever **reads**. Roomier than
/// the providers cap — config.toml is stock grok's file and may legitimately be
/// large — but still bounded, so a runaway file or a device node dropped in its
/// place can never be read into memory here.
pub(crate) const MAX_CONFIG_BYTES: u64 = 10 * 1024 * 1024;

/// Pre-read gate, the same shape [`xai_grok_config::providers_layer`] applies
/// at startup: one [`std::fs::metadata`] call **before** the path is opened, so
/// a fifo can never block and `/dev/zero` can never be slurped.
///
/// `Ok(false)` = absent, treat as empty. `Ok(true)` = safe to read. `Err` =
/// refuse. The message carries only the path and sizes, never file content.
fn gate_file(path: &Path, max_bytes: u64, what: &str) -> Result<bool> {
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

fn read_existing(path: &Path) -> Result<String> {
    if !gate_file(path, MAX_PROVIDERS_BYTES, "providers.toml")? {
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

/// `config.toml` is read, never written.
///
/// Gate failures **abort**: config.toml decides what `install` skips and what
/// it flags as shadowed, so silently treating an unreadable or oversized one as
/// absent would change what gets written. Parse failures stay tolerated (the
/// file belongs to stock grok too) and are never rendered.
fn read_config_for_install(home: &Path) -> Result<Option<toml::Value>> {
    let path = home.join("config.toml");
    if !gate_file(&path, MAX_CONFIG_BYTES, "config.toml")? {
        return Ok(None);
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    Ok(toml::from_str::<toml::Value>(&raw).ok())
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

/// The result of one [`write_providers_toml`] call.
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
fn write_providers_toml(path: &Path, rendered: &str, previous: &str) -> Result<WriteOutcome> {
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
