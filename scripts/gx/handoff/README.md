# gx handoff-matrix harness

Runs the leader-mode "handoff matrix" (ticket A1, `charliek/grok-build#7`, commit C2):
proves that a TUI running in `--leader` mode and a separate ACP client talking to the
same leader socket can hand a session back and forth (list, load/replay, prompt,
interject, resume, remote-create, survive a disconnect). See
`docs/gx/HANDOFF_MATRIX.md` for the write-up and results table this harness produces.

Anything found broken here is **recorded, not fixed** — this is a verification
harness, not a bugfix PR.

## Files

- `acp_client.py` — a from-scratch ACP-over-leader-socket client. Implements the exact
  wire framing from `crates/codegen/xai-grok-shell/src/leader/protocol.rs` (4-byte
  big-endian length prefix + JSON) and the `ClientMessage`/`ServerMessage` envelope
  (`register` / `acp` / `registered` / `leader_ready`), then standard JSON-RPC 2.0
  inside the `acp` payload. Usable as a library (`run_matrix.py` imports it) or
  standalone: `python3 acp_client.py --socket <sock> session-list`.
- `tui_pty.py` — spawns the `gx` TUI in a real PTY (`pty.fork`, 120x40) and gives you
  `send_line`, `wait_until_contains`, `screen_text`. Not a full VT100 emulator (no `pyte`,
  no pip deps allowed), but not a flat ANSI strip either: everything the PTY produces is fed
  to a small hand-rolled `TerminalGrid` that interprets `\r`, `\n`, backspace, `CSI K`
  (erase-in-line) and `CSI A/B/C/D` (cursor movement) as an actual 2D character grid, and
  `screen_text()` reads that grid. Colors, alt-screen, absolute cursor positioning and OSC
  titles are parsed and discarded. The grid is what makes "does this token appear on the
  rendered screen" a reliable assertion: the TUI redraws its pinned status region in place,
  and concatenating raw bytes (or blindly deleting `\r`) smears a spinner redraw together
  with the text underneath it — `ROGER` came back as `RGER` interleaved with braille spinner
  glyphs on the first run of the `prompt` cell (see `docs/gx/HANDOFF_MATRIX.md`, "Harness
  notes"). `strip_ansi()` / `raw_screen_text()` keep the old flat strip, for debugging a
  `screen_text()` mismatch only — do not assert on them.
- `run_matrix.py` — runs all 7 cells end to end, writes `results.json` and one
  sanitized transcript per cell to `docs/gx/handoff/<cell>.md`.

Everything is Python 3 stdlib only — no pip installs.

## Isolation rules (do not skip)

- **Never** run against the user's real `~/.grok`. Always pass a scratch `GROK_HOME`.
- **Never** use `gx`/`grok` from `$PATH`. Always pass an explicit `GX_BIN`.
- The scratch `GROK_HOME` should contain only copies (mode `0600`) of
  `config.toml`, `providers.toml`, `auth.json`, `openai-codex-state.json`,
  `models_cache.json`, `trusted_folders.toml` from the real one — nothing else. The
  scratch project `cwd` must be listed as `trusted` in the scratch
  `trusted_folders.toml` or the TUI will block on a trust prompt no one is there to
  answer.
- Transcripts are run through `acp_client.sanitize()` before being written, which
  redacts `sk-...` tokens, `Bearer ...` headers, and any JSON field named
  `api_key`/`token`/`access_token`/`refresh_token`/`authorization`. Re-check any new
  transcript field you add for material that looks like a secret before trusting the
  redaction regexes blindly.

## Why `GX_LEADER_SOCKET` overrides to a short `/tmp` path

AF_UNIX socket paths are capped at ~108 bytes (`SUN_LEN`) on Linux. A scratch
`GROK_HOME` nested under a session-scoped scratchpad directory (as this ticket
mandates) routinely blows past that once you append `gx-leader.sock`. The leader and
every client explicitly honor `GROK_LEADER_SOCKET`
(`crates/codegen/xai-grok-shell/src/leader/lock.rs: LEADER_SOCKET_ENV`), so
`run_matrix.py` defaults it to `/tmp/gx-handoff-<pid>.sock` unless you set it
yourself. `GROK_HOME` itself — config, auth, providers, sessions — is untouched and
stays exactly at the mandated scratch path; only the socket file moves.

One consequence: `gx leader list`/`kill` discovery scans the *default* socket/lock
name under `$GROK_HOME` and will not see a leader bound to an overridden socket path.
`run_matrix.py` and `tui_pty.py` therefore track and kill processes by PID (read from
the `.lock` file sibling of the socket, or from the PTY child pid they forked
directly) rather than relying on `gx leader kill`.

## Env vars

| Var | Required | Meaning |
|---|---|---|
| `GX_BIN` | yes | Absolute path to the `gx` binary under test. |
| `GROK_HOME` | yes | Scratch `$GROK_HOME`. Never the real one. |
| `A1_CWD` | yes | Scratch project cwd, trusted in the scratch `GROK_HOME`. |
| `GX_LEADER_SOCKET` | no | Overrides the leader socket path (see above). |
| `GX_MODEL` | no | Model id to request (default `gpt-5.6-luna`). |
| `GX_FALLBACK_MODEL` | no | Retried once if the primary model fails to answer the bootstrap prompt (default `glm-5.3-flash`). |

## Running it

```bash
export GX_BIN=/home/charliek/projects/grok-build/target/release/gx
export GROK_HOME=/path/to/scratch/a1-grok-home
export A1_CWD=/path/to/scratch/a1-cwd

python3 scripts/gx/handoff/run_matrix.py \
    --out-dir /path/to/scratch/results \
    --docs-dir docs/gx
```

Run a subset with `--cells list,load,prompt` (comma-separated cell names; see the
list at the top of `run_matrix.py`). Cells `list` through `resume` share one
TUI-owned session bootstrapped with a tiny "reply with the single word PONG" prompt;
`remote-create` and `disconnect` create their own sessions and don't depend on the
others, so they're safe to re-run alone.

The script is idempotent: it kills any stale leader left at `GX_LEADER_SOCKET`
before starting, and tears down every TUI PTY and the leader process it caused to be
spawned in a `finally` block, whether or not cells passed. It reports leftover
processes (matched by scratch `GROK_HOME` path, leader socket path, or leader pid) on
stderr and via a non-zero exit code — that list should always be empty; if it isn't,
something in the harness itself needs fixing (not a matrix finding).

### Unit tests for the harness itself

```bash
python3 scripts/gx/handoff/test_handoff.py
```

Offline (`unittest`, stdlib, no gx binary and no leader) and fast. It pins the parts of the
harness that decide whether a cell passes: that a cell reads the agent's answer
(`agent_message_text`) rather than a JSON dump that also contains the echo of the prompt it
sent, that `session/load`'s replayed notifications (`_meta.isReplay`) never count as live
post-reconnect traffic, that the lock path derived from a socket path is the leader's and not
the socket itself, and that `tui_pty.TerminalGrid` never renders a torn escape sequence into
the screen it is asserted against.

### Manual / debugging use of `acp_client.py`

```bash
python3 acp_client.py --socket /tmp/gx-handoff-XXXX.sock session-list
python3 acp_client.py --socket /tmp/gx-handoff-XXXX.sock session-new --cwd /path/to/scratch/a1-cwd
python3 acp_client.py --socket /tmp/gx-handoff-XXXX.sock session-prompt --session-id <id> --text "reply with PONG"
python3 acp_client.py --socket /tmp/gx-handoff-XXXX.sock interject --session-id <id> --text "INTERJECT-TOKEN"
python3 acp_client.py --socket /tmp/gx-handoff-XXXX.sock raw --method x.ai/sessions/list --params '{}'
```

Each subcommand connects, registers, calls `initialize`, runs the one RPC, and
exits — handy for poking at a leader you started by hand (`GROK_HOME=... GROK_LEADER_SOCKET=...
gx --leader --cwd ...`) without running the whole matrix.

`GROK_LEADER_SOCKET` is what gx itself reads (`LEADER_SOCKET_ENV` in
`crates/codegen/xai-grok-shell/src/leader/lock.rs`); `GX_LEADER_SOCKET` is only
`run_matrix.py`'s own input name for the same path. Exporting the harness's name to a
hand-started leader would leave it bound to the default `$GROK_HOME/gx-leader.sock`,
not the socket the `--socket` arguments above point at.
