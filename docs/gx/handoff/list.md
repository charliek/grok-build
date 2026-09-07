# Cell 1: list

Session id under test: `b23aec6d-4cee-4aa4-9b20-357151821e1d`

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
... [truncated, 33174 chars total]
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
          "sessionId": "b23aec6d-4cee-4aa4-9b20-357151821e1d",
          "summary": "",
          "updatedAt": "2026-09-07T12:13:57.621228109+00:00",
          "createdAt": "2026-09-07T12:13:55.599768516+00:00",
          "cwd": "<scratch>/cwd",
          "source": "local",
          "modelId": "glm-5.3-flash",
          "numMessages": 3,
          "lastActiveAt": "2026-09-07T12:13:57.621228109+00:00",
          "title": "",
          "_meta": {
            "x.ai/session": {
              "kind": "build",
              "facets": {
                "cwd": "<scratch>/cwd",
                "kind": "build"
              }
            }
          }
        }
      ],
      "_meta": {
        "x.ai/facets": {
          "scope": "window",
          "keys": [
            {
              "key": "cwd",
              "values": [
                {
                  "value": "<scratch>/cwd",
                  "count": 1
                }
              ]
            },
            {
              "key": "kind",
              "values": [
                {
                  "value": "build",
                  "count
... [truncated, 1664 chars total]
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
          "sessionId": "b23aec6d-4cee-4aa4-9b20-357151821e1d",
          "title": null,
          "cwd": "<scratch>/cwd",
          "isWorktree": false,
          "modelId": "glm-5.3-flash",
          "reasoningEffort": "medium",
          "yolo": true,
          "activity": "idle",
          "resident": true,
          "lastChangeUnixMs": 1788783237621,
          "origin": {
            "kind": "local"
          }
        }
      ]
    }
  }
}
```
