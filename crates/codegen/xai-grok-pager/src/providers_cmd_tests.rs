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

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// A synthetic two-generation preset table: `install` semantics are about
/// *shipped default history*, which the real presets do not have yet (they are
/// all first generation). Testing against this table exercises the upgrade rule
/// without waiting for the first real preset revision.
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
    let report = install_at(dir.path(), PRESETS, false).expect("install");
    let body = providers_body(dir.path());

    for preset in PRESETS.iter().filter(|p| p.install) {
        assert!(
            body.contains(&format!("[{}]", quoted_path("model_providers", preset.id))),
            "missing provider {} in:\n{body}",
            preset.id
        );
        for model in preset.models {
            assert!(
                body.contains(&format!("[{}]", quoted_path("model", model.id))),
                "missing model {} in:\n{body}",
                model.id
            );
        }
    }
    // Presets carry env_key, never api_key (the header comment mentions the
    // key by name, so check the parsed tables rather than the raw text).
    let parsed = parse_providers(dir.path());
    for (id, entry) in parsed["model_providers"].as_table().unwrap() {
        assert!(
            entry.get("api_key").is_none(),
            "install must never write key material, found one on {id}"
        );
    }
    // The Phase-2 skeletons are data-only in this build.
    assert!(
        !body.contains("openai-codex"),
        "openai-codex must not install"
    );
    assert!(!body.contains("openai-api"), "openai-api must not install");

    assert!(report.changed);
    assert!(
        report
            .added_entries
            .contains(&"model_providers.fireworks".to_owned())
    );
    assert!(
        report
            .added_entries
            .contains(&"model.\"glm-5.3\"".to_owned())
    );
    assert!(report.kept_fields.is_empty());
    assert!(report.upgraded_fields.is_empty());
}

#[test]
fn install_mirrors_the_live_glm_openrouter_and_fireworks_shapes() {
    let dir = home();
    install_at(dir.path(), PRESETS, false).expect("install");
    let parsed = parse_providers(dir.path());

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

    let ox = &parsed["model"]["openrouter/ox-alpha"];
    assert_eq!(ox["model"].as_str(), Some("stealth/ox-alpha"));
    assert_eq!(ox["context_window"].as_integer(), Some(200_000));
    assert_eq!(ox["stream_tool_calls"].as_bool(), Some(false));

    // Five Fireworks models, every one with an explicit context window, a
    // fully-qualified wire id, and streamed tool calls off.
    let fireworks: Vec<(&String, &toml::Value)> = parsed["model"]
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
        parsed["model"]["fireworks/deepseek-v4-flash"]["model"].as_str(),
        Some("accounts/fireworks/models/deepseek-v4-flash-0731")
    );
    assert_eq!(
        parsed["model"]["fireworks/kimi-k3"]["context_window"].as_integer(),
        Some(1_048_576)
    );
}

#[test]
fn install_second_run_is_byte_identical_and_reports_no_change() {
    let dir = home();
    install_at(dir.path(), PRESETS, false).expect("first install");
    let first = providers_body(dir.path());

    let report = install_at(dir.path(), PRESETS, false).expect("second install");
    let second = providers_body(dir.path());

    assert_eq!(first, second, "second install must be byte-identical");
    assert!(!report.changed, "second install must report no change");
    assert!(report.added_entries.is_empty());
    assert!(report.added_fields.is_empty());
    assert!(report.upgraded_fields.is_empty());
}

#[cfg(unix)]
#[test]
fn install_writes_providers_toml_0600_and_reclamps_a_loosened_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = home();
    let first = install_at(dir.path(), PRESETS, false).expect("install");
    let path = providers_path(dir.path());
    assert_eq!(mode_of(&path), 0o600, "providers.toml must be owner-only");
    assert_eq!(first.reclamped_from, None, "a fresh file is born 0600");

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let report = install_at(dir.path(), PRESETS, false).expect("second install");
    assert_eq!(
        mode_of(&path),
        0o600,
        "a no-op install must still clamp the mode back"
    );
    // The byte-identical early return is exactly the path a repeat install
    // takes; the clamp must happen there AND be reported, not swallowed.
    assert!(!report.changed, "the document is byte-identical");
    assert_eq!(
        report.reclamped_from,
        Some(0o644),
        "a loosened key file must be reported loudly, not clamped in silence"
    );

    // And once it is back at 0600, nothing is reported.
    let quiet = install_at(dir.path(), PRESETS, false).expect("third install");
    assert_eq!(quiet.reclamped_from, None);
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
fn install_never_creates_or_touches_config_toml() {
    let dir = home();
    let config = dir.path().join("config.toml");
    fs::write(&config, "[ui]\ncompact_mode = true\n").unwrap();
    let before = fs::read_to_string(&config).unwrap();

    install_at(dir.path(), PRESETS, false).expect("install");
    assert_eq!(fs::read_to_string(&config).unwrap(), before);

    // And with no config.toml at all, install must not invent one.
    let empty = home();
    install_at(empty.path(), PRESETS, false).expect("install");
    assert!(!empty.path().join("config.toml").exists());
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

    install_at(dir.path(), PRESETS, false).expect("install");
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
    install_at(dir.path(), PRESETS, false).expect("second install");
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

    let report = install_at(dir.path(), SYNTH_PRESETS, false).expect("install");
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
    assert!(
        report
            .added_fields
            .contains(&"model_providers.synth.env_key".to_owned())
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

    let report = install_at(dir.path(), PRESETS, false).expect("install");
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

    let report = install_at(dir.path(), SYNTH_PRESETS, true).expect("forced install");
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

    let report = install_at(dir.path(), SYNTH_PRESETS, false).expect("install");
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
    install_at(dir.path(), SYNTH_PRESETS, false).expect("second install");
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

    let report = install_at(dir.path(), SYNTH_PRESETS, false).expect("install");
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
    let again = install_at(dir.path(), SYNTH_PRESETS, false).expect("second install");
    assert_eq!(again.shadows_config, report.shadows_config);
}

#[test]
fn install_refuses_to_overwrite_a_malformed_providers_toml() {
    let dir = home();
    let path = providers_path(dir.path());
    let junk = "this is not = = toml [[[\n";
    fs::write(&path, junk).unwrap();

    let err = install_at(dir.path(), PRESETS, false).expect_err("must refuse");
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
    let err = install_at(dir.path(), PRESETS, false).expect_err("must refuse");
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

    let err = install_at(dir.path(), SYNTH_PRESETS, false).expect_err("must abort");
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
    let err = install_at(dir.path(), SYNTH_PRESETS, false).expect_err("must abort");
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

    let report = install_at(dir.path(), SYNTH_PRESETS, false).expect("install");
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
    let report = install_at(dir.path(), SYNTH_PRESETS, false).expect("install");
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
    install_at(dir.path(), PRESETS, false).expect("install");

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

    assert!(unset_key_at(dir.path(), "fireworks").expect("unset-key"));
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
    assert!(!unset_key_at(dir.path(), "fireworks").expect("second unset-key"));
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

[model_providers.openrouter]
base_url = "https://openrouter.ai/api/v1"

[model_providers.my-own]
base_url = "https://mine.example.test/v1"
"#,
    )
    .unwrap();

    set_key_at(dir.path(), "openrouter", "sk-or-secret-9876").expect("set-key");
    let body = providers_body(dir.path());
    assert!(body.contains("# keep me"), "{body}");
    assert!(body.contains("[model_providers.my-own]"), "{body}");
    assert!(body.contains(r#"api_key = "sk-or-secret-9876""#), "{body}");
}

#[test]
fn set_key_rejects_an_empty_key_and_an_unknown_provider() {
    let dir = home();
    install_at(dir.path(), PRESETS, false).expect("install");
    let before = providers_body(dir.path());

    let err = set_key_at(dir.path(), "fireworks", "   ").expect_err("empty key");
    assert!(err.to_string().contains("empty key"), "got: {err}");

    let err = set_key_at(dir.path(), "openai-codex", "sk-x").expect_err("phase-2 preset");
    assert!(
        err.to_string().contains("not available in this build"),
        "got: {err}"
    );

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
    install_at(dir.path(), PRESETS, false).expect("install");
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
        r#"[model_providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
# rotated quarterly
api_key = "old" # vault-managed
"#,
    )
    .unwrap();

    set_key_at(dir.path(), "openrouter", "sk-or-new-secret-4321").expect("set-key");
    let body = providers_body(dir.path());
    assert!(
        body.contains("# vault-managed"),
        "the trailing comment on the value was lost:\n{body}"
    );
    assert!(
        body.contains("# rotated quarterly"),
        "the comment above the key was lost:\n{body}"
    );
    assert!(
        body.contains(r#"api_key = "sk-or-new-secret-4321""#),
        "{body}"
    );
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

    install_at(dir.path(), SYNTH_PRESETS, false).expect("install");
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

    install_at(dir.path(), SYNTH_PRESETS, true).expect("forced install");
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

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_covers_configured_unconfigured_and_env_key_cases() {
    let dir = home();
    install_at(dir.path(), PRESETS, false).expect("install");
    set_key_at(dir.path(), "zai-coding-plan", "zai-key-abcdefgh").expect("set-key");

    let env = |name: &str| match name {
        "OPENROUTER_API_KEY" => Some("sk-or-env-value-wxyz".to_owned()),
        _ => None,
    };
    let report = status_report(dir.path(), None, &env);
    let rendered = render_status(&report, 0);

    let zai = report
        .providers
        .iter()
        .find(|p| p.id == "zai-coding-plan")
        .expect("zai present");
    assert!(zai.in_providers && !zai.in_config);
    assert_eq!(zai.key, KeySource::ProvidersFile("…efgh".to_owned()));
    assert_eq!(zai.models, vec!["glm-5.3".to_owned()]);

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

    // openai-codex is a preset but is not installed in this build.
    let codex = report
        .providers
        .iter()
        .find(|p| p.id == "openai-codex")
        .expect("openai-codex listed");
    assert!(!codex.in_providers && !codex.in_config);

    // Rendered shapes.
    assert!(
        rendered.contains("configured   yes  (providers.toml)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("configured   no   (run `gx providers install`)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("key          yes  …efgh  (providers.toml api_key)"),
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
    assert!(rendered.contains("models       1  (glm-5.3)"), "{rendered}");
}

#[test]
fn status_never_prints_more_than_the_last_four_characters_of_a_key() {
    let dir = home();
    install_at(dir.path(), PRESETS, false).expect("install");
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
    let err = install_at(dir.path(), PRESETS, false).expect_err("must refuse");
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
    // A config.toml that does not parse is tolerated (it belongs to stock grok
    // too) — and must not be quoted back on the way past.
    let dir = home();
    fs::write(dir.path().join("config.toml"), leaky_toml()).unwrap();
    let report = install_at(dir.path(), PRESETS, false).expect("install proceeds");
    assert_no_key_fragment("install report", &format!("{report:?}"));
    assert_no_key_fragment("providers.toml body", &providers_body(dir.path()));
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

    let err = install_at(dir.path(), PRESETS, false).expect_err("must refuse");
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

    let err = install_at(dir.path(), PRESETS, false).expect_err("must refuse");
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

    let err = install_at(dir.path(), PRESETS, false).expect_err("must abort");
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

    let report = install_at(dir.path(), PRESETS, false).expect("install");
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        ProvidersCommand::Login(Phase2Args {
            provider: Phase2Provider::Openai
        })
    ));
    assert!(matches!(
        parse(&["gx", "providers", "token", "openai"]),
        ProvidersCommand::Token(Phase2Args {
            provider: Phase2Provider::Openai
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
        "only the declared Phase-2 providers parse"
    );
}

#[test]
fn phase2_placeholders_announce_themselves_without_pretending_to_work() {
    assert_eq!(
        phase2_unavailable_message("login", Phase2Provider::Openai),
        "gx providers login openai: not yet available in this build"
    );
    assert_eq!(
        phase2_unavailable_message("token", Phase2Provider::Openai),
        "gx providers token openai: not yet available in this build"
    );
    assert_eq!(PHASE2_EXIT_CODE, 2);
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
            }
        }
    }

    // The OAuth preset must not carry env_key: a static credential beats the
    // auth-provider token in resolution and would shadow the codex login.
    let codex = PRESETS.iter().find(|p| p.id == "openai-codex").unwrap();
    assert!(!codex.install, "openai-codex is Phase 2, not installed yet");
    assert!(
        codex.fields.iter().all(|f| f.key != "env_key"),
        "openai-codex must not carry env_key"
    );
}
