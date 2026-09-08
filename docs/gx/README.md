# gx

`gx` is a fork of [`xai-org/grok-build`](https://github.com/xai-org/grok-build)'s `grok`
coding TUI that adds third-party model providers — Fireworks (including Kimi and
DeepSeek), Z.AI's GLM coding plan, OpenRouter, Meta Muse Spark, and OpenAI on a
ChatGPT/Codex plan — on top of everything stock `grok` already does. It ships as a separate binary, `gx`,
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
# or pin: mise install "github:charliek/grok-build@gx-v1.0.10-gx.4"
gx --version
```

mise auto-selects the platform asset (macos-arm64 / linux-x86_64) and exposes the
`gx` binary on PATH via its shims.

**Fresh releases take up to a day to appear via `latest`.** mise's default
`minimum_release_age` is 24 hours (a supply-chain cooling-off window), so right after
a gx cut, `mise upgrade github:charliek/grok-build` or `@latest` will report "up to
date" on the previous tag. To take a new release immediately, pin the exact tag as
above, or bypass the age filter once:

```bash
mise upgrade github:charliek/grok-build --minimum-release-age 0
```

After 24 hours a plain `mise upgrade github:charliek/grok-build` picks it up normally.

`gx providers install` writes `openai-codex.auth.command = "gx"`. That sentinel
means the running gx binary mints ChatGPT tokens **in-process** — it never
PATH-execs `gx`, and a leftover absolute path from an older install with the
same helper args is also intercepted. A mise upgrade and `target/release/gx`
therefore do **not** need `gx providers install` for ChatGPT turns to keep
working. Restart the session after switching binaries; there is no hot reload.

Re-run `gx providers install` after `codex login` to a **different** ChatGPT
account, so the baked `chatgpt-account-id` header matches. `gx providers token
openai` still exists for debugging (same JSON stdout the helper used to print).

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

gx-only provider/model configuration (Fireworks, openai-codex, openai-api) lives in
`$GROK_HOME/providers.toml` (default `~/.grok/providers.toml`) — **stock `grok` never
reads this file.** It is merged *inside the user tier*, over `config.toml`, so it
behaves like any other user-authority config for everything that reads the effective
config (including `grok inspect`, with one cosmetic caveat noted below).

Stock-compatible presets (GLM, OpenRouter, Meta) are written to the shared
`$GROK_HOME/config.toml` so stock `grok` can use them too. They are **not** also
copied into `providers.toml` (that overlay would shadow for gx).

```
gx providers install
```

Writes the shipped provider/model presets into the matching file (created if absent,
mode `0600`): stock-compatible shapes into `config.toml`, gx-only shapes into
`providers.toml`. It's idempotent and merge-aware:

- adds anything missing,
- upgrades a field only while its current value still equals a value gx has *ever*
  shipped as a default (i.e. you never touched it),
- leaves anything you've hand-edited alone unless you pass `--force`,
- reports what it added / upgraded / kept / forced,
- preserves unrelated `config.toml` tables (`[cli]`, `[ui]`, `[plugins]`, …).

```
gx providers set-key fireworks
```

Prompts for the key with no terminal echo (or reads it from piped stdin — e.g. `cat
key.txt | gx providers set-key fireworks`). The key is **never** accepted as a
command-line argument, so it never lands in shell history or a process listing.
`gx providers unset-key <provider>` removes a stored key and falls back to that
provider's `env_key`.

There is **no hot reload** — a session reads `config.toml` / `providers.toml` once at
startup. **Restart any running gx (and, for stock-compatible entries, grok) sessions**
after `install`, `set-key`, or `unset-key` for the change to take effect; every
mutating command prints a reminder of this.

Keys can also come from environment variables (`gx providers install` writes the
`env_key` for each so an already-exported variable is picked up with no key ever
touching disk):

- GLM: `ZHIPU_API_KEY` or `ZAI_API_KEY`
- OpenRouter: `OPENROUTER_API_KEY`
- Meta: `META_API_KEY` or `MODEL_API_KEY` (Muse CLI name first)
- Fireworks: `FIREWORKS_API_KEY`

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
| OpenRouter | `openrouter/gemini-3.8-flash` | low / medium / high | medium |
| Meta | `muse-spark-1.3` | minimal / low / medium / high / xhigh | high |
| Meta | `muse-spark-1.3-contributor` | minimal / low / medium / high / xhigh | high |
| OpenAI (ChatGPT plan) | `gpt-6-astra` | low / medium / high / xhigh / max | low |
| OpenAI (ChatGPT plan) | `gpt-5.6-sol` | low / medium / high / xhigh / max | low |
| OpenAI (ChatGPT plan) | `gpt-5.6-terra` / `gpt-5.6-luna` | low / medium / high / xhigh / max | medium |

`muse-spark-1.3` is the Standard tier: prompts are not used for training.
`muse-spark-1.3-contributor` is the discounted Contributor tier: your content,
including inter-session messages, may be used for product improvement.

`glm-5.3-highspeed` also exists on Z.AI's coding-plan API but is tier-gated (the API
returns "current subscription plan does not yet include access"); add it by hand to
`config.toml` if your plan is upgraded to include it.

`openrouter/glm-5.3-flash` was retired (2026-08-27): the Z.AI coding-plan `glm-5.3-flash`
above now covers the same model directly, so a metered OpenRouter duplicate was redundant.
`gx providers install` never deletes an entry that falls out of the shipped catalog, so an
existing `[model."openrouter/glm-5.3-flash"]` (and, further back, `[model."openrouter/ox-alpha"]`)
from an earlier install is left in place; remove it by hand from the file `install` wrote
it to (`config.toml` for current stock-compatible installs, or `providers.toml` if it
was written by an older gx) if you no longer want it.

OpenRouter ships `openrouter/minimax-m3` (a cheap 1M-context generalist,
$0.30/$1.20 per M tokens, verified 2026-08-27) and
`openrouter/gemini-3.8-flash`. OpenRouter's public catalog reports Gemini 3.8 Flash with a
1,048,576-token context window, 65,536 maximum output tokens, tool calling, and mandatory
low / medium / high reasoning (medium by default), verified 2026-09-05.

The metered OpenRouter copies of `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna` were
removed from the shipped catalog on 2026-09-05; the direct ChatGPT-plan entries remain under
`openai-codex`, alongside the new `gpt-6-astra`. Existing
`[model."openrouter/gpt-5.6-..."]` tables remain in `config.toml` because
`gx providers install` preserves entries that leave the catalog; delete those three tables
by hand if an earlier gx installed them and you no longer want them in `/model`.

`gpt-6-astra` uses Codex's current 272,000-token active context window and supports
low / medium / high / xhigh / max reasoning. Its default is low, matching the Codex
catalog. Availability depends on OpenAI's rollout, your ChatGPT plan, and workspace policy.

Fireworks, GLM, OpenRouter, and Meta entries carry `stream_tool_calls = false` and an
explicit `context_window` gx sets itself, since grok's model catalog has no entry for a
third-party id. Run `gx providers status` to see exactly what's configured and where
each value came from (`providers.toml` vs `config.toml` vs environment).

### `tool_result_images`

When a tool returns an image (`read_file` on a screenshot, say), grok puts the image
*inside* the `tool` message. That is an xAI extension — the OpenAI Chat Completions spec
allows only text there — and Meta's Muse gateway rejects it with
`messages[N].content did not match any supported type`. So on any **non-xAI Chat
Completions** provider gx hoists those images into a short `user` message emitted right
after the tool message(s) that produced them; xAI and the Responses/Messages backends keep
the inline shape. Set `tool_result_images = "inline"` or `"hoist"` on a `[model."<id>"]`
table in `config.toml` (or `providers.toml`) to override that per-provider default if your
endpoint disagrees with the guess.

A loopback base URL is treated as xAI's cli-chat-proxy, so a local OpenAI-compatible server
(Ollama, LM Studio) keeps the inline shape by default. That is the behaviour it already had;
set `tool_result_images = "hoist"` on it if its server rejects images in a tool message.

### `supports_vision`

Some third-party endpoints reject an image outright, in any role, regardless of how its
tool-result images are shaped. Set `supports_vision = false` on a `[model."<id>"]` table to
say so; gx then strips every image from the request before the first attempt instead of
paying a guaranteed failed request to find out. `glm-5.3` ships with this set, since Z.AI's
coding-plan endpoint 400s on an image anywhere in the request
(`messages.content.type is invalid, allowed values: ['text']`, verified live);
`glm-5.3-flash` is vision-capable and does not carry it. The image itself is never deleted —
it stays in the conversation transcript, and only the outgoing request drops it, so it comes
back if you later switch to a vision-capable model. The key is absent by default (meaning
`true`); stock grok does not understand it and logs a harmless unknown-field warning since
the GLM preset lives in the shared `config.toml`.

## Remote lane

A gx build starts a **leader** by default (stock grok does not), and that leader hosts a
loopback HTTP/SSE façade over your sessions on loopback — the "remote lane". It
exists so a phone on an SSH port-forward can list sessions, follow a turn, send a prompt
and answer an approval, while the TUI on your desk keeps working on the same session.

```bash
gx remote status          # what is listening, on which port, and is it healthy
gx remote up              # start a leader (and therefore a lane) if there is none
gx doctor                 # the leader decision, the socket/lock, the token, the lanes
```

The port defaults to **2421** but is not fixed: `GX_REMOTE_PORT` overrides it, a leader on a
non-default relay always takes an ephemeral one, and a lane whose preferred port is busy falls
back to an ephemeral port rather than failing to start. Read the real URL from `gx remote status`
or from the `url` field of the discovery record — never assume the default when scripting.

Note that a plain `gx` already starts both, so `gx remote up` is for the case where no gx is
running and you want the lane anyway. To go the other way: `GX_REMOTE_DISABLE=1` keeps the leader
but starts no lane and opens no port, while `gx --no-leader` or `[cli] use_leader = false` stops
the detached leader from existing at all, and therefore the lane with it.

Two things you need in order to talk to it, both under `$GROK_HOME`:
`gx-remote.json` (the discovery record — URL, pid, instance id) and `gx-remote.token`
(the bearer token, mode `0600`). Neither `gx remote` nor `gx doctor` ever prints the
token itself, only its path.

The lane is loopback-only and there is no TLS: reaching it from another machine is SSH's
job.

```bash
# Ask the far side which port it actually bound rather than assuming 2421.
PORT=$(ssh host 'python3 -c "import json,glob;print(json.load(open(glob.glob(\"${GROK_HOME:-$HOME/.grok}/gx-remote*.json\")[0]))[\"url\"].rsplit(\":\",1)[1])"')
ssh -N -L "$PORT:127.0.0.1:$PORT" host &
TOKEN=$(ssh host 'cat "${GROK_HOME:-$HOME/.grok}/gx-remote.token"')
curl -s "http://127.0.0.1:$PORT/v1/healthz"       # no token needed; match instanceId first
curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/v1/sessions"
```

`ssh host gx remote status` is the readable version of that first line if a gx is on the far
side's `PATH`.

**Full reference — every endpoint, the SSE resume contract, the approval bodies, the
error codes and the accepted risks: [`docs/gx/REMOTE_API.md`](REMOTE_API.md).**

To turn it off, see "Coexistence" below.

## Hook environment and roost tabs

One leader serves every TUI on the machine, and it runs every session's hooks — so a hook
cannot simply read the leader's own environment to work out who it is reporting to. It
would report to whichever terminal happened to start the leader first, which for
[roost](https://github.com/charliek/roost) means every session's events landing in one
arbitrary tab.

So the identity travels **per client**. A TUI launched inside a roost tab carries
`ROOST_TAB_ID`, `ROOST_SOCKET` and `ROOST_AGENT_HOOK` in its environment; gx registers
those three with the leader, and the leader stamps them into the hook environment of the
sessions **that client** creates or attaches to. All three or none: a partial set is
dropped, because a hook with a socket and no tab id has nothing useful to say.

Sessions that carry no identity — the ones the remote lane creates for a phone, headless
runs, and any TUI started outside a roost tab — get every `ROOST_*` name the leader
inherited set to the **empty string** in their hooks' environment, rather than left to
whatever the leader happened to inherit. That is deliberate: an empty `ROOST_AGENT_HOOK`
makes roost's hook take its no-op branch, so such a session is simply invisible to roost
instead of reporting into whichever tab started the leader.

The values are taken from the client's registration and never from a request body, and
attaching from the phone never changes a session's identity — only a TUI can, and only for
its own sessions.

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
- **Leader on by default, and a loopback listener with it.** Unlike stock grok, a gx
  build starts a leader when nothing says otherwise, and that leader binds a loopback port
  for the remote lane (2421 by default; see above for when it differs). Both are opt-out:

  | knob | effect |
  |---|---|
  | `gx --no-leader` | no leader, therefore no lane, for that invocation |
  | `[cli] use_leader = false` in `config.toml` | no leader by default — note this is the **shared** config, so it turns leader mode off for stock grok too |
  | `GX_REMOTE_DISABLE=1` | leader as usual, **no lane** |
  | `GX_REMOTE_PORT=<n>` | a different loopback port (busy ports fall back to an ephemeral one; an unparseable value warns and falls back to the default port) |
  | `gx leader kill` | stop the running leaders, and their lanes, now — gracefully: each leader flushes its sessions (running their `SessionEnd` hooks) before exiting, and the command waits up to 15s per leader and exits non-zero if one is still running |

  `gx doctor` prints which of these is in force, plus the socket, the lock's pid, and
  whether the token file exists with mode `0600`.
- **What stock grok does and does not share.** It never reads `providers.toml`, never
  binds the gx leader socket (the stem differs), never starts a lane or opens a port, and
  its own updater behaves exactly as upstream ships it. `config.toml` *is* shared, though:
  gx only changes the built-in **default** for `[cli] use_leader`, so if you write that key
  yourself, stock grok reads the same value out of the same file.
- **Splash mark.** A gx binary paints the StrideLabs owl on the welcome screen (and
  the compact minimal-mode card). Stock `grok` still shows the Grok `g`. Copy next to
  the mark ("Grok Build", version badge) is unchanged.

One rough edge, cosmetic only: the two binaries share a **models cache** under
`$GROK_HOME`, and each build's provider/model set differs, so alternating between `gx`
and `grok` on the same `$GROK_HOME` causes the cache to be repeatedly rebuilt
("thrash"). It doesn't break anything — just extra I/O on the first run after a switch.

## VM / shed notes

To test gx on Linux (e.g. in an ephemeral `shed`):

1. Build the `linux-x86_64` artifact (from source or a release download) and copy it in.
2. Provide keys either as environment variables (`FIREWORKS_API_KEY`,
   `ZHIPU_API_KEY`/`ZAI_API_KEY`, `OPENROUTER_API_KEY`, `META_API_KEY`/`MODEL_API_KEY`)
   or by writing `providers.toml` / `config.toml` directly — whichever mechanism your
   shed/VM's env-injection actually surfaces to the process (verify empirically; don't
   assume).
3. **Do not enable shed `--egress`** — its network policy hard-denies the Tailscale
   CGNAT range, which is an unrelated, unnecessary trap for a task that only needs
   outbound HTTPS to the provider APIs.

## Uninstall / rollback

gx installs nothing outside of:

- the `gx` binary itself (wherever you put it — delete it),
- `~/.grok/providers.toml` (gx-only; delete it),
- `~/.codex/.gx-auth.lock`, if present (gx's OpenAI refresh lock; safe to delete any
  time gx isn't actively refreshing).

Stock-compatible presets (GLM, OpenRouter, Meta) are written into the shared
`config.toml`. Leave those entries if you still want stock `grok` to use them; delete
the `[model_providers.<id>]` / `[model.<id>]` tables by hand if you want them gone.
Everything else under `$GROK_HOME` — the rest of `config.toml`, auth, sessions — is
shared state stock `grok` also owns. Removing the gx-only items above plus any
stock-compatible tables you no longer want returns the machine to a stock-grok-only
state.

## Known limitations

Being upfront about the rough edges:

- **Mid-session provider switches.** Codex (ChatGPT plan) sealed reasoning
  (`encrypted_content`) is not decryptable by Grok, and Grok's sealed
  blobs are not decryptable by Codex. A **user** `/model` switch between
  those two may lossily compact (upstream family-switch). Independently,
  gx drops foreign sealed reasoning on the Codex wire and, on a Grok
  `encrypted_content` 400, strips sealed blobs from that request and
  retries once. Chat Completions hops (GLM, Kimi, OpenRouter, Meta) keep
  history: they ignore item ids and sealed blobs. Resume/load does not
  compact. Empty-id reasoning from GLM/Fireworks no longer 400s Codex
  (`1.0.12+gx.6` and this change). A `1.0.10+gx.4` binary does not have
  these fixes — upgrade.
- **Muse Spark reasoning does not carry across tool turns.** gx replays
  assistant tool calls and tool results on Chat Completions, but Meta does
  not return replayable reasoning on that surface, so each tool result
  starts a fresh reasoning pass. Same class as GLM/Kimi; Responses replay
  is future work.
- **Third-party retry/429 tuning is stock-xAI-tuned.** gx does not have bespoke
  backoff/retry curves for Fireworks, Z.AI, OpenRouter, Meta, or OpenAI — it inherits
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
- **ChatGPT/Codex may emit SSE `keepalive` frames.** gx ignores those frames
  (`crates/codegen/xai-grok-sampler/src/gx_responses_sse.rs`). Idle timeout is still
  on typed events (300s default); skipped keepalives do not reset it.
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
- **A gx build leaves a leader process and a loopback port behind.** Leader-on-by-default
  plus the remote lane means running `gx` once starts a detached process holding
  `127.0.0.1:2421`, and it outlives the TUI. That is the point (it is what makes a phone
  handoff possible), but it is a change in what a gx invocation costs you; see the
  opt-out table under "Coexistence" and `gx doctor`.
- **The remote lane is loopback + bearer token, and nothing more.** No TLS, no
  non-loopback bind, no per-session authorization: whoever can read
  `$GROK_HOME/gx-remote.token` can drive every session on the machine. That is the same
  authority as "can read your `$GROK_HOME`", deliberately, and reaching it remotely is
  SSH's job. A stale discovery record can also name a recycled port, so a client must
  check `/v1/healthz`'s `instanceId` before sending the token — see
  [`docs/gx/REMOTE_API.md`](REMOTE_API.md).
- **A session stays resident while a TUI is attached to it, and after the remote lane
  loads it — but the lane alone does not keep it that way.** When a session's last TUI
  exits, an *idle* session is unloaded even if the lane is still subscribed; the lane
  reloads it transparently on the phone's next request, so nothing is lost but that one
  extra round trip. A *busy* session — a turn running, or an approval waiting for an
  answer — stays resident and keeps delivering to the lane, which is what makes answering
  a prompt from your phone after closing the laptop work at all. Known limitation: that
  decision is taken once, at the moment of the disconnect, and nothing re-checks it when
  the turn ends — so a session whose TUI leaves **mid-turn** stays resident (and its
  `SessionEnd` hooks unfired) until some later disconnect names it again, or until the
  leader exits.
