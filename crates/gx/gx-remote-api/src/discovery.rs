//! The discovery record: how anything outside this process finds the lane.
//!
//! One JSON file per leader, `$GROK_HOME/gx-remote<suffix>.json`, written after the listener is
//! bound and removed on shutdown. `<suffix>` is empty only for the one socket that is this
//! `$GROK_HOME`'s default; every other socket gets a suffix derived from its **full** path, so
//! several leaders on one `$GROK_HOME` (different `GROK_LEADER_SOCKET`, or a non-default relay URL)
//! get distinct records instead of clobbering each other. See [`suffix_for_socket`].
//!
//! Writes are atomic: [`write_record`] renames a fully written temp file into place, so a reader
//! never sees a half-written record and two lanes racing on one path cannot interleave their bytes.
//!
//! `instanceId` is what makes removal safe. A crashed lane leaves its record behind; the next lane
//! overwrites it. If the crashed one is somehow still running its shutdown path, it must not delete
//! the *new* lane's record — hence [`remove_if_ours`], which compares the id before unlinking.
//! Readers additionally treat a record whose `pid` is dead as stale.

use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Same `0600` rationale as the token: the record names the token file and the port.
const RECORD_MODE: u32 = 0o600;

/// What a discovery reader (`gx remote status`, roost, shed) gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryRecord {
    /// Loopback base URL, e.g. `http://127.0.0.1:2421`.
    pub url: String,
    /// PID of the leader process hosting the lane. A dead pid means the record is stale.
    pub pid: u32,
    /// Random 128-bit hex, minted per lane start. Guards [`remove_if_ours`].
    pub instance_id: String,
    /// The leader socket this lane is attached to; the record's identity.
    pub socket_path: String,
    /// Absolute path of the bearer-token file. The value is never in here.
    pub token_file: String,
    /// `gx` build version string.
    pub version: String,
    /// Unix milliseconds at bind time.
    pub started_at: u64,
}

/// A fresh random 128-bit instance id, hex encoded.
pub fn new_instance_id() -> String {
    let bytes: [u8; 16] = rand::random();
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The leader socket a lane on `grok_home` attaches to **by default**: this build's stem, no
/// ws-URL suffix, directly under `$GROK_HOME`.
///
/// Derived from the leader's own `socket_path_for_ws_url_in` rather than from a local copy of the
/// stem, so gx's `gx-leader` / stock's `leader` split can never drift out of sync with
/// `leader::lock::leader_file_stem`.
pub fn default_leader_socket(grok_home: &Path) -> PathBuf {
    xai_grok_shell::leader::socket_path_for_ws_url_in(grok_home, "")
}

/// The record's file name suffix: `""` for `default_socket`, else a hash of the **full**
/// `socket_path`.
///
/// Deriving the suffix from the socket's *file name* was a collision waiting to happen:
/// `/a/gx-leader.sock` and `/b/gx-leader.sock` have the same file name, so under one `$GROK_HOME`
/// both landed on `gx-remote.json` and the two leaders took turns clobbering each other's record —
/// leaving a reader pointed at whichever wrote last. Two leaders on one home is not exotic; it is
/// precisely what `GROK_LEADER_SOCKET` exists for.
///
/// So only the socket that *is* this home's default keeps the plain `gx-remote.json`; everything
/// else hashes the whole path, directory included. The comparison is a plain path equality, not a
/// `canonicalize` — the socket need not exist yet when the record path is computed, and the failure
/// mode of a spelling mismatch is a hashed name where a plain one would have done, which is
/// harmless (still unique, still deterministic).
///
/// The hash is `DefaultHasher`, matching `leader::compute_ws_url_suffix`'s idiom upstream: stable
/// within a build, which is all a per-machine discovery file needs. The full 64 bits are kept
/// rather than upstream's truncation to 32, since these names must not collide.
fn suffix_for_socket(socket_path: &Path, default_socket: &Path) -> String {
    if socket_path == default_socket {
        return String::new();
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    socket_path.hash(&mut hasher);
    format!("-{:016x}", hasher.finish())
}

/// Path of the discovery record for a lane attached to `socket_path`.
pub fn record_path(grok_home: &Path, socket_path: &Path) -> PathBuf {
    record_path_against(grok_home, socket_path, &default_leader_socket(grok_home))
}

/// [`record_path`] with the home's default socket passed in.
///
/// Split out because `is_gx_build()` is compiled in (see `xai_grok_version`), so a test that called
/// [`record_path`] directly would only ever assert whichever flavor the test binary happens to be:
/// this way both layouts are exercised in one process.
fn record_path_against(grok_home: &Path, socket_path: &Path, default_socket: &Path) -> PathBuf {
    let suffix = suffix_for_socket(socket_path, default_socket);
    grok_home.join(format!("gx-remote{suffix}.json"))
}

/// Write (or replace) the record at `path`, mode `0600`, **atomically**.
///
/// A fresh temp file in the same directory is written, `fchmod`-ed, fsynced and then `rename`-d
/// over `path`. Truncating `path` in place was not safe: two leaders racing on one record (which
/// the old suffix derivation made possible, and a restart makes possible anyway) could interleave
/// their writes, and a reader could catch the file mid-write and get a parse error where it should
/// have got either the old record or the new one. `rename` within one directory is atomic, so a
/// reader only ever sees one whole record.
pub fn write_record(path: &Path, record: &DiscoveryRecord) -> anyhow::Result<()> {
    let mut body = serde_json::to_vec_pretty(record).context("serializing the discovery record")?;
    body.push(b'\n');

    let temp = temp_path_for(path);
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(RECORD_MODE)
            .open(&temp)?;
        // `OpenOptions::mode` is masked by the umask; `fchmod` on the handle we already hold is
        // not, and cannot be raced by a path lookup.
        file.set_permissions(std::fs::Permissions::from_mode(RECORD_MODE))?;
        file.write_all(&body)?;
        file.sync_all()
    };

    if let Err(err) = write() {
        let _ = std::fs::remove_file(&temp);
        return Err(err).with_context(|| format!("writing {}", temp.display()));
    }
    if let Err(err) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(err)
            .with_context(|| format!("renaming {} onto {}", temp.display(), path.display()));
    }
    Ok(())
}

/// A hidden, unique sibling of `path` to stage a record in. Same directory, so the `rename` is a
/// same-filesystem one; pid plus randomness, so two lanes never pick the same staging name.
fn temp_path_for(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("gx-remote.json");
    dir.join(format!(
        ".{name}.{}.{:016x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ))
}

/// Read the record at `path`.
pub fn read_record(path: &Path) -> anyhow::Result<DiscoveryRecord> {
    let body = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&body).with_context(|| format!("parsing {}", path.display()))
}

/// Delete the record at `path` **only** if it still names `instance_id`.
///
/// Returns whether it was removed. A missing file, an unparseable one, or one belonging to a
/// different instance all leave the filesystem untouched — a shutting-down lane must never take a
/// live successor's record with it.
///
/// # Residual race
///
/// There is no atomic "unlink this inode" on Linux, and the lane deliberately runs without a lock
/// file, so "is it ours?" and "delete it" cannot be one operation. The window is narrowed to the
/// smallest thing that is not a lock:
///
/// 1. open the record **once** and read and parse it from that file descriptor, so a replacement
///    mid-read cannot make us decide on a mixture of two records;
/// 2. immediately before unlinking, re-`stat` the *path* and require the same `(dev, ino)`. Since
///    [`write_record`] renames a brand-new inode into place, any successor that replaced the record
///    while we were parsing shows up here as a different inode, and we leave it alone.
///
/// What remains is the gap between that final `stat` and the `unlink` — microseconds, and only
/// reachable by a successor that publishes its record in exactly that gap. The consequence if it
/// loses that race is a missing (never a wrong) record: the successor's own lane is still bound and
/// serving, and `GET /v1/healthz` is what a client is required to trust anyway (see the accepted
/// risks in the crate docs).
pub fn remove_if_ours(path: &Path, instance_id: &str) -> bool {
    let Some((record, dev, ino)) = read_record_with_identity(path) else {
        return false;
    };
    if record.instance_id != instance_id {
        tracing::debug!(
            path = %path.display(),
            "gx-remote-api: discovery record belongs to another instance; leaving it"
        );
        return false;
    }

    unlink_if_same_inode(path, dev, ino)
}

/// Unlink `path` only if it still names the `(dev, ino)` we read the record from.
///
/// The last narrowing step of [`remove_if_ours`]; see its "Residual race" section for what is left
/// after it.
fn unlink_if_same_inode(path: &Path, dev: u64, ino: u64) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(now) if now.dev() == dev && now.ino() == ino => {}
        Ok(_) => {
            tracing::debug!(
                path = %path.display(),
                "gx-remote-api: discovery record was replaced while we read it; leaving it"
            );
            return false;
        }
        Err(_) => return false,
    }

    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "gx-remote-api: could not remove the discovery record");
            false
        }
    }
}

/// Read and parse the record at `path` from a single open handle, alongside the `(dev, ino)` of the
/// inode it actually came from. `None` for anything unreadable or unparseable.
fn read_record_with_identity(path: &Path) -> Option<(DiscoveryRecord, u64, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    let mut body = Vec::new();
    file.read_to_end(&mut body).ok()?;
    let record = serde_json::from_slice(&body).ok()?;
    Some((record, meta.dev(), meta.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(instance_id: &str) -> DiscoveryRecord {
        DiscoveryRecord {
            url: "http://127.0.0.1:2421".into(),
            pid: 4242,
            instance_id: instance_id.into(),
            socket_path: "/home/u/.grok/gx-leader.sock".into(),
            token_file: "/home/u/.grok/gx-remote.token".into(),
            version: "1.0.16+gx.10".into(),
            started_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn only_this_homes_default_socket_gets_the_unsuffixed_record() {
        let home = Path::new("/home/u/.grok");
        // Both build flavors, in one process: `record_path` itself can only see the compiled one.
        for stem in ["gx-leader", "leader"] {
            let default = home.join(format!("{stem}.sock"));
            assert_eq!(
                record_path_against(home, &default, &default),
                home.join("gx-remote.json"),
                "{stem}"
            );
        }
    }

    #[test]
    fn the_real_record_path_agrees_with_this_builds_default_socket() {
        let home = Path::new("/home/u/.grok");
        assert_eq!(
            record_path(home, &default_leader_socket(home)),
            home.join("gx-remote.json")
        );
    }

    #[test]
    fn same_named_sockets_in_different_directories_get_different_records() {
        // The collision this fix exists for: the old derivation used only the socket's file name,
        // so both of these produced `gx-remote.json` under one `$GROK_HOME` and the two leaders
        // clobbered each other.
        let home = Path::new("/home/u/.grok");
        let default = home.join("gx-leader.sock");
        let a = record_path_against(home, Path::new("/a/gx-leader.sock"), &default);
        let b = record_path_against(home, Path::new("/b/gx-leader.sock"), &default);

        assert_ne!(a, b, "two leaders must not share one record");
        assert_ne!(a, home.join("gx-remote.json"), "neither is the default");
        assert_ne!(b, home.join("gx-remote.json"));
        // Deterministic within a build.
        assert_eq!(
            a,
            record_path_against(home, Path::new("/a/gx-leader.sock"), &default)
        );
    }

    #[test]
    fn a_non_default_socket_beside_the_default_one_is_still_suffixed() {
        // A relay-URL-suffixed socket lives in `$GROK_HOME` and shares the stem, but it is not the
        // default, so it gets a path-derived name of its own rather than the plain one.
        let home = Path::new("/home/u/.grok");
        let default = home.join("gx-leader.sock");
        for other in ["gx-leader-1a2b3c4d.sock", "leader.sock", "my.sock"] {
            let path = record_path_against(home, &home.join(other), &default);
            assert_ne!(path, home.join("gx-remote.json"), "{other}");
            assert!(
                path.file_name().unwrap().to_str().unwrap().len() > "gx-remote.json".len(),
                "{other}: expected a suffixed name, got {}",
                path.display()
            );
        }
    }

    #[test]
    fn a_record_round_trips_as_camel_case_and_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gx-remote.json");
        write_record(&path, &sample("aa11")).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["instanceId"], "aa11");
        assert_eq!(raw["socketPath"], "/home/u/.grok/gx-leader.sock");
        assert_eq!(raw["tokenFile"], "/home/u/.grok/gx-remote.token");
        assert_eq!(raw["startedAt"], 1_700_000_000_000_u64);
        assert_eq!(read_record(&path).unwrap(), sample("aa11"));
    }

    #[test]
    fn an_overwrite_stays_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gx-remote.json");
        write_record(&path, &sample("aa11")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_record(&path, &sample("bb22")).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(read_record(&path).unwrap().instance_id, "bb22");
    }

    #[test]
    fn a_write_replaces_the_inode_and_leaves_no_temp_file_behind() {
        // The proof that the write is a rename, not a truncate: the record's inode changes, so a
        // reader holding the old file keeps reading a whole old record instead of watching bytes
        // appear underneath it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gx-remote.json");
        write_record(&path, &sample("aa11")).unwrap();
        let first = std::fs::metadata(&path).unwrap().ino();

        write_record(&path, &sample("bb22")).unwrap();
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            first,
            "an in-place truncate is what lets two writers interleave"
        );

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "gx-remote.json")
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn every_intermediate_state_of_a_write_parses() {
        // A truncating writer exposes an empty file and then partial JSON. With a rename, the only
        // states the path can ever be in are "absent", "old record" and "new record".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gx-remote.json");
        for id in ["aa11", "bb22", "cc33"] {
            write_record(&path, &sample(id)).unwrap();
            assert_eq!(read_record(&path).unwrap().instance_id, id);
        }
    }

    #[test]
    fn remove_if_ours_deletes_only_our_own_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gx-remote.json");
        write_record(&path, &sample("ours")).unwrap();

        assert!(
            !remove_if_ours(&path, "someone-else"),
            "a foreign instance id must not delete the record"
        );
        assert!(path.exists(), "the record must survive a foreign id");

        assert!(remove_if_ours(&path, "ours"));
        assert!(!path.exists());
    }

    #[test]
    fn a_record_replaced_between_the_read_and_the_unlink_survives() {
        // The narrowing, driven directly: a successor's `write_record` renames a *new* inode over
        // the path, so the identity we captured while reading no longer matches and the unlink is
        // skipped. Without this the shutting-down lane would delete a live successor's record.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gx-remote.json");
        write_record(&path, &sample("ours")).unwrap();
        let (_, dev, ino) = read_record_with_identity(&path).unwrap();

        write_record(&path, &sample("successor")).unwrap();
        assert!(
            !unlink_if_same_inode(&path, dev, ino),
            "a replaced record must not be unlinked"
        );
        assert!(path.exists(), "the successor's record must survive");
        assert_eq!(read_record(&path).unwrap().instance_id, "successor");

        // The un-raced path still removes it.
        let (_, dev, ino) = read_record_with_identity(&path).unwrap();
        assert!(unlink_if_same_inode(&path, dev, ino));
        assert!(!path.exists());
    }

    #[test]
    fn remove_if_ours_tolerates_a_missing_or_corrupt_record() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gx-remote.json");
        assert!(!remove_if_ours(&missing, "ours"));

        std::fs::write(&missing, b"not json").unwrap();
        assert!(!remove_if_ours(&missing, "ours"));
        assert!(
            missing.exists(),
            "an unparseable record is left for a human"
        );
    }

    #[test]
    fn instance_ids_are_128_bit_hex_and_unique() {
        let a = new_instance_id();
        let b = new_instance_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
