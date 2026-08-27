# gx

`gx` is a fork of [`xai-org/grok-build`](https://github.com/xai-org/grok-build)'s `grok`
coding TUI that adds third-party model providers — Fireworks (including Kimi and
DeepSeek), Z.AI's GLM coding plan, OpenRouter, and OpenAI on a ChatGPT/Codex plan — on
top of everything stock `grok` already does. It ships as a separate binary, `gx`,
installed side by side with stock `grok` on the same machine. Both binaries share
`$GROK_HOME` (default `~/.grok`) — config, auth, and sessions are the same files for
both — so this document is as much about **coexistence** as it is about setup. See
`CLAUDE.md` at the repo root for the maintenance/contribution contract; this document is
for *running* gx on a machine, not for hacking on it.

## Version scheme

gx versions look like `<upstream-version>+gx.<n>`, e.g. `1.0.8+gx.1`: the upstream
`grok` version it was built from, plus a `+gx.<n>` **build-metadata** suffix (never a
prerelease tag). That's deliberate:

- semver build metadata sorts *above* an otherwise-equal version, so `1.0.8+gx.1`
  compares as newer than plain `1.0.8` — a stock release of the same upstream version
  never looks like an "upgrade" over the gx build to anything comparing versions.
- it is *not* a prerelease (`-alpha`, `-rc…`), so none of the channel logic that
  special-cases prereleases fires for it.

`scripts/gx/version.sh` computes this string from `crates/codegen/xai-grok-version/Cargo.toml`'s
`[package] version` plus `scripts/gx/GX_RELEASE` (a bare integer, bumped once per gx
release cut). See `docs/gx/UPSTREAM_SYNC.md` and the root `CLAUDE.md` for how the two
combine over an upstream rebase.

One consequence worth knowing about up front: an **org policy that pins
`required_maximum_version` to the exact upstream base version** (e.g. `1.0.8`) will
refuse to start a `1.0.8+gx.1` build — the gx build compares as *above* that ceiling.
This is a real, if narrow, operational trap; see Known limitations below.

## Install

### Install via mise (recommended)

The releases are directly installable with [mise](https://mise.jdx.dev)'s GitHub
backend — verified against this repo's release layout:

```bash
mise use -g "github:charliek/grok-build"            # latest release, activated globally
# or pin: mise install "github:charliek/grok-build@gx-v1.0.10-gx.2"
gx --version
```

mise auto-selects the platform asset (macos-arm64 / linux-x86_64) and exposes the
`gx` binary on PATH via its shims.

### Build from source

```
scripts/gx/build.sh [--out ~/.local/bin/gx]
```

Requires `cargo`, `python3`, and `protoc` on `PATH` (same toolchain as building stock
`grok` — see `rust-toolchain.toml`). The script drives `cargo build -p
xai-grok-pager-bin --release` with `GROK_VERSION` stamped to the gx version string, and
copies the resulting binary to `gx` next to wherever cargo put it. `--out <path>` also
installs a copy at that path (parent directories are created as needed) — e.g.:

```
scripts/gx/build.sh --out ~/.local/bin/gx
```

No crate is renamed: `gx` is a renamed release artifact of the existing
`xai-grok-pager-bin` crate (bin name `xai-grok-pager`). Nothing here touches your `grok`
install.

### Download a release

CI publishes a release for every `gx-v*` tag pushed to `gx/main`
(`.github/workflows/release.yml`), with `macos-arm64` and `linux-x86_64` archives plus
`.sha256` checksums. Tags spell the version as `gx-v<upstream-version>-gx.<n>` — e.g.
`gx-v1.0.8-gx.1` — because a literal `+` in a git tag is legal but awkward to work with
across shells/URLs/filenames; the tag's `-gx.` maps to the `+gx.` in `gx --version`
one-for-one (CI asserts this at release time).

The Linux binary is built on `ubuntu-22.04` and therefore linked against **glibc ≥
2.35**. It will not run on an older glibc (Ubuntu 20.04, Debian 11, and similar). This is
a deliberate tradeoff documented in `.github/workflows/release.yml`, not an oversight —
if you need an older baseline, you currently have to build from source with your own
cross toolchain.

## Provider setup

All gx-only provider/model configuration lives in `$GROK_HOME/providers.toml`
(default `~/.grok/providers.toml`) — **stock `grok` never reads this file.** It is
merged *inside the user tier*, over `config.toml`, so it behaves like any other
user-authority config for everything that reads the effective config (including
`grok inspect`, with one cosmetic caveat noted below).

```
gx providers install
```

Writes the shipped provider/model presets into `providers.toml` (created if absent,
mode `0600`). It's idempotent and merge-aware:

- adds anything missing,
- upgrades a field only while its current value still equals a value gx has *ever*
  shipped as a default (i.e. you never touched it),
- leaves anything you've hand-edited alone unless you pass `--force`,
- reports what it added / upgraded / kept / forced.

`gx providers install` does **not** touch `config.toml` — not one code path in it
writes there.

```
gx providers set-key fireworks
```

Prompts for the key with no terminal echo (or reads it from piped stdin — e.g. `cat
key.txt | gx providers set-key fireworks`). The key is **never** accepted as a
command-line argument, so it never lands in shell history or a process listing.
`gx providers unset-key <provider>` removes a stored key and falls back to that
provider's `env_key`.

There is **no hot reload** — a gx session reads `providers.toml` once at startup.
**Restart any running gx sessions** after `install`, `set-key`, or `unset-key` for the
change to take effect; every mutating command prints a reminder of this.

GLM (Z.AI coding plan) and OpenRouter also work today via plain `config.toml` entries
(same shape works on stock `grok`) or via environment variables:

- GLM: `ZHIPU_API_KEY` or `ZAI_API_KEY`
- OpenRouter: `OPENROUTER_API_KEY`
- Fireworks: `FIREWORKS_API_KEY`

`gx providers install` writes the `env_key` for each of these so an already-exported
variable is picked up with no key ever touching disk.

### OpenAI (ChatGPT plan)

OpenAI runs on your ChatGPT/Codex-plan credentials, not a static API key:

1. `codex login` first — gx does not implement its own OAuth flow; it reads
   `~/.codex/auth.json` (or `$CODEX_HOME/auth.json`), the codex CLI's own credential
   store.
2. `gx providers login openai` is a thin, honest wrapper: it finds `codex` on `PATH` and
   runs `codex login` for you.
3. `gx providers status` shows the credential's expiry, account (redacted), and plan —
   read-only, straight out of `auth.json`.
4. gx refreshes the access token itself only when it is actually expired (matching
   codex's own 5-minute expiry skew), under a gx-owned exclusive lock
   (`.gx-auth.lock`, mode `0600`, next to `auth.json` — never a byte written inside
   `auth.json` itself). No eager/background rotation.

**Rotation race, documented and accepted as residual:** the refresh-token exchange
*rotates* the refresh token — the old one dies the instant a new one is issued. codex-rs
itself takes no lock on `auth.json` and can truncate-rewrite it at any time. gx's lock
only coordinates other gx processes; it cannot make a concurrently-running `codex`
process respect it. The window is small by construction (gx only refreshes when the
token is genuinely expired, at most a few seconds every ~12 hours per machine), but if a
live `codex` process and gx do refresh at the same instant, one of the two refresh
tokens dies. The fix in that case is the same as any expired-credential case: run
`codex login` again.

Run `gx providers status` any time something looks wrong — it is the single diagnostic
entry point for both the providers layer and the codex credential.

## Models shipped

| Provider | Model | Reasoning effort | Default |
|---|---|---|---|
| Fireworks | `fireworks/kimi-k3` | low / medium / high / xhigh / max | high |
| Fireworks | `fireworks/qwen3p8-max` | low / medium / high / xhigh / max | high |
| Fireworks | `fireworks/deepseek-v4-pro` | low / medium / high / xhigh / max | high |
| Fireworks | `fireworks/kimi-k2p7-code` | low / medium / high / xhigh / max | high |
| Fireworks | `fireworks/deepseek-v4-flash` | low / medium / high / xhigh / max | high |
| Z.AI | `glm-5.3` | low / high / max | max |
| Z.AI | `glm-5.3-flash` | low / high / max | high |
| OpenRouter | `openrouter/minimax-m3` | none | n/a |
| OpenRouter | `openrouter/gpt-5.6-sol` | low / medium / high / xhigh | medium |
| OpenRouter | `openrouter/gpt-5.6-terra` | low / medium / high / xhigh | medium |
| OpenRouter | `openrouter/gpt-5.6-luna` | low / medium / high / xhigh | medium |
| OpenAI (ChatGPT plan) | `gpt-5.6-sol` / `gpt-5.6-terra` / `gpt-5.6-luna` | low / medium / high / xhigh | medium |

`glm-5.3-highspeed` also exists on Z.AI's coding-plan API but is tier-gated (the API
returns "current subscription plan does not yet include access"); add it by hand to
`providers.toml` if your plan is upgraded to include it.

`openrouter/glm-5.3-flash` was retired (2026-08-27): the Z.AI coding-plan `glm-5.3-flash`
above now covers the same model directly, so a metered OpenRouter duplicate was redundant.
`gx providers install` never deletes an entry that falls out of the shipped catalog, so an
existing `[model."openrouter/glm-5.3-flash"]` (and, further back, `[model."openrouter/ox-alpha"]`)
from an earlier install is left in place; remove it by hand from `providers.toml` if you no
longer want it.

In its place, OpenRouter now ships `openrouter/minimax-m3` (a cheap 1M-context generalist,
$0.30/$1.20 per M tokens) and OpenRouter twins of the three ChatGPT-plan GPT-5.6 models —
`openrouter/gpt-5.6-sol` / `-terra` / `-luna` — as a metered overflow route for when the
`openai-codex` plan-metered preset is rate-limited or unavailable; sol is currently half of
OpenAI-direct pricing ($2/$10 vs $4/$20 per M) while terra and luna match it. All four were
verified on OpenRouter with tools support, pricing verified 2026-08-27. One pricing nuance:
OpenRouter's `:batch` variants of these models are half-price again but async-only, so they
are not substitutes for this preset's interactive, synchronous use.

Fireworks, GLM, and OpenRouter entries carry `stream_tool_calls = false` and an explicit
`context_window` gx sets itself, since grok's model catalog has no entry for a
third-party id. Run `gx providers status` to see exactly what's configured and where
each value came from (`providers.toml` vs `config.toml` vs environment).

## Coexistence with stock grok

Both binaries default to `$GROK_HOME=~/.grok` and share `config.toml`, auth, and
session state. gx neutralizes the two places that would otherwise collide:

- **Leader socket.** gx binds a distinct leader socket/lock pair,
  `$GROK_HOME/gx-leader.sock` / `gx-leader.lock`, instead of stock grok's
  `leader.sock` / `leader.lock` — so a gx client never attaches to a stock leader or
  vice versa, even though both watch the same `$GROK_HOME`
  (`crates/codegen/xai-grok-shell/src/leader/lock.rs`).
- **Auto-update.** gx's auto-updater is unconditionally disabled: `gx update` (with or
  without `--check`) prints `gx manages its own releases — see docs/gx/README.md` and
  exits 0 instead of touching stock grok's release channel. gx releases only ever come
  from this fork's own GitHub releases (above), never from xAI's update service.
- **Stock grok is unaffected.** It never reads `providers.toml`, never sees the gx
  leader socket, and its own updater behaves exactly as upstream ships it.

One rough edge, cosmetic only: the two binaries share a **models cache** under
`$GROK_HOME`, and each build's provider/model set differs, so alternating between `gx`
and `grok` on the same `$GROK_HOME` causes the cache to be repeatedly rebuilt
("thrash"). It doesn't break anything — just extra I/O on the first run after a switch.

## VM / shed notes

To test gx on Linux (e.g. in an ephemeral `shed`):

1. Build the `linux-x86_64` artifact (from source or a release download) and copy it in.
2. Provide keys either as environment variables (`FIREWORKS_API_KEY`,
   `ZHIPU_API_KEY`/`ZAI_API_KEY`, `OPENROUTER_API_KEY`) or by writing
   `providers.toml` directly — whichever mechanism your shed/VM's env-injection
   actually surfaces to the process (verify empirically; don't assume).
3. **Do not enable shed `--egress`** — its network policy hard-denies the Tailscale
   CGNAT range, which is an unrelated, unnecessary trap for a task that only needs
   outbound HTTPS to the provider APIs.

## Uninstall / rollback

gx installs nothing outside of:

- the `gx` binary itself (wherever you put it — delete it),
- `~/.grok/providers.toml` (gx-only; delete it),
- `~/.codex/.gx-auth.lock`, if present (gx's OpenAI refresh lock; safe to delete any
  time gx isn't actively refreshing).

Everything else under `$GROK_HOME` — `config.toml`, auth, sessions — is shared state
stock `grok` also owns, and gx never modifies it differently than stock grok would.
Removing the three items above returns the machine to a stock-grok-only state with zero
residue.

## Known limitations

Being upfront about the rough edges:

- **Third-party retry/429 tuning is stock-xAI-tuned.** gx does not have bespoke
  backoff/retry curves for Fireworks, Z.AI, OpenRouter, or OpenAI — it inherits
  whatever grok's sampler does for xAI's own API, which may not be ideal for a
  different provider's rate-limit behavior.
- **Session resume degrades across binaries.** A session started against a gx-only
  model (e.g. `fireworks/kimi-k3`) and later resumed from stock `grok` will not resolve
  that model — stock grok has no `providers.toml` and no catalog entry for it.
- **The ChatGPT/Codex backend is private and undocumented.** gx's OpenAI integration
  talks to `https://chatgpt.com/backend-api/codex`, an internal endpoint with no public
  contract. It can drift without notice, and using a ChatGPT-plan credential this way
  sits in a gray area relative to OpenAI's account policies. Accepted risk, not a
  guarantee.
- **`--leader-socket` / `$GROK_LEADER_SOCKET` bypass gx/stock socket separation.**
  These are general-purpose overrides (e.g. for running two branch builds
  side by side) that ignore the gx-vs-stock distinction entirely — if you set one
  explicitly, keeping gx and stock grok from colliding is on you.
- **`grok inspect`-style provenance is cosmetic-wrong for providers.toml values.**
  Because the providers layer merges into the same user tier as `config.toml`,
  provenance reporting attributes a `providers.toml` value to `config.toml`. The value
  itself is correct; only the reported source file is off.
- **`providers.toml` over 1 MiB is skipped entirely at runtime.** It's a small,
  hand-maintained file, so this should never come up in practice — but there's no
  partial-load fallback if it does; gx logs a warning and proceeds as if the file were
  absent.
- **An org policy pinning `required_maximum_version` to the exact upstream base
  version refuses to start gx.** See "Version scheme" above — this is inherent to using
  build metadata to signal a fork build, not a bug to be fixed.
