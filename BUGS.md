# gx bugs

Diagnosis from gx session `01a07278-4913-76b3-8676-b52da3e667b9` (Promptly planning,
2026-09-05) plus local session search. The previous session was asked to write this
file and did not; the recap still said it was pending.

Executable fix plan: `~/.claude/plans/grok-build/003-codex-keepalive.md`.

---

## BUG-1 (P0) — ChatGPT/Codex `keepalive` SSE event kills the turn

**Status:** fixed. Codex/ChatGPT Responses SSE liveness frames (`keepalive` /
`ping` / `heartbeat`) and unknown top-level `type` values are skipped in
`crates/codegen/xai-grok-sampler/src/gx_responses_sse.rs`, selected by the
existing `codex_compat` flag. Strict xAI Responses decoding is unchanged.

Skipped frames never reach L2 (`stream/responses.rs`) and do **not** reset
the 300s typed-event idle timeout.

**Symptom.** Mid-turn, gx surfaces:

```
Session error: Internal error: {
  "message": "serialization error: unknown variant `keepalive`, expected one of
  `response.created`, `response.in_progress`, `response.completed`, ..."
}
```

Serde points at **line 1 column 19**, which is the closing quote of:

```json
{"type":"keepalive"}
```

The turn is aborted. Work already written to disk can survive; the agent session does
not. Retry does not help: `SamplingError::Serialization` is **fatal**
(`crates/codegen/xai-grok-sampler/src/retry.rs` `classify_error` falls through to
`RetryDecision::Fatal`).

**Who hits it.** Confirmed on `gpt-6-astra` (`model_provider = "openai-codex"`,
`api_backend = "responses"`, `codex_compat = true`). Same decoder is used for every
ChatGPT-plan model on that provider (`gpt-5.6-sol` / terra / luna). It is intermittent
because the server only emits the frame as a liveness heartbeat during longer
generations:

| Child (promptly session `01a07278`) | Model | Duration | Result |
|---|---|---|---|
| `01a072a1-df97-7d32-9cb8-be6c6179795e` | gpt-6-astra | 994s, 8 model calls | failed keepalive |
| `01a072b1-f123-72c1-9cc5-ea92ebfcf6a0` | gpt-6-astra | 966s | completed (no heartbeat that turn) |
| `01a072c1-0e6f-7ff1-958e-a390a65be5f4` | gpt-6-astra | 184s, 14 model calls | failed keepalive |

The parent Astra session also recorded `turn_ended outcome=error` on several turns.
Local session search found **58** `unknown variant \`keepalive\`` hits across promptly
and grok-build gx sessions. No other production unknown Responses `type` values
appeared (test fixtures use `bogus` / `ultra`).

**Root cause.** gx deserializes each Responses SSE `data:` payload with
`async-openai`'s closed internally-tagged enum:

```rust
// ~/.cargo/git/checkouts/async-openai-…/async-openai/src/types/responses/stream.rs
#[serde(tag = "type")]
pub enum ResponseStreamEvent { /* response.created, …, error */ }
```

Call site: `deserialize_response_event` in
`crates/codegen/xai-grok-sampler/src/client.rs`. On unknown `type` it logs and returns
`SamplingError::Serialization`. The SSE scan at the same file already **swallows** one
non-enum event (`response.doom_loop_check`) *before* that deserialize, with
`Some(None)` so `filter_map` skips it. `keepalive` is not on that intercept list.

Display prefix `serialization error: ` is
`SERIALIZATION_DISPLAY_PREFIX` in `crates/codegen/xai-grok-sampling-types/src/error.rs`.
ACP wraps it as `Internal error: { "message": "serialization error: …" }`.

**What Codex does (pulled `../thirdparty/codex` `dfea985976`).** Codex does **not**
use a closed enum. `codex-rs/codex-api/src/sse/responses.rs` parses
`ResponsesStreamEvent { kind: String, … }`. Unknown kinds, including anything that is
not a handled `response.*` event, hit `_ => { debug!(…); Ok(None) }` and the stream
continues. JSON that fails to parse is `continue`'d, not fatal. So Codex never dies on
`{"type":"keepalive"}`.

**Fix shape (see plan 003).** Do not add a `Keepalive` variant to the third-party
enum and do not grow `deserialize_response_event` (upstream xAI decoder). gx is a
long-lived rebase fork: put Codex stream policy in a **new module** with a
`ResponsesSseFrameHandler` trait, selected by existing `codex_compat`. `client.rs`
gets one `// gx:` call site.

1. `CodexResponsesSseHandler`: skip liveness (`keepalive` / `ping` / `heartbeat`)
   by SSE `event:` name **or** JSON `"type"` before typed deserialize.
2. Same handler: if deserialize still fails with serde `unknown variant`, skip
   with a rate-limited warn (Astra is new). Malformed JSON and missing fields on
   *known* variants still fail.
3. `StrictResponsesSseHandler`: today's `deserialize_response_event` (xAI unchanged).

Skipped frames never reach L2 (`stream/responses.rs`), so they do **not** reset the
300s typed-event idle timeout. That is accepted existing policy (same class as Chat
Completions empty chunks), not part of this fix. Do not inject dummy events to keep
the turn alive.

**Non-goals for this bug.** Empty ChatGPT `response.completed.output: []` (already
reassembled from `output_item.done` in `stream/responses.rs`). Encrypted-content 400s
(already have a dedicated retry). Chat Completions keepalives (empty chunks; different
backend). WebSocket pings.

**Verify.** Unit: splice `{"type":"keepalive"}` into a valid Responses fixture and
assert `response.completed` still arrives. Live: rebuild `gx`, `/model gpt-6-astra`,
run a multi-minute tool-using turn; it must not die with this serialization error.

---

## Other notes (not this bug)

- `unknown variant \`ultra\`` / `bogus` in grok-build sessions are test/catalog
  probes, not ChatGPT stream frames.
- Promptly planning docs already mention this host failure as a fixture requirement
  for Promptly itself; that does not fix gx.
