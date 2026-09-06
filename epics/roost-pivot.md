# Epic: Roost Pivot — gx's part

> A gx-owned file, outside `docs/`, so it never rides an upstream sync or
> a published site. It is a pointer plus the rules that apply in this
> repo — never a copy of the roadmap.

**Why this exists — read first:**
https://claude.ai/code/artifact/add27f67-3d15-4541-bd3f-eda3f34fcc48
(Private — opens with the owner's claude.ai login. A 404 from anywhere
else is expected, not a broken link.)
Sections that matter here: §02 (the handoff matrix — the one red cell is
gx's), §04 Q9 (where gx fits, and the leader trap), §06 Track A.

**Tracking:** https://github.com/users/charliek/projects/4 —
`Epic: Roost Pivot`. Your PR body must contain
`Closes charliek/grok-build#<n>`. Status moves by itself when that merges.
Never edit board status by hand.

## This repo's items

The issue is authoritative; this table is a map.

```bash
gh issue list -R charliek/grok-build --state open --search "in:title [A"
```

| ID | issue | phase | one line |
|---|---|---|---|
| A0 | [#6](https://github.com/charliek/grok-build/issues/6) | RP/M2 | **build gx** — nothing gx-only is verifiable until this runs |
| A1 | [#7](https://github.com/charliek/grok-build/issues/7) | RP/M2 | re-run the handoff matrix on gx: does `x.ai/interject` live-update an attached TUI? does a session survive a dropped socket? |
| A2 | [#8](https://github.com/charliek/grok-build/issues/8) | RP/M2 | leader-on by default — otherwise nothing external can see a session |
| A3 | [#9](https://github.com/charliek/grok-build/issues/9) | RP/M3 | `gx-remote-api`: HTTP/SSE on the leader, approvals as addressable state |

**Start with A0**, and start it now — it is a long unattended compile and
everything else here waits on it.

## Rules that apply in this repo

- **Fork discipline is the design constraint.** New gx-only files
  preferred; edits to upstream-owned files are surgical and marked
  `// gx:`; tests are the targeted set, never the full workspace; `gx/main`
  is force-pushed on each upstream rebase. A3 is a new crate for exactly
  this reason — extending ACP's `ext_method` dispatch would conflict on
  every rebase.
- **Mount on the leader, not only `agent serve`.** `agent serve` boots its
  own agent and is not a window into TUI-owned sessions. The leader is,
  and it is default-off (A2). Note `grok agent --leader serve` exits;
  `serve --leader-socket <path>` is the form that works.
- **Loopback only.** The remote lane binds `127.0.0.1`; SSH is the
  transport. No TLS, no network listener, within this epic.
- **Do not inherit `x.ai/interject`'s missing ownership check.** Any new
  surface keys authorization on `xai-message-delivery-core`'s
  `Principal`/`Operation`, even on loopback.
- **Approvals become state, not requests.** ACP's reverse-request model
  needs a live connection; phones lose connections. `PendingInteraction` /
  `InteractionResolved` and the leader's first-answer-wins are the
  primitives — expose them as `GET`/`POST` resources.
- **gx stays badged `grok` in roost** (shares `$GROK_HOME` and the
  source). It stamps an opt-in key in `tab.agent_report`'s `metadata` map
  once A3 exists; a separate identity waits until the two actually
  diverge.

## Cross-repo edges

- **A1 ↔ roost R6.** Roost's grok adapter currently gates turn-end on
  `Stop`, which re-fires per continuation (false idle) and does not hear
  `StopFailure`. A1 verifies against the fixed adapter; until R6 lands,
  expect a false idle in roost's dot after a continuation.
- **A3 → roost R8.** Once the remote lane exists, gx stamps the metadata
  key R8 documents.
- **A3 → shed-mobile / shed.** The phone's gx transcript and approvals
  consume this surface; shape it for a client that reconnects, with
  `Last-Event-ID` resume off the `updates.jsonl` offset.
- The shared `$GROK_HOME` means roost's hook file
  (`$GROK_HOME/hooks/roost.json`) applies to gx unchanged — do not fork
  the hook contract.
