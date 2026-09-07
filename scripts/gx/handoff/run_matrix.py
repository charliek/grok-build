#!/usr/bin/env python3
"""Run the gx leader-mode "handoff matrix" (ticket A1 / commit C2) end to end.

Cells (see docs/gx/HANDOFF_MATRIX.md for the write-up):
  1. list             -- external ACP client lists a TUI-owned session.
  2. load             -- external client session/load's the TUI-owned session, gets replay.
  3. prompt           -- external client session/prompt's the TUI-owned session; streams back.
  4. interject        -- external client interjects into a running TUI turn.
  5. resume           -- TUI quits and --resume's the session; prior remote turn is visible.
  6. remote-create    -- client creates+prompts a session with no TUI; TUI --resume's it.
  7. disconnect       -- client's socket drops mid-turn; a fresh connection sees it survive.

This script is self-contained (stdlib only) and is meant to be safe to re-run:
it always tears down whatever it started (TUI PTYs + the leader process it
caused to be spawned), and never touches the user's real $GROK_HOME.

Usage:
    GX_BIN=/path/to/gx GROK_HOME=/scratch/home A1_CWD=/scratch/cwd \\
        python3 run_matrix.py --out-dir /scratch/results --docs-dir <repo>/docs/gx

Required env:
    GX_BIN      -- path to the gx binary under test (never PATH `gx`/`grok`).
    GROK_HOME   -- scratch $GROK_HOME (never the user's real ~/.grok).
    A1_CWD      -- scratch project cwd, must be a trusted folder in GROK_HOME.

Optional env:
    GX_LEADER_SOCKET -- override leader socket path. Defaults to a short path
        under /tmp (NOT under GROK_HOME): AF_UNIX socket paths are capped at
        ~108 bytes (SUN_LEN) on Linux, and a scratch GROK_HOME nested under a
        session-scoped scratchpad directory routinely blows past that. The
        leader and every client still honor GROK_LEADER_SOCKET explicitly
        (crates/.../leader/lock.rs), so this is a supported override, not a
        hack -- GROK_HOME itself (config/auth/providers/sessions) is
        untouched and stays at the mandated scratch path.
    GX_MODEL    -- model id to request (default: gpt-5.6-luna). Falls back to
        GX_FALLBACK_MODEL (default: glm-5.3-flash) if the primary model fails.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import signal
import socket as socketlib
import sys
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import acp_client as acp  # noqa: E402
import tui_pty  # noqa: E402


# --------------------------------------------------------------------------
# Environment / config
# --------------------------------------------------------------------------


@dataclass
class Env:
    gx_bin: str
    grok_home: str
    cwd: str
    leader_socket: str
    model: str
    fallback_model: str
    out_dir: Path
    docs_dir: Path
    handoff_docs_dir: Path

    @property
    def lock_path(self) -> str:
        return re.sub(r"\.sock$", ".lock", self.leader_socket)


def load_env(args: argparse.Namespace) -> Env:
    gx_bin = os.environ.get("GX_BIN")
    grok_home = os.environ.get("GROK_HOME")
    cwd = os.environ.get("A1_CWD")
    if not gx_bin or not grok_home or not cwd:
        sys.exit("GX_BIN, GROK_HOME and A1_CWD must all be set in the environment")
    if not os.path.isfile(gx_bin):
        sys.exit(f"GX_BIN does not exist: {gx_bin}")
    leader_socket = os.environ.get("GX_LEADER_SOCKET") or f"/tmp/gx-handoff-{os.getpid()}.sock"
    model = os.environ.get("GX_MODEL", "gpt-5.6-luna")
    fallback_model = os.environ.get("GX_FALLBACK_MODEL", "glm-5.3-flash")
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    docs_dir = Path(args.docs_dir)
    handoff_docs_dir = docs_dir / "handoff"
    handoff_docs_dir.mkdir(parents=True, exist_ok=True)
    return Env(
        gx_bin=gx_bin,
        grok_home=grok_home,
        cwd=cwd,
        leader_socket=leader_socket,
        model=model,
        fallback_model=fallback_model,
        out_dir=out_dir,
        docs_dir=docs_dir,
        handoff_docs_dir=handoff_docs_dir,
    )


# --------------------------------------------------------------------------
# Result bookkeeping
# --------------------------------------------------------------------------


@dataclass
class CellResult:
    cell: str
    title: str
    result: str = "fail"  # pass | fail | partial
    invocations: list = field(default_factory=list)
    model: str = ""
    notes: str = ""
    transcript_file: str = ""


RESULTS: list = []


def record(cr: CellResult) -> None:
    RESULTS.append(cr)


def dump_json(obj: Any, cap: int = 1500) -> str:
    """Pretty-print JSON for a transcript, capped so one huge field (e.g. the full
    slash-command list in an `available_commands_update` notification, or the full
    model roster in `initialize`'s result) doesn't balloon the file. Truncates the
    *serialized* text, not the structure, so it stays valid to skim even if not
    valid JSON after the cut."""
    text = json.dumps(acp.sanitize(obj), indent=2)
    if len(text) <= cap:
        return text
    return text[:cap] + f"\n... [truncated, {len(text)} chars total]"


def dump_json_compact(obj: Any, cap: int = 600) -> str:
    text = json.dumps(acp.sanitize(obj))
    if len(text) <= cap:
        return text
    return text[:cap] + f"... [truncated, {len(text)} chars total]"


def write_transcript(env: Env, cell: str, lines: list) -> str:
    fname = f"{cell}.md"
    path = env.handoff_docs_dir / fname
    body = "\n".join(lines)
    path.write_text(acp.sanitize(body) if isinstance(acp.sanitize(body), str) else body)
    return str(path)


# --------------------------------------------------------------------------
# Leader / TUI process bookkeeping (so teardown can find everything)
# --------------------------------------------------------------------------

SPAWNED_TUIS: list = []


def track_tui(sess: tui_pty.TuiSession) -> tui_pty.TuiSession:
    SPAWNED_TUIS.append(sess)
    return sess


def wait_for_socket(path: str, timeout: float = 30.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if os.path.exists(path):
            try:
                s = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
                s.settimeout(1.0)
                s.connect(path)
                s.close()
                return True
            except OSError:
                pass
        time.sleep(0.2)
    return False


def leader_pid(env: Env) -> Optional[int]:
    try:
        with open(env.lock_path) as f:
            return int(f.read().strip())
    except (OSError, ValueError):
        return None


def kill_pid(pid: int, grace: float = 3.0) -> None:
    try:
        os.kill(pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        return
    deadline = time.time() + grace
    while time.time() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.1)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def cleanup_stale_leader(env: Env) -> None:
    pid = leader_pid(env)
    if pid:
        kill_pid(pid)
    for p in (env.leader_socket, env.lock_path):
        try:
            os.remove(p)
        except OSError:
            pass


def teardown(env: Env) -> list:
    leftovers = []
    for sess in SPAWNED_TUIS:
        try:
            sess.terminate()
        except Exception:  # noqa: BLE001
            pass
    pid = leader_pid(env)
    if pid:
        kill_pid(pid)
    for p in (env.leader_socket, env.lock_path):
        try:
            os.remove(p)
        except OSError:
            pass
    # Verify: nothing of ours should be alive now. We check /proc/<pid>/exe against
    # the resolved GX_BIN path rather than grepping cmdline text for "target/release/gx":
    # this whole harness is itself invoked as a shell command containing that very
    # substring, and a naive `pgrep -af` matches that ancestor shell, not a real gx
    # process. Comparing the actual executable inode/path has no such false positive.
    try:
        gx_real = os.path.realpath(env.gx_bin)
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            try:
                exe = os.readlink(f"/proc/{entry}/exe")
            except OSError:
                continue
            if os.path.realpath(exe) != gx_real:
                continue
            try:
                with open(f"/proc/{entry}/cmdline", "rb") as f:
                    cmdline = f.read().replace(b"\0", b" ").decode(errors="replace").strip()
            except OSError:
                cmdline = "<unreadable>"
            leftovers.append(f"{entry} {cmdline}")
    except OSError:
        pass
    return leftovers


# --------------------------------------------------------------------------
# Cells
# --------------------------------------------------------------------------

MARKER_PONG = "PONG"
MARKER_ROGER = "ROGER"
MARKER_TANGO = "TANGO"
# A screen-scrape marker, not a credential: the harness greps the TUI for this exact
# string to prove an interjection rendered. Named MARKER, never TOKEN — gitleaks'
# generic-api-key rule fires on a *_TOKEN name holding a high-entropy string.
INTERJECT_MARKER = "INTERJECT-7A3F"


def start_tui_with_session(env: Env, session_id: str, model: str) -> tui_pty.TuiSession:
    sess = tui_pty.start_tui(
        env.gx_bin,
        env.grok_home,
        env.cwd,
        env.leader_socket,
        extra_args=["--session-id", session_id, "--model", model],
    )
    track_tui(sess)
    return sess


def resume_tui(env: Env, session_id: str, model: Optional[str] = None) -> tui_pty.TuiSession:
    extra = ["--resume", session_id]
    if model:
        extra += ["--model", model]
    sess = tui_pty.start_tui(env.gx_bin, env.grok_home, env.cwd, env.leader_socket, extra_args=extra)
    track_tui(sess)
    return sess


def cell_bootstrap(env: Env) -> dict:
    """Not a matrix cell by itself: gets a TUI-owned session up with one prior turn.
    Returns {"session_id", "tui", "model_used"}."""
    session_id = str(uuid.uuid4())
    model = env.model
    tui = start_tui_with_session(env, session_id, model)
    tui.pump(2.0)
    booted = tui.wait_until_contains("❯", timeout=25.0) or tui.wait_until_matches(re.compile(r"Grok|gpt|glm", re.I), timeout=5.0)
    tui.send_line("reply with the single word " + MARKER_PONG)
    got = tui.wait_until_contains(MARKER_PONG, timeout=60.0)
    if not got and model != env.fallback_model:
        # Retry with the fallback model on a fresh session id.
        tui.terminate()
        SPAWNED_TUIS.remove(tui)
        session_id = str(uuid.uuid4())
        model = env.fallback_model
        tui = start_tui_with_session(env, session_id, model)
        tui.pump(2.0)
        tui.send_line("reply with the single word " + MARKER_PONG)
        got = tui.wait_until_contains(MARKER_PONG, timeout=60.0)
    return {"session_id": session_id, "tui": tui, "model_used": model, "bootstrap_ok": got, "booted": booted}


def cell_list(env: Env, session_id: str, model: str) -> CellResult:
    cr = CellResult(cell="list", title="list: external client lists TUI-owned session", model=model)
    client = None
    lines = ["# Cell 1: list", "", f"Session id under test: `{session_id}`", ""]
    try:
        client = acp.new_client(env.leader_socket, client_type="gx-handoff-list")
        cr.invocations.append(f"connect+register unix socket {env.leader_socket}, client_type=gx-handoff-list")
        init_resp = acp.initialize(client)
        cr.invocations.append('call initialize {"protocolVersion":"0.1"}')
        lines += ["## initialize response (capped)", "```json", dump_json(init_resp), "```", ""]

        list_resp = acp.session_list(client, cwd=env.cwd)
        cr.invocations.append(f'call x.ai/session/list {{"cwd":"{env.cwd}"}}')
        lines += ["## x.ai/session/list response", "```json", dump_json(list_resp), "```", ""]

        roster_resp = acp.sessions_roster(client)
        cr.invocations.append("call x.ai/sessions/list {}")
        lines += ["## x.ai/sessions/list response", "```json", dump_json(roster_resp), "```", ""]

        found_in_list = session_id in json.dumps(list_resp)
        found_in_roster = session_id in json.dumps(roster_resp)
        cwd_in_list = env.cwd in json.dumps(list_resp)
        if found_in_list or found_in_roster:
            cr.result = "pass"
            cr.notes = (
                f"session found in x.ai/session/list={found_in_list} (cwd match={cwd_in_list}), "
                f"x.ai/sessions/list={found_in_roster}"
            )
        else:
            cr.result = "fail"
            cr.notes = "session id not present in either x.ai/session/list or x.ai/sessions/list response"
    except Exception as e:  # noqa: BLE001
        cr.result = "fail"
        cr.notes = f"exception: {e!r}"
        lines.append(f"\nEXCEPTION: {e!r}\n")
    finally:
        if client:
            client.close()
    cr.transcript_file = write_transcript(env, "list", lines)
    return cr


def cell_load(env: Env, session_id: str, model: str) -> CellResult:
    cr = CellResult(cell="load", title="load/replay: client session/load's the TUI-owned session", model=model)
    client = None
    lines = ["# Cell 2: load/replay", "", f"Session id under test: `{session_id}`", ""]
    try:
        client = acp.new_client(env.leader_socket, client_type="gx-handoff-load")
        acp.initialize(client)
        cr.invocations.append(f'connect+register+initialize, then call session/load {{"sessionId":"{session_id}","cwd":"{env.cwd}"}}')
        resp = acp.session_load(client, session_id, env.cwd, timeout=30.0)
        lines += ["## session/load response", "```json", dump_json(resp), "```", ""]
        notes_after = client.collect_notifications_for(3.0)
        all_notes = client.notifications_snapshot()
        replay_updates = [n for n in all_notes if n.method == "session/update"]
        lines += [
            f"## session/update notifications received (replay): {len(replay_updates)}",
            "```json",
        ]
        for n in replay_updates[:20]:
            lines.append(dump_json_compact(n.payload))
        lines += ["```", ""]
        blob = json.dumps([acp.sanitize(n.payload) for n in replay_updates])
        if replay_updates and MARKER_PONG in blob:
            cr.result = "pass"
            cr.notes = f"{len(replay_updates)} session/update replay notifications received, prior turn ({MARKER_PONG}) visible"
        elif replay_updates:
            cr.result = "partial"
            cr.notes = f"{len(replay_updates)} session/update notifications received but prior answer not found verbatim"
        else:
            cr.result = "fail"
            cr.notes = "no session/update replay notifications received after session/load"
    except Exception as e:  # noqa: BLE001
        cr.result = "fail"
        cr.notes = f"exception: {e!r}"
        lines.append(f"\nEXCEPTION: {e!r}\n")
    finally:
        if client:
            client.close()
    cr.transcript_file = write_transcript(env, "load", lines)
    return cr


def cell_prompt(env: Env, session_id: str, tui: tui_pty.TuiSession, model: str) -> CellResult:
    cr = CellResult(cell="prompt", title="prompt: client session/prompt's the TUI-owned session", model=model)
    client = None
    lines = ["# Cell 3: prompt", "", f"Session id under test: `{session_id}`", ""]
    try:
        client = acp.new_client(env.leader_socket, client_type="gx-handoff-prompt")
        acp.initialize(client)
        acp.session_load(client, session_id, env.cwd, timeout=30.0)  # subscribe first, like a real second client would
        cr.invocations.append(
            f'connect+register+initialize+session/load, then call session/prompt {{"sessionId":"{session_id}",'
            f'"prompt":[{{"type":"text","text":"reply with the single word {MARKER_ROGER}"}}]}}'
        )
        resp = acp.session_prompt(client, session_id, f"reply with the single word {MARKER_ROGER}", timeout=60.0)
        lines += ["## session/prompt response", "```json", dump_json(resp), "```", ""]
        notes = client.notifications_snapshot()
        updates = [n for n in notes if n.method == "session/update"]
        blob = json.dumps([acp.sanitize(n.payload) for n in updates])
        streamed_to_client = MARKER_ROGER in blob
        lines += [f"## session/update notifications during turn: {len(updates)}", "```json"]
        for n in updates[-20:]:
            lines.append(dump_json_compact(n.payload))
        lines += ["```", ""]

        on_tui_screen = tui.wait_until_contains(MARKER_ROGER, timeout=15.0)
        lines += [f"## TUI screen tail (proves/disproves it also rendered {MARKER_ROGER})", "```", tui.screen_text(1500), "```"]

        if streamed_to_client and on_tui_screen:
            cr.result = "pass"
            cr.notes = "answer streamed to external client via session/update AND rendered on the TUI screen"
        elif streamed_to_client or on_tui_screen:
            cr.result = "partial"
            cr.notes = f"streamed_to_client={streamed_to_client} on_tui_screen={on_tui_screen}"
        else:
            cr.result = "fail"
            cr.notes = f"{MARKER_ROGER} appeared neither in client notifications nor on the TUI screen"
    except Exception as e:  # noqa: BLE001
        cr.result = "fail"
        cr.notes = f"exception: {e!r}"
        lines.append(f"\nEXCEPTION: {e!r}\n")
    finally:
        if client:
            client.close()
    cr.transcript_file = write_transcript(env, "prompt", lines)
    return cr


def cell_interject(env: Env, session_id: str, tui: tui_pty.TuiSession, model: str) -> CellResult:
    cr = CellResult(cell="interject", title="interject renders in the running TUI", model=model)
    client = None
    lines = ["# Cell 4: interject", "", f"Session id under test: `{session_id}`", ""]
    try:
        tui.send_line("count slowly from 1 to 30, writing exactly one number per line and nothing else")
        cr.invocations.append("TUI: type 'count slowly from 1 to 30, writing exactly one number per line and nothing else' + Enter")
        # Give the turn time to actually start streaming before we interject.
        started = tui.wait_until_matches(re.compile(r"[Rr]esponding|Thinking|Worked for|stop\]"), timeout=10.0)
        time.sleep(1.0)

        client = acp.new_client(env.leader_socket, client_type="gx-handoff-interject")
        acp.initialize(client)
        cr.invocations.append(f'connect+register+initialize, then call x.ai/interject {{"sessionId":"{session_id}","text":"{INTERJECT_MARKER}"}}')
        resp = acp.interject(client, session_id, INTERJECT_MARKER, timeout=15.0)
        lines += ["## x.ai/interject response", "```json", dump_json(resp), "```", ""]

        # Ext notifications from the agent carry a leading "_" on the wire (the ACP
        # crate's encode/decode symmetry -- see acp_client.py's interject() docstring).
        note = client.wait_for_notification(lambda n: n.method == "_x.ai/session/interjection", timeout=10.0)
        lines += [
            "## x.ai/session/interjection notification received by the client",
            "```json",
            dump_json_compact(note.payload) if note else "null",
            "```",
            "",
        ]

        on_screen = tui.wait_until_contains(INTERJECT_MARKER, timeout=20.0)
        context = tui.find_context(INTERJECT_MARKER) if on_screen else None
        # Let the (short) turn finish so the next cell starts clean.
        tui.pump(20.0)
        lines += [
            f"## TUI screen around the interjection token (found={on_screen})",
            "```",
            context or tui.screen_text(2000),
            "```",
        ]

        if on_screen and note is not None:
            cr.result = "pass"
            cr.notes = "token rendered on the running TUI without a resume, AND client received x.ai/session/interjection"
        elif on_screen:
            cr.result = "partial"
            cr.notes = "token rendered on TUI screen but client did not observe an x.ai/session/interjection notification"
        else:
            cr.result = "fail"
            cr.notes = f"turn_started={started}; interjection token never appeared on the TUI screen"
    except Exception as e:  # noqa: BLE001
        cr.result = "fail"
        cr.notes = f"exception: {e!r}"
        lines.append(f"\nEXCEPTION: {e!r}\n")
    finally:
        if client:
            client.close()
    cr.transcript_file = write_transcript(env, "interject", lines)
    return cr


def cell_resume(env: Env, session_id: str, tui: tui_pty.TuiSession, model: str) -> CellResult:
    cr = CellResult(cell="resume", title="resume: quit TUI, --resume, remote turn still visible", model=model)
    lines = ["# Cell 5: resume", "", f"Session id under test: `{session_id}`", ""]
    try:
        tui.send_slash_exit()
        cr.invocations.append("TUI: type '/exit' + Enter")
        exited = False
        deadline = time.time() + 10.0
        while time.time() < deadline:
            if not tui.is_alive():
                exited = True
                break
            tui.pump(0.3)
        if not exited:
            tui.send_ctrl_c()
            tui.pump(1.0)
            tui.send_ctrl_c()
            deadline = time.time() + 5.0
            while time.time() < deadline and tui.is_alive():
                tui.pump(0.3)
            exited = not tui.is_alive()
        tui.terminate()  # belt and suspenders
        if tui in SPAWNED_TUIS:
            SPAWNED_TUIS.remove(tui)

        tui2 = resume_tui(env, session_id, model=model)
        cr.invocations.append(f"new TUI process: gx --leader --resume {session_id} --model {model}")
        tui2.pump(3.0)
        found_roger = tui2.wait_until_contains(MARKER_ROGER, timeout=25.0)
        found_interject = INTERJECT_MARKER in tui2.screen_text()
        lines += [
            f"TUI cleanly exited before resume: {exited}",
            "## resumed TUI screen tail",
            "```",
            tui2.screen_text(3000),
            "```",
        ]
        if found_roger:
            cr.result = "pass"
            cr.notes = f"remote turn from cell 3 ({MARKER_ROGER}) visible after --resume; clean exit={exited}; interject token also present={found_interject}"
        else:
            cr.result = "fail"
            cr.notes = f"{MARKER_ROGER} not visible on the resumed TUI screen; clean exit={exited}"
    except Exception as e:  # noqa: BLE001
        cr.result = "fail"
        cr.notes = f"exception: {e!r}"
        lines.append(f"\nEXCEPTION: {e!r}\n")
    cr.transcript_file = write_transcript(env, "resume", lines)
    return cr


def cell_remote_create(env: Env, model: str) -> CellResult:
    cr = CellResult(cell="remote-create", title="remote-create: client creates+prompts with no TUI, then TUI --resume's it", model=model)
    client = None
    lines = ["# Cell 6: remote-create", ""]
    try:
        client = acp.new_client(env.leader_socket, client_type="gx-handoff-remote-create")
        acp.initialize(client)
        cr.invocations.append(f'connect+register+initialize, then call session/new {{"cwd":"{env.cwd}"}}')
        new_resp = acp.session_new(client, env.cwd, timeout=30.0)
        lines += ["## session/new response", "```json", dump_json(new_resp), "```", ""]
        session_id = (new_resp.get("result") or {}).get("sessionId")
        if not session_id:
            raise RuntimeError(f"session/new did not return a sessionId: {new_resp}")
        lines.append(f"Remote-created session id: `{session_id}`\n")

        cr.invocations.append(f'call session/prompt {{"sessionId":"{session_id}","prompt":[{{"type":"text","text":"reply with the single word {MARKER_TANGO}"}}]}}')
        prompt_resp = acp.session_prompt(client, session_id, f"reply with the single word {MARKER_TANGO}", timeout=60.0)
        lines += ["## session/prompt response", "```json", dump_json(prompt_resp), "```", ""]
        client.close()
        client = None

        tui = resume_tui(env, session_id, model=model)
        cr.invocations.append(f"new TUI process: gx --leader --resume {session_id} --model {model}")
        tui.pump(3.0)
        found = tui.wait_until_contains(MARKER_TANGO, timeout=25.0)
        lines += ["## TUI screen tail after --resume", "```", tui.screen_text(3000), "```"]
        tui.terminate()
        if tui in SPAWNED_TUIS:
            SPAWNED_TUIS.remove(tui)

        if found:
            cr.result = "pass"
            cr.notes = f"remotely-created+prompted turn ({MARKER_TANGO}) visible in a TUI opened with --resume {session_id}"
        else:
            cr.result = "fail"
            cr.notes = f"{MARKER_TANGO} not visible on the --resume'd TUI screen"
    except Exception as e:  # noqa: BLE001
        cr.result = "fail"
        cr.notes = f"exception: {e!r}"
        lines.append(f"\nEXCEPTION: {e!r}\n")
    finally:
        if client:
            client.close()
    cr.transcript_file = write_transcript(env, "remote-create", lines)
    return cr


def cell_disconnect(env: Env, model: str) -> CellResult:
    cr = CellResult(cell="disconnect", title="disconnect survival: prompt continues/completes across a client reconnect", model=model)
    client_a = None
    client_b = None
    lines = ["# Cell 7: disconnect survival", ""]
    try:
        client_a = acp.new_client(env.leader_socket, client_type="gx-handoff-disconnect-a")
        acp.initialize(client_a)
        cr.invocations.append(f'client A: connect+register+initialize, then call session/new {{"cwd":"{env.cwd}"}}')
        new_resp = acp.session_new(client_a, env.cwd, timeout=30.0)
        session_id = (new_resp.get("result") or {}).get("sessionId")
        if not session_id:
            raise RuntimeError(f"session/new did not return a sessionId: {new_resp}")
        lines.append(f"Session id: `{session_id}`\n")

        long_prompt = "count slowly from 1 to 20, writing exactly one number per line and nothing else"
        rid = acp.session_prompt_async(client_a, session_id, long_prompt)
        cr.invocations.append(f'client A: fire-and-forget session/prompt (id={rid}) "{long_prompt}", then close socket mid-turn')
        time.sleep(1.5)
        pre_disconnect_notes = client_a.notifications_snapshot()
        client_a.close()
        lines += [f"Notifications observed on client A before disconnect: {len(pre_disconnect_notes)}", ""]

        client_b = acp.new_client(env.leader_socket, client_type="gx-handoff-disconnect-b")
        acp.initialize(client_b)
        cr.invocations.append(f'client B (fresh connection): register+initialize, then session/load {{"sessionId":"{session_id}","cwd":"{env.cwd}"}}')
        load_resp = acp.session_load(client_b, session_id, env.cwd, timeout=30.0)
        lines += ["## session/load response (client B)", "```json", dump_json(load_resp), "```", ""]

        post_notes = client_b.collect_notifications_for(15.0)
        all_notes_b = client_b.notifications_snapshot()
        updates = [n for n in all_notes_b if n.method == "session/update"]
        blob = json.dumps([acp.sanitize(n.payload) for n in updates])
        lines += [f"## session/update notifications on client B: {len(updates)}", "```json"]
        for n in updates[-25:]:
            lines.append(dump_json_compact(n.payload))
        lines += ["```", ""]

        completed = "20" in blob and re.search(r"\b1\s*[\s\S]*20\b", blob) is not None
        still_streaming = len(post_notes) > 0
        if completed:
            cr.result = "pass"
            cr.notes = "turn completed (full 1..20 answer visible) to the reconnected client"
        elif still_streaming:
            cr.result = "pass"
            cr.notes = f"turn was still streaming to the reconnected client B ({len(post_notes)} new session/update notifications after reconnect)"
        elif updates:
            cr.result = "partial"
            cr.notes = "replay contained prior updates but no further streaming/new content observed after reconnect"
        else:
            cr.result = "fail"
            cr.notes = "no session/update activity observed on the reconnected client at all"
    except Exception as e:  # noqa: BLE001
        cr.result = "fail"
        cr.notes = f"exception: {e!r}"
        lines.append(f"\nEXCEPTION: {e!r}\n")
    finally:
        if client_a:
            client_a.close()
        if client_b:
            client_b.close()
    cr.transcript_file = write_transcript(env, "disconnect", lines)
    return cr


# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out-dir", required=True, help="Where to write results.json")
    p.add_argument("--docs-dir", required=True, help="docs/gx directory (transcripts go to <docs-dir>/handoff/)")
    p.add_argument("--cells", default="list,load,prompt,interject,resume,remote-create,disconnect")
    args = p.parse_args()

    env = load_env(args)
    cleanup_stale_leader(env)

    wanted = set(args.cells.split(","))
    leftovers: list = []
    try:
        boot = cell_bootstrap(env)
        session_id = boot["session_id"]
        tui = boot["tui"]
        model = boot["model_used"]
        if not wait_for_socket(env.leader_socket, timeout=20.0):
            print("WARNING: leader socket never became connectable", file=sys.stderr)
        if not boot["bootstrap_ok"]:
            print("WARNING: bootstrap PONG prompt never landed; downstream cells will likely fail", file=sys.stderr)

        if "list" in wanted:
            record(cell_list(env, session_id, model))
        if "load" in wanted:
            record(cell_load(env, session_id, model))
        if "prompt" in wanted:
            record(cell_prompt(env, session_id, tui, model))
        if "interject" in wanted:
            record(cell_interject(env, session_id, tui, model))
        if "resume" in wanted:
            record(cell_resume(env, session_id, tui, model))
        if "remote-create" in wanted:
            record(cell_remote_create(env, model))
        if "disconnect" in wanted:
            record(cell_disconnect(env, model))
    finally:
        leftovers = teardown(env)

    results_path = env.out_dir / "results.json"
    results_path.write_text(
        json.dumps(
            {
                "results": [r.__dict__ for r in RESULTS],
                "leftover_processes": leftovers,
                "env": {
                    "gx_bin": env.gx_bin,
                    "grok_home": env.grok_home,
                    "cwd": env.cwd,
                    "leader_socket": env.leader_socket,
                    "model": env.model,
                    "fallback_model": env.fallback_model,
                },
            },
            indent=2,
        )
    )
    print(f"Wrote {results_path}")
    for r in RESULTS:
        print(f"  {r.cell:16s} {r.result:8s} {r.notes}")
    if leftovers:
        print("LEFTOVER PROCESSES:", file=sys.stderr)
        for line in leftovers:
            print(f"  {line}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
