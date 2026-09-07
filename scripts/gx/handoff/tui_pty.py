#!/usr/bin/env python3
"""Drive the gx TUI inside a PTY for the handoff matrix (A1 / C2).

Deliberately does NOT do full VT100 emulation (no pyte, no pip deps allowed).
Instead of a real terminal grid it runs a small hand-rolled one (see
`TerminalGrid` below): the TUI's pinned status region redraws in place with
bare `\\r` and `CSI K`/cursor-movement sequences, and naively concatenating
raw bytes (or blindly deleting `\\r`) smears an in-place spinner redraw
together with the text it's overwriting -- e.g. "ROGER" showed up as
"RGER" interleaved with spinner braille glyphs during manual verification of
this harness (docs/gx/handoff/prompt.md, first run). `TerminalGrid`
interprets `\\r`, `\\n`, backspace, `CSI K` (erase in line) and `CSI A/B/C/D`
(cursor movement) as an actual 2D character grid, which is enough to make
"does this token appear in the final rendered screen" assertions reliable
without a full VT100 emulator library.

stdlib only.
"""
from __future__ import annotations

import fcntl
import os
import pty
import re
import signal
import struct
import termios
import time
from dataclasses import dataclass, field
from typing import Optional

CSI_RE = re.compile(rb"\x1b\[([0-9;?]*)([a-zA-Z@])")
OSC_RE = re.compile(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
OTHER_ESC_RE = re.compile(rb"\x1b[()#][0-9A-Za-z]|\x1b[=>OM78]")
ANSI_RE = re.compile(
    rb"\x1b\[[0-9;?]*[a-zA-Z]"  # CSI sequences
    rb"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"  # OSC sequences
    rb"|\x1b[()#][0-9A-Za-z]"  # charset selection
    rb"|\x1b[=>OM78]"  # misc single-char escapes
    rb"|\r"
)


# Upper bound on how much of a still-unterminated escape sequence `TerminalGrid.feed()` will
# hold back waiting for the rest. Every sequence a TUI actually emits is far shorter than this;
# a stash that keeps growing means the stream is not really inside a sequence (a stray ESC
# ahead of text, say), and holding it forever would swallow the screen output behind it.
MAX_PENDING_ESCAPE = 512


def _is_incomplete_escape(data: bytes, i: int) -> bool:
    """Could `data[i:]` -- which starts with ESC -- still be the *prefix* of an escape
    sequence that the next read will complete?

    This is what decides whether `feed()` stashes the tail or gives up and skips the ESC, and
    it has to be answered from the bytes themselves. The previous "fewer than 16 bytes left"
    heuristic got both directions wrong: a sequence longer than the threshold (a long OSC
    title, a truecolor SGR run) was rendered into the grid as literal parameter text, and a
    short sequence could also land past it depending on where the read boundary fell.

    True only while the sequence is genuinely unterminated. An already-terminated sequence
    returns False even when the regexes above do not model it (`ESC [ > 4 ; 2 m`, private
    parameter bytes) -- so it is skipped now rather than stashed forever."""
    tail = data[i:]
    if len(tail) > MAX_PENDING_ESCAPE:
        return False
    if len(tail) == 1:
        return True  # a lone trailing ESC: any sequence could still follow
    intro = tail[1]
    if intro == 0x5B:  # '[' -- CSI: parameter bytes, then intermediates, then one final byte
        seen_intermediate = False
        for b in tail[2:]:
            if 0x40 <= b <= 0x7E:  # final byte: the sequence is complete
                return False
            if 0x20 <= b <= 0x2F:  # intermediate
                seen_intermediate = True
                continue
            if 0x30 <= b <= 0x3F and not seen_intermediate:  # parameter
                continue
            return False  # not a well-formed CSI at all
        return True
    if intro == 0x5D:  # ']' -- OSC: a string running until BEL or ST (ESC \), as OSC_RE has it
        body = tail[2:]
        if 0x07 in body:
            return False
        esc = body.find(0x1B)
        if esc == -1:
            return True  # still collecting the string
        return esc == len(body) - 1  # a trailing ESC may yet turn out to be the ST's `ESC \`
    if intro in b"()#":  # charset designation (ESC I F): one byte still to come
        return len(tail) == 2
    # ESC 7/8/=/>/O/M are complete at two bytes; anything else is not a sequence we model.
    return False


def strip_ansi(raw: bytes) -> str:
    """Legacy helper kept for anything that wants a flat, order-preserving strip
    (e.g. dumping raw-ish output for a human to read). Prefer `TerminalGrid` for
    assertions -- see the module docstring for why."""
    return ANSI_RE.sub(b"", raw).decode("utf-8", errors="replace")


class TerminalGrid:
    """A minimal 2D character grid that understands just enough control codes to
    render in-place redraws (spinners, status lines) correctly: CR, LF, backspace,
    `CSI K` (erase in line), and `CSI A/B/C/D` (cursor movement). Everything else
    (colors, alt-screen, absolute cursor positioning, OSC titles, ...) is parsed
    and discarded rather than applied -- adequate for "does token X appear
    anywhere in the rendered screen", not a general terminal emulator.
    """

    def __init__(self, cols: int = 120, max_rows: int = 4000):
        self.cols = cols
        self.max_rows = max_rows
        self.lines: list = [[]]
        self.row = 0
        self.col = 0
        self._pending = b""

    def feed(self, data: bytes) -> None:
        data = self._pending + data
        self._pending = b""
        i = 0
        n = len(data)
        while i < n:
            b = data[i]
            if b == 0x1B:  # ESC
                m = CSI_RE.match(data, i)
                if m:
                    self._apply_csi(m.group(1).decode(), m.group(2).decode())
                    i = m.end()
                    continue
                m = OSC_RE.match(data, i)
                if m:
                    i = m.end()
                    continue
                m = OTHER_ESC_RE.match(data, i)
                if m:
                    i = m.end()
                    continue
                # Nothing matched. Either the chunk ends mid-sequence (stash it for the next
                # feed(), so a torn escape is never rendered as text), or this really is not a
                # sequence we model (fall through and skip the ESC, as before).
                #
                # The test is what the bytes *are*, not how many are left: a fixed byte-count
                # threshold rendered any longer sequence (a long OSC title, an SGR truecolor
                # run) into the grid as literal parameter text whenever the read boundary fell
                # inside it, and a short sequence split unluckily across two reads could exceed
                # it too.
                if _is_incomplete_escape(data, i):
                    self._pending = data[i:]
                    break
                i += 1
                continue
            if b == 0x0D:  # \r
                self.col = 0
                i += 1
                continue
            if b == 0x0A:  # \n
                self.row += 1
                self.col = 0
                self._ensure_row(self.row)
                i += 1
                continue
            if b == 0x08:  # backspace
                self.col = max(0, self.col - 1)
                i += 1
                continue
            if b == 0x07:  # BEL
                i += 1
                continue
            # Decode one UTF-8 codepoint (best-effort).
            ch, adv = self._decode_one(data, i)
            self._put(ch)
            i += adv
        self._trim()

    @staticmethod
    def _decode_one(data: bytes, i: int):
        b0 = data[i]
        if b0 < 0x80:
            return chr(b0), 1
        length = 1
        if b0 & 0xE0 == 0xC0:
            length = 2
        elif b0 & 0xF0 == 0xE0:
            length = 3
        elif b0 & 0xF8 == 0xF0:
            length = 4
        chunk = data[i : i + length]
        try:
            return chunk.decode("utf-8"), length
        except UnicodeDecodeError:
            return "�", 1

    def _ensure_row(self, row: int) -> None:
        while len(self.lines) <= row:
            self.lines.append([])

    def _put(self, ch: str) -> None:
        self._ensure_row(self.row)
        line = self.lines[self.row]
        while len(line) <= self.col:
            line.append(" ")
        line[self.col] = ch
        self.col += 1

    def _apply_csi(self, params: str, final: str) -> None:
        parts = [p for p in params.replace("?", "").split(";") if p != ""]
        nums = [int(p) for p in parts if p.isdigit()]
        n1 = nums[0] if nums else 1
        if final == "A":  # cursor up
            self.row = max(0, self.row - n1)
        elif final == "B":  # cursor down
            self.row += n1
            self._ensure_row(self.row)
        elif final in ("C", "a"):  # cursor forward
            self.col += n1
        elif final == "D":  # cursor back
            self.col = max(0, self.col - n1)
        elif final == "E":  # cursor next line
            self.row += n1
            self.col = 0
            self._ensure_row(self.row)
        elif final == "F":  # cursor previous line
            self.row = max(0, self.row - n1)
            self.col = 0
        elif final == "G":  # cursor horizontal absolute
            self.col = max(0, n1 - 1)
        elif final in ("H", "f"):  # cursor position row;col (1-indexed)
            r = nums[0] - 1 if len(nums) >= 1 else 0
            c = nums[1] - 1 if len(nums) >= 2 else 0
            self.row = max(0, r)
            self.col = max(0, c)
            self._ensure_row(self.row)
        elif final == "K":  # erase in line
            self._ensure_row(self.row)
            mode = nums[0] if nums else 0
            line = self.lines[self.row]
            if mode == 0:
                del line[self.col :]
            elif mode == 1:
                for j in range(0, min(self.col, len(line))):
                    line[j] = " "
            elif mode == 2:
                self.lines[self.row] = []
        elif final == "J":  # erase in display
            mode = nums[0] if nums else 0
            if mode == 2 or mode == 3:
                self.lines = [[]]
                self.row = 0
                self.col = 0
        # SGR ('m'), cursor show/hide, scroll region, etc: no grid effect, ignored.

    def _trim(self) -> None:
        if len(self.lines) > self.max_rows:
            drop = len(self.lines) - self.max_rows
            self.lines = self.lines[drop:]
            self.row = max(0, self.row - drop)

    def text(self) -> str:
        return "\n".join("".join(line).rstrip() for line in self.lines)


@dataclass
class TuiSession:
    cmd: list
    cwd: str
    env: dict
    cols: int = 120
    rows: int = 40
    pid: int = field(init=False, default=-1)
    fd: int = field(init=False, default=-1)
    _buf: bytes = field(init=False, default=b"")
    _grid: "TerminalGrid" = field(init=False, default=None)

    def start(self) -> None:
        pid, fd = pty.fork()
        if pid == 0:
            # Forked child. Nothing may escape this block: an exception here would print a
            # Python traceback into the PTY (which the parent then scrapes as if it were TUI
            # output) and leave a second copy of the harness running. Every path either execs
            # or `os._exit`s.
            try:
                os.chdir(self.cwd)
            except BaseException as e:  # noqa: BLE001
                os.write(2, f"chdir to {self.cwd} failed: {e!r}\n".encode())
                os._exit(126)
            try:
                os.execvpe(self.cmd[0], self.cmd, self.env)
            except BaseException as e:  # noqa: BLE001
                os.write(2, f"execvpe failed: {e!r}\n".encode())
            # Only reachable if execvpe failed; a successful exec never returns.
            os._exit(127)
        self.pid = pid
        self.fd = fd
        self._grid = TerminalGrid(cols=self.cols)
        winsz = struct.pack("HHHH", self.rows, self.cols, 0, 0)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, winsz)

    def pump(self, duration: float) -> None:
        """Read whatever the PTY produces for up to `duration` seconds."""
        import select

        # `terminate()` closes the master and parks `fd` at -1; whatever the grid already holds
        # is still readable, but there is nothing left to pump.
        if self.fd < 0:
            return
        end = time.time() + duration
        while time.time() < end:
            remaining = max(0.0, end - time.time())
            r, _, _ = select.select([self.fd], [], [], min(0.2, remaining) if remaining > 0 else 0.2)
            if self.fd in r:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                self._buf += chunk
                if len(self._buf) > 4_000_000:
                    self._buf = self._buf[-2_000_000:]
                self._grid.feed(chunk)

    def screen_text(self, tail_chars: Optional[int] = None) -> str:
        """The rendered screen, reconstructed through `TerminalGrid` (handles
        in-place spinner/status-line redraws correctly -- see module docstring).
        `tail_chars` slices the end of the flattened text, not "the last N rows"."""
        text = self._grid.text()
        if tail_chars:
            return text[-tail_chars:]
        return text

    def raw_screen_text(self, tail_chars: Optional[int] = None) -> str:
        """The naive flat strip (order-preserving, but smears in-place redraws).
        Occasionally useful for debugging a `screen_text()` mismatch."""
        text = strip_ansi(self._buf)
        if tail_chars:
            return text[-tail_chars:]
        return text

    def raw_bytes(self) -> bytes:
        return self._buf

    def wait_until_contains(self, needle: str, timeout: float = 20.0, poll: float = 0.3) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if needle in self.screen_text():
                return True
            self.pump(poll)
        return needle in self.screen_text()

    def wait_until_answered(
        self, needle: str, prompt_text: str, timeout: float = 20.0, poll: float = 0.3
    ) -> bool:
        """Wait for `needle` to appear on a screen line that is NOT the echo of our own prompt.

        The TUI renders what you typed (`❯ reply with the single word ROGER`) as well as what the
        agent answered (a bare `ROGER` line). A plain `wait_until_contains(needle)` is therefore
        satisfied the instant the prompt is echoed, before the model has produced anything -- the
        same flaw a reviewer found in the client-side check, which searched a blob containing the
        same echo. Ignore any line that carries the prompt text, and require the needle somewhere
        else.
        """
        deadline = time.time() + timeout
        while True:
            if self._answered(needle, prompt_text):
                return True
            if time.time() >= deadline:
                return self._answered(needle, prompt_text)
            self.pump(poll)

    def _answered(self, needle: str, prompt_text: str) -> bool:
        probe = prompt_text.strip()
        for line in self.screen_text().splitlines():
            if probe and probe in line:
                continue  # the echo of what we typed
            if needle in line:
                return True
        return False

    def wait_until_matches(self, pattern: "re.Pattern", timeout: float = 20.0, poll: float = 0.3) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if pattern.search(self.screen_text()):
                return True
            self.pump(poll)
        return bool(pattern.search(self.screen_text()))

    def find_context(self, needle: str, before: int = 400, after: int = 400) -> Optional[str]:
        """A window of the rendered screen around the last occurrence of `needle`.
        Use this instead of `screen_text(tail_N)` once more output has arrived after
        the token you care about landed -- the tail would otherwise show only the
        (irrelevant) output that came after it."""
        text = self.screen_text()
        idx = text.rfind(needle)
        if idx < 0:
            return None
        start = max(0, idx - before)
        end = min(len(text), idx + len(needle) + after)
        return text[start:end]

    def _write(self, raw: bytes) -> None:
        if self.fd < 0:
            raise RuntimeError("this TUI session has been terminated; its PTY master is closed")
        os.write(self.fd, raw)

    def send_text(self, text: str) -> None:
        self._write(text.encode())

    def send_line(self, text: str) -> None:
        self._write(text.encode() + b"\r")

    def send_keys(self, raw: bytes) -> None:
        self._write(raw)

    def send_ctrl_c(self) -> None:
        self._write(b"\x03")

    def send_slash_exit(self) -> None:
        """Type /exit and press enter -- the documented slash command
        (crates/codegen/xai-grok-shell/src/session/slash_commands.rs: "exit"/"quit")."""
        self.send_line("/exit")

    def is_alive(self) -> bool:
        if self.pid <= 0:
            return False
        try:
            wpid, _ = os.waitpid(self.pid, os.WNOHANG)
        except ChildProcessError:
            return False
        return wpid == 0

    def close_fd(self) -> None:
        """Close the PTY master exactly once. Idempotent, and leaves `fd` at -1 so the read/write
        helpers can tell the session is finished rather than hitting EBADF on a recycled fd."""
        if self.fd < 0:
            return
        fd, self.fd = self.fd, -1
        try:
            os.close(fd)
        except OSError:
            pass

    def terminate(self, grace: float = 3.0) -> None:
        """Stop the child and release the PTY master.

        The master fd is closed on *every* path -- including "already exited" and "the pid is
        gone" -- or the matrix leaks one descriptor per TUI it starts for the life of the run.
        Safe to call more than once."""
        try:
            if self.pid > 0 and self.is_alive():
                try:
                    os.kill(self.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                else:
                    deadline = time.time() + grace
                    while time.time() < deadline and self.is_alive():
                        time.sleep(0.1)
                if self.is_alive():
                    try:
                        os.kill(self.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    try:
                        os.waitpid(self.pid, 0)
                    except ChildProcessError:
                        pass
        finally:
            self.close_fd()


def start_tui(
    gx_bin: str,
    grok_home: str,
    cwd: str,
    leader_socket: str,
    extra_args: Optional[list] = None,
    extra_env: Optional[dict] = None,
    cols: int = 120,
    rows: int = 40,
) -> TuiSession:
    env = dict(os.environ)
    env["GROK_HOME"] = grok_home
    env["GROK_LEADER_SOCKET"] = leader_socket
    env["TERM"] = "xterm-256color"
    env["COLUMNS"] = str(cols)
    env["LINES"] = str(rows)
    if extra_env:
        env.update(extra_env)
    cmd = [gx_bin, "--leader", "--cwd", cwd, "--no-alt-screen"] + (extra_args or [])
    sess = TuiSession(cmd=cmd, cwd=cwd, env=env, cols=cols, rows=rows)
    sess.start()
    return sess
