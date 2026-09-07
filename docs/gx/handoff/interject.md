# Cell 4: interject

Session id under test: `c63f9ebb-c4d1-4c48-9bc3-150d066f19ca`

## x.ai/interject response
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "result": {
      "status": "queued"
    }
  }
}
```

## x.ai/session/interjection notification received by the client
```json
{"jsonrpc": "2.0", "method": "_x.ai/session/interjection", "params": {"sessionId": "c63f9ebb-c4d1-4c48-9bc3-150d066f19ca", "text": "INTERJECT-7A3F"}}
```

## TUI screen around the interjection token (found=True)
```

  <scratch>/a1-c… 13K / 272K


     ❯ count slowly from 1 to 30, writing exactly one number per line and nothing else                      11:19 PM



     ❯ INTERJECT-7A3F                                                                                       11:19 PM




                                                                                                                       █
                                                                                                                       █

```