#!/usr/bin/env python3
"""Offline unit tests for the handoff harness itself (stdlib `unittest`, no gx binary, no
leader, no network).

The matrix cells can only be trusted if the harness's own assertions are trustworthy, and
three of them were not:

  * a cell that searched a JSON dump of every notification for its marker was satisfied by the
    echo of the prompt it had just sent (the prompt *is* "reply with the single word ROGER");
  * the disconnect cell counted `session/load`'s replayed notifications as "still streaming",
    so it would have reported a surviving turn for one that died with the first client;
  * the PTY grid stashed a torn escape sequence only while fewer than 16 bytes were left,
    rendering the parameters of anything longer into the screen as literal text.

Run:  python3 scripts/gx/handoff/test_handoff.py
"""
from __future__ import annotations

import json
import os
import sys
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import acp_client as acp  # noqa: E402
import run_matrix  # noqa: E402
import tui_pty  # noqa: E402


def note(payload: dict, method: str = "session/update") -> acp.Notification:
    return acp.Notification(ts=0.0, envelope_type="acp", method=method, payload=payload)


def session_update(update: dict, meta: dict = None) -> acp.Notification:
    params = {"sessionId": "sess-1", "update": update}
    if meta is not None:
        params["_meta"] = meta
    return note({"jsonrpc": "2.0", "method": "session/update", "params": params})


class AgentMessageTextTests(unittest.TestCase):
    """Cells 2 and 3 assert on the agent's answer, which is only what
    `agent_message_text()` returns -- never a dump of the notification list."""

    def test_prompt_echo_alone_does_not_count_as_an_answer(self):
        # The single notification a dead turn still produces: the leader echoing our own prompt
        # back as a user_message_chunk. Its text contains the marker verbatim.
        updates = [
            session_update(
                {
                    "sessionUpdate": "user_message_chunk",
                    "content": {
                        "type": "text",
                        "text": f"reply with the single word {run_matrix.MARKER_ROGER}",
                    },
                }
            )
        ]
        # What the old check did -- and why it proved nothing.
        blob = json.dumps([n.payload for n in updates])
        self.assertIn(run_matrix.MARKER_ROGER, blob)
        # What the cells assert now.
        self.assertNotIn(run_matrix.MARKER_ROGER, run_matrix.agent_message_text(updates))
        self.assertEqual(run_matrix.agent_message_text(updates), "")

    def test_agent_answer_is_joined_across_chunk_boundaries(self):
        # A live run really did split ROGER into "RO" + "GER" (docs/gx/handoff/prompt.md).
        updates = [
            session_update(
                {
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": "reply with the single word ROGER"},
                }
            ),
            session_update(
                {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "RO"}}
            ),
            session_update(
                {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "GER"}}
            ),
        ]
        self.assertEqual(run_matrix.agent_message_text(updates), "ROGER")
        self.assertIn(run_matrix.MARKER_ROGER, run_matrix.agent_message_text(updates))

    def test_pong_echo_alone_does_not_count_as_replayed_answer(self):
        # Same shape for cell 2, whose prompt is "reply with the single word PONG".
        updates = [
            session_update(
                {
                    "sessionUpdate": "user_message_chunk",
                    "content": {
                        "type": "text",
                        "text": f"reply with the single word {run_matrix.MARKER_PONG}",
                    },
                }
            )
        ]
        self.assertIn(run_matrix.MARKER_PONG, json.dumps([n.payload for n in updates]))
        self.assertNotIn(run_matrix.MARKER_PONG, run_matrix.agent_message_text(updates))


class ReplayFilterTests(unittest.TestCase):
    """Cell 7's "still streaming" must count only live post-reconnect traffic."""

    def test_replay_stamp_is_recognized_wherever_it_sits(self):
        on_params = session_update(
            {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "1"}},
            meta={"isReplay": True},
        )
        wrapped = note(
            {
                "jsonrpc": "2.0",
                "method": "_x.ai/session/update",
                "params": {
                    "params": {
                        "sessionId": "sess-1",
                        "update": {"sessionUpdate": "hook_annotation"},
                        "_meta": {"isReplay": True},
                    }
                },
            },
            method="_x.ai/session/update",
        )
        on_envelope = note(
            {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": "sess-1"},
                "_meta": {"isReplay": True},
            }
        )
        for n in (on_params, wrapped, on_envelope):
            self.assertTrue(run_matrix.is_replay_update(n), n.payload)

    def test_replay_only_window_is_not_still_streaming(self):
        replayed = [
            session_update(
                {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": str(i)},
                },
                meta={"isReplay": True},
            )
            for i in range(1, 6)
        ]
        # The old check: any notification in the listen window at all.
        self.assertGreater(len(replayed), 0)
        # The check now: only live (non-replay) session/update traffic.
        live = [n for n in replayed if not run_matrix.is_replay_update(n)]
        live_updates = [n for n in live if n.method == "session/update"]
        self.assertEqual(live_updates, [])
        self.assertFalse(len(live_updates) > 0)

    def test_live_updates_still_count(self):
        live = session_update(
            {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "7"}}
        )
        self.assertFalse(run_matrix.is_replay_update(live))
        window = [live] + [
            session_update({"sessionUpdate": "agent_message_chunk"}, meta={"isReplay": True})
        ]
        self.assertEqual(len([n for n in window if not run_matrix.is_replay_update(n)]), 1)

    def test_unparseable_payload_is_not_treated_as_replay(self):
        self.assertFalse(run_matrix.is_replay_update(note("not-json-at-all")))
        self.assertFalse(run_matrix.is_replay_update(session_update({}, meta={})))


class LockPathTests(unittest.TestCase):
    """The lock path the harness kills/unlinks must be the leader's, and never the socket."""

    @staticmethod
    def env_with_socket(socket_path: str) -> run_matrix.Env:
        return run_matrix.Env(
            gx_bin="/nonexistent/gx",
            grok_home="/nonexistent/home",
            cwd="/nonexistent/cwd",
            leader_socket=socket_path,
            model="m",
            fallback_model="f",
            out_dir=Path("/nonexistent/out"),
            docs_dir=Path("/nonexistent/docs"),
            handoff_docs_dir=Path("/nonexistent/docs/handoff"),
        )

    def test_sock_suffix_becomes_lock(self):
        self.assertEqual(
            self.env_with_socket("/tmp/gx-handoff-123.sock").lock_path,
            "/tmp/gx-handoff-123.lock",
        )

    def test_socket_without_sock_suffix_gets_a_distinct_lock(self):
        for socket_path in ("/tmp/gx-handoff-123", "/tmp/gx.handoff/leader", "/tmp/my.socket"):
            with self.subTest(socket_path=socket_path):
                lock = self.env_with_socket(socket_path).lock_path
                self.assertNotEqual(lock, socket_path)
                self.assertTrue(lock.endswith(".lock"), lock)
        # Matches the leader's own `socket.with_extension("lock")` derivation.
        self.assertEqual(self.env_with_socket("/tmp/my.socket").lock_path, "/tmp/my.lock")
        self.assertEqual(
            self.env_with_socket("/tmp/gx.handoff/leader").lock_path, "/tmp/gx.handoff/leader.lock"
        )


class TerminalGridEscapeTests(unittest.TestCase):
    """A torn escape sequence must never reach the grid as text."""

    @staticmethod
    def feed_one_byte_at_a_time(data: bytes) -> tui_pty.TerminalGrid:
        grid = tui_pty.TerminalGrid()
        for k in range(len(data)):
            grid.feed(data[k : k + 1])
        return grid

    def test_long_csi_split_one_byte_at_a_time_leaks_nothing(self):
        # 31 bytes -- comfortably past the old 16-byte stash threshold, which is exactly the
        # case that used to render "[38;2;255;128;0;48;2;12;34;56m" into the screen.
        csi = b"\x1b[38;2;255;128;0;48;2;12;34;56m"
        self.assertGreater(len(csi), 16)
        grid = self.feed_one_byte_at_a_time(b"before" + csi + b"after")
        self.assertEqual(grid.text(), "beforeafter")
        for leak in ("38", "255", "128", "[", ";", "m"):
            self.assertNotIn(leak, grid.text(), f"{leak!r} leaked into the grid")

    def test_long_osc_split_one_byte_at_a_time_leaks_nothing(self):
        osc = b"\x1b]0;gx handoff matrix -- a title well past sixteen bytes\x07"
        grid = self.feed_one_byte_at_a_time(b"before" + osc + b"after")
        self.assertEqual(grid.text(), "beforeafter")

    def test_erase_line_still_applies_when_split(self):
        # The sequence has to keep *working* when reassembled, not merely stay invisible.
        grid = self.feed_one_byte_at_a_time(b"stale\rnew\x1b[K")
        self.assertEqual(grid.text(), "new")

    def test_whole_sequence_in_one_chunk_is_unchanged(self):
        grid = tui_pty.TerminalGrid()
        grid.feed(b"before\x1b[38;2;255;128;0;48;2;12;34;56mafter")
        self.assertEqual(grid.text(), "beforeafter")

    def test_terminated_sequence_we_do_not_model_is_not_stashed_forever(self):
        # `ESC [ > 4 ; 2 m` uses a private parameter byte, so CSI_RE does not match it. It is
        # still terminated, so it must be skipped now rather than held back waiting for an end
        # that already arrived (which would swallow every byte behind it).
        grid = self.feed_one_byte_at_a_time(b"a\x1b[>4;2mb")
        self.assertEqual(grid._pending, b"")
        self.assertIn("a", grid.text())
        self.assertIn("b", grid.text())

    def test_lone_trailing_esc_is_held_not_rendered(self):
        grid = tui_pty.TerminalGrid()
        grid.feed(b"text\x1b")
        self.assertEqual(grid.text(), "text")
        self.assertEqual(grid._pending, b"\x1b")
        grid.feed(b"[2K")  # erase the whole line: the stashed ESC was kept, not dropped
        self.assertEqual(grid.text(), "")

    def test_runaway_stash_is_bounded(self):
        # An unterminated OSC must not hold the screen hostage forever.
        grid = tui_pty.TerminalGrid()
        grid.feed(b"\x1b]0;" + b"x" * (tui_pty.MAX_PENDING_ESCAPE + 10))
        self.assertEqual(grid._pending, b"")
        self.assertIn("x", grid.text())


class AnsweredNotEchoedTests(unittest.TestCase):
    """The TUI renders our own prompt too, so a bare `contains` proves nothing."""

    class FakeScreen:
        """Only what `_answered` touches: a screen_text() the test controls."""

        def __init__(self, text: str) -> None:
            self._text = text

        def screen_text(self, *_args, **_kwargs) -> str:
            return self._text

    def answered(self, screen_text: str) -> bool:
        return tui_pty.TuiSession._answered(
            self.FakeScreen(screen_text),
            "ROGER",
            "reply with the single word ROGER",
        )

    def test_the_echo_of_our_prompt_alone_is_not_an_answer(self):
        screen = "  \u276f reply with the single word ROGER      6:07 AM\n  Thought for 0.2s\n"
        # The old assertion -- a plain substring test -- is satisfied here, which is the bug.
        self.assertIn("ROGER", screen)
        self.assertFalse(self.answered(screen), "only the echo is on screen; nothing was answered")

    def test_the_agents_answer_counts(self):
        screen = (
            "  \u276f reply with the single word ROGER      6:07 AM\n"
            "  Thought for 0.2s\n"
            "  ROGER                                   6:07 AM\n"
        )
        self.assertTrue(self.answered(screen))

    def test_an_empty_screen_is_not_an_answer(self):
        self.assertFalse(self.answered(""))


if __name__ == "__main__":
    unittest.main(verbosity=2)
