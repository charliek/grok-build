# gx: known-good ChatGPT/Codex request bodies

Captured live from `POST https://chatgpt.com/backend-api/codex/responses` during
the gx Phase-0A spike (2026-08-25) — every one of these bodies was answered
`200` by the real endpoint. They are the contract
`conversation/responses_tests.rs` asserts gx's `codex_compat` output against:
same keys, same types, nothing the validator rejects.

| file | what it proves |
| --- | --- |
| `12-grok-shape-store-false.json` | grok's own minimal Responses body plus `store: false` is accepted |
| `13a-turn1.json` / `13b-turn2-replay.json` | multi-turn reasoning replay with `store: false` + `include: ["reasoning.encrypted_content"]` |
| `14a-tool-turn1.json` / `14b-tool-turn2-output.json` | a function-call round trip (`function_call` → `function_call_output`) |

**Sanitization.** The bearer is `<REDACTED>` and the account id is
`<REDACTED-ACCOUNT-ID>` as captured. Each file was further trimmed to
`{case, url, status, request_headers, request_body}`: the SSE event stream and
the response headers were dropped, because the assertions only need the request
shape and those sections carried a pseudonymous user id and an opaque
server-side turn-state blob that have no business in a repo.

Nothing here is a credential, and nothing here is replayed against a network.
