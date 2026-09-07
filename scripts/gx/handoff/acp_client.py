#!/usr/bin/env python3
"""Minimal ACP-over-leader-socket client for the gx handoff matrix (A1 / C2).

Wire format, read from crates/codegen/xai-grok-shell/src/leader/protocol.rs:
  - Frame: 4-byte big-endian length prefix + that many bytes of JSON.
  - Client -> server envelope (`ClientMessage`, serde tag = "type", snake_case):
        {"type": "register", "client_type": "...", "mode": "stdio", "capabilities": {...}}
        {"type": "acp", "payload": "<JSON-RPC 2.0 string>"}
        {"type": "control", "request_id": "...", "command": {...}}
        {"type": "ping"}
        {"type": "disconnect"}
  - Server -> client envelope (`ServerMessage`, same tagging):
        {"type": "registered", "client_id": N, "ready": bool, ...}
        {"type": "acp", "payload": "<JSON-RPC 2.0 string>"}
        {"type": "leader_ready"}
        {"type": "shutting_down", "reason": "...", "delay_ms": N}
        {"type": "shutdown"}
        {"type": "pong"}
        {"type": "error", "code": N, "message": "..."}

Inside the "acp" payload is standard JSON-RPC 2.0. The leader namespaces
request ids per-connection internally but restores the original id before
forwarding a response back to the client that sent the request, so from a
single client's point of view ids behave exactly like talking to a bare ACP
agent (crates/codegen/xai-grok-shell/src/leader/server.rs:
`rewrite_request_id` / `restore ... in place`).

Notifications for a session (session/update, x.ai/session/interjection, ...)
are broadcast to every client that has touched that sessionId (session/new,
session/load, session/prompt, or x.ai/interject all subscribe the sender --
see `extract_session_id` call sites in server.rs). This client library queues
every unmatched inbound "acp" payload as a "notification" for the caller to
inspect.

stdlib only, no pip dependencies.
"""
from __future__ import annotations

import json
import socket
import struct
import sys
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Optional

MAX_MESSAGE_SIZE = 64 * 1024 * 1024


class LeaderProtocolError(RuntimeError):
    pass


class LeaderTimeout(RuntimeError):
    pass


def _read_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise LeaderProtocolError("connection closed while reading frame")
        buf.extend(chunk)
    return bytes(buf)


def read_frame(sock: socket.socket) -> bytes:
    header = _read_exact(sock, 4)
    (length,) = struct.unpack(">I", header)
    if length > MAX_MESSAGE_SIZE:
        raise LeaderProtocolError(f"frame too large: {length}")
    return _read_exact(sock, length)


def write_frame(sock: socket.socket, data: bytes) -> None:
    if len(data) > MAX_MESSAGE_SIZE:
        raise LeaderProtocolError(f"frame too large: {len(data)}")
    sock.sendall(struct.pack(">I", len(data)) + data)


@dataclass
class Notification:
    ts: float
    envelope_type: str  # "acp" (jsonrpc notification) or a raw ServerMessage type
    method: Optional[str]
    payload: Any


@dataclass
class LeaderClient:
    """A single leader-socket connection, speaking ACP JSON-RPC over the framed IPC."""

    socket_path: str
    client_type: str = "gx-handoff-matrix"
    mode: str = "stdio"
    capabilities: dict = field(default_factory=dict)
    connect_timeout: float = 10.0

    sock: socket.socket = field(init=False, default=None)
    registration: dict = field(init=False, default=None)
    _next_id: int = field(init=False, default=1)
    _lock: threading.Lock = field(init=False, default_factory=threading.Lock)
    _responses: dict = field(init=False, default_factory=dict)
    _notifications: list = field(init=False, default_factory=list)
    _reverse_requests: list = field(init=False, default_factory=list)
    _server_events: list = field(init=False, default_factory=list)  # raw non-acp ServerMessages
    _closed: bool = field(init=False, default=False)
    _reader_thread: threading.Thread = field(init=False, default=None)
    _on_notification: Optional[Callable[[Notification], None]] = field(init=False, default=None)

    def connect(self) -> None:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(self.connect_timeout)
        s.connect(self.socket_path)
        s.settimeout(None)
        self.sock = s
        self._reader_thread = threading.Thread(target=self._reader_loop, daemon=True)
        self._reader_thread.start()

    def _reader_loop(self) -> None:
        try:
            while not self._closed:
                try:
                    data = read_frame(self.sock)
                except (LeaderProtocolError, OSError):
                    break
                try:
                    msg = json.loads(data)
                except json.JSONDecodeError:
                    continue
                self._handle_server_message(msg)
        finally:
            self._closed = True

    def _handle_server_message(self, msg: dict) -> None:
        mtype = msg.get("type")
        if mtype == "registered":
            with self._lock:
                self.registration = msg
        elif mtype == "acp":
            payload = msg.get("payload", "")
            try:
                rpc = json.loads(payload)
            except json.JSONDecodeError:
                rpc = None
            if isinstance(rpc, dict) and "id" in rpc and "method" not in rpc:
                # A response to one of our requests.
                with self._lock:
                    self._responses[rpc["id"]] = rpc
            else:
                note = Notification(
                    ts=time.time(),
                    envelope_type="acp",
                    method=(rpc or {}).get("method") if isinstance(rpc, dict) else None,
                    payload=rpc if rpc is not None else payload,
                )
                with self._lock:
                    if isinstance(rpc, dict) and "id" in rpc and "method" in rpc:
                        # Reverse request from the agent (e.g. permission request). We don't
                        # answer these in the handoff matrix (prompts are tool-free), but we
                        # must not silently drop them without recording that they happened.
                        self._reverse_requests.append(note)
                    else:
                        self._notifications.append(note)
                if self._on_notification:
                    self._on_notification(note)
        else:
            with self._lock:
                self._server_events.append(msg)

    # -- registration -----------------------------------------------------

    def register(self, timeout: float = 10.0) -> dict:
        write_frame(
            self.sock,
            json.dumps(
                {
                    "type": "register",
                    "client_type": self.client_type,
                    "mode": self.mode,
                    "capabilities": self.capabilities,
                }
            ).encode(),
        )
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self._lock:
                if self.registration is not None:
                    reg = self.registration
                    break
            time.sleep(0.02)
        else:
            raise LeaderTimeout("timed out waiting for 'registered'")
        if not reg.get("ready", True):
            # Wait for a LeaderReady server event.
            deadline = time.time() + timeout
            while time.time() < deadline:
                with self._lock:
                    if any(e.get("type") == "leader_ready" for e in self._server_events):
                        break
                time.sleep(0.02)
            else:
                raise LeaderTimeout("timed out waiting for 'leader_ready'")
        return reg

    # -- raw ACP send/recv --------------------------------------------------

    def send_acp_raw(self, rpc_text: str) -> None:
        write_frame(self.sock, json.dumps({"type": "acp", "payload": rpc_text}).encode())

    def next_id(self) -> int:
        with self._lock:
            i = self._next_id
            self._next_id += 1
            return i

    def call(self, method: str, params: Optional[dict] = None, timeout: float = 30.0, req_id: Optional[int] = None) -> dict:
        """Send a JSON-RPC request and block for its response. Returns the full JSON-RPC response dict."""
        rid = req_id if req_id is not None else self.next_id()
        rpc = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}
        self.send_acp_raw(json.dumps(rpc))
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self._lock:
                if rid in self._responses:
                    return self._responses.pop(rid)
            time.sleep(0.02)
        raise LeaderTimeout(f"timed out waiting for response to {method} (id={rid})")

    def notify(self, method: str, params: Optional[dict] = None) -> None:
        """Send a JSON-RPC notification (no id, no response expected)."""
        rpc = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        self.send_acp_raw(json.dumps(rpc))

    # -- notification inspection --------------------------------------------

    def notifications_snapshot(self) -> list:
        with self._lock:
            return list(self._notifications)

    def reverse_requests_snapshot(self) -> list:
        with self._lock:
            return list(self._reverse_requests)

    def wait_for_notification(self, predicate: Callable[[Notification], bool], timeout: float = 15.0) -> Optional[Notification]:
        deadline = time.time() + timeout
        seen = 0
        while time.time() < deadline:
            with self._lock:
                notes = self._notifications[seen:]
                seen = len(self._notifications)
            for n in notes:
                if predicate(n):
                    return n
            time.sleep(0.05)
        return None

    def collect_notifications_for(self, duration: float) -> list:
        start_len = len(self._notifications)
        time.sleep(duration)
        with self._lock:
            return list(self._notifications[start_len:])

    # -- lifecycle ------------------------------------------------------------

    def close(self) -> None:
        self._closed = True
        try:
            self.sock.close()
        except OSError:
            pass


# ---- high-level convenience helpers used by run_matrix.py -------------------


def new_client(socket_path: str, client_type: str = "gx-handoff-matrix", capabilities: Optional[dict] = None) -> LeaderClient:
    c = LeaderClient(socket_path=socket_path, client_type=client_type, capabilities=capabilities or {})
    c.connect()
    c.register()
    return c


def initialize(client: LeaderClient, timeout: float = 15.0) -> dict:
    return client.call("initialize", {"protocolVersion": "0.1"}, timeout=timeout)


def session_new(client: LeaderClient, cwd: str, timeout: float = 30.0) -> dict:
    return client.call("session/new", {"cwd": cwd, "mcpServers": []}, timeout=timeout)


def session_load(client: LeaderClient, session_id: str, cwd: str, timeout: float = 30.0) -> dict:
    return client.call("session/load", {"sessionId": session_id, "cwd": cwd, "mcpServers": []}, timeout=timeout)


def session_prompt(client: LeaderClient, session_id: str, text: str, timeout: float = 60.0) -> dict:
    return client.call(
        "session/prompt",
        {"sessionId": session_id, "prompt": [{"type": "text", "text": text}]},
        timeout=timeout,
    )


def session_prompt_async(client: LeaderClient, session_id: str, text: str) -> int:
    """Send session/prompt without blocking for the response; returns the request id."""
    rid = client.next_id()
    rpc = {
        "jsonrpc": "2.0",
        "id": rid,
        "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": text}]},
    }
    client.send_acp_raw(json.dumps(rpc))
    return rid


def interject(client: LeaderClient, session_id: str, text: str, timeout: float = 15.0) -> dict:
    # Ext/custom ACP methods (anything not in the core ACP method set) MUST carry a
    # leading "_" on the wire: agent-client-protocol 0.10.4's decode_request() only
    # routes to ext_method() via `method.strip_prefix('_')`; the bare name 404s
    # ("Method not found") -- confirmed empirically running this harness.
    return client.call("_x.ai/interject", {"sessionId": session_id, "text": text}, timeout=timeout)


def session_list(client: LeaderClient, cwd: Optional[str] = None, timeout: float = 15.0) -> dict:
    params = {"cwd": cwd} if cwd else {}
    return client.call("_x.ai/session/list", params, timeout=timeout)


def sessions_roster(client: LeaderClient, timeout: float = 15.0) -> dict:
    return client.call("_x.ai/sessions/list", {}, timeout=timeout)


def extract_text_from_session_update(payload: dict) -> str:
    """Best-effort extraction of any human-readable text out of a session/update notification."""
    try:
        params = payload.get("params", {})
        update = params.get("update", {})
        chunks = []
        content = update.get("content")
        if isinstance(content, dict) and "text" in content:
            chunks.append(content["text"])
        for key in ("text", "delta"):
            if key in update and isinstance(update[key], str):
                chunks.append(update[key])
        return " ".join(chunks)
    except AttributeError:
        return ""


def sanitize(obj: Any) -> Any:
    """Redact anything that looks like a secret before it is written to a transcript file."""
    text = json.dumps(obj) if not isinstance(obj, str) else obj
    import re

    patterns = [
        (re.compile(r"sk-[A-Za-z0-9_-]{6,}"), "sk-***REDACTED***"),
        (re.compile(r"Bearer\s+[A-Za-z0-9._-]+", re.IGNORECASE), "Bearer ***REDACTED***"),
        (re.compile(r'"(api_key|apiKey|token|access_token|refresh_token|authorization)"\s*:\s*"[^"]*"', re.IGNORECASE),
         r'"\1": "***REDACTED***"'),
    ]
    for pat, repl in patterns:
        text = pat.sub(repl, text)
    if not isinstance(obj, str):
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            return text
    return text


# ---- CLI --------------------------------------------------------------------


def _cli() -> int:
    import argparse

    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--socket", required=True, help="Path to the leader unix socket (GROK_LEADER_SOCKET / $GROK_HOME/gx-leader.sock)")
    p.add_argument("--client-type", default="gx-handoff-matrix-cli")
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("session-list")
    sub.add_parser("sessions-roster")

    sn = sub.add_parser("session-new")
    sn.add_argument("--cwd", required=True)

    sl = sub.add_parser("session-load")
    sl.add_argument("--session-id", required=True)
    sl.add_argument("--cwd", required=True)
    sl.add_argument("--listen-secs", type=float, default=2.0)

    spr = sub.add_parser("session-prompt")
    spr.add_argument("--session-id", required=True)
    spr.add_argument("--text", required=True)
    spr.add_argument("--timeout", type=float, default=60.0)

    ij = sub.add_parser("interject")
    ij.add_argument("--session-id", required=True)
    ij.add_argument("--text", required=True)

    raw = sub.add_parser("raw")
    raw.add_argument("--method", required=True)
    raw.add_argument("--params", default="{}")
    raw.add_argument("--notification", action="store_true")

    args = p.parse_args()

    client = new_client(args.socket, client_type=args.client_type)
    initialize(client)

    try:
        if args.cmd == "session-list":
            print(json.dumps(sanitize(session_list(client)), indent=2))
        elif args.cmd == "sessions-roster":
            print(json.dumps(sanitize(sessions_roster(client)), indent=2))
        elif args.cmd == "session-new":
            print(json.dumps(sanitize(session_new(client, args.cwd)), indent=2))
        elif args.cmd == "session-load":
            resp = session_load(client, args.session_id, args.cwd)
            notes = client.collect_notifications_for(args.listen_secs)
            print(json.dumps(sanitize({"response": resp, "notifications": [n.payload for n in notes]}), indent=2))
        elif args.cmd == "session-prompt":
            print(json.dumps(sanitize(session_prompt(client, args.session_id, args.text, timeout=args.timeout)), indent=2))
        elif args.cmd == "interject":
            print(json.dumps(sanitize(interject(client, args.session_id, args.text)), indent=2))
        elif args.cmd == "raw":
            params = json.loads(args.params)
            if args.notification:
                client.notify(args.method, params)
                print("{}")
            else:
                print(json.dumps(sanitize(client.call(args.method, params)), indent=2))
    finally:
        client.close()
    return 0


if __name__ == "__main__":
    sys.exit(_cli())
