# Cell 4: interject

Session id under test: `b23aec6d-4cee-4aa4-9b20-357151821e1d`

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
{"jsonrpc": "2.0", "method": "_x.ai/session/interjection", "params": {"sessionId": "b23aec6d-4cee-4aa4-9b20-357151821e1d", "text": "INTERJECT-7A3F"}}
```

## TUI screen around the interjection token (found=True)
```text

  <scratch> 12K / 1.0M


     ❯ count slowly from 1 to 30, writing exactly one number per line and nothing else                       7:14 AM



     ❯ INTERJECT-7A3F                                                                                        7:14 AM




                                                                                                                       █
                                                                                                                       █
                                                          
```