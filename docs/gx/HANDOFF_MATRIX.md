# gx leader-mode handoff matrix (ticket A1, `charliek/grok-build#7`, commit C2)

Proves that a `gx` TUI running in `--leader` mode and a separate ACP client talking to
the same leader socket can hand a session back and forth: list it, load/replay it,
prompt it, interject into a running turn, resume across a TUI restart, create+drive a
session with no TUI at all, and survive a client disconnecting mid-turn.

This is a **verification** pass: everything below was actually run against a real
build, over the real leader-socket wire protocol, with real (tiny) model turns. Nothing
found broken here was fixed as part of this exercise — see "Findings to file" at the
bottom.

- **Binary under test:** `gx 1.0.16+gx.10` (`a7029c1e27d1`), built from `gx/main`
  (`/home/charliek/projects/grok-build/target/release/gx`).
- **Date:** 2026-09-07.
- **Host:** Linux popos 7.1.5-76070105-generic x86_64, Python 3.12.3 (harness is stdlib
  only, no pip deps).
- **Models used:** `gpt-5.6-luna` (OpenAI ChatGPT-plan via gx's `openai-codex`
  provider) for every cell — the configured default worked on the first attempt in
  every run, so the `glm-5.3-flash` fallback path was never exercised for real (it was
  code-reviewed but not empirically triggered).

## Environment

- Scratch `$GROK_HOME`: a private copy (mode `0600`) of `config.toml`, `providers.toml`,
  `auth.json`, `openai-codex-state.json`, `models_cache.json`, `trusted_folders.toml`
  from the real `~/.grok` — never the real `~/.grok` itself.
- Scratch project cwd: a directory containing only a `README.md`, added as `trusted` in
  the scratch `trusted_folders.toml`.
- **Leader socket override:** the scratch `$GROK_HOME` lives under a long,
  session-scoped scratchpad path. AF_UNIX socket paths are capped at ~108 bytes
  (`SUN_LEN`) on Linux, and `$GROK_HOME/gx-leader.sock` blew past that
  (`Leader server error error=Io(Error { kind: InvalidInput, message: "path must be
  shorter than SUN_LEN" })`, confirmed empirically on the first attempt). Every run
  below instead set `GROK_LEADER_SOCKET=/tmp/gx-handoff-<pid>.sock`, which
  `crates/codegen/xai-grok-shell/src/leader/lock.rs` (`LEADER_SOCKET_ENV`) explicitly
  supports for exactly this purpose. `$GROK_HOME` itself (config/auth/providers/sessions)
  is untouched by this override.
- Harness: `scripts/gx/handoff/{acp_client.py,tui_pty.py,run_matrix.py}` (Python 3
  stdlib only). See `scripts/gx/handoff/README.md` for how to re-run it and the full
  isolation rules.

## Wire-protocol facts nailed down while building this (not previously written up)

- **Framing:** 4-byte big-endian length prefix + JSON, confirmed by reading
  `leader/protocol.rs::{read_frame,write_frame}` directly (not documented elsewhere).
- **Ext/custom ACP methods require a leading `_` on the wire.** `x.ai/session/list`,
  `x.ai/sessions/list`, and `x.ai/interject` all 404'd with `Method not found` (code
  `-32601`) until sent as `_x.ai/session/list`, `_x.ai/sessions/list`,
  `_x.ai/interject`. Root cause, confirmed by reading the vendored
  `agent-client-protocol` crate (`0.10.4`) source: `decode_request`/`decode_notification`
  only route to `ext_method`/`ext_notification` via `method.strip_prefix('_')`; the
  encode side (`ExtRequest`/`ExtNotification`) adds the same prefix back
  (`format!("_{}", args.method)`). This is symmetric and applies to notifications the
  *server* sends too — the `x.ai/session/interjection` broadcast arrives as
  `_x.ai/session/interjection`. Standard ACP methods (`initialize`, `session/new`,
  `session/load`, `session/prompt`) are unaffected — no prefix.
- **Any inbound message carrying a `sessionId` auto-subscribes the sender to that
  session's broadcasts** (`leader/server.rs`, `extract_session_id` call sites around
  the main dispatch loop) — a bare `session/prompt` or `x.ai/interject` is enough to
  start receiving that session's `session/update` notifications; no separate "subscribe"
  call exists or is needed.
- **Response ids are namespaced per-connection internally and restored before being
  sent back** (`rewrite_request_id` / the response-side restore in `leader/server.rs`),
  so a single client's request/response ids behave exactly like talking to a bare ACP
  agent even though the leader multiplexes many clients.

## Results

| # | Cell | Result | Exact invocation | Model | Notes |
|---|------|--------|-------------------|-------|-------|
| 1 | **list** | pass | TUI: `gx --leader --session-id <id> --model gpt-5.6-luna --cwd <a1-cwd>`, prompted "reply with the single word PONG". Client: connect+register+`initialize`, then `_x.ai/session/list {"cwd":"<a1-cwd>"}` and `_x.ai/sessions/list {}` | gpt-5.6-luna | Session id found in both responses; cwd matched in `session/list`. |
| 2 | **load/replay** | pass | Client: `session/load {"sessionId":"<id>","cwd":"<a1-cwd>","mcpServers":[]}` | gpt-5.6-luna | 6 `session/update` replay notifications arrived after the response; prior turn's `PONG` answer was present verbatim in the replay stream. |
| 3 | **prompt** | pass | Client: `session/load` (to subscribe) then `session/prompt {"sessionId":"<id>","prompt":[{"type":"text","text":"reply with the single word ROGER"}]}` | glm-5.3-flash (re-run) | Answer streamed to the client as `session/update`s **and** rendered on the TUI's own screen. **Re-run 2026-09-07** after review: the first run's retained transcript captured the screen *tail*, and the turn had already drawn past the answer, so the artifact did not back the claim even though the assertion had passed. The cell now records the screen region around the marker (`find_context`, which cell 4 already used); `docs/gx/handoff/prompt.md` shows the rendered `ROGER` under "the proof". |
| 4 | **interject renders in the running TUI** | pass | TUI: typed "count slowly from 1 to 30, writing exactly one number per line and nothing else" + Enter. ~1s later, client: connect+register+`initialize`, then `_x.ai/interject {"sessionId":"<id>","text":"INTERJECT-7A3F"}` | gpt-5.6-luna | `INTERJECT-7A3F` appeared on the running TUI's screen with **no resume**. The client also received a `_x.ai/session/interjection` broadcast notification carrying the same text. |
| 5 | **resume** | pass | TUI: typed `/exit` + Enter (clean exit confirmed via PTY child reaping). New process: `gx --leader --resume <id> --model gpt-5.6-luna` | gpt-5.6-luna | The remotely-driven `ROGER` turn from cell 3 (and the `INTERJECT-7A3F` text from cell 4) were both visible in the resumed TUI's replayed screen. |
| 6 | **remote-create** | pass | Client only (no TUI attached): `session/new {"cwd":"<a1-cwd>","mcpServers":[]}` → new id, then `session/prompt {"sessionId":"<new-id>",...,"text":"reply with the single word TANGO"}`. Then a fresh TUI: `gx --leader --resume <new-id> --model gpt-5.6-luna` | gpt-5.6-luna | `TANGO` visible in the TUI opened purely via `--resume`, proving a session created and driven end-to-end with zero TUI involvement is fully resumable. |
| 7 | **disconnect survival** | pass | Client A: `session/new`, then fire-and-forget `session/prompt` for "count slowly from 1 to 20, ...", `time.sleep(1.5)`, then closed its socket mid-turn. Client B (fresh connection): register+`initialize`+`session/load {"sessionId":"<id>","cwd":"<a1-cwd>"}` | glm-5.3-flash (re-run) | The turn was **not** aborted by the disconnect: client B observed the full `1..20` answer. **Re-run 2026-09-07** after review: the original check was `"20" in blob and re.search(r"\b1\s*[\s\S]*20\b", blob)`, which is nearly unconditional once a `20` appears anywhere — and it searched the raw JSON, matching ids and token counts the agent never emitted. It now joins the `agent_message_chunk` texts in order and requires every number 1..20 individually; the census is recorded in `docs/gx/handoff/disconnect.md`. |

Full per-cell request/response JSON (sanitized) and TUI screen captures are in
`docs/gx/handoff/<cell>.md`:
[`list.md`](handoff/list.md), [`load.md`](handoff/load.md), [`prompt.md`](handoff/prompt.md),
[`interject.md`](handoff/interject.md), [`resume.md`](handoff/resume.md),
[`remote-create.md`](handoff/remote-create.md), [`disconnect.md`](handoff/disconnect.md).

## Harness notes worth knowing before re-running this

- **No real VT100 emulation, but not naive either.** `tui_pty.py`'s `TerminalGrid`
  interprets `\r`, `\n`, backspace, `CSI K` (erase-in-line) and `CSI A/B/C/D`
  (cursor movement) as an actual 2D character grid. The first version of this harness
  just deleted `\r`/ANSI codes from a flat byte stream, which **smeared an in-place
  spinner redraw together with the text underneath it** — the `prompt` cell's `ROGER`
  answer literally rendered as interleaved fragments like `RGER` mixed with spinner
  braille glyphs and turn counters, and came back `partial` (streamed to the client
  fine, "not found" on the TUI screen) until this was fixed. Point of the callout: if
  a future re-run reports a TUI-screen assertion failing, check whether it's a real
  regression or another such rendering artifact before writing it up as a finding.
- **Capture the region around your marker, not the screen tail.** Cells 3 and 7 were re-run on
  2026-09-07 after a PR review pointed out that their retained artifacts did not actually back their
  own `pass`. Neither assertion was wrong — the evidence was. Cell 3 waited until the grid contained
  `ROGER`, then saved `screen_text(1500)`, by which time the turn had drawn past it, so the saved
  tail showed only a spinner; it now saves `find_context(marker)`. Cell 7's completion test was
  `"20" in blob and re.search(r"\b1\s*[\s\S]*20\b", blob)` over the raw JSON, which is nearly
  unconditional once any `20` appears and happily matched event ids and token counts; it now joins
  the `agent_message_chunk` texts and requires each of 1..20. If you add a cell, make its transcript
  contain the thing it claims, and make the check reject a partial stream.
- **Leader lifecycle is tracked by PID, not by `gx leader kill`.** Because the socket
  path is overridden (see above), `gx leader list`/`kill`'s discovery (which scans the
  *default* `$GROK_HOME/gx-leader.sock` name) does not see the leader this harness
  spawns. `run_matrix.py` reads the authoritative PID out of the `.lock` file sibling
  of the socket and kills that directly, and separately tracks every TUI PTY child pid
  it forked. Verified clean at the end of every run in this document: zero leftover
  `gx` processes (checked via `/proc/<pid>/exe` resolving to the exact `GX_BIN` path,
  not a `pgrep` substring match — a naive `pgrep -af "target/release/gx"` false-positives
  on the invoking shell wrapper itself, since its own command text contains that
  substring).

## Findings to file

None. All 7 cells passed on the current build (`gx 1.0.16+gx.10`) once the harness sent
ext methods with the correct `_`-prefixed wire name and rendered the TUI screen through
an actual small character grid instead of a flat ANSI strip — both are harness bugs,
not product bugs, and are already fixed in the version of the harness committed here.
Two behaviors are worth keeping an eye on in future re-runs even though they didn't
block anything this time:

- The `_`-prefix requirement for every `x.ai/*` extension method is easy to get wrong
  from the client side and is not documented anywhere outside the vendored
  `agent-client-protocol` crate source. Any new external-client integration (this
  harness included) should link back to this doc or to `protocol.rs` rather than
  rediscovering it.
- Cell 7's "disconnect survival" success depended on a client reconnecting and issuing
  `session/load` within the harness's 15s notification-collection window while the
  20-number counting turn was still short enough to still be running server-side; a
  much longer gap between disconnect and reconnect, or a much shorter turn, was not
  exercised here and would be worth a follow-up cell if this matrix is extended.
