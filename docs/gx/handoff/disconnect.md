# Cell 7: disconnect survival

Session id: `01a07bca-f886-7d93-8a7e-5271f28508b2`

Notifications observed on client A before disconnect: 8

## session/load response (client B)
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "models": {
      "currentModelId": "grok-4.6",
      "availableModels": [
        {
          "modelId": "grok-4.6",
          "name": "Grok 4.6",
          "description": "SpaceXAI's latest frontier model",
          "_meta": {
            "totalContextTokens": 500000,
            "agentType": "grok-build-plan",
            "supportsReasoningEffort": true,
            "reasoningEffort": "medium",
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
   
... [truncated, 36387 chars total]
```

## session/update notifications on client B: 80
```json
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-308", "agentTimestampMs": 1788783305174, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 53}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "9"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-309", "agentTimestampMs": 1788783305174, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 54}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-310", "agentTimestampMs": 1788783305223, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 55}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "10"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-311", "agentTimestampMs": 1788783305223, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 56}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-312", "agentTimestampMs": 1788783305223, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 57}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "11"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-313", "agentTimestampMs": 1788783305223, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 58}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-314", "agentTimestampMs": 1788783305280, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 59}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "12"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-315", "agentTimestampMs": 1788783305280, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 60}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-316", "agentTimestampMs": 1788783305280, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 61}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "13"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-317", "agentTimestampMs": 1788783305280, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 62}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-318", "agentTimestampMs": 1788783305312, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 63}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "14"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-319", "agentTimestampMs": 1788783305312, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 64}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-320", "agentTimestampMs": 1788783305312, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 65}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "15"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-321", "agentTimestampMs": 1788783305312, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 66}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-322", "agentTimestampMs": 1788783305357, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 67}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "16"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-323", "agentTimestampMs": 1788783305357, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 68}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-324", "agentTimestampMs": 1788783305357, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 69}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "17"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-325", "agentTimestampMs": 1788783305357, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 70}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-326", "agentTimestampMs": 1788783305403, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 71}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "18"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-327", "agentTimestampMs": 1788783305403, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 72}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-328", "agentTimestampMs": 1788783305403, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 73}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "19"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-329", "agentTimestampMs": 1788783305403, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 74}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-330", "agentTimestampMs": 1788783305458, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 75}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "20"}}, "_meta": {"totalTokens": 1635, "eventId": "01a07bca-f886-7d93-8a7e-5271f28508b2-331", "agentTimestampMs": 1788783305458, "promptId": "3cc0a8f1-4415-437d-bd62-b2bc15e49cde", "streamStartMs": 1788783295163, "turnStartMs": 1788783294686, "updateType": "AgentMessageChunk", "chunkId": 76}}}
{"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "01a07bca-f886-7d93-8a7e-5271f28508b2", "update": {"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "compact", "description": "Compress conversation history to save context window", "input": {"hint": "optional context about what to preserve"}}, {"name": "always-approve", "description": "Toggle always-approve mode (skip all permission prompts)", "input": {"hint": "on|off"}}, {"name": "context", "description": "Show context window usage and session stats", "input": null}, {"name": "plugins", "descr... [truncated, 22372 chars total]
```

## post-reconnect notifications in the 15s listen window: 87 total, 0 replay-stamped (`_meta.isReplay`), 87 live (78 of them session/update)

## 1..20 census on the reconnected client: all of 1..20 present in the streamed answer
