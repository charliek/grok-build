# Cell 6: remote-create

## session/new response
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "sessionId": "01a07bca-bb49-7b23-95e1-4638cc62ebad",
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
         
... [truncated, 36576 chars total]
```

Remote-created session id: `01a07bca-bb49-7b23-95e1-4638cc62ebad`

## session/prompt response
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "stopReason": "end_turn",
    "_meta": {
      "sessionId": "01a07bca-bb49-7b23-95e1-4638cc62ebad",
      "requestId": "54a344c8-f261-49db-8f35-d055169f4c8d",
      "promptId": "54a344c8-f261-49db-8f35-d055169f4c8d",
      "totalTokens": 17330,
      "modelId": "grok-4.6",
      "inputTokens": 17302,
      "outputTokens": 28,
      "cachedReadTokens": 0,
      "reasoningTokens": 22,
      "usage": {
        "inputTokens": 17302,
        "outputTokens": 28,
        "totalTokens": 17330,
        "cachedReadTokens": 0,
        "cacheCreationTokens": 0,
        "reasoningTokens": 22,
        "modelCalls": 1,
        "apiDurationMs": 3176,
        "costUsdTicks": 59112400,
        "modelUsage": {
          "grok-4.6-build": {
            "inputTokens": 17302,
            "outputTokens": 28,
            "totalTokens": 17330,
            "cachedReadTokens": 0,
            "cacheCreationTokens": 0,
            "reasoningTokens": 22,
            "modelCalls": 1,
            "apiDurationMs": 3176,
            "costUsdTicks": 59112400
          }
        },
        "numTurns": 1
      }
    }
  }
}
```

## TUI screen tail after --resume
```text

  <scratch> 1.6K / 500K


     ❯ reply with the single word TANGO                                                                      7:14 AM


     ◆ Thought for 1.7s

     TANGO                                                                                                   7:14 AM

     Worked for 3.2s

     Switched to GLM 5.3 Flash (Z.AI) (high effort)


















   Run /doctor for details and fixes.

  ╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
  │ ❯                                                                                                                │
  ╰─────────────────────────────────────────────────────────────────── GLM 5.3 Flash (Z.AI) (high) · always-approve ─╯

  Shift+Tab:mode  │  Ctrl+.:shortcuts

```