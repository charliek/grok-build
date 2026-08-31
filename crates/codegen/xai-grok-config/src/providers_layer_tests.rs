//! gx: providers-layer tests — precedence, allowlist, malformed input, and
//! visibility through every consumer path.

use super::*;

use crate::ConfigLayers;
use crate::loader::{deep_merge_toml, load_user_tier_for};

fn write(dir: &std::path::Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

/// The user tier as gx sees it: `config.toml` with `providers.toml` over it.
fn gx_user_tier(home: &std::path::Path) -> toml::Value {
    load_user_tier_for(Some(home), true).unwrap()
}

/// `ConfigLayers` whose user tier is the gx-loaded tier for `home`, so the
/// consumer-path assertions exercise the real load, not a hand-built table.
fn layers_with_gx_user_tier(home: &std::path::Path) -> ConfigLayers {
    ConfigLayers {
        user: gx_user_tier(home),
        ..Default::default()
    }
}

const CONFIG_TOML: &str = r#"
[models]
default = "grok-4"

[model_providers.zai-coding-plan]
base_url = "https://config.example/v1"
env_key = "FROM_CONFIG"

[model."glm-5.3"]
model_provider = "zai-coding-plan"
context_window = 111
"#;

const PROVIDERS_TOML: &str = r#"
[model_providers.zai-coding-plan]
base_url = "https://providers.example/v1"

[model_providers.fireworks]
base_url = "https://api.fireworks.ai/inference/v1"

[model."glm-5.3"]
context_window = 222

[auth_provider.openai-codex]
command = "gx-openai-token"
"#;

// -- precedence within the user tier ---------------------------------------

/// The core contract: for the same key, `providers.toml` wins over
/// `config.toml`; unmentioned keys from `config.toml` survive the merge, and
/// tables only `providers.toml` defines are added.
#[test]
fn providers_layer_wins_over_config_toml_inside_the_user_tier() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);
    write(home.path(), PROVIDERS_FILENAME, PROVIDERS_TOML);

    let tier = gx_user_tier(home.path());

    // Same key in both files: the providers layer wins.
    assert_eq!(
        tier["model_providers"]["zai-coding-plan"]["base_url"].as_str(),
        Some("https://providers.example/v1"),
    );
    assert_eq!(
        tier["model"]["glm-5.3"]["context_window"].as_integer(),
        Some(222),
    );
    // Sibling keys the providers layer does not mention survive from config.toml.
    assert_eq!(
        tier["model_providers"]["zai-coding-plan"]["env_key"].as_str(),
        Some("FROM_CONFIG"),
    );
    assert_eq!(
        tier["model"]["glm-5.3"]["model_provider"].as_str(),
        Some("zai-coding-plan"),
    );
    // Tables only the providers layer defines land in the tier.
    assert_eq!(
        tier["model_providers"]["fireworks"]["base_url"].as_str(),
        Some("https://api.fireworks.ai/inference/v1"),
    );
    assert_eq!(
        tier["auth_provider"]["openai-codex"]["command"].as_str(),
        Some("gx-openai-token"),
    );
    // Everything else in config.toml is untouched.
    assert_eq!(tier["models"]["default"].as_str(), Some("grok-4"));
}

/// `deep_merge_toml` REPLACES arrays rather than concatenating, and the
/// providers layer inherits that: a preset's `env_key` list wholly replaces the
/// one in `config.toml`.
#[test]
fn providers_layer_arrays_replace_rather_than_concatenate() {
    let home = tempfile::tempdir().unwrap();
    write(
        home.path(),
        "config.toml",
        "[model_providers.p]\nenv_key = [\"A\", \"B\"]\n",
    );
    write(
        home.path(),
        PROVIDERS_FILENAME,
        "[model_providers.p]\nenv_key = [\"C\"]\n",
    );

    let tier = gx_user_tier(home.path());
    let keys: Vec<_> = tier["model_providers"]["p"]["env_key"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["C"]);
}

// -- absence, stock builds, no home ----------------------------------------

/// No `providers.toml` at all: the user tier is exactly `config.toml`.
#[test]
fn absent_providers_file_leaves_the_user_tier_untouched() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);

    assert_eq!(
        gx_user_tier(home.path()),
        load_user_tier_for(Some(home.path()), false).unwrap(),
    );
    assert!(load_providers_layer_for(Some(home.path()), true).is_none());
}

/// A STOCK build must behave byte-identically to a build with no providers
/// layer: the file is present and valid, and is still not read.
#[test]
fn stock_build_does_not_read_providers_toml() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);
    write(home.path(), PROVIDERS_FILENAME, PROVIDERS_TOML);

    let stock = load_user_tier_for(Some(home.path()), false).unwrap();
    let config_only: toml::Value = toml::from_str(CONFIG_TOML).unwrap();
    assert_eq!(stock, config_only);
    assert!(stock.get("auth_provider").is_none());
    assert!(stock["model_providers"].get("fireworks").is_none());

    // And the gate is the `is_gx` flag alone, checked before any disk access.
    assert!(load_providers_layer_for(Some(home.path()), false).is_none());
}

/// With no resolvable `$GROK_HOME` there is nothing to read — the providers
/// layer must not fall back to a cwd-relative file, matching
/// `load_user_config_layer`'s rule.
#[test]
fn no_home_yields_no_providers_layer() {
    assert!(load_providers_layer_for(None, true).is_none());
    assert_eq!(
        load_user_tier_for(None, true).unwrap(),
        toml::Value::Table(Default::default()),
    );
}

// -- malformed / hostile input ---------------------------------------------

/// A syntax error skips the whole layer, leaves `config.toml`'s content intact,
/// never fails, and never rewrites the offending file.
#[test]
fn malformed_providers_toml_is_skipped_and_config_toml_survives() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);
    let bad = "[model_providers.p]\nbase_url = = not valid toml\n";
    write(home.path(), PROVIDERS_FILENAME, bad);

    let tier = gx_user_tier(home.path());
    assert_eq!(tier, toml::from_str::<toml::Value>(CONFIG_TOML).unwrap());
    assert_eq!(
        tier["model_providers"]["zai-coding-plan"]["base_url"].as_str(),
        Some("https://config.example/v1"),
    );

    // The layer is read-only: the malformed file is byte-for-byte unchanged.
    assert_eq!(
        std::fs::read_to_string(providers_layer_path(home.path())).unwrap(),
        bad,
    );
}

/// Defensive: a non-table root skips the layer instead of replacing the user
/// tier wholesale (`deep_merge_toml` on a non-table override assigns over the
/// base).
#[test]
fn non_table_root_skips_the_layer() {
    let path = std::path::Path::new("/nonexistent/providers.toml");
    assert!(filter_providers_layer(toml::Value::Integer(7), path).is_none());
    assert!(filter_providers_layer(toml::Value::String("x".into()), path).is_none());
    assert!(
        filter_providers_layer(toml::Value::Array(vec![toml::Value::Integer(1)]), path).is_none()
    );
}

/// Disallowed top-level tables are warned about and dropped; the allowlisted
/// ones in the same file still load. Notably a providers layer cannot smuggle
/// in `[[campaigns]]`, `[[version_overrides]]`, `mcp_servers`, `hooks`, or the
/// permission/auth tables.
#[test]
fn disallowed_top_level_tables_are_ignored_and_allowed_ones_still_merge() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", "[permission]\nmode = \"ask\"\n");
    write(
        home.path(),
        PROVIDERS_FILENAME,
        r#"
[permission]
mode = "bypass"

[mcp_servers.evil]
command = "nc"

[hooks]
PreToolUse = []

[auth]
disable_api_key_auth = true

[[campaigns]]
id = "c1"

[[version_overrides]]
minimum_version = "0.0.1"

[model_providers.fireworks]
base_url = "https://api.fireworks.ai/inference/v1"
"#,
    );

    let tier = gx_user_tier(home.path());
    // The allowlisted table came through.
    assert_eq!(
        tier["model_providers"]["fireworks"]["base_url"].as_str(),
        Some("https://api.fireworks.ai/inference/v1"),
    );
    // Everything else was dropped — including a would-be escalation of a
    // security-gated table that config.toml already set.
    assert_eq!(tier["permission"]["mode"].as_str(), Some("ask"));
    for dropped in [
        "mcp_servers",
        "hooks",
        "auth",
        "campaigns",
        "version_overrides",
    ] {
        assert!(tier.get(dropped).is_none(), "{dropped} must not be merged");
    }
}

/// An allowlisted key that is not a table is ignored rather than clobbering the
/// corresponding `config.toml` table with a scalar.
#[test]
fn non_table_allowlisted_key_is_ignored() {
    let home = tempfile::tempdir().unwrap();
    write(
        home.path(),
        "config.toml",
        "[model.\"glm-5.3\"]\ncontext_window = 111\n",
    );
    write(
        home.path(),
        PROVIDERS_FILENAME,
        "model = 5\nmodel_providers = \"nope\"\n[auth_provider.ok]\ncommand = \"c\"\n",
    );

    let tier = gx_user_tier(home.path());
    assert_eq!(
        tier["model"]["glm-5.3"]["context_window"].as_integer(),
        Some(111),
    );
    assert!(tier.get("model_providers").is_none());
    assert_eq!(tier["auth_provider"]["ok"]["command"].as_str(), Some("c"));
}

/// A file with nothing but disallowed tables contributes no layer at all.
#[test]
fn providers_file_with_only_disallowed_tables_contributes_nothing() {
    let home = tempfile::tempdir().unwrap();
    write(
        home.path(),
        PROVIDERS_FILENAME,
        "[telemetry]\nmode = \"on\"\n",
    );
    assert!(load_providers_layer_for(Some(home.path()), true).is_none());
}

/// An empty file is as good as an absent one.
#[test]
fn empty_providers_file_contributes_nothing() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), PROVIDERS_FILENAME, "");
    assert!(load_providers_layer_for(Some(home.path()), true).is_none());
}

// -- visibility through every consumer path --------------------------------

/// The providers layer is part of the *disk* user tier, so it must be visible
/// identically through the overlay-inclusive merge, the overlay-free merge the
/// security gates read, and the campaign-aware disk-only merge.
#[test]
fn providers_layer_visible_through_every_consumer_path() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);
    write(home.path(), PROVIDERS_FILENAME, PROVIDERS_TOML);
    let layers = layers_with_gx_user_tier(home.path());

    for (name, merged) in [
        ("effective_config_base", layers.effective_config_base()),
        (
            "effective_config_base_without_overlay",
            layers.effective_config_base_without_overlay(),
        ),
        (
            "effective_config_disk_only",
            layers.effective_config_disk_only(),
        ),
        (
            "effective_config_with_campaigns",
            layers.effective_config_with_campaigns(&[], &Default::default()),
        ),
    ] {
        assert_eq!(
            merged["model_providers"]["zai-coding-plan"]["base_url"].as_str(),
            Some("https://providers.example/v1"),
            "{name} must see the providers layer",
        );
        assert_eq!(
            merged["auth_provider"]["openai-codex"]["command"].as_str(),
            Some("gx-openai-token"),
            "{name} must see the providers layer",
        );
    }
}

// -- interaction with the other tiers --------------------------------------

/// The providers layer sits at user authority, so requirements and MDM still
/// clamp over it exactly as they clamp over `config.toml`.
#[test]
fn requirements_and_mdm_still_win_over_the_providers_layer() {
    let home = tempfile::tempdir().unwrap();
    write(
        home.path(),
        "config.toml",
        "[model.\"glm-5.3\"]\ncontext_window = 111\n",
    );
    write(
        home.path(),
        PROVIDERS_FILENAME,
        "[model.\"glm-5.3\"]\ncontext_window = 222\nmodel_provider = \"zai\"\n",
    );

    let mut layers = layers_with_gx_user_tier(home.path());
    layers.user_requirements =
        Some(toml::from_str("[model.\"glm-5.3\"]\ncontext_window = 333\n").unwrap());
    assert_eq!(
        layers.effective_config_base()["model"]["glm-5.3"]["context_window"].as_integer(),
        Some(333),
    );

    layers.mdm_requirements =
        Some(toml::from_str("[model.\"glm-5.3\"]\ncontext_window = 444\n").unwrap());
    let merged = layers.effective_config_base();
    assert_eq!(
        merged["model"]["glm-5.3"]["context_window"].as_integer(),
        Some(444),
    );
    // Fields the admin does not clamp still come from the providers layer.
    assert_eq!(
        merged["model"]["glm-5.3"]["model_provider"].as_str(),
        Some("zai"),
    );
    // And the overlay-free path the gates read agrees.
    assert_eq!(
        layers.effective_config_base_without_overlay()["model"]["glm-5.3"]["context_window"]
            .as_integer(),
        Some(444),
    );
}

/// The `GROK_CONFIG` overlay's fencing is untouched: a confined overlay still
/// cannot carry `model_providers` / `[model.*]` / `auth_provider`, so the
/// providers layer's entries survive the overlay-inclusive merge unchanged,
/// while an allowlisted overlay key still applies.
#[test]
fn env_overlay_fencing_is_unaffected_by_the_providers_layer() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);
    write(home.path(), PROVIDERS_FILENAME, PROVIDERS_TOML);

    let mut overlay: toml::Table = toml::from_str(
        r#"
[models]
default_reasoning_effort = "high"

[model_providers.zai-coding-plan]
base_url = "https://overlay.example/v1"

[model."glm-5.3"]
context_window = 999

[auth_provider.injected]
command = "evil"
"#,
    )
    .unwrap();
    crate::config_override::retain_overlay_allowed(&mut overlay);
    // The overlay's dangerous tables are dropped at the choke point, before it
    // ever reaches the merge.
    assert!(overlay.get("model_providers").is_none());
    assert!(overlay.get("model").is_none());
    assert!(overlay.get("auth_provider").is_none());

    let layers = ConfigLayers {
        env_overlay: Some(toml::Value::Table(overlay)),
        ..layers_with_gx_user_tier(home.path())
    };
    let merged = layers.effective_config_base();
    assert_eq!(
        merged["model_providers"]["zai-coding-plan"]["base_url"].as_str(),
        Some("https://providers.example/v1"),
    );
    assert_eq!(
        merged["model"]["glm-5.3"]["context_window"].as_integer(),
        Some(222),
    );
    assert_eq!(
        merged["auth_provider"]["openai-codex"]["command"].as_str(),
        Some("gx-openai-token"),
    );
    assert!(merged["auth_provider"].get("injected").is_none());
    // The overlay's own allowlisted key still applies.
    assert_eq!(
        merged["models"]["default_reasoning_effort"].as_str(),
        Some("high"),
    );
}

/// The layer is merged *inside* the user tier, at user authority — not as a rank
/// of its own. Merging the tier over managed reproduces exactly the same result
/// as merging config.toml then providers.toml, so no consumer can observe the
/// providers layer outranking anything `config.toml` would not have outranked.
#[test]
fn providers_layer_ranks_exactly_at_user_authority() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);
    write(home.path(), PROVIDERS_FILENAME, PROVIDERS_TOML);

    let managed: toml::Value = toml::from_str(
        "[model_providers.zai-coding-plan]\nbase_url = \"https://managed/v1\"\ntimeout = 5\n",
    )
    .unwrap();

    let layers = ConfigLayers {
        managed: managed.clone(),
        ..layers_with_gx_user_tier(home.path())
    };

    let mut expected = managed;
    deep_merge_toml(
        &mut expected,
        &toml::from_str::<toml::Value>(CONFIG_TOML).unwrap(),
    );
    deep_merge_toml(
        &mut expected,
        &toml::from_str::<toml::Value>(PROVIDERS_TOML).unwrap(),
    );
    assert_eq!(layers.effective_config_base(), expected);
    // Managed-only keys still survive under the user tier.
    assert_eq!(
        layers.effective_config_base()["model_providers"]["zai-coding-plan"]["timeout"]
            .as_integer(),
        Some(5),
    );
}

/// `$VAR` expansion is applied to the providers layer exactly as it is to
/// `config.toml`, so the two user-tier files can't diverge on how a value like
/// `"${SOME_KEY}"` is read.
#[test]
fn providers_layer_expands_env_vars_like_config_toml() {
    const RAW: &str = "${GX_TEST_PROVIDERS_BASE}/v1";
    let home = tempfile::tempdir().unwrap();
    write(
        home.path(),
        "config.toml",
        &format!("[model_providers.from_config]\nbase_url = \"{RAW}\"\n"),
    );
    write(
        home.path(),
        PROVIDERS_FILENAME,
        &format!("[model_providers.from_providers]\nbase_url = \"{RAW}\"\n"),
    );

    let tier = gx_user_tier(home.path());
    let from_config = tier["model_providers"]["from_config"]["base_url"]
        .as_str()
        .unwrap();
    let from_providers = tier["model_providers"]["from_providers"]["base_url"]
        .as_str()
        .unwrap();
    assert_eq!(from_providers, from_config);
    assert_eq!(
        from_providers,
        crate::loader::expand_env_vars_in_string(RAW),
    );
}

/// The allowlist is exactly the three provider-shaped tables — a guard so a
/// future edit cannot widen it without this test noticing.
#[test]
fn allowlist_is_exactly_the_three_provider_tables() {
    assert_eq!(
        PROVIDERS_LAYER_TABLES,
        &["model", "model_providers", "auth_provider"],
    );
}

// -- pre-read gate: not-a-regular-file / oversized (HIGH finding) ----------

/// A `providers.toml` over [`MAX_PROVIDERS_LAYER_BYTES`] is skipped by the
/// `metadata` size check before it is ever opened; `config.toml` is
/// unaffected, and the file itself is left untouched (never truncated).
#[test]
fn oversized_providers_toml_is_skipped_with_a_warning() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);

    // Pad a syntactically valid table out past the cap with a TOML comment so
    // the test can't accidentally pass because the file failed to parse for
    // an unrelated reason.
    let padding = "#".repeat(MAX_PROVIDERS_LAYER_BYTES as usize + 1);
    let oversized = format!("[model_providers.fireworks]\nbase_url = \"https://x\"\n{padding}\n");
    assert!(oversized.len() as u64 > MAX_PROVIDERS_LAYER_BYTES);
    write(home.path(), PROVIDERS_FILENAME, &oversized);

    assert!(load_providers_layer_for(Some(home.path()), true).is_none());
    let tier = gx_user_tier(home.path());
    assert_eq!(tier, toml::from_str::<toml::Value>(CONFIG_TOML).unwrap());

    // Read-only: the oversized file is left exactly as written.
    assert_eq!(
        std::fs::read_to_string(providers_layer_path(home.path())).unwrap(),
        oversized,
    );
}

/// A directory at the `providers.toml` path is not a regular file, so the
/// `is_file()` check skips it cleanly (no error, no attempted read) instead of
/// failing startup.
#[test]
fn directory_at_providers_toml_path_is_skipped_cleanly() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);
    std::fs::create_dir(providers_layer_path(home.path())).unwrap();

    assert!(load_providers_layer_for(Some(home.path()), true).is_none());
    let tier = gx_user_tier(home.path());
    assert_eq!(tier, toml::from_str::<toml::Value>(CONFIG_TOML).unwrap());
}

/// A symlink to a regular `providers.toml` still loads: `std::fs::metadata`
/// follows symlinks, so the gate evaluates the target's type and size, not the
/// link's.
#[cfg(unix)]
#[test]
fn symlink_to_regular_providers_toml_still_loads() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);

    let real = home.path().join("providers.real.toml");
    std::fs::write(&real, PROVIDERS_TOML).unwrap();
    let link = providers_layer_path(home.path());
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let tier = gx_user_tier(home.path());
    assert_eq!(
        tier["model_providers"]["fireworks"]["base_url"].as_str(),
        Some("https://api.fireworks.ai/inference/v1"),
    );
    assert_eq!(
        tier["auth_provider"]["openai-codex"]["command"].as_str(),
        Some("gx-openai-token"),
    );
}

/// A symlink to an oversized target is still caught by the size check —
/// `metadata` resolves to the target's length, not the link's.
#[cfg(unix)]
#[test]
fn symlink_to_oversized_providers_toml_is_skipped() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", CONFIG_TOML);

    let padding = "#".repeat(MAX_PROVIDERS_LAYER_BYTES as usize + 1);
    let oversized = format!("[model_providers.fireworks]\nbase_url = \"https://x\"\n{padding}\n");
    let real = home.path().join("providers.real.toml");
    std::fs::write(&real, &oversized).unwrap();
    let link = providers_layer_path(home.path());
    std::os::unix::fs::symlink(&real, &link).unwrap();

    assert!(load_providers_layer_for(Some(home.path()), true).is_none());
}

// -- stale openai-codex helper path (mise upgrade) -------------------------

fn helper_args() -> Vec<String> {
    GX_TOKEN_HELPER_ARGS
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

#[test]
fn stale_gx_helper_fallback_replaces_a_missing_absolute_gx() {
    let missing = "/no/such/gx-install/gx";
    assert!(
        !std::path::Path::new(missing).exists(),
        "fixture path must not exist on this host"
    );
    assert_eq!(
        stale_gx_helper_fallback(missing, Some(&helper_args()), "/now/gx"),
        Some("/now/gx".to_owned()),
    );
}

#[test]
fn stale_gx_helper_fallback_keeps_an_existing_gx() {
    let dir = tempfile::tempdir().unwrap();
    let present = dir.path().join("gx");
    std::fs::write(&present, "#!/bin/sh\n").unwrap();
    assert_eq!(
        stale_gx_helper_fallback(
            &present.to_string_lossy(),
            Some(&helper_args()),
            "/now/gx",
        ),
        None,
        "a helper that still exists must keep minting through that path"
    );
}

#[test]
fn stale_gx_helper_fallback_ignores_wrappers_wrong_args_and_relative_paths() {
    let missing_wrapper = "/no/such/gx-with-vault";
    let missing_gx = "/no/such/gx-install/gx";
    assert_eq!(
        stale_gx_helper_fallback(missing_wrapper, Some(&helper_args()), "/now/gx"),
        None,
        "filename is not gx: a user wrapper"
    );
    assert_eq!(
        stale_gx_helper_fallback(
            missing_gx,
            Some(&[
                "providers".into(),
                "token".into(),
                "openai".into(),
                "--json".into()
            ]),
            "/now/gx",
        ),
        None,
        "extra arg: not the shipped helper"
    );
    assert_eq!(
        stale_gx_helper_fallback(missing_gx, None, "/now/gx"),
        None,
        "shell form (no args) is not the shipped helper"
    );
    assert_eq!(
        stale_gx_helper_fallback("gx", Some(&helper_args()), "/now/gx"),
        None,
        "bare gx is resolved against PATH at spawn time"
    );
    assert_eq!(
        stale_gx_helper_fallback("./gx", Some(&helper_args()), "/now/gx"),
        None,
        "relative paths are never something gx itself baked"
    );
}

#[test]
fn stale_gx_helper_fallback_replaces_a_leftover_directory_named_gx() {
    let dir = tempfile::tempdir().unwrap();
    let leftover = dir.path().join("gx");
    std::fs::create_dir(&leftover).unwrap();
    assert_eq!(
        stale_gx_helper_fallback(
            &leftover.to_string_lossy(),
            Some(&helper_args()),
            "/now/gx",
        ),
        Some("/now/gx".to_owned()),
        "a directory named gx is not a helper we can exec"
    );
}

/// Load must surface the file as-is. Spawn-time fallback is what mints after
/// a mise upgrade; rewriting here would make inspect/status disagree with disk.
#[test]
fn load_does_not_rewrite_a_missing_openai_codex_helper() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("gone").join("gx");
    let body = format!(
        "[model_providers.openai-codex]\n\
         auth = {{ command = \"{}\", args = [\"providers\", \"token\", \"openai\"], timeout_secs = 120 }}\n",
        missing.display()
    );
    write(home.path(), PROVIDERS_FILENAME, &body);

    let layer = load_providers_layer_for(Some(home.path()), true).expect("layer loads");
    assert_eq!(
        layer["model_providers"]["openai-codex"]["auth"]["command"].as_str(),
        Some(missing.to_str().unwrap()),
    );
    assert_eq!(
        std::fs::read_to_string(providers_layer_path(home.path())).unwrap(),
        body,
        "the layer must never write providers.toml on load"
    );
}
