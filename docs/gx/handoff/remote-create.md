# Cell 6: remote-create

## session/new response
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "sessionId": "01a07a18-3fb3-7bb2-8eab-23c62547bd90",
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

... [truncated, 36141 chars total]
```

Remote-created session id: `01a07a18-3fb3-7bb2-8eab-23c62547bd90`

## session/prompt response
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "stopReason": "end_turn",
    "_meta": {
      "sessionId": "01a07a18-3fb3-7bb2-8eab-23c62547bd90",
      "requestId": "7dfb3372-019a-4deb-bb7f-4bb72b204a47",
      "promptId": "7dfb3372-019a-4deb-bb7f-4bb72b204a47",
      "totalTokens": 13187,
      "modelId": "gpt-5.6-luna",
      "inputTokens": 13181,
      "outputTokens": 6,
      "cachedReadTokens": 12800,
      "reasoningTokens": 0,
      "usage": {
        "inputTokens": 13181,
        "outputTokens": 6,
        "totalTokens": 13187,
        "cachedReadTokens": 12800,
        "cacheCreationTokens": 0,
        "reasoningTokens": 0,
        "modelCalls": 1,
        "apiDurationMs": 1974,
        "modelUsage": {
          "gpt-5.6-luna": {
            "inputTokens": 13181,
            "outputTokens": 6,
            "totalTokens": 13187,
            "cachedReadTokens": 12800,
            "cacheCreationTokens": 0,
            "reasoningTokens": 0,
            "modelCalls": 1,
            "apiDurationMs": 1974
          }
        },
        "numTurns": 1
      }
    }
  }
}
```

## TUI screen tail after --resume
```

  <scratch>/a1-… 1.6K / 272K


     ❯ reply with the single word TANGO                                                                     11:20 PM


     TANGO                                                                                                  11:20 PM

     Worked for 2.0s






















   Run /doctor for details and fixes.

  ╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
  │ ❯                                                                                                                │
  ╰─────────────────────────────────────────────────────────────── GPT-5.6 Luna (ChatGPT) (medium) · always-approve ─╯

  Shift+Tab:mode  │  Ctrl+.:shortcuts

```