# google-translate-bridge

A Chrome that makes the token of Google Translate, for `okazuri-search-api` (`[search] google_bridge_url`).

Google gives the Advanced (Gemini) translation only to a request that has the header `x-goog-batchexecute-bgr`, a token of its bot
check that only its own script can make, for one text. The bridge opens `translate.google.com` with the text in the address, holds back
the request that the page makes for it (it is failed in the browser, so Google does not get it twice) and hands it over as JSON; the
server sends it itself.

```
GET /token?from=ja&to=en&text=パエトーン強襲戦
  -> {"model":1,"request":{"url":"https://translate.google.com/_/...","headers":{...},"body":"f.req=..."}}
GET /translate?from=ja&to=en&text=...      (the browser sends the request and gives the translation: for a look)
  -> {"translation":"The Phaethon Assault","model":1}
GET /health
```

Only `ja` and `en`, 300 characters at most, one text at a time (at least `MIN_DELAY_MS` between two).

## Run

Node 22 or later and Google Chrome; nothing to install.

```sh
BRIDGE_TOKEN=secret HOST=100.x.y.z node tools/google-translate-bridge/server.mjs
```

| variable | default | |
|---|---|---|
| `HOST` | `127.0.0.1` | the address to listen on: the Tailscale address of the Mac, to reach it from the server |
| `PORT` | `8130` | |
| `BRIDGE_TOKEN` | none | a request needs `Authorization: Bearer <it>` |
| `CHROME` | the one of macOS | the path of Chrome |
| `PROFILE` | `./profile` | the profile of Chrome (the model "高度" is remembered in it) |
| `MIN_DELAY_MS` | `1500` | |
| `HEADLESS` | `1` | `0` shows the window |
| `DEBUG` | none | logs the held-back requests |

On the server: `google_bridge_url = "http://100.x.y.z:8130"` and `google_bridge_token = "secret"` under `[search]`.

`--disable-blink-features=AutomationControlled` matters: a Chrome that says it is automated gets a token that Google does not
honour (the answer is then the one without a token). Stop the bridge before starting it again: a Chrome that is still running with the
same profile makes the new one misbehave. The check of the token can change at any time: the ignored test of `gtranslate.rs` says when.
