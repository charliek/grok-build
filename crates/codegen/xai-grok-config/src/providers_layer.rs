//! gx: the **providers layer** — `$GROK_HOME/providers.toml`.
//!
//! A gx-only file that carries the fork's model/provider/auth-helper entries so
//! they never have to live in `config.toml`, which is shared byte-for-byte with a
//! stock `grok` install on the same `$GROK_HOME`. Stock grok has no knowledge of
//! this file and never reads it; gx merges it **inside the user tier**, over
//! `config.toml` (see [`crate::loader::load_from_disk`]), so every consumer of the
//! user layer — the effective config, the overlay-free merges the security gates
//! read, `effective_config_disk_only`, `grok inspect` — sees it identically.
//!
//! Deliberately **not** an "overlay": that word names the fenced `GROK_CONFIG`
//! env machinery in [`crate::env_overlay`], which drops exactly the tables this
//! layer exists to carry. The providers layer is a disk layer at user authority:
//! it wins over `config.toml`, and requirements / MDM still clamp over it.
//!
//! Contract:
//! - Allowlisted top-level tables only ([`PROVIDERS_LAYER_TABLES`]); every other
//!   top-level key is warned about and ignored, and the allowed ones still load.
//! - Unparsable file, unreadable file, or a non-table root: warn and skip the
//!   whole layer. Never fails startup, and never modifies the file.
//! - Not a regular file (fifo, device, directory, ...) or over
//!   [`MAX_PROVIDERS_LAYER_BYTES`]: warn and skip, checked *before* any read so a
//!   fifo can never block startup and a device node like `/dev/zero` can never be
//!   read into memory. A symlink to a regular file is followed and loads
//!   normally — `std::fs::metadata` resolves symlinks, so only the target's type
//!   and size matter.
//! - Absent file: silently contributes nothing.

use std::path::{Path, PathBuf};

/// gx: providers-layer filename, alongside `config.toml` under `$GROK_HOME`.
pub const PROVIDERS_FILENAME: &str = "providers.toml";

/// gx: hard cap on `providers.toml`'s size, checked via [`std::fs::metadata`]
/// before the file is ever opened for reading.
///
/// `providers.toml` is a small, hand-maintained config file — a handful of
/// `[model_providers.*]` / `[model.*]` / `[auth_provider.*]` tables — so 1 MiB
/// is generous headroom over any real file while still refusing a huge file
/// that would otherwise be read fully into memory on every startup.
const MAX_PROVIDERS_LAYER_BYTES: u64 = 1024 * 1024;

/// gx: the only top-level tables the providers layer may contribute.
///
/// Narrow on purpose. These three are precisely the tables a third-party model
/// entry needs, and precisely the tables that never round-trip through the
/// settings writer (`save_config` merges `cli`/`models`/`ui`/`harness`/`session`/
/// `privacy`/`consent`/`skills`/`telemetry`/`features` and
/// `toolset.ask_user_question` only), so a providers-layer value can never be
/// copied back out into the shared `config.toml` by a settings save.
pub const PROVIDERS_LAYER_TABLES: &[&str] = &["model", "model_providers", "auth_provider"];

/// gx: path to `<home>/providers.toml`.
pub fn providers_layer_path(home: &Path) -> PathBuf {
    home.join(PROVIDERS_FILENAME)
}

/// gx: load the providers layer for a resolved `$GROK_HOME`, or `None` when it
/// contributes nothing (stock build, no home, absent file, unreadable or
/// malformed file).
///
/// `is_gx` is a parameter, not a call to [`xai_grok_version::is_gx_build`], so
/// both flavors are unit-testable without touching process env — the same seam
/// shape as `leader_file_stem_for` / `get_installer_for`.
pub(crate) fn load_providers_layer_for(home: Option<&Path>, is_gx: bool) -> Option<toml::Value> {
    if !is_gx {
        return None;
    }
    let path = providers_layer_path(home?);
    if !providers_layer_file_is_safe_to_read(&path) {
        return None;
    }
    // `load_toml_file` returns an empty table for an absent file and expands
    // `$VAR` exactly as it does for `config.toml`, so the two user-tier files
    // behave the same way. Any error (syntax, EACCES) downgrades to a skip: the
    // providers layer must never fail startup.
    //
    // TOCTOU: the file could be replaced (by a symlink swap, or a fifo/device
    // dropped in place of the regular file that passed the check above)
    // between the `metadata` call and this read. Accepted for a local,
    // user-owned config file — closing it would need an fd-based read with the
    // metadata check re-run on the open fd, which is more machinery than a
    // single-user config file on local disk warrants.
    let value = match crate::loader::load_toml_file(&path) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "gx: skipping providers layer that failed to load or parse; \
                 config.toml is unaffected and the file is left untouched"
            );
            return None;
        }
    };
    filter_providers_layer(value, &path)
}

/// gx: pre-read gate for `providers.toml`, checked with a single
/// [`std::fs::metadata`] call before the file is ever opened.
///
/// `true` means "absent, or present, a regular file (symlinks resolved), and
/// within [`MAX_PROVIDERS_LAYER_BYTES`]" — proceed to
/// [`crate::loader::load_toml_file`], which handles an absent path itself.
/// `false` means the layer was skipped here (warned, except for the silent
/// absent-file case) and the caller must not read the file.
///
/// `std::fs::metadata` follows symlinks, so a symlink to a regular file is
/// treated exactly like the regular file it points to (loads normally); a
/// symlink to a fifo, character device (`/dev/zero`), or directory is caught
/// by the `is_file()` check below without ever calling `open` on it — `open`
/// on a fifo can block indefinitely, so this check must never open the path
/// first to find out.
fn providers_layer_file_is_safe_to_read(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if !meta.is_file() {
                tracing::warn!(
                    path = %path.display(),
                    "gx: skipping providers layer: not a regular file (fifo, device, \
                     directory, or similar) — refusing to open it"
                );
                return false;
            }
            if meta.len() > MAX_PROVIDERS_LAYER_BYTES {
                tracing::warn!(
                    path = %path.display(),
                    len = meta.len(),
                    max = MAX_PROVIDERS_LAYER_BYTES,
                    "gx: skipping providers layer: file exceeds the max size"
                );
                return false;
            }
            true
        }
        // Absent file: `load_toml_file` already treats this as an empty table,
        // so stay silent here too and let it take that path.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "gx: skipping providers layer: metadata unreadable"
            );
            false
        }
    }
}

/// gx: keep only [`PROVIDERS_LAYER_TABLES`], warning about everything else.
///
/// Pure, so the allowlist and the malformed-root behavior are testable without a
/// filesystem. `None` means "this layer contributes nothing".
pub(crate) fn filter_providers_layer(value: toml::Value, source: &Path) -> Option<toml::Value> {
    let toml::Value::Table(table) = value else {
        tracing::warn!(
            path = %source.display(),
            "gx: skipping providers layer: root is not a TOML table"
        );
        return None;
    };
    let mut kept = toml::map::Map::new();
    for (key, entry) in table {
        if !PROVIDERS_LAYER_TABLES.contains(&key.as_str()) {
            tracing::warn!(
                path = %source.display(),
                table = %key,
                "gx: ignoring unsupported providers-layer table; only {:?} are read here — \
                 put anything else in config.toml",
                PROVIDERS_LAYER_TABLES
            );
            continue;
        }
        if !entry.is_table() {
            tracing::warn!(
                path = %source.display(),
                table = %key,
                "gx: ignoring providers-layer key that is not a table"
            );
            continue;
        }
        kept.insert(key, entry);
    }
    if kept.is_empty() {
        return None;
    }
    Some(toml::Value::Table(kept))
}

/// gx: the args the openai-codex preset bakes into `auth.args`. Shared with
/// `gx providers install` so the runtime fallback recognizes the same helper.
pub const GX_TOKEN_HELPER_ARGS: &[&str] = &["providers", "token", "openai"];

/// gx: command to spawn when a baked helper path is gone: this binary when it
/// is named `gx`, otherwise the bare `gx` on PATH.
pub fn gx_helper_replacement() -> String {
    match std::env::current_exe() {
        Ok(path) if path.is_file() && path.file_name().is_some_and(|n| n == "gx") => {
            path.to_string_lossy().into_owned()
        }
        _ => "gx".to_owned(),
    }
}

/// gx: if `command` is an absolute path to a missing `gx` binary invoked with
/// the shipped helper args, return `replacement`. `None` means leave it.
///
/// Narrow on purpose: a wrapper whose filename is not `gx`, a relative path,
/// or different args is the user's and must not be rewritten.
pub fn stale_gx_helper_fallback(
    command: &str,
    args: Option<&[String]>,
    replacement: &str,
) -> Option<String> {
    let path = Path::new(command);
    if !path.is_absolute() || !path.file_name().is_some_and(|n| n == "gx") {
        return None;
    }
    let args_match = args.is_some_and(|a| {
        a.len() == GX_TOKEN_HELPER_ARGS.len()
            && a.iter()
                .map(String::as_str)
                .eq(GX_TOKEN_HELPER_ARGS.iter().copied())
    });
    if !args_match || path.is_file() {
        return None;
    }
    Some(replacement.to_owned())
}

#[cfg(test)]
#[path = "providers_layer_tests.rs"]
mod tests;
