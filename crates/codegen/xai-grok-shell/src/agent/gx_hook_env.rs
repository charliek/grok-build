//! gx: the roost tab identity a session's hooks must report (issue #14).
//!
//! A gx build starts a leader by default and runs every session's hooks inside that leader
//! process. The leader inherits the environment of whichever TUI happened to spawn it, so with two
//! TUIs open on one leader every session's hooks reported the FIRST tab's `ROOST_TAB_ID`. roost
//! claims a tab only on `SessionStart` and needs an existing owner for every later event, so the
//! misattribution is not cosmetic: the second tab's events land on the first tab's row, and a
//! session created by the remote lane inherits the same stale identity.
//!
//! The fix carries the identity per client instead of per process: the TUI reads these three
//! variables from its OWN environment, sends them to the leader as a registration capability, the
//! leader stamps them into each of that client's session requests, and the session actor merges
//! them into every hook spec's `extra_env`.
//!
//! ## Neutralizing what the leader inherited
//!
//! Carrying the identity is only half of it: hook children inherit the leader's environment, so a
//! session that carries NO identity would still see the `ROOST_*` variables of whichever tab
//! started the leader and would report as that tab. The fork does not fix that by editing the
//! leader's own environment — `std::env::remove_var` is `unsafe` in edition 2024 precisely because
//! another thread may be reading the environment concurrently, and by the time the leader could
//! call it the async runtime is already up, so the "before any thread" precondition does not hold.
//!
//! Instead the leader takes a READ-ONLY snapshot of the NAMES of every `ROOST_*` variable it
//! inherited, once at startup ([`snapshot_inherited_roost_names`]). The session actor then writes
//! each of those names as an EMPTY string into every hook spec's `extra_env` before overlaying the
//! session's own identity. The child's environment is the only thing mutated, and only for that
//! spawn. Net effect:
//!
//! - A session with no carried identity sees `ROOST_AGENT_HOOK=""`, so roost's hook command
//!   (`if [ -n "${ROOST_AGENT_HOOK:-}" ] …`) takes its else branch, drains stdin, prints `{}` and
//!   reports nothing. The session is invisible to roost rather than impersonating a tab.
//! - A session WITH an identity sees its own three values.
//! - An inherited `ROOST_LEASE` — a bearer credential, and not something a session's hooks have
//!   any business holding — no longer reaches a hook child at all, whichever case applies.
//!
//! The snapshot is deliberately empty in any process that never calls
//! [`snapshot_inherited_roost_names`]. A non-leader gx TUI runs its sessions in its own process,
//! where the inherited `ROOST_*` variables ARE the right identity; neutralizing them there would
//! blind roost for exactly the configuration that never had the bug.
//!
//! ## Trust boundary
//!
//! The leader socket is same-user, and registration is not an ownership check: any process that
//! can open the socket can already drive sessions and write hook files, so it can already cause
//! hook execution. What this feature must guarantee is narrower, and is guaranteed: a request body
//! can never supply the identity (the leader strips `_meta["gx/hookEnv"]` from every session
//! request and stamps its own), and an observer can neither set nor clear one.

use std::collections::BTreeMap;

/// The variables carried from a TUI process to its sessions' hook spawns.
///
/// Deliberately a fixed list, not a `ROOST_*` prefix sweep: what travels to another process's
/// child hooks is an explicit contract, so a future `ROOST_` variable cannot start leaking by
/// accident.
pub const CARRIED: [&str; 3] = ["ROOST_TAB_ID", "ROOST_SOCKET", "ROOST_AGENT_HOOK"];

/// The `_meta` key the leader stamps this onto a session request under, and the agent reads back.
/// Namespaced like the protocol's other vendor keys so it cannot collide with an upstream one.
pub const META_KEY: &str = "gx/hookEnv";

/// Cap on a single carried value. Well above any real tab id, socket path or hook path; low
/// enough that a hostile client cannot make the leader hold an unbounded map per connection.
const MAX_VALUE_LEN: usize = 4096;

/// Read [`CARRIED`] from this process's environment, **all or none**.
///
/// A partial set would misattribute rather than degrade: a tab id is meaningless to roost without
/// the socket to reach it on, and `ROOST_AGENT_HOOK` (the executable roost's hook command runs) is
/// what makes the pair actionable at all. Returning the whole set or nothing keeps a
/// half-configured environment from claiming a tab it cannot talk to.
///
/// Reads exactly the three names via `var_os` rather than iterating `std::env::vars()`: iterating
/// would pick up anything a shell happened to export.
pub fn from_process_env() -> BTreeMap<String, String> {
    from_lookup(|name| std::env::var_os(name).and_then(|v| v.into_string().ok()))
}

/// [`from_process_env`]'s logic over an arbitrary lookup, so the all-or-none rule is testable
/// without mutating the real process environment.
pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for name in CARRIED {
        match lookup(name) {
            Some(value) if !value.is_empty() => {
                out.insert(name.to_string(), value);
            }
            // Missing or empty: drop the whole set (see the doc comment).
            _ => return BTreeMap::new(),
        }
    }
    validate(out)
}

/// Prefix that names a roost variable, as bytes: the sweep below matches on the `OsStr`'s encoded
/// bytes rather than requiring valid UTF-8, so a non-UTF-8 `ROOST_…` name is still SEEN.
const ROOST_ENV_PREFIX: &[u8] = b"ROOST_";

/// The names of the `ROOST_*` variables the leader inherited, captured once at startup.
///
/// Unset in any process that never called [`snapshot_inherited_roost_names`] — see the module
/// docs on why a non-leader process must keep its inherited identity.
static INHERITED_ROOST_NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Snapshot the names of every `ROOST_*` variable this process inherited. Call once, at leader
/// startup, before any session exists.
///
/// Reads the environment and never writes it: reading is safe from any thread, which is the whole
/// point of doing it this way (see the module docs). Idempotent — a second call keeps the first
/// snapshot, so a test or a re-entrant startup path cannot shift the baseline under a live
/// session.
pub fn snapshot_inherited_roost_names() {
    let (names, unrepresentable) = collect_roost_names(std::env::vars_os().map(|(name, _)| name));
    let count = names.len();
    if INHERITED_ROOST_NAMES.set(names).is_err() {
        tracing::debug!("gx: inherited ROOST_* names already snapshotted; keeping the first");
        return;
    }
    if count > 0 || unrepresentable > 0 {
        tracing::info!(
            count,
            unrepresentable,
            "gx: snapshotted the inherited ROOST_* names; a session's hooks see them empty \
             unless the session carries a roost identity"
        );
    }
}

/// The snapshot [`snapshot_inherited_roost_names`] took, or an empty slice if it never ran.
pub fn inherited_roost_names() -> &'static [String] {
    match INHERITED_ROOST_NAMES.get() {
        Some(names) => names.as_slice(),
        None => &[],
    }
}

/// [`snapshot_inherited_roost_names`]'s sweep over an arbitrary name list, so it is testable
/// without touching the real process environment. Returns the sorted, de-duplicated names and the
/// count of matching names that are not valid UTF-8.
///
/// A non-UTF-8 name matches the prefix here — the match is on bytes — but `HookSpec::extra_env` is
/// a `HashMap<String, String>` and cannot hold it, so it is counted rather than silently skipped:
/// the log line is what makes the gap visible. The name itself is never logged; it is environment
/// content.
fn collect_roost_names(names: impl Iterator<Item = std::ffi::OsString>) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut unrepresentable = 0usize;
    for name in names {
        if !name.as_encoded_bytes().starts_with(ROOST_ENV_PREFIX) {
            continue;
        }
        match name.into_string() {
            Ok(name) => out.push(name),
            Err(_) => unrepresentable += 1,
        }
    }
    out.sort();
    out.dedup();
    (out, unrepresentable)
}

/// Read a session request's stamped [`META_KEY`] value, distinguishing **absent** from
/// **present-but-empty**.
///
/// - `None` — the key is absent. The requesting client is an observer (the leader deliberately
///   stamps nothing for one) or predates this fork's stamp. Either way the session's existing
///   identity must be left exactly as it is: an observer opening a session must not evict the TUI
///   that owns it.
/// - `Some(map)` — the leader stamped the requesting client's registered identity. An EMPTY map is
///   meaningful, not a failure: it is a non-observer client with no roost identity of its own, and
///   it CLEARS whatever the session was carrying, so a session handed to such a client stops
///   reporting the previous tab.
///
/// A present value that is not an object, or carries a non-string entry, is treated as
/// present-and-empty: it cannot have come from the leader, and the safe reading of an unusable
/// identity is "no identity".
pub fn from_meta_json(value: Option<&serde_json::Value>) -> Option<BTreeMap<String, String>> {
    let value = value?;
    let Some(object) = value.as_object() else {
        return Some(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (key, value) in object {
        let Some(text) = value.as_str() else {
            // All-or-none again: a map with one unusable entry is not a usable identity.
            return Some(BTreeMap::new());
        };
        out.insert(key.clone(), text.to_string());
    }
    Some(validate(out))
}

/// Accept `map` only if it is EXACTLY the [`CARRIED`] set (or empty), with every value one this
/// fork is willing to hand to a hook child. Anything else yields an empty map.
///
/// **The all-or-none completeness rule is enforced here**, not only in [`from_lookup`], so every
/// caller inherits it — registration hands a client's map straight to this function without going
/// through `from_lookup` at all. A subset is refused for the reason `from_lookup` refuses to build
/// one: `ROOST_AGENT_HOOK` on its own names the executable every hook of the session then runs,
/// with no tab or socket to make it meaningful, and a tab id without its socket misattributes.
///
/// Also rejects a key outside [`CARRIED`] (a client must not choose which variables the leader
/// exports into hook children), an empty or oversized value, and any value carrying NUL or a
/// newline — both of which would end up inside a `sh -c` command line the hook runner builds.
///
/// Never logs a value, and never logs a key either: a rejected key is by definition
/// client-controlled text, so it does not belong in the leader's log.
pub fn validate(map: BTreeMap<String, String>) -> BTreeMap<String, String> {
    if map.is_empty() {
        return map;
    }
    // Exactly CARRIED: the length check plus the containment check together rule out both a
    // subset and a superset (and therefore any key outside CARRIED).
    let complete = map.len() == CARRIED.len() && CARRIED.iter().all(|name| map.contains_key(*name));
    let bad_value = map.values().any(|value| {
        value.is_empty() || value.len() > MAX_VALUE_LEN || value.contains(['\0', '\n', '\r'])
    });
    if !complete || bad_value {
        tracing::warn!(
            entries = map.len(),
            "gx: rejected a client's hook env (it is not exactly the carried set, or an entry \
             failed validation)"
        );
        return BTreeMap::new();
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn full() -> BTreeMap<String, String> {
        map(&[
            ("ROOST_AGENT_HOOK", "/usr/local/bin/roost"),
            ("ROOST_SOCKET", "/run/user/1000/roost.sock"),
            ("ROOST_TAB_ID", "5"),
        ])
    }

    // ── from_lookup: all or none ────────────────────────────────────

    #[test]
    fn from_lookup_carries_the_whole_set_when_every_name_is_present() {
        let source = full();
        let got = from_lookup(|name| source.get(name).cloned());
        assert_eq!(got, source, "a complete environment must carry through");
        assert_eq!(
            got.len(),
            CARRIED.len(),
            "exactly the CARRIED names, nothing else"
        );
    }

    #[test]
    fn from_lookup_drops_everything_when_any_one_name_is_missing() {
        for missing in CARRIED {
            let mut source = full();
            source.remove(missing);
            assert!(
                from_lookup(|name| source.get(name).cloned()).is_empty(),
                "a tab id without its socket (or vice versa) would misattribute; \
                 dropping {missing} must drop the whole set"
            );
        }
    }

    #[test]
    fn from_lookup_treats_an_empty_value_as_missing() {
        for empty in CARRIED {
            let mut source = full();
            source.insert(empty.to_string(), String::new());
            assert!(
                from_lookup(|name| source.get(name).cloned()).is_empty(),
                "an exported-but-empty {empty} is not an identity"
            );
        }
    }

    #[test]
    fn from_lookup_reads_only_the_carried_names() {
        let asked = std::cell::RefCell::new(Vec::<String>::new());
        let source = full();
        let _ = from_lookup(|name| {
            asked.borrow_mut().push(name.to_string());
            source.get(name).cloned()
        });
        let mut asked = asked.into_inner();
        asked.sort();
        let mut expected: Vec<String> = CARRIED.iter().map(|n| (*n).to_string()).collect();
        expected.sort();
        assert_eq!(
            asked, expected,
            "from_process_env must look up exactly CARRIED, never sweep the environment"
        );
    }

    #[test]
    fn from_lookup_applies_validation_to_what_it_read() {
        let mut source = full();
        source.insert(
            "ROOST_TAB_ID".to_string(),
            "5\nROOST_SOCKET=/evil".to_string(),
        );
        assert!(
            from_lookup(|name| source.get(name).cloned()).is_empty(),
            "a newline in a real environment value must be rejected here too, \
             not just on the leader's side"
        );
    }

    // ── validate ────────────────────────────────────────────────────

    #[test]
    fn validate_accepts_the_exact_carried_set() {
        assert_eq!(
            validate(full()),
            full(),
            "exactly the three carried names is the one accepted non-empty shape"
        );
    }

    #[test]
    fn validate_accepts_an_empty_map() {
        assert!(
            validate(BTreeMap::new()).is_empty(),
            "no identity is a valid state (it stamps an empty object, which clears)"
        );
    }

    /// The completeness rule is enforced HERE, so registration — which calls `validate` directly,
    /// never through `from_lookup` — cannot accept a client carrying only `ROOST_AGENT_HOOK`.
    #[test]
    fn validate_rejects_a_subset_of_the_carried_set() {
        for keep in CARRIED {
            let one = map(&[(keep, "x")]);
            assert!(
                validate(one).is_empty(),
                "{keep} alone is not an identity: a hook executable with no tab or socket, or a \
                 tab with no way to reach it"
            );
        }
        let mut two = full();
        two.remove("ROOST_SOCKET");
        assert!(
            validate(two).is_empty(),
            "two of three is still a subset, and still misattributes"
        );
    }

    #[test]
    fn validate_rejects_a_superset_of_the_carried_set() {
        let mut extra_carried_name = full();
        extra_carried_name.insert("ROOST_EXTRA".to_string(), "x".to_string());
        assert!(
            validate(extra_carried_name).is_empty(),
            "a surviving subset could still claim a tab, with the wrong pieces"
        );

        let mut foreign = full();
        foreign.insert("PATH".to_string(), "/tmp/evil".to_string());
        assert!(
            validate(foreign).is_empty(),
            "a client must not choose which variables the leader exports into hook children"
        );
    }

    #[test]
    fn validate_rejects_an_empty_value() {
        let mut m = full();
        m.insert("ROOST_SOCKET".to_string(), String::new());
        assert!(validate(m).is_empty());
    }

    #[test]
    fn validate_rejects_an_oversized_value() {
        let mut m = full();
        m.insert("ROOST_SOCKET".to_string(), "x".repeat(MAX_VALUE_LEN + 1));
        assert!(
            validate(m).is_empty(),
            "the leader must not hold an unbounded per-client map"
        );

        let mut at_limit = full();
        at_limit.insert("ROOST_SOCKET".to_string(), "x".repeat(MAX_VALUE_LEN));
        assert_eq!(
            at_limit.clone(),
            validate(at_limit),
            "the cap is inclusive; only past it is a rejection"
        );
    }

    #[test]
    fn validate_rejects_control_characters_in_a_value() {
        for bad in ["a\0b", "a\nb", "a\rb"] {
            let mut m = full();
            m.insert("ROOST_AGENT_HOOK".to_string(), bad.to_string());
            assert!(
                validate(m).is_empty(),
                "{bad:?} would end up inside the `sh -c` command line the hook runner builds"
            );
        }
    }

    // ── from_meta_json: present vs absent ───────────────────────────

    /// The distinction Finding 1 turns on: an ABSENT key is an observer (or a pre-gx client) and
    /// must leave the session's identity alone, while a PRESENT-but-empty one is a TUI with no
    /// roost identity and must clear it.
    #[test]
    fn from_meta_json_separates_an_absent_key_from_an_empty_one() {
        assert_eq!(
            from_meta_json(None),
            None,
            "an absent key must not be reported as an empty identity: that would let an observer \
             clear the owning tab's"
        );
        assert_eq!(
            from_meta_json(Some(&serde_json::json!({}))),
            Some(BTreeMap::new()),
            "a stamped empty object is a real instruction: clear the session's identity"
        );
    }

    #[test]
    fn from_meta_json_reads_a_stamped_identity() {
        let stamped = serde_json::json!({
            "ROOST_AGENT_HOOK": "/usr/local/bin/roost",
            "ROOST_SOCKET": "/run/user/1000/roost.sock",
            "ROOST_TAB_ID": "5",
        });
        assert_eq!(from_meta_json(Some(&stamped)), Some(full()));
    }

    #[test]
    fn from_meta_json_treats_an_unusable_value_as_a_cleared_identity() {
        for unusable in [
            serde_json::json!(5),
            serde_json::json!("ROOST_TAB_ID=5"),
            serde_json::json!({ "ROOST_TAB_ID": 5, "ROOST_SOCKET": "/s", "ROOST_AGENT_HOOK": "/h" }),
            serde_json::json!({ "PATH": "/tmp/evil" }),
        ] {
            assert_eq!(
                from_meta_json(Some(&unusable)),
                Some(BTreeMap::new()),
                "{unusable} cannot have come from the leader; the safe reading is 'no identity', \
                 not 'leave the old one'"
            );
        }
    }

    // ── the inherited-name snapshot ─────────────────────────────────

    #[test]
    fn collect_roost_names_takes_every_roost_variable_and_nothing_else() {
        let (names, unrepresentable) = collect_roost_names(
            [
                "ROOST_TAB_ID",
                "ROOST_SOCKET",
                "ROOST_AGENT_HOOK",
                // Not in CARRIED: the sweep is by prefix, because an inherited ROOST_LEASE is a
                // bearer credential that must not reach a hook child either.
                "ROOST_LEASE",
                "PATH",
                "GROK_HOME",
                "ROOSTER",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        );
        assert_eq!(
            names,
            vec![
                "ROOST_AGENT_HOOK",
                "ROOST_LEASE",
                "ROOST_SOCKET",
                "ROOST_TAB_ID",
            ],
            "the sweep is by ROOST_ prefix, not by the CARRIED list, and `ROOSTER` is not a match"
        );
        assert_eq!(unrepresentable, 0);
    }

    /// The sweep matches on the `OsStr` bytes, so a non-UTF-8 `ROOST_…` name is seen rather than
    /// silently skipped — it is counted, because `extra_env` cannot represent it.
    #[test]
    #[cfg(unix)]
    fn collect_roost_names_counts_a_non_utf8_name_instead_of_missing_it() {
        use std::os::unix::ffi::OsStringExt;
        let (names, unrepresentable) = collect_roost_names(
            [
                std::ffi::OsString::from("ROOST_TAB_ID"),
                std::ffi::OsString::from_vec(b"ROOST_\xff\xfe".to_vec()),
            ]
            .into_iter(),
        );
        assert_eq!(names, vec!["ROOST_TAB_ID"]);
        assert_eq!(
            unrepresentable, 1,
            "a non-UTF-8 ROOST_ name must be counted, so the gap is visible in the log"
        );
    }

    /// A process that never snapshotted keeps its own inherited identity — the non-leader TUI
    /// case, where the inherited variables are the CORRECT ones.
    #[test]
    fn inherited_roost_names_is_empty_without_a_snapshot() {
        assert!(
            inherited_roost_names().is_empty(),
            "only a leader takes the snapshot; every other process must leave hook children's \
             ROOST_* alone"
        );
    }
}
