//! Installed grok CLI version, kept in sync with the shipping binaries.

use std::sync::OnceLock;

use semver::Version;

pub const TEST_VERSION_ENV: &str = "GROK_TEST_VERSION";

pub const VERSION: &str = match option_env!("GROK_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// The release pipeline always injects `GROK_VERSION`; without it the build is from source.
pub const IS_DEV_BUILD: bool = option_env!("GROK_VERSION").is_none();

/// Runtime-injected `"<version> (<shortcommit>)"` string.
/// Only the release binary stamps the commit in its own build.rs and injects it here at startup, so the lib crates don't recompile on every commit.
static FULL_VERSION: OnceLock<&'static str> = OnceLock::new();

/// Inject the binary's stamped `"<version> (<shortcommit>)"` string.
/// Idempotent: the first set wins, repeats are ignored.
pub fn set_full_version(v: &'static str) {
    let _ = FULL_VERSION.set(v);
}

/// The injected version-with-commit string, or plain [`VERSION`] when no binary has called [`set_full_version`] (e.g. lib tests, dev harnesses).
pub fn full_version() -> &'static str {
    FULL_VERSION.get().copied().unwrap_or(VERSION)
}

/// Returns the [`TEST_VERSION_ENV`] override when set, otherwise [`VERSION`].
/// The env value is trimmed so non-semver-aware callers can pass the result straight into parsing.
pub fn installed() -> String {
    std::env::var(TEST_VERSION_ENV)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| VERSION.to_string())
}

pub fn installed_semver() -> Result<Version, semver::Error> {
    Version::parse(&installed())
}

/// gx: semver BUILD-METADATA marker that identifies a `gx` fork build
/// (e.g. `1.0.8+gx.1`). It is build metadata, never a prerelease, so channel
/// logic that special-cases `pre` never fires; the `semver` crate sorts it
/// just *above* the equal-numbered stock release, so a stock release of the
/// same number never looks like an upgrade.
pub const GX_BUILD_MARKER: &str = "+gx.";

/// gx: `true` when `version` carries the gx build-metadata marker. Pure, so the
/// fork's behavior gates are unit-testable without touching process env.
pub fn version_is_gx(version: &str) -> bool {
    version.contains(GX_BUILD_MARKER)
}

/// gx: the single build-time discriminator for "this is a gx build". Everything
/// the fork has to do differently from stock grok (leader socket name, updater
/// suppression) keys off this one predicate.
///
/// Deliberately reads the *compiled* [`VERSION`] and NOT [`installed`]: these
/// are identity gates, not version-comparison inputs. Honoring
/// [`TEST_VERSION_ENV`] here would let a stray env var make a gx binary bind
/// the stock leader socket and re-enable the stock auto-updater at runtime.
/// Behavior that needs to vary per flavor in tests takes an `is_gx: bool`
/// parameter instead (see `leader_file_stem_for`, `get_installer_for`).
pub fn is_gx_build() -> bool {
    version_is_gx(VERSION)
}

/// Formats the compiled version with a channel label for user-facing display, e.g. `"0.2.5 [stable]"`.
/// `channel_label` is pre-formatted by `xai_grok_update::channel_label()`: `" [alpha]"`, `" [stable]"`, or `""` when no pointer is cached.
pub fn display_version(channel_label: &str) -> String {
    format!("{}{}", VERSION, channel_label)
}

/// Like [`display_version`], but for the full `"0.2.5 (abc1234)"` string.
pub fn display_version_with_commit(version_with_commit: &str, channel_label: &str) -> String {
    format!("{}{}", version_with_commit, channel_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that the channel label is appended for alpha, stable, and empty labels.
    #[test]
    fn test_display_version_formatting_matrix() {
        let cases: &[(&str, &str, &str)] = &[
            // (version_with_commit,    label,        expected_suffix)
            ("0.2.5 (abc1234)", " [alpha]", "0.2.5 (abc1234) [alpha]"),
            ("0.2.5 (abc1234)", " [stable]", "0.2.5 (abc1234) [stable]"),
            ("0.2.5 (abc1234)", "", "0.2.5 (abc1234)"),
            (
                "0.1.220-alpha.2 (def0)",
                " [alpha]",
                "0.1.220-alpha.2 (def0) [alpha]",
            ),
        ];
        for (vwc, label, expected) in cases {
            assert_eq!(
                display_version_with_commit(vwc, label),
                *expected,
                "display_version_with_commit({:?}, {:?})",
                vwc,
                label,
            );
        }
        // display_version uses compiled VERSION, so verify only that the label appends
        assert_eq!(display_version(""), VERSION);
        assert!(display_version(" [stable]").ends_with("[stable]"));
    }

    /// gx: the fork discriminator fires only on the `+gx.` build-metadata
    /// marker, never on stock or alpha versions.
    #[test]
    fn version_is_gx_matches_only_gx_build_metadata() {
        assert!(version_is_gx("1.0.8+gx.1"));
        assert!(version_is_gx("1.0.8+gx.12"));
        assert!(!version_is_gx("1.0.8"));
        assert!(!version_is_gx("1.0.8-alpha.2"));
        assert!(!version_is_gx("1.0.8+deadbeef"));
        // The marker is build metadata, not a prerelease, and sorts just above
        // the equal-numbered stock release (semver 1.x orders build metadata).
        let gx: Version = "1.0.8+gx.1".parse().unwrap();
        let stock: Version = "1.0.8".parse().unwrap();
        assert!(gx.pre.is_empty(), "gx marker must not be a prerelease");
        assert!(gx > stock, "gx build must not look older than stock 1.0.8");
    }

    /// gx: the build discriminator is compiled in, so a runtime
    /// `GROK_TEST_VERSION` can never flip a gx build into stock behavior (wrong
    /// leader socket, stock auto-update re-enabled) or vice versa. `installed()`
    /// still honors the override — only the identity gate is pinned.
    #[test]
    fn is_gx_build_ignores_test_version_override() {
        let compiled = version_is_gx(VERSION);
        assert_eq!(is_gx_build(), compiled);

        let prev = std::env::var_os(TEST_VERSION_ENV);
        for probe in ["1.0.8+gx.1", "1.0.8"] {
            // SAFETY: this crate's tests do not otherwise read process env.
            unsafe { std::env::set_var(TEST_VERSION_ENV, probe) };
            assert_eq!(installed(), probe, "installed() still honors the override");
            assert_eq!(
                is_gx_build(),
                compiled,
                "is_gx_build() must track compiled VERSION, not {TEST_VERSION_ENV}={probe}"
            );
        }
        // SAFETY: see above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var(TEST_VERSION_ENV, v),
                None => std::env::remove_var(TEST_VERSION_ENV),
            }
        }
    }

    #[test]
    fn full_version_falls_back_then_first_set_wins() {
        assert_eq!(full_version(), VERSION);
        set_full_version("first (aaaaaaa)");
        assert_eq!(full_version(), "first (aaaaaaa)");
        set_full_version("second (bbbbbbb)");
        assert_eq!(full_version(), "first (aaaaaaa)");
    }
}
