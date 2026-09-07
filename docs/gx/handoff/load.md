# Cell 2: load/replay

Session id under test: `c63f9ebb-c4d1-4c48-9bc3-150d066f19ca`

## session/load response
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "models": {
      "currentModelId": "gpt-5.6-luna",
      "availableModels": [
        {
          "modelId": "grok-4.6",
          "name": "Grok 4.6",
          "description": "SpaceXAI's latest frontier model",
          "_meta": {
            "totalContextTokens": 500000,
            "agentType": "grok-build-plan",
            "supportsReasoningEffort": true,
            "reasoningEffort": "high",
            "reasoningEfforts": [
              {
                "id": "xhigh",
                "value": "xhigh",
                "label": "Extra High Effort",
                "description": "Highest effort and reasoning level",
                "default": false
              },
              {
                "id": "high",
                "value": "high",
                "label": "High Effort",
                "description": "Higher implementation quality with extensive reasoning",
                "default": true
              },
              {
                "id": "medium",
                "value": "medium",
                "label": "Medium Effort",
                "description": "Balanced effort with standard implementation and testing",
                "default": false
              },
              {
                "id": "low",
                "value": "low",
                "label": "Low Effort",
                "description": "Quick, fast implementations",
                "default": false
              }
            ]

... [truncated, 35956 chars total]
```

## session/update notifications received (replay): 6
```json
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca", "update": {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "reply with the single word PONG"}, "_meta": {"modelId": "gpt-5.6-luna", "promptIndex": 0}}, "_meta": {"eventId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca-2", "agentTimestampMs": 1788754773429, "isReplay": true, "x.ai/leaderClientId": 4}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca", "update": {"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "compact", "description": "Compress conversation history to save context window", "input": {"hint": "optional context about what to preserve"}}, {"name": "always-approve", "description": "Toggle always-approve mode (skip all permission prompts)", "input": {"hint": "on|off"}}, {"name": "context", "description": "Show context window usage and session stats", "input": null}, {"name": "plugins", "descr... [truncated, 20993 chars total]
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca", "update": {"sessionUpdate": "session_info_update", "title": "Reply with single word PONG"}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "P"}}, "_meta": {"totalTokens": 1622, "eventId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca-5", "agentTimestampMs": 1788754774664, "promptId": "b9c97a7a-a117-4414-b364-629f09ca57fd", "streamStartMs": 1788754774353, "turnStartMs": 1788754773445, "updateType": "AgentMessageChunk", "chunkId": 1}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "ONG"}}, "_meta": {"totalTokens": 1622, "eventId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca-6", "agentTimestampMs": 1788754774682, "promptId": "b9c97a7a-a117-4414-b364-629f09ca57fd", "streamStartMs": 1788754774353, "turnStartMs": 1788754773445, "updateType": "AgentMessageChunk", "chunkId": 2}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca", "update": {"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "compact", "description": "Compress conversation history to save context window", "input": {"hint": "optional context about what to preserve"}}, {"name": "always-approve", "description": "Toggle always-approve mode (skip all permission prompts)", "input": {"hint": "on|off"}}, {"name": "context", "description": "Show context window usage and session stats", "input": null}, {"name": "plugins", "descr... [truncated, 21026 chars total]
```
