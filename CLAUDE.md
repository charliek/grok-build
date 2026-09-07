# CLAUDE.md — gx fork maintenance contract

This repo is `gx`: a fork of [`xai-org/grok-build`](https://github.com/xai-org/grok-build)
(the `grok` coding TUI) that adds third-party model providers — Fireworks, Z.AI's GLM
coding plan, OpenRouter, and OpenAI on a ChatGPT/Codex plan — while staying trivially
rebaseable on upstream and coexisting safely with a stock `grok` install on the same
machine and `$GROK_HOME`. The shipped binary is `gx`, installed side by side with stock
`grok`. If you are an agent or human picking up work here with no prior context, read
this file fully before touching anything.

For end-user setup/operations (install, provider setup, coexistence details, known
limitations), see `docs/gx/README.md`. That document assumes gx is already built; this
one is about maintaining the fork itself.

## THE BRANCH MODEL

- **`main` is a pristine mirror of upstream `xai-org/grok-build`'s `main`.**
  Fast-forward-only. **NEVER commit to `main`.** It exists only so `git merge --ff-only
  upstream/main` has somewhere to land and diffs against upstream are trivial.
- **`gx/main` is the default branch.** All gx commits live here, rebased on top of
  `main`. **All feature branches and PRs target `gx/main`**, never `main`.
- **`gx/main` is force-pushed on every upstream rebase.** A local branch based on a
  pre-rebase `gx/main` must be updated with `git rebase --onto gx/main
  <old_gx_main_sha> <your-branch>` after a sync — a plain `git pull` or `git merge` will
  not do the right thing. See `docs/gx/UPSTREAM_SYNC.md`.

## Where gx code lives

Everything gx-specific is either in its own file/module (preferred) or marked inline
with a `// gx:` comment where it has to live inside a file upstream also owns (`grep -rn
"// gx:" crates` or `grep -rn "^//! gx:" crates` to find every touch point):

- **CLI surface:** `crates/codegen/xai-grok-pager/src/providers_cmd.rs` (+
  `providers_cmd_tests.rs`) — the `gx providers install|set-key|unset-key|status|login|token`
  subcommand tree. Wired into `crates/codegen/xai-grok-pager/src/app/cli.rs` (`Command`
  enum) and dispatched in `crates/codegen/xai-grok-pager-bin/src/main.rs`.
- **OpenAI/Codex credentials:** mint/lock live in
  `crates/codegen/xai-grok-login/src/gx_openai_codex.rs` (in-process seam
  in `auth_provider.rs`; upstream extracted auth into `xai-grok-login`).
  Pager `openai_codex_auth.rs` is a thin façade plus `run_login`
  (`codex` PATH lookup is pager diagnostics). The shell still re-exports
  this crate as `xai_grok_shell::auth`.
- **Providers layer (config):** `crates/codegen/xai-grok-config/src/providers_layer.rs`
  (+ `providers_layer_tests.rs`) — loads `$GROK_HOME/providers.toml` and merges it into
  the user config tier, over `config.toml`. Also touches
  `crates/codegen/xai-grok-config/src/loader.rs` and `lib.rs` (`// gx:` markers) at the
  points where the providers layer is spliced into the merge.
- **Sampling/wire compat + codex response shaping:** third-party-model fixes in
  `crates/codegen/xai-grok-sampling-types/` (wire-type compat for non-xAI backends), and
  `// gx:` markers in `crates/codegen/xai-grok-shell/src/sampling/conversation.rs` and
  `crates/codegen/xai-grok-shell/src/session/helpers/session_compact.rs` where the
  codex/OpenAI-compat response shape needs sanitizing.
- **Welcome splash:** `crates/codegen/xai-grok-pager/assets/gx/` (owl braille + SVG
  snapshot) and a `// gx:` gate in
  `crates/codegen/xai-grok-pager/src/views/welcome/logo.rs` (`is_gx_build()` picks the
  owl; same 5/7-row budget as upstream). Never overwrite `assets/logo/`.
- **Remote lane (`gx-remote-api`):** `crates/gx/gx-remote-api/` — the fork's first
  gx-owned *crate* (`crates/gx/` is the home for future ones): a loopback HTTP/SSE façade
  over the leader, attached to it as an **observer** ACP client. Wired up in:
  - `crates/codegen/xai-grok-pager/src/gx_remote_lane.rs` — hosts it inside the leader
    process; called from one `// gx:`-marked hunk in `xai-grok-pager-bin/src/main.rs`'s
    `AgentCmd::Leader` arm.
  - `crates/codegen/xai-grok-pager/src/remote_cmd.rs` (+ `remote_cmd_tests.rs`) — the
    `gx remote status|up` CLI, wired into `app/cli.rs`'s `Command` enum and dispatched in
    `main.rs` beside `Command::Providers`.
  - `crates/codegen/xai-grok-pager/src/doctor_cmd/gx_leader.rs` (+ `gx_leader_tests.rs`)
    — the `gx doctor` leader + lane section; one marked hunk each in `doctor_cmd/mod.rs`,
    `human.rs` and `json.rs`. The section is absent entirely on a stock build, which is
    what keeps upstream's exact-output doctor fixtures passing.
  - `crates/codegen/xai-grok-hooks/src/gx_remote.rs` (+ `gx_remote_tests.rs`) — the
    process-global lane URL, stamped into `to_hook_json()` as `gxRemote` by one marked
    hunk in `event.rs` (roost reads it as `gx.remote`).
  - `crates/codegen/xai-grok-shell/src/leader/` — the `observer` capability
    (`protocol.rs`, five marked hunks in `server.rs`, `server_gx_tests.rs`).
  - Root `Cargo.toml` `members` (one `# gx:`-marked line).

  End-user/API docs: `docs/gx/REMOTE_API.md`.
- **Coexistence:** `crates/codegen/xai-grok-shell/src/leader/lock.rs` (leader socket
  naming), `crates/codegen/xai-grok-update/src/auto_update.rs` (updater neutered),
  `crates/codegen/xai-grok-version/src/lib.rs` (`is_gx_build`, the single build-flavor
  discriminator everything above keys off of).
- **Build/release/sync tooling:** `scripts/gx/` (`build.sh`, `version.sh`,
  `GX_RELEASE`, `sync-upstream.sh`).
- **Docs:** `docs/gx/` (this fork's own docs), plus this file.
- **CI:** `.github/workflows/` (`ci.yml`, `release.yml`, `upstream-drift.yml`) —
  upstream ships no `.github/` at all, so this directory is entirely gx's and never
  conflicts with a sync.

## The `// gx:` marker rule

Any change to a file upstream also owns must be **minimal and surgical**, and marked
with a `// gx:` (or `//! gx:` for a module-level doc comment) comment explaining what and
why. This is not a style preference — it is what makes `docs/gx/UPSTREAM_SYNC.md`'s
rebase runbook tractable: a conflict inside a `// gx:`-marked hunk is expected and
usually mechanical to resolve; a conflict anywhere else is a signal to stop and read
carefully. Grep for the marker before assuming a file is untouched:

```
grep -rn "// gx:" crates
grep -rn "^//! gx:" crates
```

Never make a broad, unmarked change to an upstream file. If a change can live entirely
in a new gx-only file instead, prefer that.

## Build

```
scripts/gx/build.sh [--out <path>]
```

Builds `xai-grok-pager-bin` in release mode with `GROK_VERSION` stamped to
`scripts/gx/version.sh`'s output, then copies the artifact to `gx`. Requires `cargo`,
`python3`, and `protoc` on `PATH`.

The version string is `<upstream-version>+gx.<n>`: `<upstream-version>` comes from the
`[package] version` in `crates/codegen/xai-grok-version/Cargo.toml` (kept in lockstep
with upstream by every rebase — never edit it by hand), `<n>` from the plain integer in
`scripts/gx/GX_RELEASE` (bump this once per gx release cut, not per commit). It is build
metadata (`+gx.N`), never a prerelease tag — deliberately, so it sorts *above* an
otherwise-equal stock version. See `docs/gx/README.md`'s "Version scheme" section for
the operational consequence of that choice.

## Tests — always the targeted set, never the full workspace

The full `xai-grok-shell` suite is huge, and `xai-grok-pager`'s `tests/` directory has a
pre-existing broken target upstream that doesn't link with `--all-targets`. CI, the
weekly drift check, and the sync runbook all use exactly this targeted set — use it
locally too, and do not add `--workspace --all-targets` runs to CI:

```
cargo check -p xai-grok-pager-bin --locked
cargo test -p xai-grok-sampling-types --locked
cargo test -p xai-grok-config --locked
cargo test -p xai-grok-sampler --lib --locked
cargo test -p xai-grok-pager --lib --locked
cargo test -p xai-grok-update --locked -- \
  --skip install_scripts_allow_custom_https_proxy_url \
  --skip install_scripts_refuse_bad_proxy_url_for_deployment_key
cargo test -p xai-grok-version --locked
cargo test -p xai-grok-shell --lib --locked -- leader:: agent::model_providers::tests:: agent::reasoning_family session_compact
cargo test -p xai-grok-login --lib --locked -- auth_provider::tests::resolve_auth_program gx_openai_codex auth_provider::tests::shipped_gx_helper
cargo test -p xai-grok-shell --locked --bin chat-history-downgrade
cargo test -p gx-remote-api --locked
cargo test -p xai-message-delivery-core --locked
```

This is the gate for every commit that touches gx code, every sync, and every PR into
`gx/main` (`.github/workflows/ci.yml`).

## Release procedure

1. Bump `scripts/gx/GX_RELEASE` if this release doesn't already have a bumped value for
   the current upstream base (a rebase alone doesn't require a bump; a new gx feature
   release does).
2. Tag `gx/main` at the commit to release: `gx-v<upstream-version>-gx.<n>`, matching
   what `scripts/gx/version.sh` reports with `+gx.` replaced by `-gx.` — e.g. upstream
   `1.0.8` + `GX_RELEASE=1` tags as `gx-v1.0.8-gx.1`.

   ```
   git tag gx-v1.0.8-gx.1 gx/main
   git push origin gx-v1.0.8-gx.1
   ```
3. Pushing the tag triggers `.github/workflows/release.yml`: builds `macos-arm64` and
   `linux-x86_64`, verifies `gx --version` matches the tag-derived version, packages
   `.tar.gz` + `.sha256`, and publishes a GitHub release with both archives attached.
   The workflow itself asserts the tag-to-version mapping is correct — a mismatched tag
   fails the build rather than shipping a mislabeled artifact.

## Sync procedure

Full runbook: `docs/gx/UPSTREAM_SYNC.md`. Scripted implementation:
`scripts/gx/sync-upstream.sh [--dry-run] [--push]`. Short version: refuse a dirty
worktree, fetch upstream, verify upstream didn't rewrite history, back up `gx/main`,
fast-forward `main`, rebase `gx/main` onto it, abort (don't blind-resolve) on any
conflict outside gx-owned files, run the targeted test gate, range-diff to confirm only
patch movement happened, push with `--force-with-lease` (never a bare force). A weekly
`upstream-drift.yml` workflow runs a non-pushing rebase dry-run and files a tracking
issue if it stops being clean, so drift is visible between real syncs.

## Coexistence rules

Both `gx` and stock `grok` default to `$GROK_HOME=~/.grok` and share it. The rules that
keep them from stepping on each other:

- **Leader socket:** gx binds `$GROK_HOME/gx-leader.sock` / `gx-leader.lock`; stock grok
  binds `leader.sock` / `leader.lock`. This is `leader_file_stem_for(is_gx_build)` in
  `crates/codegen/xai-grok-shell/src/leader/lock.rs` — never hardcode `"leader"`
  anywhere gx-specific; always go through that function (or `is_gx_build()` directly if
  you're deciding gx-vs-stock behavior, not specifically a socket path).
- **Updater:** `xai_grok_version::is_gx_build()` is the single chokepoint
  (`get_installer_for` in `crates/codegen/xai-grok-update/src/auto_update.rs`) that
  makes every update code path a no-op for a gx build. `gx update` prints `gx manages
  its own releases — see docs/gx/README.md` and exits 0. Never reintroduce a path that
  lets a gx build consume a stock grok release, or vice versa.
- **`providers.toml` vs `config.toml`: NEVER put a gx-only provider or model in the
  shared `config.toml`.** Fireworks and openai-codex (and openai-api) stay in
  `$GROK_HOME/providers.toml` (`xai_grok_config::providers_layer`), which only a gx
  build ever reads — an entry stock grok doesn't understand (per-message `model_id` /
  strict schemas, `codex_compat`, an `api_backend` it has no client for) will 400 at
  request time on stock grok. **Stock-compatible** presets (GLM, OpenRouter, Meta) are
  written to `config.toml` by `gx providers install` so stock grok can use them.
  `providers.toml` remains the gx-only overlay for the rest. Do not write a
  stock-compatible entry into both files (the overlay would shadow for gx).

## Secrets

- **No keys in git.** Not in commits, not in fixtures, not in test data beyond an
  obviously-fake placeholder.
- **`providers.toml` is written mode `0600`** and CI's `secret-scan` job
  (`gitleaks`) runs over full history on every push/PR to `gx/main` — don't disable it,
  don't add an allowlist entry to hide a real credential.
- **A provider key is never accepted as a command-line argument.** `gx providers
  set-key` reads from a no-echo TTY prompt or piped stdin only — argv is visible in
  shell history and process listings. Any new credential-entry command must follow the
  same rule.
- Same rule for the OpenAI/codex path: `xai-grok-login/src/gx_openai_codex.rs`
  reads and refreshes `~/.codex/auth.json` and writes its own `.gx-auth.lock`
  beside it (mode `0600`, never a byte inside `auth.json` itself) — never log a
  token, refresh token, or Authorization header, even at debug level.
