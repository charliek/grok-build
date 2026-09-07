# Cell 1: list

Session id under test: `c63f9ebb-c4d1-4c48-9bc3-150d066f19ca`

## initialize response (capped)
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": 1,
    "agentCapabilities": {
      "loadSession": true,
      "promptCapabilities": {
        "image": false,
        "audio": false,
        "embeddedContext": true
      },
      "mcpCapabilities": {
        "http": true,
        "sse": true
      },
      "sessionCapabilities": {
        "list": {},
        "resume": {},
        "close": {}
      },
      "auth": {},
      "_meta": {
        "x.ai/fs_notify": true,
        "x.ai/hooks": {
          "blockingEvents": [
            "pre_tool_use",
            "stop",
            "subagent_stop"
          ],
          "decisions": [
            "deny",
            "block"
          ],
          "stopSignals": [
            "continue",
            "stopReason",
            "additionalContext"
          ]
        },
        "x.ai/capabilities": {
          "toolOverrides": {
            "x_keyword_search": true,
            "x_semantic_search": true,
            "x_user_search": false,
            "x_thread_fetch": false
          }
        }
      }
    },
    "authMethods": [
      {
        "id": "xai.api_key",
        "name": "xai.api_key",
        "description": "XAI_API_KEY or api_key/env_key in config.toml"
      },
      {
        "id": "cached_token",
        "name": "cached_token",
        "description": "Cached token from ~/.grok/auth.json"
      },
      {
        "id": "grok.com",
        "name": "Grok",
        "description": "Sign in with Grok
... [truncated, 33172 chars total]
```

## x.ai/session/list response
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "result": {
      "sessions": [
        {
          "sessionId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca",
          "summary": "",
          "updatedAt": "2026-09-07T04:19:33.443052086+00:00",
          "createdAt": "2026-09-07T04:19:33.158649864+00:00",
          "cwd": "<scratch>/a1-cwd",
          "source": "local",
          "modelId": "gpt-5.6-luna",
          "numMessages": 1,
          "lastActiveAt": "2026-09-07T04:19:33.443052086+00:00",
          "title": "",
          "_meta": {
            "x.ai/session": {
              "kind": "build",
              "facets": {
                "cwd": "<scratch>/a1-cwd",
                "kind": "build"
              }
            }
          }
        },
        {
          "sessionId": "01a07a16-89b9-7d81-9309-9403bd8369c3",
          "summary": "Count slowly from 1 to 20 per line",
          "updatedAt": "2026-09-07T04:18:19.902543050+00:00",
          "createdAt": "2026-09-07T04:18:12.545981492+00:00",
          "cwd": "<scratch>/a1-cwd",
          "source": "local",
          "modelId": "gpt-5.6-luna",
          "numMessages": 4,
          "lastActiveAt": "2026-09-07T04:18:16.450527512+00:00",
          "lastTur
... [truncated, 14119 chars total]
```

## x.ai/sessions/list response
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "result": {
      "sessions": [
        {
          "sessionId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca",
          "title": null,
          "cwd": "<scratch>/a1-cwd",
          "isWorktree": false,
          "modelId": "gpt-5.6-luna",
          "reasoningEffort": "medium",
          "yolo": true,
          "activity": "working",
          "resident": true,
          "lastChangeUnixMs": 1788754773785,
          "origin": {
            "kind": "local"
          }
        },
        {
          "sessionId": "01a07a16-89b9-7d81-9309-9403bd8369c3",
          "title": "Count slowly from 1 to 20 per line",
          "cwd": "<scratch>/a1-cwd",
          "isWorktree": false,
          "modelId": "gpt-5.6-luna",
          "reasoningEffort": "medium",
          "yolo": false,
          "activity": "dormant",
          "lastTurnSummary": "Counted from 1 through 20, one number per line",
          "resident": false,
          "lastChangeUnixMs": 1788754696450,
          "origin": {
            "kind": "local"
          }
        },
        {
          "sessionId": "01a07a16-6b2e-7e22-b522-b039a9190eb0",
          "title": "Request reply with single word TANGO",
          "cwd": "<scratch>
... [truncated, 8571 chars total]
```
