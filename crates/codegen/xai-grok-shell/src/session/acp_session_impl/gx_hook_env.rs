//! gx: apply a session's roost identity to the hooks it spawns (issue #14).
//!
//! The leader stamps the attaching client's registered identity into the session request's
//! `_meta["gx/hookEnv"]`; `MvpAgent` forwards it here as [`SessionCommand::SetHookEnv`]. This
//! module merges it into every hook spec's `extra_env`, which
//! `xai_grok_hooks::runner::command` applies to the child process BEFORE the `GROK_*` identity
//! vars — so a hook still cannot spoof those, and the hooks crate needs no change at all.
//!
//! ## Why a pristine copy
//!
//! The effective registry is re-derived from a pristine snapshot every time, never patched in
//! place. A user hook may declare its own `env` entry with one of these names; if the identity
//! were layered onto the last derived registry, clearing it (an empty map, which is how a session
//! handed to a client with no identity stops reporting the old tab) would have to *remove* the key
//! and would take the user's value with it. Re-deriving from the pristine copy restores exactly
//! what the user wrote.
//!
//! ## Neutralizing the leader's inherited `ROOST_*`
//!
//! Hook children inherit the leader process's environment, which is whichever TUI spawned it. So
//! before the session's own identity is overlaid, every `ROOST_*` name the leader inherited
//! (`xai_grok_shell::agent::gx_hook_env::inherited_roost_names`, snapshotted read-only at leader
//! startup) is written into the spec's `extra_env` as an EMPTY string. A session with no identity
//! therefore sees `ROOST_AGENT_HOOK=""` and reports nothing at all, instead of impersonating the
//! tab that started the leader; a session with one sees its own values. Nothing outside the hook
//! child's environment is touched — the leader's own environment is never mutated.
//!
//! The neutralization uses `entry().or_insert` so a user hook's explicit `env: {ROOST_…}` still
//! wins over it, exactly as it does when the session has no identity today. The session's own
//! identity is overlaid afterwards and outranks both.

use std::collections::BTreeMap;
use std::sync::Arc;

use xai_grok_hooks::discovery::HookRegistry;

use super::SessionActor;

/// Per-session hook-identity state. One field on [`SessionActor`] so the fork adds one line to
/// each of its constructors, and `Default` so every one of them is the same line.
#[derive(Default)]
pub(crate) struct GxHookEnvState {
    /// The identity currently layered onto the live registry. Empty means "none".
    env: std::cell::RefCell<BTreeMap<String, String>>,
    /// The registry as built (or rebuilt), before any identity was layered on.
    /// Captured lazily on the first apply, and refreshed by [`SessionActor::gx_hook_registry_rebuilt`].
    pristine: std::cell::RefCell<Option<Arc<HookRegistry>>>,
    /// Whether `pristine` has been captured. Distinguishes "not captured yet" from "captured, and
    /// the session genuinely has no hooks".
    pristine_captured: std::cell::Cell<bool>,
}

/// Build the effective registry: `pristine` with every inherited `ROOST_*` name neutralized to an
/// empty string and `env` merged over the top, in every spec's `extra_env`.
///
/// `inherited` is a parameter rather than a direct read of the process-global so the two rules —
/// neutralize, then overlay — are testable without a process-wide snapshot.
fn derive(
    pristine: Option<&Arc<HookRegistry>>,
    env: &BTreeMap<String, String>,
    inherited: &[String],
) -> Option<Arc<HookRegistry>> {
    let base = pristine?;
    if env.is_empty() && inherited.is_empty() {
        return Some(base.clone());
    }
    let mut specs = (**base).clone().into_specs();
    for spec in &mut specs {
        for name in inherited {
            // `or_insert`, not `insert`: a user hook that set one of these itself keeps its value,
            // the same as it does on a build that never inherited anything.
            spec.extra_env.entry(name.clone()).or_default();
        }
        for (key, value) in env {
            spec.extra_env.insert(key.clone(), value.clone());
        }
    }
    let mut derived = HookRegistry::default();
    derived.append_specs(specs);
    // `matcher` survives the clone, but rebuilding from `configured_matcher` is what every other
    // registry-rebuild path in this crate does; keep the invariant identical.
    derived.recompile_matchers();
    Some(Arc::new(derived))
}

impl SessionActor {
    /// The identity this session's hooks currently export. Empty when it has none.
    pub(crate) fn gx_hook_env(&self) -> BTreeMap<String, String> {
        self.gx_hook_env.env.borrow().clone()
    }

    /// Apply `env` as this session's hook identity, re-deriving the live registry from the
    /// pristine one.
    ///
    /// Returns `true` when the session's identity moved from one non-empty tab to a DIFFERENT
    /// non-empty tab. roost claims a tab only on `SessionStart` (every later event needs an
    /// existing owner for that session), so the caller must re-fire `SessionStart` with source
    /// `"resume"` for the new tab to take ownership. A first claim on a cold spawn does not need
    /// it: the `SessionStart` the spawn already dispatches carries the identity, because this
    /// command is queued ahead of it.
    pub(crate) fn gx_apply_hook_env(&self, env: BTreeMap<String, String>) -> bool {
        let env = crate::agent::gx_hook_env::validate(env);
        let previous = self.gx_hook_env.env.replace(env.clone());
        if !self.gx_hook_env.pristine_captured.get() {
            let current = self.hook_registry.borrow().clone();
            *self.gx_hook_env.pristine.borrow_mut() = current;
            self.gx_hook_env.pristine_captured.set(true);
        }
        self.gx_rederive_hook_registry();
        tracing::debug!(
            session_id = %self.session_info.id.0,
            entries = env.len(),
            "gx: applied hook env to the session's hook registry"
        );
        !previous.is_empty() && !env.is_empty() && previous != env
    }

    /// Put the pristine registry back as the live one, dropping the layered identity.
    ///
    /// For a reload path that mutates the LIVE registry in place (`Arc::make_mut`) rather than
    /// replacing it: without this the identity would be baked into the next pristine snapshot, and
    /// a later clear could no longer restore a user hook's own value of the same name. Pair every
    /// call with [`Self::gx_hook_registry_rebuilt`] once the reload has finished.
    /// A no-op before the first apply, when there is nothing layered on and no snapshot to restore.
    pub(crate) fn gx_restore_pristine_hook_registry(&self) {
        if !self.gx_hook_env.pristine_captured.get() {
            return;
        }
        let pristine = self.gx_hook_env.pristine.borrow().clone();
        *self.hook_registry.borrow_mut() = pristine;
    }

    /// Re-capture the pristine registry after the session rebuilt it (plugin/hook reload), then
    /// re-layer the identity so a reload does not silently drop the tab.
    pub(crate) fn gx_hook_registry_rebuilt(&self) {
        let rebuilt = self.hook_registry.borrow().clone();
        *self.gx_hook_env.pristine.borrow_mut() = rebuilt;
        self.gx_hook_env.pristine_captured.set(true);
        self.gx_rederive_hook_registry();
    }

    fn gx_rederive_hook_registry(&self) {
        let derived = {
            let pristine = self.gx_hook_env.pristine.borrow();
            let env = self.gx_hook_env.env.borrow();
            derive(
                pristine.as_ref(),
                &env,
                crate::agent::gx_hook_env::inherited_roost_names(),
            )
        };
        *self.hook_registry.borrow_mut() = derived;
    }

    /// Re-fire `SessionStart` so roost's claim moves to the tab that just took the session over.
    /// Mirrors `SessionCommand::DispatchSessionStartHook`'s arm with source `"resume"`.
    pub(crate) async fn gx_dispatch_session_start_for_resume(&self) {
        let envelope = self.fire_hook(
            xai_grok_hooks::event::HookEventName::SessionStart,
            None,
            xai_grok_hooks::event::HookPayload::SessionStart {
                source: "resume".to_string(),
                model_id: None,
                agent_type: None,
            },
        );
        let Some(registry) = self.hook_registry.borrow().clone() else {
            return;
        };
        let ctx = self.hook_run_ctx();
        let results = xai_grok_hooks::dispatcher::dispatch_non_blocking(
            &registry,
            xai_grok_hooks::event::HookEventName::SessionStart,
            &envelope,
            &ctx,
        )
        .await;
        self.send_hook_execution("session_start", None, None, &results)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot a leader that inherited no `ROOST_*` at all took — and what every non-leader
    /// process has. Most of these cases are about the identity overlay, not the neutralization.
    const NONE_INHERITED: &[String] = &[];

    /// The names a leader spawned from inside a roost tab would have snapshotted: the three
    /// carried ones plus a lease, which is a bearer credential and never carried.
    fn inherited(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn tab(id: &str) -> BTreeMap<String, String> {
        env(&[
            ("ROOST_AGENT_HOOK", "/usr/local/bin/roost"),
            ("ROOST_SOCKET", "/run/roost.sock"),
            ("ROOST_TAB_ID", id),
        ])
    }

    /// A spec carrying a user-authored `env` of its own, one of whose keys collides with a
    /// carried name.
    fn spec_with_env(name: &str, extra: &[(&str, &str)]) -> xai_grok_hooks::config::HookSpec {
        xai_grok_hooks::config::HookSpec {
            name: name.to_string(),
            event: xai_grok_hooks::event::HookEventName::SessionStart,
            handler_type: xai_grok_hooks::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: Some("report-tab.sh".into()),
            command_raw: Some("report-tab.sh".to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 1000,
            source_dir: std::path::PathBuf::from("/tmp"),
            extra_env: extra
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            layer: xai_grok_hooks::config::HookProvenance::User,
        }
    }

    fn registry(specs: Vec<xai_grok_hooks::config::HookSpec>) -> Arc<HookRegistry> {
        let mut reg = HookRegistry::default();
        reg.append_specs(specs);
        Arc::new(reg)
    }

    fn extra_env_of(reg: &HookRegistry, hook: &str) -> BTreeMap<String, String> {
        reg.find_by_name(hook)
            .expect("hook must survive the derivation")
            .extra_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    #[test]
    fn derive_merges_the_identity_into_every_spec() {
        let base = registry(vec![
            spec_with_env("a", &[]),
            spec_with_env("b", &[("MY_OWN", "keep-me")]),
        ]);
        let derived =
            derive(Some(&base), &tab("5"), NONE_INHERITED).expect("a non-empty registry derives");

        for hook in ["a", "b"] {
            assert_eq!(
                extra_env_of(&derived, hook)
                    .get("ROOST_TAB_ID")
                    .map(String::as_str),
                Some("5"),
                "every hook of the session must report the session's tab"
            );
        }
        assert_eq!(
            extra_env_of(&derived, "b")
                .get("MY_OWN")
                .map(String::as_str),
            Some("keep-me"),
            "a user hook's own env entry of a DIFFERENT name must survive"
        );
        assert_eq!(
            extra_env_of(&base, "a").get("ROOST_TAB_ID"),
            None,
            "the pristine registry must not be mutated by a derivation"
        );
    }

    #[test]
    fn derive_from_pristine_restores_a_user_value_the_identity_had_shadowed() {
        // The whole reason the pristine copy exists: this hook sets ROOST_TAB_ID itself.
        let base = registry(vec![spec_with_env(
            "mine",
            &[("ROOST_TAB_ID", "user-chosen")],
        )]);

        let with_tab = derive(Some(&base), &tab("5"), NONE_INHERITED).unwrap();
        assert_eq!(
            extra_env_of(&with_tab, "mine")
                .get("ROOST_TAB_ID")
                .map(String::as_str),
            Some("5"),
            "the session's identity wins while the session has one"
        );

        // Clearing re-derives from pristine, rather than removing the key from `with_tab`.
        let cleared = derive(Some(&base), &BTreeMap::new(), NONE_INHERITED).unwrap();
        assert_eq!(
            extra_env_of(&cleared, "mine")
                .get("ROOST_TAB_ID")
                .map(String::as_str),
            Some("user-chosen"),
            "clearing the identity must restore the user's own value, not erase the key"
        );
    }

    #[test]
    fn derive_of_an_empty_identity_is_the_pristine_registry() {
        let base = registry(vec![spec_with_env("a", &[])]);
        let derived = derive(Some(&base), &BTreeMap::new(), NONE_INHERITED).unwrap();
        assert!(
            extra_env_of(&derived, "a").is_empty(),
            "no identity must stamp nothing at all (the pre-gx behaviour)"
        );
    }

    #[test]
    fn derive_without_a_registry_stays_absent() {
        assert!(
            derive(None, &tab("5"), NONE_INHERITED).is_none(),
            "a session with no hooks must not gain one"
        );
    }

    // ── neutralizing what the leader inherited ──────────────────────

    /// A session with NO identity must see every inherited `ROOST_*` as an empty string, so
    /// roost's hook command takes its `else` branch and the session reports nothing — rather than
    /// inheriting, and impersonating, the tab that started the leader.
    #[test]
    fn a_session_with_no_identity_sees_every_inherited_roost_name_empty() {
        let leader_env = inherited(&[
            "ROOST_AGENT_HOOK",
            "ROOST_LEASE",
            "ROOST_SOCKET",
            "ROOST_TAB_ID",
        ]);
        let base = registry(vec![spec_with_env("a", &[("MY_OWN", "keep-me")])]);

        let derived = derive(Some(&base), &BTreeMap::new(), &leader_env)
            .expect("a non-empty registry derives");
        let got = extra_env_of(&derived, "a");

        for name in &leader_env {
            assert_eq!(
                got.get(name).map(String::as_str),
                Some(""),
                "{name} must reach the hook child empty: the leader's own environment is never \
                 mutated, so the child's is the only place this can be neutralized"
            );
        }
        assert_eq!(
            got.get("MY_OWN").map(String::as_str),
            Some("keep-me"),
            "neutralization touches ROOST_* only"
        );
        assert!(
            extra_env_of(&base, "a").get("ROOST_TAB_ID").is_none(),
            "the pristine registry must not be mutated by a derivation"
        );
    }

    /// A session WITH an identity sees its own three values, and an empty string for any OTHER
    /// inherited `ROOST_*` — notably `ROOST_LEASE`, a bearer credential that is not carried and
    /// must not reach a hook child.
    #[test]
    fn a_session_with_an_identity_overrides_the_neutralized_names() {
        let leader_env = inherited(&[
            "ROOST_AGENT_HOOK",
            "ROOST_LEASE",
            "ROOST_SOCKET",
            "ROOST_TAB_ID",
        ]);
        let base = registry(vec![spec_with_env("a", &[])]);

        let derived =
            derive(Some(&base), &tab("5"), &leader_env).expect("a non-empty registry derives");
        let got = extra_env_of(&derived, "a");

        assert_eq!(got.get("ROOST_TAB_ID").map(String::as_str), Some("5"));
        assert_eq!(
            got.get("ROOST_SOCKET").map(String::as_str),
            Some("/run/roost.sock")
        );
        assert_eq!(
            got.get("ROOST_AGENT_HOOK").map(String::as_str),
            Some("/usr/local/bin/roost"),
            "the session's own identity must win over the neutralization"
        );
        assert_eq!(
            got.get("ROOST_LEASE").map(String::as_str),
            Some(""),
            "an inherited lease is a bearer credential the session never carried; it must not \
             reach a hook child even when the session HAS an identity"
        );
    }

    /// The neutralization must not stomp a hook that deliberately sets one of these itself: that
    /// is the same value it would have gotten on a build with no inherited variables at all.
    #[test]
    fn a_user_hooks_own_roost_value_outranks_the_neutralization() {
        let leader_env = inherited(&["ROOST_TAB_ID"]);
        let base = registry(vec![spec_with_env(
            "mine",
            &[("ROOST_TAB_ID", "user-chosen")],
        )]);

        let derived = derive(Some(&base), &BTreeMap::new(), &leader_env).unwrap();
        assert_eq!(
            extra_env_of(&derived, "mine")
                .get("ROOST_TAB_ID")
                .map(String::as_str),
            Some("user-chosen"),
            "an explicit `env:` entry in the hook's own config still wins"
        );
    }

    // ── through the live actor ──────────────────────────────────────

    async fn actor_with_hooks(specs: Vec<xai_grok_hooks::config::HookSpec>) -> SessionActor {
        let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
        let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
        let actor =
            super::super::support::create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx)
                .await;
        *actor.hook_registry.borrow_mut() = Some(registry(specs));
        actor
    }

    fn live_extra_env(actor: &SessionActor, hook: &str) -> BTreeMap<String, String> {
        let registry = actor
            .hook_registry
            .borrow()
            .clone()
            .expect("the session must still have a registry");
        extra_env_of(&registry, hook)
    }

    /// The whole point, through the command the leader's stamp turns into: every hook of the
    /// session exports the tab, and a user hook's own `env` of a different name is untouched.
    #[tokio::test(flavor = "current_thread")]
    async fn set_hook_env_stamps_every_spec_and_spares_a_user_env_entry() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let actor = actor_with_hooks(vec![
                    spec_with_env("plain", &[]),
                    spec_with_env("with-own-env", &[("MY_OWN", "keep-me")]),
                ])
                .await;

                let claimed = actor.gx_apply_hook_env(tab("5"));
                assert!(
                    !claimed,
                    "a first claim rides the SessionStart the spawn already dispatches"
                );
                assert_eq!(actor.gx_hook_env(), tab("5"));

                for hook in ["plain", "with-own-env"] {
                    assert_eq!(
                        live_extra_env(&actor, hook)
                            .get("ROOST_TAB_ID")
                            .map(String::as_str),
                        Some("5"),
                        "{hook} must export the session's tab"
                    );
                }
                assert_eq!(
                    live_extra_env(&actor, "with-own-env")
                        .get("MY_OWN")
                        .map(String::as_str),
                    Some("keep-me"),
                    "a user hook's own env entry must survive the stamp"
                );
            })
            .await;
    }

    /// A hand-over between two live tabs must ask for a fresh `SessionStart`; every other
    /// transition must not, or roost would see a claim it did not need.
    #[tokio::test(flavor = "current_thread")]
    async fn only_a_live_tab_handover_asks_for_a_resume_session_start() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let actor = actor_with_hooks(vec![spec_with_env("plain", &[])]).await;

                assert!(!actor.gx_apply_hook_env(tab("5")), "first claim: no resume");
                assert!(
                    !actor.gx_apply_hook_env(tab("5")),
                    "re-applying the SAME tab is not a hand-over"
                );
                assert!(
                    actor.gx_apply_hook_env(tab("6")),
                    "tab 5 -> tab 6 must re-fire SessionStart; roost claims only on that event"
                );
                assert!(
                    !actor.gx_apply_hook_env(BTreeMap::new()),
                    "clearing hands the session to nobody, so there is no new owner to claim it"
                );
                assert!(
                    !actor.gx_apply_hook_env(tab("7")),
                    "claiming from an unclaimed session is a first claim, not a hand-over"
                );
            })
            .await;
    }

    /// Clearing must restore the pristine registry, not leave the last tab's values behind.
    #[tokio::test(flavor = "current_thread")]
    async fn clearing_removes_the_stamp_and_restores_a_user_value() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let actor = actor_with_hooks(vec![spec_with_env(
                    "mine",
                    &[("ROOST_TAB_ID", "user-chosen")],
                )])
                .await;

                actor.gx_apply_hook_env(tab("5"));
                assert_eq!(
                    live_extra_env(&actor, "mine")
                        .get("ROOST_SOCKET")
                        .map(String::as_str),
                    Some("/run/roost.sock"),
                );

                actor.gx_apply_hook_env(BTreeMap::new());
                assert_eq!(
                    live_extra_env(&actor, "mine").get("ROOST_SOCKET"),
                    None,
                    "clearing must remove what the session stamped"
                );
                assert_eq!(
                    live_extra_env(&actor, "mine")
                        .get("ROOST_TAB_ID")
                        .map(String::as_str),
                    Some("user-chosen"),
                    "and must restore the user hook's own value it had shadowed"
                );
            })
            .await;
    }

    /// A registry rebuild (hook/plugin reload) re-captures pristine and re-layers the identity,
    /// so a reload does not silently drop the tab.
    #[tokio::test(flavor = "current_thread")]
    async fn a_registry_rebuild_re_layers_the_identity() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let actor = actor_with_hooks(vec![spec_with_env("old", &[])]).await;
                actor.gx_apply_hook_env(tab("5"));

                // Stand in for a reload: replace the registry wholesale, then announce it.
                *actor.hook_registry.borrow_mut() = Some(registry(vec![spec_with_env("new", &[])]));
                actor.gx_hook_registry_rebuilt();

                assert_eq!(
                    live_extra_env(&actor, "new")
                        .get("ROOST_TAB_ID")
                        .map(String::as_str),
                    Some("5"),
                    "a hook that appeared during a reload must still report the session's tab"
                );
            })
            .await;
    }

    /// A client cannot smuggle an arbitrary variable in as an identity, even past the leader.
    #[tokio::test(flavor = "current_thread")]
    async fn an_invalid_identity_is_rejected_at_the_actor_too() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let actor = actor_with_hooks(vec![spec_with_env("plain", &[])]).await;
                let mut forged = tab("5");
                forged.insert("PATH".to_string(), "/tmp/evil".to_string());

                actor.gx_apply_hook_env(forged);

                assert!(actor.gx_hook_env().is_empty());
                assert!(
                    live_extra_env(&actor, "plain").is_empty(),
                    "a rejected identity must stamp nothing at all"
                );
            })
            .await;
    }
}
