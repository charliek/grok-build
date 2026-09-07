# gx remote lane (`gx-remote-api`)

A loopback HTTP/SSE façade over the grok **leader**, so a phone on an SSH port-forward can read —
and steer — the sessions running in a TUI on this machine.

This is a gx-only feature. Stock `grok` has nothing like it, and a stock-flavoured binary never
starts one.

- **Crate:** `crates/gx/gx-remote-api`
- **Host:** `crates/codegen/xai-grok-pager/src/gx_remote_lane.rs`, inside the leader process
- **CLI:** `gx remote status [--json]`, `gx remote up`; `gx doctor` prints a `gx leader` section
- **Bind:** `127.0.0.1` only, port `2421` by default

---

## What it is

The leader is a Unix-socket JSON-RPC multiplexer, not an HTTP server, so the lane is **not** a
route bolted onto it. It is one more ACP client of the leader — registered as
`client_type = "gx-remote-api"` with the gx `observer` capability, which makes it inert in the
leader's routing:

- never a session **driver**, so terminal/filesystem tool calls are never routed at a phone,
- never the **last-active** client,
- ignored by **exit-on-disconnect**, so a manually started leader still exits when its last real
  client leaves,
- **identity-only** meta injection on `session/load`, so touching a session from the phone does not
  switch off the TUI's status line or its fs/terminal routing.

It *is* a full subscriber, so it receives session fan-out and keeps a session **resident** — which
is the handoff the whole thing exists for: a session the phone has touched survives its TUI
exiting.

```text
  phone ──http──> 127.0.0.1:2421 ─┐
                                  │  gx-remote-api
                     AcpClient ───┤  id assignment, correlation, fan-out
                    LeaderLink ───┘  raw JSON-RPC payload strings
                         │
                   LeaderClient ── framed IPC ──> $GROK_HOME/gx-leader.sock
```

---

## How it starts

A gx build **starts a leader by default** (stock grok does not). The leader starts the lane. So in
practice: run `gx` once, and the lane is up.

Concretely, `main.rs`'s `agent leader` arm spawns the host task before awaiting `run_leader`,
because lock acquisition, `write_pid` and the socket bind all happen inside `run_leader` with no
post-bind hook. The task then:

1. polls the leader lock's PID every 100 ms (30 s cap) until it is **this process's**. A file check
   would not do: on Unix `listener_is_ready` is only `path.exists()`, and a killed predecessor
   leaves its socket file behind. If a *different live* process holds the lock, the task stands
   down — that leader hosts its own lane.
2. calls `gx_remote_api::serve`, which connects to the leader with bounded retry (250 ms backoff,
   30 s cap) until the `Register` handshake completes — the only thing that proves the listener is
   actually accepting.
3. binds `127.0.0.1:<port>`, writes the discovery record, and serves until the leader exits or the
   ACP link closes.

Failures warn and give up. A leader that cannot start a lane is still a perfectly good leader.

### Turning it off

| what | effect |
|---|---|
| `gx --no-leader` | no leader, therefore no lane, for that invocation |
| `[cli] use_leader = false` in `$GROK_HOME/config.toml` | no leader by default (shared with stock grok) |
| `GX_REMOTE_DISABLE=1` | leader as usual, **no lane** |
| `gx leader kill` | stops running leaders (and their lanes) |
| `GX_REMOTE_PORT=<n>` | bind a different loopback port; an unparseable value warns and falls back |

`gx doctor` prints which of these is in force.

---

## Discovery record and token

### The record

One JSON file per leader, written **after** the bind and removed on shutdown only if its
`instanceId` still matches:

```text
$GROK_HOME/gx-remote.json                    # the home's default leader socket
$GROK_HOME/gx-remote-<16 hex>.json           # any other socket (GROK_LEADER_SOCKET, non-default relay URL)
```

Mode `0600`, written atomically (temp file + `rename`), so a reader never sees a half-written
record.

```json
{
  "url": "http://127.0.0.1:2421",
  "pid": 48213,
  "instanceId": "9f2c4b1e7a0d3c5f8e1b2a4d6c8e0f13",
  "socketPath": "/home/you/.grok/gx-leader.sock",
  "tokenFile": "/home/you/.grok/gx-remote.token",
  "version": "1.0.16+gx.11",
  "startedAt": 1757203201123
}
```

Readers treat a record whose `pid` is dead as stale. `gx remote status` enumerates every
`gx-remote*.json` under `$GROK_HOME`, so several leaders on one home each show up separately.

### The token

`$GROK_HOME/gx-remote.token` — one per `$GROK_HOME`, shared by every leader on it (the token
authenticates the *human*, not the leader instance).

- 32 random bytes, lowercase hex (64 characters)
- created with `O_EXCL`, mode `0600`
- read once through `O_NOFOLLOW` and validated from the file descriptor: **refused**, never
  silently repaired, if it is a symlink, if its mode is not `0600`, if another uid owns it, or if
  the contents are not exactly 64 hex characters
- compared in constant time; never logged, never in a response, never printed by `gx remote` or
  `gx doctor` (both report the *path* only)

Send it as `Authorization: Bearer <token>` on every request **except `GET /v1/healthz`**. A
`?token=<token>` query parameter is accepted as a documented second choice, for SSE clients
(`EventSource`) that cannot set headers — it lands in proxy logs and shell history, so prefer the
header wherever you can.

### The client contract (read this before you send the token)

A loopback port outlives the process that bound it, and ports get recycled. A stale record can name
a port that a completely different local program now owns. So:

> **Call the token-free `GET /v1/healthz` first, check that its `instanceId` equals the record's,
> and only then send the bearer token.**

A client that skips that check can hand the token to whatever bound the port next. `gx remote
status` and `gx doctor` both do this check, and report a lane as unreachable when the ids disagree.

---

## Over SSH

The lane binds loopback only, deliberately: a LAN-visible listener would put every session on the
machine behind one bearer token. Reaching it from elsewhere is SSH's job.

```bash
# Forward the lane to your laptop
ssh -N -L 2421:127.0.0.1:2421 host &

# Read the token (never copy it into a command line you keep)
TOKEN=$(ssh host 'cat "${GROK_HOME:-$HOME/.grok}/gx-remote.token"')

# Confirm you are talking to the lane you think you are
curl -s http://127.0.0.1:2421/v1/healthz
ssh host 'cat "${GROK_HOME:-$HOME/.grok}/gx-remote.json"'   # compare instanceId

# Then use it
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:2421/v1/sessions
```

If the leader uses a non-default port (`GX_REMOTE_PORT`, or the preferred port was busy), read the
real one out of the discovery record — or run `ssh host gx remote status --json`.

---

## Endpoints

All bodies are JSON. `{id}` is a session id. Every route except `/v1/healthz` requires the token.

Set `AUTH="Authorization: Bearer $TOKEN"` for the examples below.

### `GET /v1/healthz` — the one unauthenticated route

```bash
curl -s http://127.0.0.1:2421/v1/healthz
```

```json
{ "ok": true, "version": "1.0.16+gx.11", "leaderPid": 48213,
  "instanceId": "9f2c4b1e7a0d3c5f8e1b2a4d6c8e0f13", "build": "gx" }
```

### `GET /v1/sessions` — the roster

```bash
curl -s -H "$AUTH" http://127.0.0.1:2421/v1/sessions
```

```json
{ "sessions": [
  { "sessionId": "01a0…", "title": "fix the flaky test", "cwd": "/home/you/proj",
    "activity": "working", "resident": true, "modelId": "grok-4-fast",
    "lastChangeUnixMs": 1757203255000, "attached": false,
    "pendingApprovals": 0, "approximate": true }
] }
```

`attached` is whether *this lane* has loaded the session — not whether a TUI is on it.
`approximate` is `true` while `pendingApprovals` is inferred from `activity` rather than counted
(which is the case for any session the lane has not attached).

Neither this route nor `GET /v1/sessions/{id}` attaches: a roster poll must not pin every session
resident.

### `GET /v1/sessions/{id}` — one row

Falls back to the full local session list for a dormant session the roster does not carry;
`404 unknown_session` otherwise.

### `GET /v1/sessions/{id}/history` — the persisted transcript

```bash
curl -s -H "$AUTH" "http://127.0.0.1:2421/v1/sessions/$SID/history?offset=-50&limit=50"
```

`offset` may be negative (count back from the end). The response is
`{ "updates": [ …envelopes… ], "totalCount": 812, "hasMore": true, "lastEventId": "01a0…-57" }`.
`lastEventId` is the newest id **in the returned page**, found by reverse-scanning it — it is
`null` when no line in the page carried one.

This is the first route that touches a session's content, so it is the first that attaches.

### `POST /v1/sessions` — start a session

```bash
curl -s -H "$AUTH" -H 'content-type: application/json' \
  -d '{"cwd":"/home/you/proj","text":"summarize the diff"}' \
  http://127.0.0.1:2421/v1/sessions
```

`201 { "sessionId": "01b1…" }`. The lane is an observer, so a session created this way has **no
driver** until a TUI attaches; driver-only reverse-requests and scheduled prompt injections are
dropped by the leader in the meantime.

### `POST /v1/sessions/{id}/messages` — say something

```bash
curl -s -H "$AUTH" -H 'content-type: application/json' \
  -d '{"text":"also run the tests","mode":"queue"}' \
  "http://127.0.0.1:2421/v1/sessions/$SID/messages"
```

`mode` is `"queue"` (default) or `"interject"`.

- `queue` → `session/prompt`. Its JSON-RPC response arrives when the **turn ends**, so the lane
  does not await it: `202 { "accepted": true, "mode": "queue" }` means *queued*, and the turn plays
  out on the event stream.
- `interject` → `x.ai/interject`, which answers immediately:
  `202 { "accepted": true, "mode": "interject", "status": "…" }`.

### `POST /v1/sessions/{id}/cancel`

```bash
curl -s -X POST -H "$AUTH" "http://127.0.0.1:2421/v1/sessions/$SID/cancel"
```

`202 { "accepted": true }`. `session/cancel` is a notification: the 202 says the cancel was handed
to the leader, not that the turn has stopped. The turn's actual end arrives on the stream.

### `GET /v1/sessions/{id}/events` — see [SSE](#sse) below

### Approvals — see [the approval lifecycle](#approvals) below

---

## Attach policy

The first request that touches a session's *content* (`/history`, `/events`, `/messages`,
`/cancel`, `/approvals`) performs a `session/load` with `_meta.noReplay: true`, **awaits it**, and
only then proceeds (`503 leader_unavailable` on failure). Attached sessions stay attached; there is
no explicit or idle detach in this cut, so a session the phone touched stays resident in the leader
until the leader exits.

Pending approvals raised before the first touch are recovered by the leader's replay-on-attach —
which happens *after* the load response, asynchronously, so a client whose very first call is
`GET …/approvals` may see nothing and find the approval a moment later on the stream.

---

## SSE

`GET /v1/sessions/{id}/events`. Frames:

| `event:` | `id:` | `data` | meaning |
|---|---|---|---|
| `update` | the full `eventId`, when the frame has one | normalized envelope | one session event, same shape `/history` returns |
| `session` | never | the session's summary row | the roster changed; re-fetch `/v1/sessions/{id}` |
| `approval` | never | the approval resource | an interaction opened, was answered, or resolved |
| `reset` | never | `{"reason": …}` | your view is not resumable; re-fetch |

A `: keepalive` comment goes out after 15 s of silence.

Only `update` carries an `id:`, and it carries the **whole** opaque string (`01a0…-57`), never the
counter alone. `session` and `approval` are state *invalidations*: they say something changed, and
the client re-reads it over the ordinary GET routes.

### The normalized envelope

Both `/history` and `update` frames carry the same shape:

```json
{ "eventId": "01a0…-57", "method": "session/update", "params": { … }, "timestamp": 1757203255000 }
```

`eventId` and `timestamp` are `null` when the source carried none. Live JSON-RPC notifications are
mapped into this shape (`jsonrpc` dropped, `timestamp` from `_meta.agentTimestampMs`); persisted
lines already have `method` / `params` / `timestamp`.

### `Last-Event-ID` resume

Send the last `update` id you saw (`EventSource` does this for you). The API parses the numeric
suffix only for ordering, and evaluates these in order — they overlap, and the order is what says
which answer wins:

1. a malformed cursor, or one whose session prefix is not this session → `reset
   {"reason":"cursor_unresolvable"}`, then live;
2. `newest` = the highest counter in the in-memory ring, or `lastEventId` from the persisted
   transcript when the ring is empty. `cursor > newest` → `reset {"cursor_unresolvable"}`, then
   live;
3. if `cursor >=` the ring's oldest, replay ring entries with a higher counter, then live;
4. otherwise read the persisted transcript, emit everything newer than the cursor, then the ring
   entries newer than that (deduplicated by `eventId`), then live.

The ring holds the last **2,000** `update` frames per session, capped at **50,000** across
sessions. A connection that falls more than **256** frames behind gets one
`reset {"reason":"slow_consumer"}` and continues from live.

Cursors survive a leader restart, because session load re-seeds the process-global event counter
above the persisted maximum and live broadcasts carry the same id as the persisted copy.
**Per-session contiguity is not promised** — the counter is process-global, so one session's ids are
sparse. Order and exact set are promised.

Approval and roster events are **not** exactly resumable: after a reconnect, reconcile them with
`GET /v1/sessions/{id}` and `GET /v1/sessions/{id}/approvals`.

```bash
curl -N -H "$AUTH" -H 'Last-Event-ID: 01a0…-57' \
  "http://127.0.0.1:2421/v1/sessions/$SID/events"
```

---

## Approvals

An approval in grok is an agent→client JSON-RPC **reverse-request**: the agent parks on a oneshot
and waits for an answer *on the connection the request arrived on*. A phone does not have a
connection that lives that long. So the lane keeps custody of the request — JSON-RPC id and all —
and lets a later HTTP call, over a completely different connection, supply the answer.

That is safe only because the leader already treats these four methods as **shared**: it broadcasts
them to every subscriber, caches the open ones per `(sessionId, toolCallId)`, replays them to a
client that attaches later, and the agent takes the **first** answer.

### Lifecycle

```text
  reverse-request  ─┐
                    ├─> pending ──POST──> submitted ──interaction_resolved──> resolved
  pending_interaction ┘   (placeholder until the request itself arrives)
```

Only `interaction_resolved` moves an entry to `resolved`. A submitted answer gets **no
acknowledgement** — first-answer-wins means a losing answer is discarded in silence, so a POST that
returns `202` means *sent*, not *accepted*.

### Routes

```bash
# Everything open, then the last 50 resolved (1 h) for this session
curl -s -H "$AUTH" "http://127.0.0.1:2421/v1/sessions/$SID/approvals"

# One of them
curl -s -H "$AUTH" "http://127.0.0.1:2421/v1/sessions/$SID/approvals/$TOOL_CALL_ID"

# Answer it
curl -s -H "$AUTH" -H 'content-type: application/json' \
  -d '{"response":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}' \
  "http://127.0.0.1:2421/v1/sessions/$SID/approvals/$TOOL_CALL_ID"
```

The resource:

```json
{ "id": "toolu_01…", "sessionId": "01a0…", "kind": "permission",
  "method": "session/request_permission", "status": "pending",
  "request": { …the raw reverse-request params… },
  "createdAt": 1757203255000, "submittedAt": null, "resolvedAt": null }
```

Routes are **session-scoped only**. A tool call id is unique inside its session's transcript and
nothing promises more, so there is deliberately no `/v1/approvals/{toolCallId}`.

`POST` returns `202 {"status":"submitted"}`, or `409 already_submitted` / `409 already_resolved`
when it was too late, or `404 unknown_approval`, or `400 bad_request` if `response` is missing or
is not a JSON object.

### One worked body per kind

`response` is forwarded **verbatim** as the JSON-RPC `result`; nothing here validates its shape (the
agent is the authority, and a lane that type-checked these would need a release every time an
option kind is added).

| `kind` | logical method | body |
|---|---|---|
| `permission` | `session/request_permission` | `{"response":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}` |
| `question` | `x.ai/ask_user_question` | `{"response":{"outcome":"accepted","answers":{"q1":["Use Postgres"]}}}` |
| `plan_approval` | `x.ai/exit_plan_mode` | `{"response":{"outcome":"approved"}}` |
| `mcp_elicitation` | `x.ai/mcp/elicit` | `{"response":{"outcome":"accept","content":{"email":"me@example.com"}}}` |

- **permission** — `optionId` must be one the request offered in `request.options[].optionId`; the
  kinds are `allow_once`, `allow_always`, `reject_once`, `reject_always`. Declining the whole turn
  is `{"outcome":{"outcome":"cancelled"}}`.
- **question** — answers are keyed by question id and each is a **list** of chosen labels. Other
  outcomes: `chat_about_this`, `skip_interview`, `cancelled`.
- **plan_approval** — a bare string outcome, `"approved"` (never `"approve"`); other outcomes are
  `cancelled` and `abandoned`, with optional feedback on a refusal.
- **mcp_elicitation** — `content` is whatever the server's `requestedSchema` asked for; other
  outcomes are `decline` and `cancel`.

### Two races to expect

- **The TUI answers first.** Your POST returns `409 already_resolved`. That is the honest report of
  a race the lane lost, not an error to retry.
- **The first read after an attach can be empty.** Replay-on-attach is asynchronous; watch the
  stream or poll once more.

---

## Authorization

The bearer token proves one thing: the caller can read the `$GROK_HOME` owner's token. That is one
local human authority, recorded as `Principal::Human` on every audited verb. It is **provenance**,
not the gate.

The gate is the *session's* state, expressed in the shared `xai-message-delivery-core` vocabulary
so the lane and the TUI cannot drift on what "interject" means:

| session activity | `queue` (prompt) | `interject` | `cancel` |
|---|---|---|---|
| `working` | allow | allow | allow |
| `needs_input` (an interaction is open) | allow (queues behind it) | `409 not_accepting` | allow |
| `idle`, `completed` | allow | `409 not_accepting` | `409 not_accepting` |
| `dormant`, `dead` | allow (after the attach) | `409 not_accepting` | `409 not_accepting` |

The activity used is the *effective* one: a session the lane holds an unanswered interaction for is
`needs_input` whatever the roster says (the roster lags; an approval does not). An activity this
build does not recognise collapses to queue-only.

`cancel` maps to the vocabulary's `InterruptAndSend`, which is a naming compromise: the lane sends
a bare `session/cancel` with nothing following it, and there is no interrupt-only operation in the
shared type.

---

## Errors

Every failure is `{"error":"<code>","message":"…"}` — the same envelope shed's hub uses, so a
mobile client parses gx errors with the code it already has. `error` is stable; `message` is prose.

| code | status | when |
|---|---|---|
| `unauthorized` | 401 | missing or wrong token (deliberately says nothing about which) |
| `bad_request` | 400 | unparseable body, empty `text`/`cwd`, bad `mode`, non-object `response` |
| `unknown_session` | 404 | no such session, anywhere |
| `unknown_approval` | 404 | no such open or recently-resolved interaction for that session |
| `already_submitted` | 409 | this lane already put an answer on the wire |
| `already_resolved` | 409 | the interaction closed — your answer, another client's, or a cancel |
| `not_accepting` | 409 | the session's state does not admit this verb (see the table above) |
| `leader_unavailable` | 503 | the leader did not answer, answered an error, or is gone — retry |

---

## Accepted risks

Deliberate, and written down so nobody has to rediscover them:

- **A well-formed `Authorization: Bearer <wrong>` shadows a valid `?token=` and returns 401.** One
  credential per request, header first. A client that sends both and gets a 401 has a wrong header,
  not a fallback to fix.
- **Unknown paths and wrong methods answer 404/405 without a token.** The token layer is a
  `route_layer`, so it runs only on a matched route. No protected handler is reachable that way;
  the only thing that leaks is whether a path exists.
- **`?token=` puts the secret in the request URI**, where shell history, proxy logs and browser
  history can keep it. Deliberate, for `EventSource`. The header is the documented default, and the
  lane never logs a full URI.
- **A stale discovery record can name a port a different local process now owns.** Hence the client
  contract above: `GET /v1/healthz`, match `instanceId`, *then* send the token.
- **Touched sessions stay resident** in the leader while it lives; memory grows with the set of
  sessions the phone has touched. Explicit and idle detach are future work.
- **API-created sessions have no driver** until a TUI attaches; scheduled prompt injections are
  dropped meanwhile.
- **Non-persisted events (approvals, roster) are not exactly resumable.** Reconcile with a GET
  after a reconnect. An API-owned journal is future work.
- **Exit-on-disconnect is evaluated only on a disconnect.** If the last non-observer client
  disconnects in the microsecond between the lane's `accept` and its `Register`, a manual leader
  that would have exited stays alive until the next disconnect. Accepted: the race can only keep a
  leader alive, never kill one early.
- **A detached leader with a loopback listener by default is a UX change** for every gx user.
  Mitigated by `gx doctor` and the disable knobs above.

Not in this cut, explicitly: TLS, non-loopback bind, config-file knobs, explicit/idle detach,
driver handoff for API-created sessions, an API-owned event journal, an OpenAPI document, and a
standalone `gx remote serve` daemon for leader-less hosts.

---

## Troubleshooting

```bash
gx remote status          # what is listening, is its pid alive, does healthz agree
gx remote status --json   # the same as an array, for scripts
gx remote up              # start a leader if there is none, then report
gx doctor                 # the leader decision, the socket/lock, the token's mode, the lanes
gx doctor --json          # the same under an additive top-level `gx` key
```

`gx remote status` exits 1 when no lane is reachable, so `gx remote status >/dev/null || gx remote
up` is a reasonable one-liner. Neither command ever prints the token.

---

## Verified

Run against the real binary on 2026-09-07, not against a mock: `gx 1.0.16+gx.11` built from this
branch at `32140f4a`, driven through `tmux` with `curl` on the other side. Every cell below was
executed; the model was `glm-5.3-flash` throughout. The lane ran under a scratch `$GROK_HOME`
seeded with a copy of the real config at mode `0600`, a scratch project directory, and
`GROK_LEADER_SOCKET` pointed at a short `/tmp` path — a socket path under a long scratch directory
overruns the ~108-byte `SUN_LEN` cap, which is a real failure we hit while building the earlier
handoff harness.

| # | What was checked | Result |
|---|---|---|
| 1 | A plain `gx` with no flags starts a leader | pass — socket and lock appeared |
| 2 | The leader starts the lane beside it | pass — discovery record and token written |
| 3 | `GET /v1/healthz` needs no token | pass — 200, `instanceId` matching the record |
| 4 | Every other route needs one | pass — 401 without, 200 with |
| 5 | A TUI-owned session is listed over HTTP | pass — with title, cwd, activity and `modelId` |
| 6 | Its transcript reads back | pass — prompt, thought, answer, `turn_completed` |
| 7 | Lazy attach | pass — `attached` flipped only on the first content request |
| 8 | **Interject from HTTP renders in the running TUI** | pass — the token appeared mid-turn and the agent answered it |
| 9 | A queued prompt from HTTP | pass — 202 while the turn ran |
| 10 | **A session created over HTTP opens in the TUI** | pass — `gx --resume` showed the remote turn |
| 11 | SSE frame shape | pass — `id:` only on persisted updates; `session` frames carry none |
| 12 | **SSE resume** | pass — resuming at `…-418` replayed `-420`, `-421`, `-424`: every frame after the cursor, in order, no duplicate, no `reset` |
| 13 | **A TUI approval is visible over HTTP** | pass — full request and all three options while the TUI showed its modal |
| 14 | **Answering it from HTTP unblocks the agent** | pass — `allow-once` by `curl`, the file was written, the TUI printed Done, status went pending → submitted → resolved |
| 15 | Answering twice | pass — 409 `already_resolved` |
| 16 | Unknown approval / session | pass — 404 `unknown_approval`, 404 `unknown_session` |
| 17 | A non-object `response` | pass — 400 `bad_request` |
| 18 | Cancel on an idle session | pass — 409 `not_accepting`, naming what is allowed |
| 19 | `gx remote status` against a live lane | pass — pid liveness, healthz, record and token paths, no token value |
| 20 | `gx doctor` | pass — the leader decision, socket and lock labelled as the default relay, lock pid liveness, the lane and its token's mode |
| 21 | A stale record after the leader is killed | pass — reported not running and unreachable, exit 1, with recovery advice |

Two things worth knowing, both found here rather than in review:

- A hint-only approval is real. When the permission classifier auto-approves, the lane still sees
  the `pending_interaction` and `interaction_resolved` pair and records an entry whose `method` and
  `request` are `null` and whose `createdAt` equals its `resolvedAt`. That is the placeholder path
  working, not a defect.
- Reaching cell 13 needs a permission the classifier will not wave through. `permission_mode = "ask"`
  is necessary but not sufficient: a `sleep` was auto-approved, while a write outside the workspace
  prompted. Use the latter shape to reproduce.

Not exercised: TLS and non-loopback binds (out of scope by design), a phone client over a real SSH
forward (the transport is ordinary TCP on loopback and was driven locally), a stock-flavoured build
at runtime (`is_gx_build()` is compiled in, so it is covered by unit tests only), and concurrent
multi-client contention on one approval beyond the two-answer race above.
