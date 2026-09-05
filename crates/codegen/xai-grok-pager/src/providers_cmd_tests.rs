//! gx: tests for `gx providers` (see `providers_cmd.rs`).
//!
//! Everything runs against a temp `$GROK_HOME`; nothing here reads or writes
//! the real `~/.grok` or `~/.codex`.

use super::*;
use crate::app::{Command, PagerArgs};
use clap::Parser as _;
use std::fs;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn home() -> TempDir {
    tempfile::tempdir().expect("temp GROK_HOME")
}

fn providers_body(home: &Path) -> String {
    fs::read_to_string(providers_path(home)).expect("providers.toml exists")
}

fn parse_providers(home: &Path) -> toml::Value {
    toml::from_str(&providers_body(home)).expect("providers.toml is valid TOML")
}

fn config_toml(home: &Path) -> PathBuf {
    home.join("config.toml")
}

fn config_body(home: &Path) -> String {
    fs::read_to_string(config_toml(home)).expect("config.toml exists")
}

fn parse_config(home: &Path) -> toml::Value {
    toml::from_str(&config_body(home)).expect("config.toml is valid TOML")
}

fn gx_only_presets() -> Vec<ProviderPreset> {
    PRESETS
        .iter()
        .copied()
        .filter(|p| !p.stock_compatible)
        .collect()
}

fn stock_presets() -> Vec<ProviderPreset> {
    PRESETS
        .iter()
        .copied()
        .filter(|p| p.stock_compatible)
        .collect()
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// A fixed install-time context. Tests must never resolve this machine's own
/// binary path or read the real `~/.codex/auth.json`, so every `install_at`
/// call takes this instead of `PresetContext::detect()`.
fn ctx() -> PresetContext {
    PresetContext::fixed(GX_BIN, Some(FIXTURE_ACCOUNT))
}

const GX_BIN: &str = "/opt/gx/bin/gx";
const FIXTURE_ACCOUNT: &str = "acct-fixture-abc123";

/// A synthetic two-generation preset table that exercises the generic shipped
/// default upgrade rule independently of any real provider's catalog history.
const OLDER_CTX: i64 = 100_000;
const CURRENT_CTX: i64 = 200_000;

const SYNTH_PROVIDER_FIELDS: &[PresetField] = &[
    PresetField::new("base_url", &[s("https://new.example.test/v1")]),
    PresetField::new("api_backend", &[s("chat_completions")]),
    PresetField::new("env_key", &[s("SYNTH_API_KEY")]),
];

const SYNTH_MODEL_FIELDS: &[PresetField] = &[
    PresetField::new("model", &[s("synth-wire-id")]),
    PresetField::new("model_provider", &[s("synth")]),
    // Two generations: `defaults[0]` is current, `defaults[1]` is what an
    // earlier gx shipped and may still be upgraded in place.
    PresetField::new("context_window", &[i(CURRENT_CTX), i(OLDER_CTX)]),
    PresetField::new("stream_tool_calls", &[b(false)]),
];

const SYNTH_PRESETS: &[ProviderPreset] = &[ProviderPreset {
    id: "synth",
    label: "Synthetic",
    install: true,
    rejects_static_key: false,
    stock_compatible: false,
    note: None,
    fields: SYNTH_PROVIDER_FIELDS,
    models: &[ModelPreset {
        id: "synth/model.v1",
        fields: SYNTH_MODEL_FIELDS,
    }],
}];

// ---------------------------------------------------------------------------
// install: shape, permissions, idempotency
// ---------------------------------------------------------------------------

#[test]
fn install_writes_every_installable_preset_and_no_api_keys() {
    let dir = home();
    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let providers = providers_body(dir.path());
    let config = config_body(dir.path());

    for preset in PRESETS.iter().filter(|p| p.install) {
        let body = if preset.stock_compatible {
            &config
        } else {
            &providers
        };
        let other = if preset.stock_compatible {
            &providers
        } else {
            &config
        };
        let header = format!("[{}]", quoted_path("model_providers", preset.id));
        assert!(
            body.contains(&header),
            "missing provider {} in the target file:\n{body}",
            preset.id
        );
        assert!(
            !other.contains(&header),
            "stock-compatible and gx-only entries must not be written to both files; {} leaked into the other file:\n{other}",
            preset.id
        );
        for model in preset.models {
            let model_header = format!("[{}]", quoted_path("model", model.id));
            assert!(
                body.contains(&model_header),
                "missing model {} in the target file:\n{body}",
                model.id
            );
            assert!(
                !other.contains(&model_header),
                "model {} leaked into the other file:\n{other}",
                model.id
            );
        }
    }
    // Presets carry env_key, never api_key (the header comment mentions the
    // key by name, so check the parsed tables rather than the raw text).
    for (label, parsed) in [
        ("providers.toml", parse_providers(dir.path())),
        ("config.toml", parse_config(dir.path())),
    ] {
        if let Some(table) = parsed.get("model_providers").and_then(|t| t.as_table()) {
            for (id, entry) in table {
                assert!(
                    entry.get("api_key").is_none(),
                    "install must never write key material, found one on {id} in {label}"
                );
            }
        }
    }
    // Every shipped preset installs in this build, OpenAI included.
    assert!(PRESETS.iter().all(|p| p.install));
    assert!(
        providers.contains("[model_providers.openai-codex]"),
        "{providers}"
    );
    assert!(
        providers.contains("[model_providers.openai-api]"),
        "{providers}"
    );
    assert!(config.contains("[model_providers.meta]"), "{config}");
    // `openai-api` deliberately ships no models: which OpenAI models a key can
    // reach is account-specific, so a shipped catalog would only go stale.
    assert!(PRESETS
        .iter()
        .find(|p| p.id == "openai-api")
        .expect("openai-api preset")
        .models
        .is_empty());

    assert!(report.changed);
    assert!(report.config_changed);
    assert!(report.providers_changed);
    assert!(report
        .added_entries
        .contains(&"model_providers.fireworks".to_owned()));
    assert!(report
        .added_entries
        .contains(&"model.\"glm-5.3\"".to_owned()));
    assert!(report
        .added_entries
        .contains(&"model.\"glm-5.3-flash\"".to_owned()));
    assert!(report
        .added_entries
        .contains(&"model.\"muse-spark-1.3\"".to_owned()));
    assert!(report.kept_fields.is_empty());
    assert!(report.upgraded_fields.is_empty());
}

#[test]
fn install_mirrors_the_live_glm_openrouter_and_fireworks_shapes() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    // GLM / OpenRouter / Meta are stock-compatible and land in config.toml;
    // Fireworks stays gx-only in providers.toml.
    let parsed = parse_config(dir.path());
    let fireworks_doc = parse_providers(dir.path());

    let glm = &parsed["model"]["glm-5.3"];
    assert_eq!(glm["model"].as_str(), Some("glm-5.3"));
    assert_eq!(glm["model_provider"].as_str(), Some("zai-coding-plan"));
    assert_eq!(glm["context_window"].as_integer(), Some(1_000_000));
    assert_eq!(glm["max_completion_tokens"].as_integer(), Some(131_072));
    assert_eq!(glm["supports_reasoning_effort"].as_bool(), Some(true));
    assert_eq!(glm["reasoning_effort"].as_str(), Some("max"));
    assert_eq!(
        glm["reasoning_efforts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        vec!["low", "high", "max"]
    );

    let glm_flash_zai = &parsed["model"]["glm-5.3-flash"];
    assert_eq!(glm_flash_zai["model"].as_str(), Some("glm-5.3-flash"));
    assert_eq!(
        glm_flash_zai["model_provider"].as_str(),
        Some("zai-coding-plan")
    );
    assert_eq!(
        glm_flash_zai["context_window"].as_integer(),
        Some(1_000_000)
    );
    assert_eq!(
        glm_flash_zai["max_completion_tokens"].as_integer(),
        Some(131_072)
    );
    assert_eq!(
        glm_flash_zai["supports_reasoning_effort"].as_bool(),
        Some(true)
    );
    assert_eq!(glm_flash_zai["reasoning_effort"].as_str(), Some("high"));
    assert_eq!(
        glm_flash_zai["reasoning_efforts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        vec!["low", "high", "max"]
    );

    let zai = &parsed["model_providers"]["zai-coding-plan"];
    assert_eq!(
        zai["base_url"].as_str(),
        Some("https://api.z.ai/api/coding/paas/v4")
    );
    assert_eq!(
        zai["env_key"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        vec!["ZHIPU_API_KEY", "ZAI_API_KEY"]
    );

    // `openrouter/glm-5.3-flash` was retired (2026-08-27): the Z.AI
    // coding-plan `glm-5.3-flash` above covers the same model directly, so a
    // metered OpenRouter duplicate would be redundant. `install` never
    // deletes an entry that falls out of the shipped catalog, so an existing
    // install keeps whatever it already wrote for that id -- there is
    // nothing left to assert about it here.
    assert!(parsed["model"].get("openrouter/glm-5.3-flash").is_none());

    let minimax = &parsed["model"]["openrouter/minimax-m3"];
    assert_eq!(minimax["model"].as_str(), Some("minimax/minimax-m3"));
    assert_eq!(minimax["context_window"].as_integer(), Some(1_048_576));
    assert_eq!(minimax["stream_tool_calls"].as_bool(), Some(false));
    // OpenRouter reports no reasoning_effort support for this model.
    assert!(minimax.get("supports_reasoning_effort").is_none());
    assert!(minimax.get("reasoning_effort").is_none());
    assert!(minimax.get("reasoning_efforts").is_none());

    let gemini = &parsed["model"]["openrouter/gemini-3.8-flash"];
    assert_eq!(gemini["model"].as_str(), Some("google/gemini-3.8-flash"));
    assert_eq!(gemini["model_provider"].as_str(), Some("openrouter"));
    assert_eq!(gemini["context_window"].as_integer(), Some(1_048_576));
    assert_eq!(gemini["max_completion_tokens"].as_integer(), Some(65_536));
    assert_eq!(gemini["stream_tool_calls"].as_bool(), Some(false));
    assert_eq!(gemini["supports_reasoning_effort"].as_bool(), Some(true));
    assert_eq!(gemini["reasoning_effort"].as_str(), Some("medium"));
    assert_eq!(
        gemini["reasoning_efforts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        vec!["low", "medium", "high"]
    );

    for id in [
        "openrouter/gpt-5.6-sol",
        "openrouter/gpt-5.6-terra",
        "openrouter/gpt-5.6-luna",
    ] {
        assert!(
            parsed["model"].get(id).is_none(),
            "{id} must not be installed through OpenRouter"
        );
    }

    let meta = &parsed["model_providers"]["meta"];
    assert_eq!(meta["base_url"].as_str(), Some("https://api.meta.ai/v1"));
    assert_eq!(meta["api_backend"].as_str(), Some("chat_completions"));
    assert_eq!(
        meta["env_key"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        vec!["META_API_KEY", "MODEL_API_KEY"]
    );
    assert!(meta.get("api_key").is_none());

    for (id, name, desc) in [
        (
            "muse-spark-1.3",
            "Muse Spark 1.3 (Meta)",
            "Muse Spark 1.3 via Meta Model API. Prompts are not used for training.",
        ),
        (
            "muse-spark-1.3-contributor",
            "Muse Spark 1.3 Contributor (Meta)",
            "Discounted Muse Spark 1.3. Your content, including inter-session messages, may be used for product improvement.",
        ),
    ] {
        let entry = &parsed["model"][id];
        assert_eq!(entry["model"].as_str(), Some(id), "{id}");
        assert_eq!(entry["name"].as_str(), Some(name), "{id}");
        assert_eq!(entry["description"].as_str(), Some(desc), "{id}");
        assert_eq!(entry["model_provider"].as_str(), Some("meta"), "{id}");
        assert_eq!(
            entry["context_window"].as_integer(),
            Some(1_048_576),
            "{id}"
        );
        assert_eq!(
            entry["max_completion_tokens"].as_integer(),
            Some(131_072),
            "{id}"
        );
        assert_eq!(entry["stream_tool_calls"].as_bool(), Some(false), "{id}");
        assert_eq!(
            entry["supports_reasoning_effort"].as_bool(),
            Some(true),
            "{id}"
        );
        assert_eq!(entry["reasoning_effort"].as_str(), Some("high"), "{id}");
        assert_eq!(
            entry["reasoning_efforts"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["minimal", "low", "medium", "high", "xhigh"],
            "{id}"
        );
        assert!(entry.get("model_family").is_none(), "{id}");
        assert!(entry.get("codex_compat").is_none(), "{id}");
        assert!(entry.get("api_key").is_none(), "{id}");
    }

    // Five Fireworks models, every one with an explicit context window, a
    // fully-qualified wire id, and streamed tool calls off.
    let fireworks: Vec<(&String, &toml::Value)> = fireworks_doc["model"]
        .as_table()
        .unwrap()
        .iter()
        .filter(|(_, v)| v.get("model_provider").and_then(toml::Value::as_str) == Some("fireworks"))
        .collect();
    assert_eq!(fireworks.len(), 5, "expected five fireworks models");
    for (id, entry) in &fireworks {
        let wire = entry["model"].as_str().expect("wire model id");
        assert!(
            wire.starts_with("accounts/fireworks/models/"),
            "{id} wire id must be fully qualified, got {wire}"
        );
        assert!(entry["context_window"].as_integer().is_some(), "{id} ctx");
        assert_eq!(entry["stream_tool_calls"].as_bool(), Some(false), "{id}");
        // Live-probed (2026-08-25): Fireworks validates `reasoning_effort`;
        // `adaptive` is accepted by Fireworks but excluded here because
        // grok's `ReasoningEffort` enum cannot express it.
        assert_eq!(
            entry["supports_reasoning_effort"].as_bool(),
            Some(true),
            "{id}"
        );
        assert_eq!(entry["reasoning_effort"].as_str(), Some("high"), "{id}");
        assert_eq!(
            entry["reasoning_efforts"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh", "max"],
            "{id}"
        );
    }
    assert_eq!(
        fireworks_doc["model"]["fireworks/deepseek-v4-flash"]["model"].as_str(),
        Some("accounts/fireworks/models/deepseek-v4-flash-0731")
    );
    assert_eq!(
        fireworks_doc["model"]["fireworks/kimi-k3"]["context_window"].as_integer(),
        Some(1_048_576)
    );
}

#[test]
fn install_preserves_retired_openrouter_gpt_entries() {
    let dir = home();
    fs::write(
        config_toml(dir.path()),
        r#"[model."openrouter/gpt-5.6-sol"]
model = "hand-picked-sol"
model_provider = "openrouter"
my_extra = "keep sol"

[model."openrouter/gpt-5.6-terra"]
model = "hand-picked-terra"
model_provider = "openrouter"
my_extra = "keep terra"

[model."openrouter/gpt-5.6-luna"]
model = "hand-picked-luna"
model_provider = "openrouter"
my_extra = "keep luna"
"#,
    )
    .unwrap();

    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let parsed = parse_config(dir.path());

    for suffix in ["sol", "terra", "luna"] {
        let id = format!("openrouter/gpt-5.6-{suffix}");
        let retired = parsed["model"][&id]
            .as_table()
            .unwrap_or_else(|| panic!("retired model table {id}"));
        assert_eq!(retired.len(), 3, "{id} must not be rewritten");
        assert_eq!(
            retired["model"].as_str(),
            Some(format!("hand-picked-{suffix}").as_str()),
            "{id}"
        );
        assert_eq!(retired["model_provider"].as_str(), Some("openrouter"));
        assert_eq!(
            retired["my_extra"].as_str(),
            Some(format!("keep {suffix}").as_str()),
            "{id}"
        );
    }
}

#[test]
fn install_second_run_is_byte_identical_and_reports_no_change() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("first install");
    let first_providers = providers_body(dir.path());
    let first_config = config_body(dir.path());

    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("second install");
    let second_providers = providers_body(dir.path());
    let second_config = config_body(dir.path());

    assert_eq!(
        first_providers, second_providers,
        "second install must be byte-identical for providers.toml"
    );
    assert_eq!(
        first_config, second_config,
        "second install must be byte-identical for config.toml"
    );
    assert!(!report.changed, "second install must report no change");
    assert!(!report.config_changed);
    assert!(!report.providers_changed);
    assert!(report.added_entries.is_empty());
    assert!(report.added_fields.is_empty());
    assert!(report.upgraded_fields.is_empty());
}

#[cfg(unix)]
#[test]
fn install_writes_providers_toml_0600_and_reclamps_a_loosened_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = home();
    let first = install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let path = providers_path(dir.path());
    let config = config_toml(dir.path());
    assert_eq!(mode_of(&path), 0o600, "providers.toml must be owner-only");
    assert_eq!(mode_of(&config), 0o600, "config.toml must be owner-only");
    assert_eq!(first.reclamped_from, None, "a fresh file is born 0600");
    assert_eq!(first.config_reclamped_from, None);

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("second install");
    assert_eq!(
        mode_of(&path),
        0o600,
        "a no-op install must still clamp the mode back"
    );
    assert_eq!(
        mode_of(&config),
        0o600,
        "config.toml must also be reclamped"
    );
    // The byte-identical early return is exactly the path a repeat install
    // takes; the clamp must happen there AND be reported, not swallowed.
    assert!(!report.changed, "the document is byte-identical");
    assert_eq!(
        report.reclamped_from,
        Some(0o644),
        "a loosened key file must be reported loudly, not clamped in silence"
    );
    assert_eq!(report.config_reclamped_from, Some(0o644));

    // And once it is back at 0600, nothing is reported.
    let quiet = install_at(dir.path(), PRESETS, false, &ctx()).expect("third install");
    assert_eq!(quiet.reclamped_from, None);
    assert_eq!(quiet.config_reclamped_from, None);
}

#[cfg(unix)]
#[test]
fn a_mode_clamp_that_cannot_run_is_an_error_not_a_silent_shrug() {
    // enforce_mode is the last line of defence for a key file; a clamp it
    // cannot even attempt must be an error, never a silent success.
    let dir = home();
    let missing = dir.path().join("does-not-exist.toml");
    let err = enforce_mode(&missing).expect_err("stat must fail");
    assert!(err.to_string().contains("to check its mode"), "got: {err}");
}

#[test]
fn gx_only_install_does_not_create_or_touch_config_toml() {
    let dir = home();
    let config = config_toml(dir.path());
    fs::write(&config, "[ui]\ncompact_mode = true\n").unwrap();
    let before = fs::read_to_string(&config).unwrap();

    install_at(dir.path(), &gx_only_presets(), false, &ctx()).expect("install");
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        before,
        "a gx-only install must leave config.toml untouched"
    );

    // And with no config.toml at all, a gx-only install must not invent one.
    let empty = home();
    install_at(empty.path(), &gx_only_presets(), false, &ctx()).expect("install");
    assert!(!config_toml(empty.path()).exists());
}

#[test]
fn stock_compatible_install_creates_config_toml_and_preserves_unrelated_tables() {
    let dir = home();
    let config = config_toml(dir.path());
    fs::write(
        &config,
        "[ui]\ncompact_mode = true\n\n[cli]\nhide_reasoning = true\n",
    )
    .unwrap();

    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let parsed = parse_config(dir.path());
    assert_eq!(
        parsed["ui"]["compact_mode"].as_bool(),
        Some(true),
        "existing [ui] must survive a stock-compatible install"
    );
    assert_eq!(parsed["cli"]["hide_reasoning"].as_bool(), Some(true));
    assert!(parsed["model_providers"].get("meta").is_some());
    assert!(parsed["model_providers"].get("zai-coding-plan").is_some());
    assert!(parsed["model_providers"].get("openrouter").is_some());
    assert!(parsed["model_providers"].get("fireworks").is_none());

    // And with no config.toml at all, a stock-compatible install creates one.
    let empty = home();
    install_at(empty.path(), &stock_presets(), false, &ctx()).expect("install");
    assert!(config_toml(empty.path()).exists());
    assert!(
        !providers_path(empty.path()).exists(),
        "a stock-only install must not create providers.toml"
    );
    let created = parse_config(empty.path());
    assert!(created["model_providers"].get("meta").is_some());
    assert!(created["model"].get("muse-spark-1.3").is_some());
    assert!(created["model"].get("muse-spark-1.3-contributor").is_some());
}

#[test]
fn install_migrates_stock_compatible_overlay_from_providers_toml_to_config_toml() {
    // Pre-partition gx wrote GLM/OpenRouter into providers.toml. After the
    // split those tables must move to config.toml (with api_key) so the
    // overlay cannot shadow.
    let dir = home();
    fs::write(config_toml(dir.path()), "[ui]\ncompact_mode = true\n").unwrap();
    fs::write(
        providers_path(dir.path()),
        r#"[model_providers.openrouter]
base_url = "https://openrouter.example.test/v1"
api_backend = "chat_completions"
env_key = "OPENROUTER_API_KEY"
api_key = "sk-test-not-real"
extra_headers = { "X-Title" = "hand-edit" }

[model."openrouter/minimax-m3"]
model = "minimax/minimax-m3"
model_provider = "openrouter"
context_window = 999999
stream_tool_calls = false

[model_providers.fireworks]
base_url = "https://api.fireworks.ai/inference/v1"
api_backend = "chat_completions"
env_key = "FIREWORKS_API_KEY"
"#,
    )
    .unwrap();

    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let config = parse_config(dir.path());
    let providers = parse_providers(dir.path());

    assert_eq!(
        config["model_providers"]["openrouter"]["api_key"].as_str(),
        Some("sk-test-not-real"),
        "the overlay api_key must land on config.toml, not be dropped"
    );
    assert_eq!(
        config["model_providers"]["openrouter"]["base_url"].as_str(),
        Some("https://openrouter.example.test/v1"),
        "hand-edited overlay fields other than api_key must survive"
    );
    assert_eq!(
        config["model_providers"]["openrouter"]["extra_headers"]["X-Title"].as_str(),
        Some("hand-edit")
    );
    assert_eq!(
        config["model"]["openrouter/minimax-m3"]["context_window"].as_integer(),
        Some(999999)
    );
    assert_eq!(config["ui"]["compact_mode"].as_bool(), Some(true));
    assert!(
        providers
            .get("model_providers")
            .and_then(|t| t.get("openrouter"))
            .is_none(),
        "openrouter must not remain in providers.toml:\n{}",
        providers_body(dir.path())
    );
    assert!(
        providers
            .get("model")
            .and_then(|t| t.get("openrouter/minimax-m3"))
            .is_none(),
        "openrouter models must not remain in providers.toml"
    );
    assert!(
        providers["model_providers"].get("fireworks").is_some(),
        "gx-only Fireworks must stay in providers.toml"
    );
    assert!(
        report
            .migrated_from_providers
            .contains(&"model_providers.openrouter".to_owned()),
        "{:?}",
        report.migrated_from_providers
    );
    assert!(
        report
            .migrated_from_providers
            .contains(&r#"model."openrouter/minimax-m3""#.to_owned()),
        "{:?}",
        report.migrated_from_providers
    );
    let debug = format!("{report:?}");
    assert!(
        !debug.contains("sk-test-not-real"),
        "migrated api_key leaked into the report: {debug}"
    );
}

#[test]
fn install_does_not_write_providers_toml_when_config_apply_fails() {
    let dir = home();
    fs::write(
        config_toml(dir.path()),
        "model_providers = \"managed elsewhere\"\n",
    )
    .unwrap();

    let err = install_at(dir.path(), PRESETS, false, &ctx()).expect_err("must abort");
    assert!(err.to_string().contains("is not a table"), "got: {err}");
    assert!(
        !providers_path(dir.path()).exists(),
        "a failed config.toml apply must not create providers.toml"
    );

    // An existing providers.toml must stay byte-identical — gx-only apply
    // would have added fields if we wrote before the config.toml failure.
    let dir = home();
    let existing =
        "[model_providers.fireworks]\nbase_url = \"https://api.fireworks.ai/inference/v1\"\n";
    fs::write(providers_path(dir.path()), existing).unwrap();
    fs::write(
        config_toml(dir.path()),
        "model_providers = \"managed elsewhere\"\n",
    )
    .unwrap();
    install_at(dir.path(), PRESETS, false, &ctx()).expect_err("must abort");
    assert_eq!(
        providers_body(dir.path()),
        existing,
        "providers.toml must be unchanged when config.toml apply fails"
    );
}

// ---------------------------------------------------------------------------
// install: preservation and merge semantics
// ---------------------------------------------------------------------------

#[test]
fn install_preserves_comments_manual_entries_and_unknown_fields() {
    let dir = home();
    let path = providers_path(dir.path());
    fs::write(
        &path,
        r#"# my own header comment
# second line

[model_providers.my-own]
base_url = "https://mine.example.test/v1"  # trailing comment
api_backend = "chat_completions"
totally_unknown_field = 42

[model_providers.fireworks]
# I set this by hand
base_url = "https://api.fireworks.ai/inference/v1"
api_key = "sk-do-not-lose-me"
my_extra = { nested = true }

[model."mine/custom"]
model = "custom"
model_provider = "my-own"
"#,
    )
    .unwrap();

    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let body = providers_body(dir.path());

    assert!(body.contains("# my own header comment"), "{body}");
    assert!(body.contains("# second line"), "{body}");
    assert!(body.contains("# trailing comment"), "{body}");
    assert!(body.contains("# I set this by hand"), "{body}");
    assert!(body.contains("totally_unknown_field = 42"), "{body}");
    assert!(body.contains("[model_providers.my-own]"), "{body}");
    assert!(body.contains(r#"[model."mine/custom"]"#), "{body}");
    // A key already in the file survives an install untouched.
    assert!(body.contains(r#"api_key = "sk-do-not-lose-me""#), "{body}");
    // And the preset's missing fields were filled in on the existing entry.
    assert!(body.contains(r#"env_key = "FIREWORKS_API_KEY""#), "{body}");

    // Still idempotent on top of a hand-edited file.
    install_at(dir.path(), PRESETS, false, &ctx()).expect("second install");
    assert_eq!(providers_body(dir.path()), body);
}

#[test]
fn install_upgrades_only_values_equal_to_an_older_shipped_default() {
    let dir = home();
    let path = providers_path(dir.path());
    fs::write(
        &path,
        format!(
            r#"[model_providers.synth]
base_url = "https://old.example.test/v1"

[model."synth/model.v1"]
context_window = {OLDER_CTX}
model = "hand-picked-wire-id"
"#
        ),
    )
    .unwrap();

    let report = install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect("install");
    let parsed = parse_providers(dir.path());

    // Equal to an older shipped default -> upgraded.
    assert_eq!(
        parsed["model"]["synth/model.v1"]["context_window"].as_integer(),
        Some(CURRENT_CTX)
    );
    assert_eq!(
        report.upgraded_fields,
        vec![r#"model."synth/model.v1".context_window"#.to_owned()]
    );

    // Never a shipped default -> user-modified, left alone.
    assert_eq!(
        parsed["model"]["synth/model.v1"]["model"].as_str(),
        Some("hand-picked-wire-id")
    );
    assert_eq!(
        parsed["model_providers"]["synth"]["base_url"].as_str(),
        Some("https://old.example.test/v1")
    );
    assert_eq!(
        report.kept_fields,
        vec![
            "model_providers.synth.base_url".to_owned(),
            r#"model."synth/model.v1".model"#.to_owned(),
        ]
    );

    // Missing fields on an existing entry are reported as added fields, not as
    // a new entry.
    assert!(report.added_entries.is_empty());
    assert!(report
        .added_fields
        .contains(&"model_providers.synth.env_key".to_owned()));
}

#[test]
fn install_upgrades_previous_gpt56_effort_defaults() {
    let dir = home();
    fs::write(
        providers_path(dir.path()),
        r#"[model."gpt-5.6-sol"]
reasoning_effort = "medium"
reasoning_efforts = ["low", "medium", "high", "xhigh"]

[model."gpt-5.6-terra"]
reasoning_effort = "medium"
reasoning_efforts = ["low", "medium", "high", "xhigh"]

[model."gpt-5.6-luna"]
reasoning_effort = "medium"
reasoning_efforts = ["low", "medium", "high", "xhigh"]
"#,
    )
    .unwrap();

    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let parsed = parse_providers(dir.path());
    let current_efforts = vec!["low", "medium", "high", "xhigh", "max"];

    for (id, expected_default) in [
        ("gpt-5.6-sol", "low"),
        ("gpt-5.6-terra", "medium"),
        ("gpt-5.6-luna", "medium"),
    ] {
        let model = &parsed["model"][id];
        assert_eq!(
            model["reasoning_effort"].as_str(),
            Some(expected_default),
            "{id}"
        );
        assert_eq!(
            model["reasoning_efforts"]
                .as_array()
                .expect("efforts")
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>(),
            current_efforts,
            "{id}"
        );
    }
    assert_eq!(
        report.upgraded_fields,
        vec![
            r#"model."gpt-5.6-sol".reasoning_effort"#.to_owned(),
            r#"model."gpt-5.6-sol".reasoning_efforts"#.to_owned(),
            r#"model."gpt-5.6-terra".reasoning_efforts"#.to_owned(),
            r#"model."gpt-5.6-luna".reasoning_efforts"#.to_owned(),
        ]
    );
}

/// Upgrade path for the 2026-08-25 Fireworks reasoning-effort probe: a
/// `providers.toml` written by a build that shipped Fireworks entries
/// *without* `reasoning_effort` / `supports_reasoning_effort` /
/// `reasoning_efforts` must gain exactly those three fields per model on the
/// next `install`, and nothing else in the file should move.
#[test]
fn install_adds_reasoning_effort_fields_to_pre_probe_fireworks_entries() {
    let dir = home();
    let path = providers_path(dir.path());
    fs::write(
        &path,
        r#"[model_providers.fireworks]
base_url = "https://api.fireworks.ai/inference/v1"
api_backend = "chat_completions"
env_key = "FIREWORKS_API_KEY"

[model."fireworks/kimi-k3"]
model = "accounts/fireworks/models/kimi-k3"
name = "Kimi K3 (Fireworks)"
description = "Moonshot Kimi K3 for coding and agentic work, served by Fireworks."
model_provider = "fireworks"
context_window = 1048576
stream_tool_calls = false

[model."fireworks/qwen3p8-max"]
model = "accounts/fireworks/models/qwen3p8-max"
name = "Qwen3.8 Max (Fireworks)"
description = "Qwen3.8 Max for large-context coding work, served by Fireworks."
model_provider = "fireworks"
context_window = 262144
stream_tool_calls = false

[model."fireworks/deepseek-v4-pro"]
model = "accounts/fireworks/models/deepseek-v4-pro"
name = "DeepSeek V4 Pro (Fireworks)"
description = "DeepSeek V4 Pro for deep coding and reasoning work, served by Fireworks."
model_provider = "fireworks"
context_window = 1048576
stream_tool_calls = false

[model."fireworks/kimi-k2p7-code"]
model = "accounts/fireworks/models/kimi-k2p7-code"
name = "Kimi K2.7 Code (Fireworks)"
description = "Moonshot Kimi K2.7 coding model, served by Fireworks."
model_provider = "fireworks"
context_window = 262144
stream_tool_calls = false

[model."fireworks/deepseek-v4-flash"]
model = "accounts/fireworks/models/deepseek-v4-flash-0731"
name = "DeepSeek V4 Flash (Fireworks)"
description = "Fast DeepSeek V4 Flash for high-throughput coding work, served by Fireworks."
model_provider = "fireworks"
context_window = 1048576
stream_tool_calls = false
"#,
    )
    .unwrap();
    let before = fs::read_to_string(&path).unwrap();

    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let parsed = parse_providers(dir.path());

    // No new Fireworks tables — every Fireworks entry already existed. (The
    // fixture omits the other presets on purpose, so `install` does add
    // *those* as fresh entries; that is out of scope for this test.)
    assert!(
        report
            .added_entries
            .iter()
            .all(|e| !e.contains("fireworks")),
        "no fireworks entry should be added: {:?}",
        report.added_entries
    );
    assert!(report.upgraded_fields.is_empty());
    assert!(report.forced_fields.is_empty());
    assert!(
        report.kept_fields.is_empty(),
        "every pre-existing field already equals the shipped default: {:?}",
        report.kept_fields
    );

    let fireworks_ids = [
        "fireworks/kimi-k3",
        "fireworks/qwen3p8-max",
        "fireworks/deepseek-v4-pro",
        "fireworks/kimi-k2p7-code",
        "fireworks/deepseek-v4-flash",
    ];
    for id in fireworks_ids {
        for field in [
            "supports_reasoning_effort",
            "reasoning_effort",
            "reasoning_efforts",
        ] {
            assert!(
                report
                    .added_fields
                    .contains(&format!("model.\"{id}\".{field}")),
                "expected model.\"{id}\".{field} in added_fields: {:?}",
                report.added_fields
            );
        }
        let entry = &parsed["model"][id];
        assert_eq!(
            entry["supports_reasoning_effort"].as_bool(),
            Some(true),
            "{id}"
        );
        assert_eq!(entry["reasoning_effort"].as_str(), Some("high"), "{id}");
        assert_eq!(
            entry["reasoning_efforts"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh", "max"],
            "{id}"
        );
    }
    assert_eq!(
        report.added_fields.len(),
        fireworks_ids.len() * 3,
        "only the three effort fields per model should be added: {:?}",
        report.added_fields
    );

    // Nothing pre-existing moved: every original line is still present
    // verbatim, and the new fields are strictly additive.
    for line in before.lines() {
        assert!(
            providers_body(dir.path()).contains(line),
            "original line dropped: {line}"
        );
    }
}

#[test]
fn install_force_replaces_user_modified_values() {
    let dir = home();
    let path = providers_path(dir.path());
    fs::write(
        &path,
        r#"[model_providers.synth]
base_url = "https://old.example.test/v1"
api_key = "sk-keep-me"

[model."synth/model.v1"]
model = "hand-picked-wire-id"
"#,
    )
    .unwrap();

    let report = install_at(dir.path(), SYNTH_PRESETS, true, &ctx()).expect("forced install");
    let parsed = parse_providers(dir.path());

    assert_eq!(
        parsed["model_providers"]["synth"]["base_url"].as_str(),
        Some("https://new.example.test/v1")
    );
    assert_eq!(
        parsed["model"]["synth/model.v1"]["model"].as_str(),
        Some("synth-wire-id")
    );
    assert!(report.kept_fields.is_empty());
    assert_eq!(
        report.forced_fields,
        vec![
            "model_providers.synth.base_url".to_owned(),
            r#"model."synth/model.v1".model"#.to_owned(),
        ]
    );
    // --force is about shipped fields; it must not touch a stored key.
    assert_eq!(
        parsed["model_providers"]["synth"]["api_key"].as_str(),
        Some("sk-keep-me")
    );
}

#[test]
fn install_skips_entries_config_toml_already_carries_identically() {
    let dir = home();
    fs::write(
        dir.path().join("config.toml"),
        r#"[model_providers.synth]
api_key = "sk-user-key-in-config"
base_url = "https://new.example.test/v1"
api_backend = "chat_completions"
env_key = "SYNTH_API_KEY"
"#,
    )
    .unwrap();

    let report = install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect("install");
    let body = providers_body(dir.path());

    assert_eq!(
        report.skipped_identical_in_config,
        vec!["model_providers.synth".to_owned()]
    );
    assert!(
        !body.contains("[model_providers.synth]"),
        "an identical config.toml entry must not be duplicated:\n{body}"
    );
    assert!(report.shadows_config.is_empty());
    // The model is not in config.toml, so it still installs.
    assert!(body.contains(r#"[model."synth/model.v1"]"#), "{body}");

    // Idempotent: the skip repeats, byte for byte.
    install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect("second install");
    assert_eq!(providers_body(dir.path()), body);
}

#[test]
fn install_warns_when_a_written_entry_shadows_a_different_config_entry() {
    let dir = home();
    fs::write(
        dir.path().join("config.toml"),
        r#"[model_providers.synth]
base_url = "https://stock-grok.example.test/v1"
"#,
    )
    .unwrap();

    let report = install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect("install");
    assert_eq!(
        report.shadows_config,
        vec![ShadowConflict {
            path: "model_providers.synth".to_owned(),
            // config.toml has only `base_url`, and a different one.
            fields: vec![
                "api_backend".to_owned(),
                "base_url".to_owned(),
                "env_key".to_owned(),
            ],
        }],
        "a differing config.toml entry must be reported as shadowed"
    );
    assert!(report.skipped_identical_in_config.is_empty());
    let parsed = parse_providers(dir.path());
    assert_eq!(
        parsed["model_providers"]["synth"]["base_url"].as_str(),
        Some("https://new.example.test/v1"),
        "providers.toml still gets the shipped value; it wins for gx"
    );
    // The warning is stable across runs.
    let again = install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect("second install");
    assert_eq!(again.shadows_config, report.shadows_config);
}

#[test]
fn install_refuses_to_overwrite_a_malformed_providers_toml() {
    let dir = home();
    let path = providers_path(dir.path());
    let junk = "this is not = = toml [[[\n";
    fs::write(&path, junk).unwrap();

    let err = install_at(dir.path(), PRESETS, false, &ctx()).expect_err("must refuse");
    assert!(
        err.to_string().contains("not valid TOML"),
        "unexpected error: {err}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), junk, "file untouched");
}

#[test]
fn install_reports_a_non_table_top_level_key_instead_of_clobbering_it() {
    let dir = home();
    let path = providers_path(dir.path());
    fs::write(&path, "model_providers = \"oops\"\n").unwrap();
    let err = install_at(dir.path(), PRESETS, false, &ctx()).expect_err("must refuse");
    assert!(err.to_string().contains("is not a table"), "got: {err}");
}

#[test]
fn install_aborts_on_a_non_table_entry_even_when_config_toml_has_an_identical_one() {
    // The type check must run BEFORE the identical-in-config short-circuit:
    // otherwise a value gx refuses to touch survives an install that says
    // "skipped — config.toml already has an identical entry" and exits 0.
    let dir = home();
    fs::write(
        dir.path().join("config.toml"),
        r#"[model_providers.synth]
base_url = "https://new.example.test/v1"
api_backend = "chat_completions"
env_key = "SYNTH_API_KEY"
"#,
    )
    .unwrap();
    let path = providers_path(dir.path());
    let junk = "[model_providers]\nsynth = \"not-a-table\"\n";
    fs::write(&path, junk).unwrap();

    let err = install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect_err("must abort");
    let text = format!("{err:#}");
    assert!(text.contains("model_providers.synth"), "got: {text}");
    assert!(text.contains("is not a table"), "got: {text}");
    assert_eq!(fs::read_to_string(&path).unwrap(), junk, "file untouched");
}

#[test]
fn install_aborts_on_an_inline_table_entry_it_would_otherwise_step_over() {
    let dir = home();
    let path = providers_path(dir.path());
    let junk = "[model_providers]\nsynth = { base_url = \"https://x.example.test\" }\n";
    fs::write(&path, junk).unwrap();
    let err = install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect_err("must abort");
    assert!(err.to_string().contains("is not a table"), "got: {err}");
    assert_eq!(fs::read_to_string(&path).unwrap(), junk, "file untouched");
}

#[test]
fn install_warns_on_a_conflict_in_a_field_no_preset_ships() {
    // Both files carry every shipped field identically and differ only in
    // `api_key` — invisible to a shipped-fields-only comparison.
    let dir = home();
    fs::write(
        dir.path().join("config.toml"),
        r#"[model_providers.synth]
base_url = "https://new.example.test/v1"
api_backend = "chat_completions"
env_key = "SYNTH_API_KEY"
api_key = "sk-config-secret-1111"
"#,
    )
    .unwrap();
    fs::write(
        providers_path(dir.path()),
        r#"[model_providers.synth]
base_url = "https://new.example.test/v1"
api_backend = "chat_completions"
env_key = "SYNTH_API_KEY"
api_key = "sk-providers-secret-2222"
"#,
    )
    .unwrap();

    let report = install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect("install");
    assert_eq!(
        report.shadows_config,
        vec![ShadowConflict {
            path: "model_providers.synth".to_owned(),
            fields: vec!["api_key".to_owned()],
        }],
        "a full-table comparison must catch a field no preset ships"
    );
    // The conflict names the field and never the values.
    let debug = format!("{:?}", report.shadows_config);
    assert!(!debug.contains("sk-config-secret"), "{debug}");
    assert!(!debug.contains("sk-providers-secret"), "{debug}");
    assert!(
        !debug.contains("1111") && !debug.contains("2222"),
        "{debug}"
    );
}

#[test]
fn install_reports_no_shadow_when_the_two_full_tables_agree() {
    let dir = home();
    let entry = r#"[model_providers.synth]
base_url = "https://new.example.test/v1"
api_backend = "chat_completions"
env_key = "SYNTH_API_KEY"
api_key = "sk-same-on-both-sides"
"#;
    fs::write(dir.path().join("config.toml"), entry).unwrap();
    fs::write(providers_path(dir.path()), entry).unwrap();
    let report = install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect("install");
    assert!(
        report
            .shadows_config
            .iter()
            .all(|s| s.path != "model_providers.synth"),
        "identical tables are not a conflict: {:?}",
        report.shadows_config
    );
}

// ---------------------------------------------------------------------------
// set-key / unset-key
// ---------------------------------------------------------------------------

#[test]
fn set_key_writes_api_key_and_unset_key_removes_it() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");

    set_key_at(dir.path(), "fireworks", "fw_secret_value_1234").expect("set-key");
    let parsed = parse_providers(dir.path());
    assert_eq!(
        parsed["model_providers"]["fireworks"]["api_key"].as_str(),
        Some("fw_secret_value_1234")
    );
    // env_key stays, so unset-key has something to fall back to.
    assert_eq!(
        parsed["model_providers"]["fireworks"]["env_key"].as_str(),
        Some("FIREWORKS_API_KEY")
    );

    assert!(unset_key_at(dir.path(), "fireworks").expect("unset-key").0);
    let parsed = parse_providers(dir.path());
    assert!(
        parsed["model_providers"]["fireworks"]
            .get("api_key")
            .is_none(),
        "api_key must be gone"
    );
    assert_eq!(
        parsed["model_providers"]["fireworks"]["env_key"].as_str(),
        Some("FIREWORKS_API_KEY"),
        "the provider falls back to env_key"
    );

    // Removing a key that is not there is a no-op, not an error.
    assert!(
        !unset_key_at(dir.path(), "fireworks")
            .expect("second unset-key")
            .0
    );
}

#[cfg(unix)]
#[test]
fn set_key_writes_0600_even_when_it_creates_the_file() {
    let dir = home();
    // No install first: set-key materializes the preset entry it needs.
    set_key_at(dir.path(), "fireworks", "fw_secret_value_1234").expect("set-key");
    let path = providers_path(dir.path());
    assert_eq!(mode_of(&path), 0o600);
    let parsed = parse_providers(dir.path());
    assert_eq!(
        parsed["model_providers"]["fireworks"]["base_url"].as_str(),
        Some("https://api.fireworks.ai/inference/v1"),
        "the preset entry is created alongside the key"
    );
}

#[test]
fn set_key_preserves_comments_and_other_entries() {
    let dir = home();
    let path = providers_path(dir.path());
    fs::write(
        &path,
        r#"# keep me

[model_providers.fireworks]
base_url = "https://api.fireworks.ai/inference/v1"

[model_providers.my-own]
base_url = "https://mine.example.test/v1"
"#,
    )
    .unwrap();

    set_key_at(dir.path(), "fireworks", "fw_secret_value_1234").expect("set-key");
    let body = providers_body(dir.path());
    assert!(body.contains("# keep me"), "{body}");
    assert!(body.contains("[model_providers.my-own]"), "{body}");
    assert!(
        body.contains(r#"api_key = "fw_secret_value_1234""#),
        "{body}"
    );
}

#[test]
fn set_key_rejects_an_empty_key_and_an_unknown_provider() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let before = providers_body(dir.path());

    let err = set_key_at(dir.path(), "fireworks", "   ").expect_err("empty key");
    assert!(err.to_string().contains("empty key"), "got: {err}");

    // A static key on the OAuth provider would *outrank* its auth helper in
    // credential resolution, silently disabling the ChatGPT login.
    let err = set_key_at(dir.path(), "openai-codex", "sk-x").expect_err("oauth provider");
    assert!(
        err.to_string().contains("does not take an API key"),
        "got: {err}"
    );
    assert!(!err.to_string().contains("sk-x"), "got: {err}");

    let err = set_key_at(dir.path(), "not-a-provider", "sk-x").expect_err("unknown provider");
    assert!(err.to_string().contains("unknown provider"), "got: {err}");
    assert!(
        !err.to_string().contains("sk-x"),
        "errors must never carry key material: {err}"
    );

    assert_eq!(providers_body(dir.path()), before, "file untouched");
}

#[test]
fn unset_key_on_an_undefined_provider_is_an_error_not_a_write() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let before = providers_body(dir.path());
    let err = unset_key_at(dir.path(), "not-a-provider").expect_err("unknown provider");
    assert!(err.to_string().contains("not defined"), "got: {err}");
    assert_eq!(providers_body(dir.path()), before);
}

#[test]
fn read_secret_from_pipe_takes_the_first_line_trimmed() {
    // The piped path stdin takes when `gx providers set-key` is not on a TTY.
    let key = read_secret_from_pipe(std::io::Cursor::new(b"fw_piped_key_5678\n".to_vec()))
        .expect("piped key");
    assert_eq!(key, "fw_piped_key_5678");

    let key = read_secret_from_pipe(std::io::Cursor::new(
        b"  fw_padded_key  \nignored second line\n".to_vec(),
    ))
    .expect("piped key");
    assert_eq!(key, "fw_padded_key");

    let err = read_secret_from_pipe(std::io::Cursor::new(Vec::new())).expect_err("empty stdin");
    assert!(err.to_string().contains("no key on stdin"), "got: {err}");
}

#[test]
fn set_key_keeps_the_comments_attached_to_the_value_it_replaces() {
    let dir = home();
    fs::write(
        providers_path(dir.path()),
        r#"[model_providers.fireworks]
base_url = "https://api.fireworks.ai/inference/v1"
# rotated quarterly
api_key = "old" # vault-managed
"#,
    )
    .unwrap();

    set_key_at(dir.path(), "fireworks", "fw_new_secret_4321").expect("set-key");
    let body = providers_body(dir.path());
    assert!(
        body.contains("# vault-managed"),
        "the trailing comment on the value was lost:\n{body}"
    );
    assert!(
        body.contains("# rotated quarterly"),
        "the comment above the key was lost:\n{body}"
    );
    assert!(body.contains(r#"api_key = "fw_new_secret_4321""#), "{body}");
    assert!(
        !body.contains(r#""old""#),
        "the old value survived:\n{body}"
    );
}

#[test]
fn install_keeps_comments_when_upgrading_and_when_forcing_a_field() {
    let dir = home();
    fs::write(
        providers_path(dir.path()),
        format!(
            r#"[model."synth/model.v1"]
# sized for the old plan
context_window = {OLDER_CTX} # revisit after the probe
model = "hand-picked-wire-id" # mine
"#
        ),
    )
    .unwrap();

    install_at(dir.path(), SYNTH_PRESETS, false, &ctx()).expect("install");
    let body = providers_body(dir.path());
    assert!(
        body.contains(&format!("context_window = {CURRENT_CTX}")),
        "the upgrade did not land:\n{body}"
    );
    assert!(
        body.contains("# revisit after the probe"),
        "an upgrade dropped the trailing comment:\n{body}"
    );
    assert!(
        body.contains("# sized for the old plan"),
        "an upgrade dropped the comment above the key:\n{body}"
    );

    install_at(dir.path(), SYNTH_PRESETS, true, &ctx()).expect("forced install");
    let body = providers_body(dir.path());
    assert!(body.contains(r#"model = "synth-wire-id""#), "{body}");
    assert!(
        body.contains("# mine"),
        "--force dropped the trailing comment:\n{body}"
    );
}

#[test]
fn read_secret_from_pipe_stops_at_the_first_line_without_draining_stdin() {
    // A stream that never ends: `read_to_string` would grow until the OOM
    // killer arrives. The take()-limited single `read_line` must stop at the cap.
    struct Endless {
        served: usize,
    }
    impl std::io::Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf.fill(b'A');
            self.served += buf.len();
            assert!(
                self.served as u64 <= MAX_PIPED_KEY_BYTES,
                "the reader must never be drained past the cap (served {})",
                self.served
            );
            Ok(buf.len())
        }
    }
    let err = read_secret_from_pipe(Endless { served: 0 }).expect_err("over the cap");
    assert!(err.to_string().contains("exceeds"), "got: {err}");

    // A short first line followed by megabytes of tail: only the line is read.
    let mut stream = b"fw_first_line_key\n".to_vec();
    stream.extend(std::iter::repeat_n(b'B', 4 * 1024 * 1024));
    let key = read_secret_from_pipe(std::io::Cursor::new(stream)).expect("first line");
    assert_eq!(key, "fw_first_line_key");
}

#[test]
fn piped_key_round_trips_into_providers_toml() {
    let dir = home();
    let key = read_secret_from_pipe(std::io::Cursor::new(b"fw_piped_key_5678\n".to_vec()))
        .expect("piped key");
    set_key_at(dir.path(), "fireworks", &key).expect("set-key");
    let parsed = parse_providers(dir.path());
    assert_eq!(
        parsed["model_providers"]["fireworks"]["api_key"].as_str(),
        Some("fw_piped_key_5678")
    );
}

#[test]
fn set_key_on_stock_compatible_provider_writes_config_toml_not_providers() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let providers_before = providers_body(dir.path());

    set_key_at(dir.path(), "meta", "sk-test-not-real").expect("set-key");

    let parsed = parse_config(dir.path());
    assert_eq!(
        parsed["model_providers"]["meta"]["api_key"].as_str(),
        Some("sk-test-not-real")
    );
    assert_eq!(
        providers_body(dir.path()),
        providers_before,
        "a stock-compatible set-key must not touch providers.toml"
    );
    assert!(
        parse_providers(dir.path())
            .get("model_providers")
            .and_then(|t| t.get("meta"))
            .is_none(),
        "meta must not be copied into providers.toml"
    );
}

#[test]
fn set_key_on_fireworks_still_writes_providers_toml() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let config_before = config_body(dir.path());

    set_key_at(dir.path(), "fireworks", "sk-test-not-real").expect("set-key");

    let parsed = parse_providers(dir.path());
    assert_eq!(
        parsed["model_providers"]["fireworks"]["api_key"].as_str(),
        Some("sk-test-not-real")
    );
    assert_eq!(
        config_body(dir.path()),
        config_before,
        "a gx-only set-key must not touch config.toml"
    );
}

#[test]
fn unset_key_removes_api_key_from_config_toml_for_meta() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    set_key_at(dir.path(), "meta", "sk-test-not-real").expect("set-key");
    assert_eq!(
        parse_config(dir.path())["model_providers"]["meta"]["api_key"].as_str(),
        Some("sk-test-not-real")
    );

    assert!(unset_key_at(dir.path(), "meta").expect("unset-key").0);
    assert!(
        parse_config(dir.path())["model_providers"]["meta"]
            .get("api_key")
            .is_none(),
        "api_key must be gone from config.toml"
    );
}

#[test]
fn set_key_auto_installs_stock_compatible_preset_into_config_toml() {
    let dir = home();
    set_key_at(dir.path(), "meta", "sk-test-not-real").expect("set-key");
    let parsed = parse_config(dir.path());
    assert_eq!(
        parsed["model_providers"]["meta"]["base_url"].as_str(),
        Some("https://api.meta.ai/v1")
    );
    assert_eq!(
        parsed["model_providers"]["meta"]["api_key"].as_str(),
        Some("sk-test-not-real")
    );
    assert!(parsed["model"].get("muse-spark-1.3").is_some());
    assert!(parsed["model"].get("muse-spark-1.3-contributor").is_some());
    assert!(
        !providers_path(dir.path()).exists(),
        "auto-install of a stock-compatible preset must not create providers.toml"
    );
}

#[test]
fn set_key_auto_install_migrates_stock_compatible_overlay_out_of_providers() {
    let dir = home();
    fs::write(
        providers_path(dir.path()),
        r#"[model_providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_backend = "chat_completions"
env_key = "OPENROUTER_API_KEY"
api_key = "sk-old-overlay-not-real"

[model."openrouter/minimax-m3"]
model = "minimax/minimax-m3"
model_provider = "openrouter"

[model_providers.fireworks]
base_url = "https://api.fireworks.ai/inference/v1"
env_key = "FIREWORKS_API_KEY"
"#,
    )
    .unwrap();

    set_key_at(dir.path(), "openrouter", "sk-test-not-real").expect("set-key");

    assert_eq!(
        parse_config(dir.path())["model_providers"]["openrouter"]["api_key"].as_str(),
        Some("sk-test-not-real")
    );
    let providers = parse_providers(dir.path());
    assert!(
        providers
            .get("model_providers")
            .and_then(|t| t.get("openrouter"))
            .is_none(),
        "the overlay must be gone so it cannot shadow the new key:\n{}",
        providers_body(dir.path())
    );
    assert!(providers["model_providers"].get("fireworks").is_some());
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_covers_configured_unconfigured_and_env_key_cases() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    set_key_at(dir.path(), "zai-coding-plan", "zai-key-abcdefgh").expect("set-key");

    let env = |name: &str| match name {
        "OPENROUTER_API_KEY" => Some("sk-or-env-value-wxyz".to_owned()),
        _ => None,
    };
    // An entry a user might add by hand and never configure: `status` must
    // still list it, and say it is not set up. Every shipped preset installs,
    // so the "not configured" rendering needs a provider that did not.
    fs::write(
        providers_path(dir.path()),
        format!(
            "{}\n[model_providers.hand-written]\nbase_url = \"https://example.test/v1\"\n",
            providers_body(dir.path())
        ),
    )
    .unwrap();
    let report = status_report(dir.path(), None, &env);
    let rendered = render_status(&report, 0);

    let zai = report
        .providers
        .iter()
        .find(|p| p.id == "zai-coding-plan")
        .expect("zai present");
    assert!(!zai.in_providers && zai.in_config);
    assert_eq!(zai.key, KeySource::ConfigFile("…efgh".to_owned()));
    assert_eq!(
        zai.models,
        vec!["glm-5.3".to_owned(), "glm-5.3-flash".to_owned()]
    );

    let openrouter = report
        .providers
        .iter()
        .find(|p| p.id == "openrouter")
        .expect("openrouter present");
    assert_eq!(
        openrouter.key,
        KeySource::Env {
            var: "OPENROUTER_API_KEY".to_owned(),
            redacted: "…wxyz".to_owned(),
        }
    );
    assert_eq!(
        openrouter.models,
        vec![
            "openrouter/gemini-3.8-flash".to_owned(),
            "openrouter/minimax-m3".to_owned(),
        ]
    );

    let fireworks = report
        .providers
        .iter()
        .find(|p| p.id == "fireworks")
        .expect("fireworks present");
    assert_eq!(
        fireworks.key,
        KeySource::EnvUnset {
            vars: vec!["FIREWORKS_API_KEY".to_owned()]
        }
    );
    assert_eq!(fireworks.models.len(), 5);

    let codex = report
        .providers
        .iter()
        .find(|p| p.id == "openai-codex")
        .expect("openai-codex listed");
    assert!(codex.in_providers && !codex.in_config);
    assert_eq!(
        codex.models,
        vec![
            "gpt-5.6-luna".to_owned(),
            "gpt-5.6-sol".to_owned(),
            "gpt-5.6-terra".to_owned(),
            "gpt-6-astra".to_owned(),
        ],
        "the four direct ChatGPT-plan models"
    );
    assert!(
        codex.env_keys.is_empty(),
        "an env_key would shadow the auth helper"
    );

    let meta = report
        .providers
        .iter()
        .find(|p| p.id == "meta")
        .expect("meta present");
    assert!(!meta.in_providers && meta.in_config);
    assert_eq!(
        meta.models,
        vec![
            "muse-spark-1.3".to_owned(),
            "muse-spark-1.3-contributor".to_owned()
        ]
    );

    // Rendered shapes.
    assert!(
        rendered.contains("configured   yes  (providers.toml)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("configured   yes  (config.toml)"),
        "{rendered}"
    );
    // Every preset installs, so the "not configured" line comes from a
    // provider that is defined nowhere -- see the synthetic entry above, which
    // is in providers.toml but is not a preset.
    assert!(
        !rendered.contains("configured   no   (run `gx providers install`)"),
        "everything shipped is installed:\n{rendered}"
    );
    assert!(rendered.contains("hand-written"), "{rendered}");
    assert!(
        rendered.contains("key          yes  …efgh  (config.toml api_key)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("key          yes  …wxyz  (env OPENROUTER_API_KEY)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("key          no   (env_key FIREWORKS_API_KEY not set)"),
        "{rendered}"
    );
    assert!(rendered.contains("models       5  ("), "{rendered}");
    assert!(
        rendered.contains("models       2  (glm-5.3, glm-5.3-flash)"),
        "{rendered}"
    );
}

#[test]
fn status_never_prints_more_than_the_last_four_characters_of_a_key() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let secret = "fw_super_secret_key_material_7777";
    set_key_at(dir.path(), "fireworks", secret).expect("set-key");

    let env = |name: &str| {
        (name == "OPENROUTER_API_KEY").then(|| "sk-or-another-secret-value".to_owned())
    };
    let rendered = render_status(&status_report(dir.path(), None, &env), 0);

    assert!(!rendered.contains(secret), "raw key leaked:\n{rendered}");
    assert!(
        !rendered.contains("fw_super"),
        "key prefix leaked:\n{rendered}"
    );
    assert!(
        !rendered.contains("sk-or-another"),
        "env key leaked:\n{rendered}"
    );
    assert!(rendered.contains("…7777"), "{rendered}");
}

#[test]
fn status_reads_config_toml_providers_and_flags_shadowing() {
    let dir = home();
    fs::write(
        dir.path().join("config.toml"),
        r#"[model_providers.zai-coding-plan]
api_key = "config-key-1234"
base_url = "https://api.z.ai/api/coding/paas/v4"

[model_providers.only-in-config]
base_url = "https://only.example.test/v1"
env_key = "ONLY_KEY"

[model."only/model"]
model_provider = "only-in-config"
"#,
    )
    .unwrap();
    fs::write(
        providers_path(dir.path()),
        r#"[model_providers.zai-coding-plan]
base_url = "https://api.z.ai/api/coding/paas/v4"
api_key = "providers-key-5678"
"#,
    )
    .unwrap();

    let report = status_report(dir.path(), None, &no_env);
    let zai = report
        .providers
        .iter()
        .find(|p| p.id == "zai-coding-plan")
        .unwrap();
    assert!(zai.in_providers && zai.in_config);
    assert_eq!(
        zai.key,
        KeySource::ProvidersFile("…5678".to_owned()),
        "providers.toml wins over config.toml"
    );

    let only = report
        .providers
        .iter()
        .find(|p| p.id == "only-in-config")
        .expect("config-only provider is listed");
    assert!(!only.in_providers && only.in_config);
    assert_eq!(only.models, vec!["only/model".to_owned()]);

    let rendered = render_status(&report, 0);
    assert!(
        rendered.contains("configured   yes  (providers.toml, shadowing config.toml)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("configured   yes  (config.toml)"),
        "{rendered}"
    );
}

#[test]
fn status_reports_a_malformed_providers_toml_instead_of_failing() {
    let dir = home();
    fs::write(providers_path(dir.path()), "nope [[[\n").unwrap();
    let report = status_report(dir.path(), None, &no_env);
    assert!(report.providers_error.is_some());
    let rendered = render_status(&report, 0);
    assert!(
        rendered.contains("WARNING: providers.toml did not parse"),
        "{rendered}"
    );
    assert!(rendered.contains("skips the whole layer"), "{rendered}");
}

#[test]
fn status_mirrors_the_runtime_deep_merge_of_config_and_providers() {
    // The exact runtime shape: config.toml defines the provider fully, and
    // providers.toml overrides ONE field. `load_user_tier_for` deep-merges
    // field by field, so config.toml's `env_key` is still in force — status
    // must resolve the key through it instead of reporting "no env_key".
    let dir = home();
    fs::write(
        dir.path().join("config.toml"),
        r#"[model_providers.zai-coding-plan]
base_url = "https://api.z.ai/api/coding/paas/v4"
api_backend = "chat_completions"
env_key = ["ZHIPU_API_KEY", "ZAI_API_KEY"]

[model."glm-5.3"]
model = "glm-5.3"
model_provider = "zai-coding-plan"
context_window = 1000000
"#,
    )
    .unwrap();
    fs::write(
        providers_path(dir.path()),
        r#"[model_providers.zai-coding-plan]
base_url = "https://proxy.example.test/v4"

[model."glm-5.3"]
context_window = 2000000
"#,
    )
    .unwrap();

    let env = |name: &str| (name == "ZAI_API_KEY").then(|| "zai-env-secret-mnop".to_owned());
    let report = status_report(dir.path(), None, &env);
    let zai = report
        .providers
        .iter()
        .find(|p| p.id == "zai-coding-plan")
        .expect("zai present");

    assert_eq!(
        zai.base_url.as_deref(),
        Some("https://proxy.example.test/v4"),
        "providers.toml wins for the field it defines"
    );
    assert_eq!(
        zai.env_keys,
        vec!["ZHIPU_API_KEY".to_owned(), "ZAI_API_KEY".to_owned()],
        "config.toml's env_key survives a providers.toml entry that only sets base_url"
    );
    assert_eq!(
        zai.key,
        KeySource::Env {
            var: "ZAI_API_KEY".to_owned(),
            redacted: "…mnop".to_owned(),
        },
        "the key resolves through the merged env_key, as it does at runtime"
    );
    assert_eq!(
        zai.models,
        vec!["glm-5.3".to_owned()],
        "the model's model_provider comes from the merged entry too"
    );

    let rendered = render_status(&report, 0);
    assert!(
        rendered.contains("key          yes  …mnop  (env ZAI_API_KEY)"),
        "{rendered}"
    );
    assert!(rendered.contains("models       1  (glm-5.3)"), "{rendered}");
}

#[test]
fn status_says_the_file_is_missing_before_the_first_install() {
    let dir = home();
    let rendered = render_status(&status_report(dir.path(), None, &no_env), 0);
    assert!(
        rendered.contains("(not created yet — run `gx providers install`)"),
        "{rendered}"
    );
}

// ---------------------------------------------------------------------------
// status: openai-codex section (fixture auth.json only)
// ---------------------------------------------------------------------------

/// Build an unsigned JWT-shaped token: header.payload.signature, payload
/// base64url-encoded. The signature is never checked (gx only reads claims).
fn fixture_jwt(payload: serde_json::Value) -> String {
    use base64::Engine as _;
    let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    format!(
        "{}.{}.{}",
        b64(br#"{"alg":"none","typ":"JWT"}"#),
        b64(payload.to_string().as_bytes()),
        b64(b"not-a-real-signature")
    )
}

fn write_codex_fixture(dir: &Path, exp: i64) -> PathBuf {
    let path = dir.join("auth.json");
    let token = fixture_jwt(serde_json::json!({
        "exp": exp,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct-fixture-abc123",
            "chatgpt_plan_type": "pro",
        },
    }));
    let doc = serde_json::json!({
        "auth_mode": "chatgpt",
        "some_unknown_future_field": {"kept": true},
        "tokens": {
            "id_token": "id-token-fixture",
            "access_token": token,
            "refresh_token": "refresh-token-fixture",
        },
        "last_refresh": "2026-08-24T00:00:00Z",
    });
    fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    path
}

#[test]
fn codex_status_reports_expiry_account_and_plan_from_a_fixture() {
    let dir = home();
    let exp = 1_800_000_000_i64;
    let path = write_codex_fixture(dir.path(), exp);

    let status = read_codex_auth(&path);
    assert!(status.present && status.error.is_none());
    assert_eq!(status.access_token_exp, Some(exp));
    assert_eq!(status.account_id_redacted.as_deref(), Some("…abc123"));
    assert_eq!(status.plan.as_deref(), Some("pro"));
    assert!(status.has_refresh_token);
    assert!(!status.has_api_key);

    let now = exp - 3_600;
    let rendered = render_codex_status(&status, now);
    assert!(rendered.contains("status       present"), "{rendered}");
    assert!(rendered.contains("expires in 1h0m"), "{rendered}");
    assert!(rendered.contains("account      …abc123"), "{rendered}");
    assert!(rendered.contains("plan         pro"), "{rendered}");
    // Redacted: never the full account id, never any token material.
    assert!(!rendered.contains("acct-fixture-abc123"), "{rendered}");
    assert!(!rendered.contains("refresh-token-fixture"), "{rendered}");

    let expired = render_codex_status(&status, exp + 120);
    assert!(expired.contains("EXPIRED 2m"), "{expired}");

    // And the fixture file is only ever read.
    let raw = fs::read_to_string(&path).unwrap();
    assert!(raw.contains("some_unknown_future_field"));
}

#[test]
fn codex_status_handles_missing_unparsable_and_api_key_files() {
    let dir = home();
    let missing = read_codex_auth(&dir.path().join("nope").join("auth.json"));
    assert!(!missing.present);
    let rendered = render_codex_status(&missing, 0);
    assert!(
        rendered.contains("not found — run `codex login`"),
        "{rendered}"
    );

    let bad = dir.path().join("bad.json");
    fs::write(&bad, "{ not json").unwrap();
    let status = read_codex_auth(&bad);
    assert!(status.present && status.error.is_some());
    assert!(render_codex_status(&status, 0).contains("unreadable"));

    let api = dir.path().join("api.json");
    fs::write(&api, r#"{"OPENAI_API_KEY":"sk-fixture-key"}"#).unwrap();
    let status = read_codex_auth(&api);
    assert!(status.has_api_key && !status.has_refresh_token);
    let rendered = render_codex_status(&status, 0);
    assert!(rendered.contains("api-key mode"), "{rendered}");
    assert!(
        !rendered.contains("sk-fixture-key"),
        "key leaked:\n{rendered}"
    );
}

#[test]
fn codex_status_renders_extreme_jwt_exp_values_without_overflowing() {
    // `exp` comes from an unverified JWT payload — an attacker-controlled i64.
    // `exp - now` and `-delta` both overflow (panic in debug) at the extremes.
    let dir = home();
    for exp in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
        let path = write_codex_fixture(dir.path(), exp);
        let status = read_codex_auth(&path);
        assert_eq!(status.access_token_exp, Some(exp));
        for now in [i64::MIN, 0, i64::MAX] {
            let rendered = render_codex_status(&status, now);
            assert!(
                rendered.contains("access token"),
                "exp={exp} now={now}:\n{rendered}"
            );
            assert!(
                rendered.contains("EXPIRED") || rendered.contains("expires in"),
                "exp={exp} now={now}:\n{rendered}"
            );
        }
    }

    // A timestamp chrono cannot represent falls back to the raw number rather
    // than dropping the line.
    let path = write_codex_fixture(dir.path(), i64::MAX);
    let rendered = render_codex_status(&read_codex_auth(&path), 0);
    assert!(rendered.contains(&i64::MAX.to_string()), "{rendered}");
}

#[test]
fn jwt_claims_decode_without_verifying_the_signature() {
    let token = fixture_jwt(serde_json::json!({"exp": 42}));
    let claims = decode_jwt_claims_unverified(&token).expect("claims");
    assert_eq!(claims["exp"].as_i64(), Some(42));
    assert!(decode_jwt_claims_unverified("not-a-jwt").is_none());
    assert!(decode_jwt_claims_unverified("a.!!!!.c").is_none());
}

#[test]
fn status_includes_the_codex_section_when_a_path_is_supplied() {
    let dir = home();
    let path = write_codex_fixture(dir.path(), 1_800_000_000);
    let report = status_report(dir.path(), Some(&path), &no_env);
    let rendered = render_status(&report, 0);
    assert!(
        rendered.contains("openai-codex credentials (codex CLI, read-only)"),
        "{rendered}"
    );
    // No path supplied -> no codex section at all.
    let rendered = render_status(&status_report(dir.path(), None, &no_env), 0);
    assert!(!rendered.contains("openai-codex credentials"), "{rendered}");
}

// ---------------------------------------------------------------------------
// parse diagnostics must not echo file content
// ---------------------------------------------------------------------------

/// A fake key on a line that does not parse. `toml`/`toml_edit` `Display`
/// prints the offending source line verbatim, so any code path that formats a
/// library parse error prints this whole string.
const LEAKY_KEY: &str = "sk-live-DO-NOT-PRINT-ME-9f2c8a1b";

fn leaky_toml() -> String {
    format!("[model_providers.fireworks]\napi_key = \"{LEAKY_KEY}\" trailing\n")
}

/// Every substring of the fake key that must not appear anywhere in output.
fn assert_no_key_fragment(what: &str, haystack: &str) {
    for fragment in [
        LEAKY_KEY,
        "sk-live-DO-NOT-PRINT-ME",
        "DO-NOT-PRINT-ME",
        "9f2c8a1b",
    ] {
        assert!(
            !haystack.contains(fragment),
            "{what} leaked `{fragment}`:\n{haystack}"
        );
    }
}

#[test]
fn status_never_echoes_a_malformed_providers_toml_source_line() {
    let dir = home();
    fs::write(providers_path(dir.path()), leaky_toml()).unwrap();

    let report = status_report(dir.path(), None, &no_env);
    let err = report.providers_error.clone().expect("a parse error");
    assert!(err.starts_with("malformed TOML at line"), "got: {err}");
    assert_no_key_fragment("status providers_error", &err);
    assert_no_key_fragment("status output", &render_status(&report, 0));
}

#[test]
fn status_never_echoes_a_malformed_config_toml_source_line() {
    let dir = home();
    fs::write(dir.path().join("config.toml"), leaky_toml()).unwrap();

    let report = status_report(dir.path(), None, &no_env);
    let err = report.config_error.clone().expect("a parse error");
    assert!(err.starts_with("malformed TOML at line"), "got: {err}");
    assert_no_key_fragment("status config_error", &err);
    assert_no_key_fragment("status output", &render_status(&report, 0));
}

#[test]
fn install_and_set_key_never_echo_a_malformed_providers_toml_source_line() {
    let dir = home();
    fs::write(providers_path(dir.path()), leaky_toml()).unwrap();

    // `{e:#}` is how anyhow errors reach stderr at the top level: the whole
    // chain, not just the outermost message.
    let err = install_at(dir.path(), PRESETS, false, &ctx()).expect_err("must refuse");
    let text = format!("{err:#}");
    assert!(text.contains("not valid TOML"), "got: {text}");
    assert!(
        text.contains("malformed TOML at line"),
        "no position in: {text}"
    );
    assert_no_key_fragment("install error chain", &text);

    let err = set_key_at(dir.path(), "fireworks", "fw_new_key").expect_err("must refuse");
    let text = format!("{err:#}");
    assert_no_key_fragment("set-key error chain", &text);
    assert!(
        !text.contains("fw_new_key"),
        "the new key leaked into the error: {text}"
    );

    let err = unset_key_at(dir.path(), "fireworks").expect_err("must refuse");
    assert_no_key_fragment("unset-key error chain", &format!("{err:#}"));
}

#[test]
fn install_never_echoes_a_malformed_config_toml_source_line() {
    // A config.toml that does not parse must abort (stock-compatible presets
    // write there) — and must not be quoted back on the way out.
    let dir = home();
    let config = config_toml(dir.path());
    fs::write(&config, leaky_toml()).unwrap();
    let before = fs::read_to_string(&config).unwrap();
    let err = install_at(dir.path(), PRESETS, false, &ctx()).expect_err("must abort");
    let text = format!("{err:#}");
    assert!(text.contains("not valid TOML"), "got: {text}");
    assert!(
        text.contains("malformed TOML at line"),
        "no position in: {text}"
    );
    assert_no_key_fragment("install error chain", &text);
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        before,
        "malformed config.toml must be left untouched"
    );
    assert!(
        !providers_path(dir.path()).exists(),
        "install must not write providers.toml when config.toml is malformed"
    );
}

#[test]
fn parse_position_reports_only_a_line_and_column() {
    let src = "a = 1\nb = \"secret-value\" oops\n";
    let span = src.find("oops").map(|i| i..i + 4);
    assert_eq!(
        parse_position(src, span),
        "malformed TOML at line 2, column 20"
    );
    assert_eq!(parse_position(src, None), "malformed TOML");
}

// ---------------------------------------------------------------------------
// pre-read gates
// ---------------------------------------------------------------------------

#[test]
fn install_refuses_a_providers_toml_over_the_runtime_cap() {
    let dir = home();
    let path = providers_path(dir.path());
    let big = format!("# {}\n", "x".repeat(MAX_PROVIDERS_BYTES as usize));
    fs::write(&path, &big).unwrap();

    let err = install_at(dir.path(), PRESETS, false, &ctx()).expect_err("must refuse");
    let text = format!("{err:#}");
    assert!(text.contains("providers.toml limit"), "got: {text}");
    assert_eq!(
        fs::read_to_string(&path).unwrap().len(),
        big.len(),
        "the oversized file is left untouched"
    );

    let report = status_report(dir.path(), None, &no_env);
    assert!(
        report
            .providers_error
            .as_deref()
            .is_some_and(|e| e.contains("providers.toml limit")),
        "status must warn instead of reading it: {:?}",
        report.providers_error
    );
}

#[test]
fn a_providers_toml_that_is_not_a_regular_file_is_never_opened() {
    // A directory stands in for the fifo/device case: `metadata` reports it as
    // not-a-file, which is the check that keeps `open` from ever blocking.
    let dir = home();
    fs::create_dir(providers_path(dir.path())).unwrap();

    let err = install_at(dir.path(), PRESETS, false, &ctx()).expect_err("must refuse");
    assert!(
        format!("{err:#}").contains("not a regular file"),
        "got: {err:#}"
    );

    let report = status_report(dir.path(), None, &no_env);
    assert!(
        report
            .providers_error
            .as_deref()
            .is_some_and(|e| e.contains("not a regular file")),
        "{:?}",
        report.providers_error
    );
}

#[test]
fn an_oversized_config_toml_aborts_install_but_only_warns_in_status() {
    let dir = home();
    let config = dir.path().join("config.toml");
    fs::write(&config, "x".repeat(MAX_CONFIG_BYTES as usize + 1)).unwrap();

    let err = install_at(dir.path(), PRESETS, false, &ctx()).expect_err("must abort");
    let text = format!("{err:#}");
    assert!(text.contains("config.toml limit"), "got: {text}");
    assert!(
        !providers_path(dir.path()).exists(),
        "install must not write when it aborts on its inputs"
    );

    // status is read-only: it warns and renders the rest.
    let report = status_report(dir.path(), None, &no_env);
    assert!(
        report
            .config_error
            .as_deref()
            .is_some_and(|e| e.contains("config.toml limit")),
        "{:?}",
        report.config_error
    );
    assert!(!report.providers.is_empty(), "status still renders");
}

#[test]
fn install_warns_when_the_rendered_file_would_exceed_the_runtime_cap() {
    let dir = home();
    let path = providers_path(dir.path());
    // Just under the read gate, but the presets push the render over it.
    let padding = "x".repeat(MAX_PROVIDERS_BYTES as usize - 32);
    fs::write(&path, format!("# {padding}\n")).unwrap();

    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    assert!(
        report
            .exceeds_runtime_cap
            .is_some_and(|len| len > MAX_PROVIDERS_BYTES),
        "install must warn that the layer will be skipped at runtime: {:?}",
        report.exceeds_runtime_cap
    );
}

// ---------------------------------------------------------------------------
// cross-process lock
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn providers_lock_serializes_concurrent_mutators() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let dir = home();
    let lock_path = providers_lock_path(dir.path());
    let inside = Arc::new(AtomicUsize::new(0));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let acquisitions = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let lock_path = lock_path.clone();
            let inside = Arc::clone(&inside);
            let overlaps = Arc::clone(&overlaps);
            let acquisitions = Arc::clone(&acquisitions);
            std::thread::spawn(move || {
                for _ in 0..10 {
                    let guard = lock_providers_at(&lock_path, std::time::Duration::from_secs(20))
                        .expect("lock");
                    acquisitions.fetch_add(1, Ordering::SeqCst);
                    if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                        overlaps.fetch_add(1, Ordering::SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    inside.fetch_sub(1, Ordering::SeqCst);
                    drop(guard);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker thread");
    }

    assert_eq!(acquisitions.load(Ordering::SeqCst), 20);
    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        0,
        "two holders were inside the critical section at once"
    );
    assert_eq!(mode_of(&lock_path), 0o600, "the lockfile is owner-only");
}

#[cfg(unix)]
#[test]
fn providers_lock_times_out_with_a_clear_error_while_another_holder_waits() {
    let dir = home();
    let lock_path = providers_lock_path(dir.path());
    let held = lock_providers_at(&lock_path, std::time::Duration::from_secs(5)).expect("first");

    let contender = lock_path.clone();
    let err = std::thread::spawn(move || {
        lock_providers_at(&contender, std::time::Duration::from_millis(80))
            .expect_err("must time out while the lock is held")
    })
    .join()
    .expect("contender thread");
    assert!(
        err.to_string().contains("another `gx providers` command"),
        "got: {err}"
    );
    assert!(
        err.to_string().contains("nothing was written"),
        "got: {err}"
    );

    drop(held);
    lock_providers_at(&lock_path, std::time::Duration::from_secs(5))
        .expect("the lock is free once the holder drops it");
}

#[cfg(unix)]
#[test]
fn a_mutating_command_holds_the_lock_for_its_whole_read_modify_write() {
    // A concurrent mutator cannot even start while install is running, which
    // is what stops an unlocked rename from dropping a stored key.
    let dir = home();
    let lock_path = providers_lock_path(dir.path());
    let held = lock_providers_at(&lock_path, std::time::Duration::from_secs(5)).expect("hold");

    let home_path = dir.path().to_path_buf();
    let blocked = std::thread::spawn(move || set_key_at(&home_path, "fireworks", "fw_blocked_key"));
    // The contender must still be waiting on the flock, not writing.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        !providers_path(dir.path()).exists(),
        "set-key wrote while another holder had the lock"
    );

    drop(held);
    blocked
        .join()
        .expect("thread")
        .expect("set-key after unlock");
    let parsed = parse_providers(dir.path());
    assert_eq!(
        parsed["model_providers"]["fireworks"]["api_key"].as_str(),
        Some("fw_blocked_key")
    );
}

// ---------------------------------------------------------------------------
// redaction
// ---------------------------------------------------------------------------

#[test]
fn redact_tail_never_exposes_more_than_requested() {
    assert_eq!(redact_tail("", 4), "(empty)");
    assert_eq!(redact_tail("abcd", 4), "…");
    assert_eq!(redact_tail("abcdefgh", 4), "…");
    assert_eq!(redact_tail("abcdefghi", 4), "…fghi");
    assert_eq!(redact_tail("acct-1234567890", 6), "…567890");
}

// ---------------------------------------------------------------------------
// clap surface
// ---------------------------------------------------------------------------

fn parse(argv: &[&str]) -> ProvidersCommand {
    let args = PagerArgs::try_parse_from(argv).expect("args should parse");
    match args.command {
        Some(Command::Providers(ProvidersArgs { command })) => command,
        other => panic!("expected providers, got {other:?}"),
    }
}

#[test]
fn clap_parses_every_providers_subcommand() {
    assert!(matches!(
        parse(&["gx", "providers", "install"]),
        ProvidersCommand::Install { force: false }
    ));
    assert!(matches!(
        parse(&["gx", "providers", "install", "--force"]),
        ProvidersCommand::Install { force: true }
    ));
    match parse(&["gx", "providers", "set-key", "fireworks"]) {
        ProvidersCommand::SetKey { provider } => assert_eq!(provider, "fireworks"),
        other => panic!("expected set-key, got {other:?}"),
    }
    match parse(&["gx", "providers", "unset-key", "fireworks"]) {
        ProvidersCommand::UnsetKey { provider } => assert_eq!(provider, "fireworks"),
        other => panic!("expected unset-key, got {other:?}"),
    }
    assert!(matches!(
        parse(&["gx", "providers", "status"]),
        ProvidersCommand::Status
    ));
    assert!(matches!(
        parse(&["gx", "providers", "login", "openai"]),
        ProvidersCommand::Login(AuthTargetArgs {
            provider: AuthTarget::Openai
        })
    ));
    assert!(matches!(
        parse(&["gx", "providers", "token", "openai"]),
        ProvidersCommand::Token(AuthTargetArgs {
            provider: AuthTarget::Openai
        })
    ));
}

#[test]
fn clap_rejects_a_key_passed_as_a_positional_argument() {
    // The whole point of the prompt/stdin design: a key must never be able to
    // reach argv, where it lands in shell history and `ps` output.
    assert!(
        PagerArgs::try_parse_from(["gx", "providers", "set-key", "fireworks", "fw_secret"])
            .is_err(),
        "set-key must take exactly one positional argument"
    );
    assert!(
        PagerArgs::try_parse_from(["gx", "providers", "set-key", "--key", "fw_secret"]).is_err(),
        "there must be no --key flag"
    );
    assert!(
        PagerArgs::try_parse_from(["gx", "providers", "unset-key", "fireworks", "fw_secret"])
            .is_err()
    );
    assert!(
        PagerArgs::try_parse_from(["gx", "providers", "set-key"]).is_err(),
        "set-key requires a provider"
    );
    assert!(
        PagerArgs::try_parse_from(["gx", "providers", "login", "anthropic"]).is_err(),
        "only the declared auth targets parse"
    );
}

#[test]
fn every_preset_is_shaped_for_the_providers_layer_allowlist() {
    // The layer only reads `model`, `model_providers`, `auth_provider`, so a
    // preset must never rely on any other top-level table.
    assert!(xai_grok_config::PROVIDERS_LAYER_TABLES.contains(&"model"));
    assert!(xai_grok_config::PROVIDERS_LAYER_TABLES.contains(&"model_providers"));

    for preset in PRESETS {
        assert!(!preset.id.is_empty());
        // No preset ships key material, ever.
        assert!(
            preset.fields.iter().all(|f| f.key != "api_key"),
            "{} must not ship an api_key",
            preset.id
        );
        for model in preset.models {
            assert!(
                model.fields.iter().any(|f| f.key == "model_provider"),
                "{} must name its provider",
                model.id
            );
            assert!(
                model
                    .fields
                    .iter()
                    .find(|f| f.key == "model_provider")
                    .is_some_and(|f| *f.current() == PresetValue::Str(preset.id)),
                "{} must point at {}",
                model.id,
                preset.id
            );

            // A model that opts into reasoning effort must ship both the
            // current default and the accepted list, and the default must be
            // a member of that list. `ReasoningEffort` (xai-grok-sampling-types)
            // has no `adaptive` variant, so no preset may ever list it.
            let supports_effort = model
                .fields
                .iter()
                .find(|f| f.key == "supports_reasoning_effort")
                .is_some_and(|f| *f.current() == PresetValue::Bool(true));
            if supports_effort {
                let effort = model
                    .fields
                    .iter()
                    .find(|f| f.key == "reasoning_effort")
                    .unwrap_or_else(|| panic!("{} must ship reasoning_effort", model.id));
                let efforts = model
                    .fields
                    .iter()
                    .find(|f| f.key == "reasoning_efforts")
                    .unwrap_or_else(|| panic!("{} must ship reasoning_efforts", model.id));
                let PresetValue::Str(default) = effort.current() else {
                    panic!("{} reasoning_effort must be a string", model.id);
                };
                let PresetValue::StrList(list) = efforts.current() else {
                    panic!("{} reasoning_efforts must be a string list", model.id);
                };
                assert!(
                    list.contains(default),
                    "{} default {default} must be one of {list:?}",
                    model.id
                );
                assert!(
                    !list.contains(&"adaptive"),
                    "{} must not list adaptive: grok's ReasoningEffort enum cannot express it",
                    model.id
                );
                assert!(
                    !list.contains(&"ultra"),
                    "{} must not list ultra: grok's ReasoningEffort enum cannot express it",
                    model.id
                );
            }
        }
    }

    // The OAuth preset must not carry env_key: a static credential beats the
    // auth-provider token in resolution and would shadow the codex login.
    let codex = PRESETS.iter().find(|p| p.id == "openai-codex").unwrap();
    assert!(
        codex.fields.iter().all(|f| f.key != "env_key"),
        "openai-codex must not carry env_key"
    );
    assert!(
        codex.rejects_static_key,
        "`set-key openai-codex` must refuse: an api_key outranks the auth helper"
    );
    // The rule the two OpenAI providers exist to keep apart.
    let api = PRESETS.iter().find(|p| p.id == "openai-api").unwrap();
    assert!(!api.rejects_static_key);
    assert!(api.fields.iter().any(|f| f.key == "env_key"));

    // Explicit, not inferred from the note string.
    for id in ["zai-coding-plan", "openrouter", "meta"] {
        assert!(
            PRESETS
                .iter()
                .find(|p| p.id == id)
                .unwrap()
                .stock_compatible,
            "{id} must write to config.toml"
        );
    }
    for id in ["fireworks", "openai-codex", "openai-api"] {
        assert!(
            !PRESETS
                .iter()
                .find(|p| p.id == id)
                .unwrap()
                .stock_compatible,
            "{id} must stay in providers.toml"
        );
    }
}

// ---------------------------------------------------------------------------
// the OpenAI presets
// ---------------------------------------------------------------------------

fn model_entry<'a>(parsed: &'a toml::Value, id: &str) -> &'a toml::Value {
    parsed["model"]
        .get(id)
        .unwrap_or_else(|| panic!("model `{id}` was installed"))
}

#[test]
fn the_openai_codex_preset_matches_the_shape_the_spike_proved() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let parsed = parse_providers(dir.path());
    let provider = &parsed["model_providers"]["openai-codex"];

    assert_eq!(
        provider["base_url"].as_str(),
        Some("https://chatgpt.com/backend-api/codex")
    );
    assert_eq!(provider["api_backend"].as_str(), Some("responses"));
    // No env_key and no api_key: either one outranks the auth helper in
    // credential resolution and would shadow the ChatGPT login.
    assert!(provider.get("env_key").is_none(), "{provider}");
    assert!(provider.get("api_key").is_none(), "{provider}");

    // The auth-helper seam: gx's own binary, by absolute path, with `args`
    // present so it execs directly instead of going through a shell.
    let auth = &provider["auth"];
    assert_eq!(auth["command"].as_str(), Some(GX_BIN));
    assert_eq!(
        auth["args"]
            .as_array()
            .expect("args")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        vec!["providers", "token", "openai"]
    );
    assert_eq!(
        auth["timeout_secs"].as_integer(),
        Some(TOKEN_HELPER_TIMEOUT_SECS),
        "the 30s default is too tight for a lock wait plus a refresh"
    );

    let headers = &provider["extra_headers"];
    assert_eq!(
        headers[CHATGPT_ACCOUNT_HEADER].as_str(),
        Some(FIXTURE_ACCOUNT)
    );
    assert_eq!(headers["originator"].as_str(), Some(GX_ORIGINATOR));

    let astra = model_entry(&parsed, "gpt-6-astra");
    assert_eq!(astra["model"].as_str(), Some("gpt-6-astra"));
    assert_eq!(astra["name"].as_str(), Some("GPT-6 Astra (ChatGPT)"));
    assert_eq!(astra["model_provider"].as_str(), Some("openai-codex"));
    // Codex's active-window value for the ChatGPT backend; its separate
    // max_context_window is not a gx model-catalog field.
    assert_eq!(astra["context_window"].as_integer(), Some(272_000));
    assert_eq!(astra["codex_compat"].as_bool(), Some(true));
    assert_eq!(astra["model_family"].as_str(), Some("openai-codex"));
    assert_eq!(astra["supports_reasoning_effort"].as_bool(), Some(true));
    assert_eq!(astra["reasoning_effort"].as_str(), Some("low"));
    assert_eq!(
        astra["reasoning_efforts"]
            .as_array()
            .expect("Astra efforts")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        vec!["low", "medium", "high", "xhigh", "max"]
    );
    for rejected in ["temperature", "top_p", "max_completion_tokens"] {
        assert!(
            astra.get(rejected).is_none(),
            "gpt-6-astra must not ship {rejected}"
        );
    }

    for (id, default_effort) in [
        ("gpt-5.6-sol", "low"),
        ("gpt-5.6-terra", "medium"),
        ("gpt-5.6-luna", "medium"),
    ] {
        let entry = model_entry(&parsed, id);
        assert_eq!(entry["model"].as_str(), Some(id), "wire id == catalog id");
        assert_eq!(entry["model_provider"].as_str(), Some("openai-codex"));
        // codex-rs's own value for the gpt-5.6 family.
        assert_eq!(entry["context_window"].as_integer(), Some(272_000), "{id}");
        assert_eq!(entry["codex_compat"].as_bool(), Some(true), "{id}");
        assert_eq!(entry["model_family"].as_str(), Some("openai-codex"), "{id}");
        assert_eq!(entry["supports_reasoning_effort"].as_bool(), Some(true));
        assert_eq!(
            entry["reasoning_effort"].as_str(),
            Some(default_effort),
            "{id}"
        );
        assert_eq!(
            entry["reasoning_efforts"]
                .as_array()
                .expect("efforts")
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh", "max"],
            "{id}: `ultra` is orchestration, not a wire effort gx can express"
        );
        // Never a temperature/top_p/max_completion_tokens: the endpoint 400s on
        // each of them, and codex_compat drops them, but shipping one would
        // still be a lie in the catalog.
        for rejected in ["temperature", "top_p", "max_completion_tokens"] {
            assert!(
                entry.get(rejected).is_none(),
                "{id} must not ship {rejected}"
            );
        }
    }
}

#[test]
fn the_openai_api_preset_is_a_plain_key_provider_with_no_catalog() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let provider = &parse_providers(dir.path())["model_providers"]["openai-api"];

    assert_eq!(
        provider["base_url"].as_str(),
        Some("https://api.openai.com/v1")
    );
    assert_eq!(provider["api_backend"].as_str(), Some("responses"));
    assert_eq!(provider["env_key"].as_str(), Some("OPENAI_API_KEY"));
    assert!(
        provider.get("auth").is_none(),
        "a key provider mints nothing"
    );

    let parsed = parse_providers(dir.path());
    let openai_api_models = parsed["model"]
        .as_table()
        .expect("model table")
        .values()
        .filter(|v| v.get("model_provider").and_then(toml::Value::as_str) == Some("openai-api"))
        .count();
    assert_eq!(openai_api_models, 0);
}

#[test]
fn install_omits_the_account_header_and_says_so_when_codex_has_no_account() {
    let dir = home();
    let ctx = PresetContext::fixed(GX_BIN, None);
    let report = install_at(dir.path(), PRESETS, false, &ctx).expect("install");

    let headers = &parse_providers(dir.path())["model_providers"]["openai-codex"]["extra_headers"];
    assert!(
        headers.get(CHATGPT_ACCOUNT_HEADER).is_none(),
        "an empty header value is worse than no header: {headers}"
    );
    assert_eq!(headers["originator"].as_str(), Some(GX_ORIGINATOR));
    // The spike showed the account header is optional today, so this is a
    // warning and not a failure — but it must be said.
    assert!(
        report
            .context_notes
            .iter()
            .any(|n| n.contains("account id")),
        "{:?}",
        report.context_notes
    );
}

#[test]
fn install_refreshes_a_moved_binary_and_a_switched_account_without_force() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("first install");

    // gx was reinstalled elsewhere and the user signed into another ChatGPT
    // account: both values are machine-derived, so re-running `install` must
    // fix them rather than treat them as hand edits.
    let moved = PresetContext::fixed("/usr/local/bin/gx", Some("acct-second-999"));
    let report = install_at(dir.path(), PRESETS, false, &moved).expect("second install");

    let provider = &parse_providers(dir.path())["model_providers"]["openai-codex"];
    assert_eq!(
        provider["auth"]["command"].as_str(),
        Some("/usr/local/bin/gx")
    );
    assert_eq!(
        provider["extra_headers"][CHATGPT_ACCOUNT_HEADER].as_str(),
        Some("acct-second-999")
    );
    assert!(
        report
            .refreshed_fields
            .contains(&"model_providers.openai-codex.auth".to_owned()),
        "{:?}",
        report.refreshed_fields
    );
    assert!(
        report
            .refreshed_fields
            .contains(&"model_providers.openai-codex.extra_headers".to_owned()),
        "{:?}",
        report.refreshed_fields
    );
    assert!(report.kept_fields.is_empty(), "{:?}", report.kept_fields);
}

#[test]
fn install_leaves_a_hand_written_auth_helper_and_headers_alone() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    // A helper that is emphatically not gx's shape, and a header bag with an
    // extra entry of the user's own.
    fs::write(
        providers_path(dir.path()),
        r#"
[model_providers.openai-codex]
base_url = "https://chatgpt.com/backend-api/codex"
api_backend = "responses"
auth = { command = "/opt/vault/mint-openai", args = ["--scope", "codex"] }
extra_headers = { originator = "gx", "x-team" = "platform" }
"#,
    )
    .unwrap();

    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let provider = &parse_providers(dir.path())["model_providers"]["openai-codex"];

    assert_eq!(
        provider["auth"]["command"].as_str(),
        Some("/opt/vault/mint-openai")
    );
    assert_eq!(
        provider["extra_headers"]["x-team"].as_str(),
        Some("platform")
    );
    assert!(
        report
            .kept_fields
            .contains(&"model_providers.openai-codex.auth".to_owned()),
        "{:?}",
        report.kept_fields
    );
    assert!(
        report
            .kept_fields
            .contains(&"model_providers.openai-codex.extra_headers".to_owned()),
        "{:?}",
        report.kept_fields
    );
}

#[test]
fn install_is_still_byte_identical_on_a_second_run_with_the_openai_presets() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("first install");
    let first_providers = providers_body(dir.path());
    let first_config = config_body(dir.path());
    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("second install");
    assert_eq!(providers_body(dir.path()), first_providers);
    assert_eq!(config_body(dir.path()), first_config);
    assert!(!report.changed);
    assert!(report.refreshed_fields.is_empty());
}

// ---------------------------------------------------------------------------
// status: account drift
// ---------------------------------------------------------------------------

/// Like [`write_codex_fixture`], with a chosen account id.
fn write_codex_fixture_for(dir: &Path, exp: i64, account: &str) -> PathBuf {
    let path = dir.join("auth.json");
    let token = fixture_jwt(serde_json::json!({
        "exp": exp,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account,
            "chatgpt_plan_type": "pro",
        },
    }));
    let doc = serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "id-token-fixture",
            "access_token": token,
            "refresh_token": "refresh-token-fixture",
        },
    });
    fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    path
}

#[test]
fn status_flags_an_installed_account_header_that_no_longer_matches() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    // The user signed into a different ChatGPT account after installing.
    let auth = write_codex_fixture_for(dir.path(), 1_800_000_000, "acct-second-999");

    let rendered = render_status(
        &status_report(dir.path(), Some(&auth), &no_env),
        1_800_000_000 - 600,
    );

    assert!(rendered.contains("STALE HEADER"), "{rendered}");
    assert!(rendered.contains("gx providers install"), "{rendered}");
    // Redacted on both sides — never the full account id.
    assert!(!rendered.contains("acct-second-999"), "{rendered}");
    assert!(!rendered.contains(FIXTURE_ACCOUNT), "{rendered}");
}

#[test]
fn status_notices_an_account_switch_once_and_then_stops() {
    let dir = home();
    let auth = write_codex_fixture_for(dir.path(), 1_800_000_000, FIXTURE_ACCOUNT);
    // First look: nothing cached, so nothing to report — and the account is
    // remembered.
    let rendered = render_status(&status_report(dir.path(), Some(&auth), &no_env), 0);
    assert!(!rendered.contains("ACCOUNT CHANGED"), "{rendered}");

    let auth = write_codex_fixture_for(dir.path(), 1_800_000_000, "acct-second-999");
    let rendered = render_status(&status_report(dir.path(), Some(&auth), &no_env), 0);
    assert!(rendered.contains("ACCOUNT CHANGED"), "{rendered}");

    // The cache was refreshed, so the same switch is not reported forever.
    let rendered = render_status(&status_report(dir.path(), Some(&auth), &no_env), 0);
    assert!(!rendered.contains("ACCOUNT CHANGED"), "{rendered}");
}

#[test]
fn status_reports_the_same_refresh_verdict_the_token_command_acts_on() {
    let dir = home();
    let exp = 1_800_000_000_i64;
    let auth = write_codex_fixture(dir.path(), exp);
    let report = status_report(dir.path(), Some(&auth), &no_env);

    // Comfortably inside the token's life.
    let fresh = render_codex_status(report.codex.as_ref().expect("codex"), exp - 3_600);
    assert!(fresh.contains("refresh      fresh (JWT exp)"), "{fresh}");

    // Inside the 5-minute skew: still valid, but gx will refresh it.
    let due = render_codex_status(report.codex.as_ref().expect("codex"), exp - 60);
    assert!(due.contains("REFRESH DUE"), "{due}");
}

// ---------------------------------------------------------------------------
// status: the installed auth helper
// ---------------------------------------------------------------------------

#[test]
fn status_warns_when_the_installed_helper_path_no_longer_exists() {
    // The preset bakes gx's own absolute path into `auth.command` so the helper
    // works without gx on PATH — which means moving or rebuilding the binary
    // leaves a dangling reference. grok's symptom is a helper that will not
    // spawn, several layers from here; this line is what names the cause.
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    let auth = write_codex_fixture(dir.path(), 1_800_000_000);

    let rendered = render_status(&status_report(dir.path(), Some(&auth), &no_env), 0);

    assert!(rendered.contains("HELPER MISSING"), "{rendered}");
    assert!(rendered.contains(GX_BIN), "{rendered}");
    assert!(
        rendered.contains("helper path missing (binary moved?)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("falls back to this binary at runtime"),
        "{rendered}"
    );
    assert!(rendered.contains("gx providers install"), "{rendered}");
}

#[test]
fn status_says_nothing_about_a_helper_that_exists_or_one_resolved_from_path() {
    let dir = home();
    let auth = write_codex_fixture(dir.path(), 1_800_000_000);
    let helper = dir.path().join("gx");
    fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();

    let write_helper = |command: &str| {
        fs::write(
            providers_path(dir.path()),
            format!(
                "[model_providers.openai-codex]\n\
                 auth = {{ command = \"{command}\", args = [\"providers\", \"token\", \"openai\"] }}\n"
            ),
        )
        .unwrap();
    };

    write_helper(&helper.display().to_string());
    let rendered = render_status(&status_report(dir.path(), Some(&auth), &no_env), 0);
    assert!(!rendered.contains("HELPER MISSING"), "{rendered}");

    // A bare command is resolved against PATH when it is spawned; `status` has
    // no business second-guessing that.
    write_helper("gx");
    let rendered = render_status(&status_report(dir.path(), Some(&auth), &no_env), 0);
    assert!(!rendered.contains("HELPER MISSING"), "{rendered}");
}

// ---------------------------------------------------------------------------
// install: which auth tables count as gx's own
// ---------------------------------------------------------------------------

/// The `auth` value out of a one-line `[model_providers.openai-codex]` fixture.
fn auth_value(inline: &str) -> toml::Value {
    let src = format!("[model_providers.openai-codex]\nauth = {inline}\n");
    toml::from_str::<toml::Value>(&src).expect("toml")["model_providers"]["openai-codex"]["auth"]
        .clone()
}

#[test]
fn only_a_machine_derived_gx_helper_counts_as_gx_shipped() {
    // `install` rewrites a "gx-shipped" auth table without `--force`, so this
    // predicate decides whose value may be silently replaced. It must recognize
    // exactly what gx writes and nothing else: the args say *what* is invoked,
    // the command says gx is what invokes it (a wrapper script calling the same
    // args is the user's), and an unexpected key means a hand edit that
    // replacing the whole inline table would delete.
    let shipped =
        |inline: &str| is_shipped_dynamic_shape(DynamicValue::GxTokenHelper, &auth_value(inline));

    // The two shapes gx itself can write: an absolute path, and the bare `gx`
    // fallback for when `current_exe` fails.
    assert!(shipped(
        r#"{ command = "/opt/gx/bin/gx", args = ["providers", "token", "openai"], timeout_secs = 120 }"#
    ));
    assert!(shipped(
        r#"{ command = "gx", args = ["providers", "token", "openai"] }"#
    ));
    // A gx installed somewhere else entirely — still gx, still refreshable.
    assert!(shipped(
        r#"{ command = "/home/u/.cargo/bin/gx", args = ["providers", "token", "openai"], timeout_secs = 300 }"#
    ));

    // A wrapper that happens to call gx's args: rewriting `command` would drop
    // the user's wrapper out of the chain.
    assert!(!shipped(
        r#"{ command = "/usr/local/bin/gx-with-vault", args = ["providers", "token", "openai"] }"#
    ));
    // Relative paths resolve against the *caller's* cwd, which gx never writes.
    assert!(!shipped(
        r#"{ command = "./gx", args = ["providers", "token", "openai"] }"#
    ));
    assert!(!shipped(
        r#"{ command = "bin/gx", args = ["providers", "token", "openai"] }"#
    ));
    // Right args, entirely different program.
    assert!(!shipped(
        r#"{ command = "/opt/vault/mint", args = ["providers", "token", "openai"] }"#
    ));
    // gx, but doing something gx never asked for.
    assert!(!shipped(
        r#"{ command = "/opt/gx/bin/gx", args = ["providers", "token", "openai", "--json"] }"#
    ));
    assert!(!shipped(r#"{ command = "/opt/gx/bin/gx", args = [] }"#));
    assert!(!shipped(r#"{ command = "/opt/gx/bin/gx" }"#));
    // A field gx does not ship: the user added it, and a wholesale replacement
    // would silently delete it.
    assert!(!shipped(
        r#"{ command = "/opt/gx/bin/gx", args = ["providers", "token", "openai"], env = { GROK_DEBUG = "1" } }"#
    ));
    assert!(!shipped(
        r#"{ command = "/opt/gx/bin/gx", args = ["providers", "token", "openai"], cwd = "/tmp" }"#
    ));
    // No command at all, and not a table.
    assert!(!shipped(r#"{ args = ["providers", "token", "openai"] }"#));
    assert!(!shipped(r#""gx providers token openai""#));
}

#[test]
fn install_keeps_a_user_wrapper_that_calls_gxs_own_helper_args() {
    // The end-to-end shape of the case above: the args match gx's exactly, so
    // only the `command` check keeps this out of `install`'s hands.
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    fs::write(
        providers_path(dir.path()),
        r#"
[model_providers.openai-codex]
base_url = "https://chatgpt.com/backend-api/codex"
auth = { command = "/usr/local/bin/gx-through-vault", args = ["providers", "token", "openai"], timeout_secs = 120 }
"#,
    )
    .unwrap();

    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("second install");

    assert_eq!(
        parse_providers(dir.path())["model_providers"]["openai-codex"]["auth"]["command"].as_str(),
        Some("/usr/local/bin/gx-through-vault"),
        "a user's wrapper is not a moved gx binary"
    );
    assert!(
        report
            .kept_fields
            .contains(&"model_providers.openai-codex.auth".to_owned()),
        "{:?}",
        report.kept_fields
    );
}

#[test]
fn install_keeps_a_gx_helper_carrying_a_field_gx_never_ships() {
    let dir = home();
    install_at(dir.path(), PRESETS, false, &ctx()).expect("install");
    fs::write(
        providers_path(dir.path()),
        r#"
[model_providers.openai-codex]
base_url = "https://chatgpt.com/backend-api/codex"
auth = { command = "/somewhere/else/gx", args = ["providers", "token", "openai"], timeout_secs = 120, env = { GROK_AUTH_DEBUG = "1" } }
"#,
    )
    .unwrap();

    let report = install_at(dir.path(), PRESETS, false, &ctx()).expect("second install");
    let auth = &parse_providers(dir.path())["model_providers"]["openai-codex"]["auth"];

    assert_eq!(auth["command"].as_str(), Some("/somewhere/else/gx"));
    assert_eq!(auth["env"]["GROK_AUTH_DEBUG"].as_str(), Some("1"));
    assert!(
        report
            .kept_fields
            .contains(&"model_providers.openai-codex.auth".to_owned()),
        "{:?}",
        report.kept_fields
    );
}
