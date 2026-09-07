//! The lane's bearer token: one file per `$GROK_HOME`, `0600`, 32 random bytes as hex.
//!
//! A loopback TCP port is reachable by every local user, so the port alone is not a boundary. The
//! token narrows the lane to whoever can read a `0600` file in `$GROK_HOME` — that is, the
//! `$GROK_HOME` owner, which is exactly shed-mobile's model (SSH into the account, then talk to the
//! forwarded port). The file is therefore treated as a credential, not as configuration:
//!
//! - created with `O_EXCL` so two racing lanes cannot both mint one,
//! - opened **once**, with `O_NOFOLLOW`, and validated from the resulting file descriptor rather
//!   than from the path — see [`read_token`],
//! - **refused** (never silently repaired) if it is a symlink, if its mode is not `0600`, if it is
//!   owned by another uid, or if its contents are not exactly 64 lowercase hex characters,
//! - compared in constant time,
//! - never logged, never `Debug`-printed, never returned by any route.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// File name of the token, under `$GROK_HOME`. One token per `$GROK_HOME`, shared by every leader
/// on that home (the token authenticates the human, not the leader instance).
pub const TOKEN_FILE_NAME: &str = "gx-remote.token";

/// Exactly the mode the file must have. Anything else means somebody widened it.
const REQUIRED_MODE: u32 = 0o600;

/// 32 bytes, hex-encoded.
const TOKEN_BYTES: usize = 32;

/// The only length a token is ever allowed to have: [`TOKEN_BYTES`] as lowercase hex.
const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// The lane's shared secret.
///
/// Has no `Debug`/`Display`/`Serialize` impl on purpose: the only way out of this type is
/// [`Token::matches`], so it cannot reach a log line or a response body by accident.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// For tests and for callers that already hold the secret.
    pub fn from_secret(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// Constant-time equality against a caller-supplied candidate.
    ///
    /// Hand-rolled rather than pulling in `subtle`, which is not a direct dependency anywhere in
    /// this workspace. The comparison is constant time **in the content** of two equal-length
    /// strings; it short-circuits on a length mismatch, which leaks only the token's length — a
    /// fixed, public 64 characters.
    pub fn matches(&self, candidate: &str) -> bool {
        ct_eq(self.0.as_bytes(), candidate.as_bytes())
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    // `black_box` keeps the accumulator from being folded into an early exit.
    std::hint::black_box(diff) == 0
}

/// Path of the token file under `grok_home`.
pub fn token_path(grok_home: &Path) -> PathBuf {
    grok_home.join(TOKEN_FILE_NAME)
}

/// Read the token, creating it if this is the first lane on this `$GROK_HOME`.
///
/// Returns an error rather than repairing anything when the existing file is not trustworthy.
pub fn load_or_create_token(grok_home: &Path) -> anyhow::Result<Token> {
    let path = token_path(grok_home);

    match create_token(&path) {
        Ok(token) => Ok(token),
        // Another lane (or an earlier run) got there first. Fall through to the read path, which
        // applies the full symlink/permission check to whatever is actually on disk.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => read_token(&path),
        Err(err) => Err(err).with_context(|| format!("creating {}", path.display())),
    }
}

/// Mint a new token at `path`, failing with `AlreadyExists` if one is already there.
fn create_token(path: &Path) -> std::io::Result<Token> {
    let secret = random_hex();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(REQUIRED_MODE)
        .open(path)?;
    file.write_all(secret.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(Token(secret))
}

/// Read an existing token, refusing anything that is not a plain `0600` regular file we own whose
/// contents are exactly [`TOKEN_HEX_LEN`] lowercase hex characters.
///
/// **The file is opened exactly once, and every check is made against that file descriptor.**
/// Validating `path` with `symlink_metadata` and then opening `path` is a TOCTOU: anybody who can
/// create a name in `$GROK_HOME` can let the check see a good file and the open see a symlink to a
/// file whose contents they chose, which would make the lane trust *their* bearer token. So:
///
/// - `O_NOFOLLOW` makes the swap fail at `open` (`ELOOP`) instead of succeeding quietly.
/// - `O_NONBLOCK` keeps a FIFO planted at the path from parking the whole leader on `open`.
/// - the type, mode and owner come from [`std::fs::File::metadata`] — the inode we hold open, which
///   nothing can substitute afterwards — and the contents are read from that same handle.
pub fn read_token(path: &Path) -> anyhow::Result<Token> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        // What `O_NOFOLLOW` reports for a symlink. Named explicitly so the refusal says why.
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => bail!(
            "{} is a symlink; refusing to read the remote token through it",
            path.display()
        ),
        Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
    };

    let meta = file
        .metadata()
        .with_context(|| format!("stat-ing the open {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != REQUIRED_MODE {
        bail!(
            "{} has mode {mode:04o}, expected {REQUIRED_MODE:04o}; fix it with `chmod 600` (not repaired automatically, because a widened token may already have leaked)",
            path.display()
        );
    }
    // A `0600` file owned by somebody else is unreadable by us anyway, so this mostly turns a
    // confusing permission error into a clear one — but it also refuses the case where we are root
    // and the mode check alone would have passed a file another user wrote.
    let euid = current_euid();
    if meta.uid() != euid {
        bail!(
            "{} is owned by uid {}, not by uid {euid}; refusing to trust another user's token",
            path.display(),
            meta.uid()
        );
    }

    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("reading {}", path.display()))?;
    let secret = contents.trim();
    if !is_well_formed(secret) {
        // Deliberately one message for empty, truncated, over-long and non-hex alike: they are all
        // "this file is not a token", and a caller must not be able to tell them apart. A crash
        // between `create_new` and `write_all` leaves an empty `0600` file, which lands here — so
        // the message has to say how to get out of it.
        bail!(
            "{path} does not hold a well-formed remote token ({TOKEN_HEX_LEN} lowercase hex characters); delete {path} and the next lane to start will mint a new one",
            path = path.display()
        );
    }
    Ok(Token(secret.to_string()))
}

/// Exactly [`TOKEN_HEX_LEN`] lowercase hex characters — the shape [`random_hex`] mints.
///
/// A partially written file must never be accepted: a one-character token is guessable in sixteen
/// requests, and the loopback port is reachable by every local user.
fn is_well_formed(secret: &str) -> bool {
    secret.len() == TOKEN_HEX_LEN
        && secret
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The effective uid of this process.
fn current_euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

fn random_hex() -> String {
    let bytes: [u8; TOKEN_BYTES] = rand::random();
    let mut out = String::with_capacity(TOKEN_BYTES * 2);
    for b in bytes {
        use std::fmt::Write as _;
        // Infallible for a String sink.
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Result::unwrap_err` needs `T: Debug`, and [`Token`] deliberately has no `Debug` impl — that
    /// missing impl is a compile-time proof the secret cannot reach a log line. So tests unwrap the
    /// error by hand.
    fn refusal(result: anyhow::Result<Token>) -> String {
        match result {
            Ok(_) => panic!("expected the token to be refused"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn a_created_token_is_0600_and_64_hex_characters() {
        let dir = tempfile::tempdir().unwrap();
        let token = load_or_create_token(dir.path()).unwrap();

        let path = token_path(dir.path());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the token file must be 0600");

        let on_disk = std::fs::read_to_string(&path).unwrap();
        let on_disk = on_disk.trim();
        assert_eq!(on_disk.len(), 64, "32 random bytes as hex");
        assert!(on_disk.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(token.matches(on_disk));
    }

    #[test]
    fn a_second_load_returns_the_same_token() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_token(dir.path()).unwrap();
        let on_disk = std::fs::read_to_string(token_path(dir.path())).unwrap();
        let second = load_or_create_token(dir.path()).unwrap();
        assert!(second.matches(on_disk.trim()));
        assert!(first.matches(on_disk.trim()));
    }

    #[test]
    fn a_widened_token_is_refused_not_repaired() {
        let dir = tempfile::tempdir().unwrap();
        load_or_create_token(dir.path()).unwrap();
        let path = token_path(dir.path());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = refusal(load_or_create_token(dir.path()));
        assert!(err.contains("mode 0644"), "{err}");

        // Still 0644: refusing must not quietly fix the file.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn a_symlinked_token_is_refused_at_open() {
        // The attack this blocks: a `0600` file whose 64 hex characters the attacker chose, reached
        // through a symlink they planted in `$GROK_HOME`. Everything about the *target* is
        // impeccable — mode, owner, shape — so the only thing that can refuse it is `O_NOFOLLOW`
        // on the open itself.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.token");
        std::fs::write(&target, format!("{}\n", "b".repeat(64))).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, token_path(dir.path())).unwrap();

        let err = refusal(load_or_create_token(dir.path()));
        assert!(err.contains("symlink"), "{err}");
    }

    /// Plant `contents` at the token path with a good mode, so only the content check can fire.
    fn plant(dir: &Path, contents: &str) -> PathBuf {
        let path = token_path(dir);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[test]
    fn only_exactly_64_lowercase_hex_characters_are_accepted() {
        let valid = "a".repeat(64);
        let dir = tempfile::tempdir().unwrap();
        plant(dir.path(), &format!("{valid}\n"));
        assert!(
            load_or_create_token(dir.path()).is_ok(),
            "64 lowercase hex characters are the token"
        );

        for (label, contents) in [
            // The crashed-between-create-and-write state.
            ("empty", String::new()),
            ("whitespace only", "\n  \n".to_string()),
            // A truncated write: one character is guessable in sixteen requests.
            ("one character", "a".to_string()),
            ("truncated", "a".repeat(63)),
            ("over-long", "a".repeat(65)),
            // Hex, right length, but not the alphabet we mint: an uppercase copy would compare
            // unequal to the file we wrote, so accepting it can only mean somebody edited it.
            ("uppercase", "A".repeat(64)),
            ("non-hex", "z".repeat(64)),
            (
                "hex with a space in it",
                format!("{} {}", "a".repeat(31), "b".repeat(32)),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            plant(dir.path(), &contents);
            let err = refusal(load_or_create_token(dir.path()));
            assert!(err.contains("well-formed remote token"), "{label}: {err}");
            // The operator has to be told how to recover, by name.
            assert!(err.contains("delete"), "{label}: {err}");
            assert!(
                err.contains(&token_path(dir.path()).display().to_string()),
                "{label}: the refusal must name the file to delete: {err}"
            );
        }
    }

    #[test]
    fn a_fifo_at_the_token_path_is_refused_rather_than_hanging() {
        // `O_NONBLOCK` is what keeps this from parking the leader forever on `open`; the type check
        // off the fd is what refuses it.
        let dir = tempfile::tempdir().unwrap();
        let path = token_path(dir.path());
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path; `mkfifo` only creates a filesystem entry.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

        let err = refusal(load_or_create_token(dir.path()));
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[test]
    fn matches_rejects_near_misses_and_prefixes() {
        let token = Token::from_secret("abc123");
        assert!(token.matches("abc123"));
        assert!(!token.matches("abc124"));
        assert!(!token.matches("abc12"));
        assert!(!token.matches("abc1234"));
        assert!(!token.matches(""));
    }

    #[test]
    fn two_tokens_minted_on_different_homes_differ() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        load_or_create_token(a.path()).unwrap();
        load_or_create_token(b.path()).unwrap();
        let sa = std::fs::read_to_string(token_path(a.path())).unwrap();
        let sb = std::fs::read_to_string(token_path(b.path())).unwrap();
        // `assert_ne!` would print both secrets into the failure output — and a failure here means
        // the RNG is broken, which is exactly when that output ends up in a CI log or a bug report.
        assert!(
            sa != sb,
            "two homes minted the same token; the values are withheld deliberately"
        );
    }
}
