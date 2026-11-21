# google-translate-bridge

A Chrome that makes the token of Google Translate, for `okazuri-search-api` (`[search] google_bridge_url`).

Google gives the Advanced (Gemini) translation only to a request that has the header `x-goog-batchexecute-bgr`, a token of its bot
check that only its own script can make, for one text. The bridge keeps the page of `translate.google.com` open: it tells the page of the
new text by putting it in the address (`history.pushState` and a `popstate`, no load of the page: some 0.1 s; the first text, and every
200th, loads the page: 2 to 8 s), holds back the request that the page makes for it (it is failed in the browser, so Google does not get
it twice) and hands it over as JSON; the server sends it itself. Everything else that the page would ask (images, fonts, its other RPCs)
is failed too.

```
GET /token?from=ja&to=en&text=パエトーン強襲戦
  -> {"model":1,"request":{"url":"https://translate.google.com/_/...","headers":{...},"body":"f.req=..."}}
GET /health
  -> {"ok":true,"warm":true,"uses":5}
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
| `CHROME_ARGS` | none | more arguments for Chrome, separated by spaces |
| `PROFILE` | `./profile` | the profile of Chrome (the model "高度" is remembered in it) |
| `MIN_DELAY_MS` | `1500` | |
| `HEADLESS` | `1` | `0` shows the window |

On the server: `google_bridge_url = "http://100.x.y.z:8130"` and `google_bridge_token = "secret"` under `[search]`.

`--disable-blink-features=AutomationControlled` matters: a Chrome that says it is automated gets a token that Google does not
honour (the answer is then the one without a token). Stop the bridge before starting it again: a Chrome that is still running with the
same profile makes the new one misbehave. The check of the token can change at any time: the ignored test of `gtranslate.rs` says when.

## In a container

`Dockerfile` makes an image with Node 22 and Chrome (Google Chrome on amd64; Google has no Linux build for arm64, so Chromium there), the
Japanese fonts and `TZ=Asia/Tokyo`:

```sh
docker build -t google-translate-bridge tools/google-translate-bridge
docker run -d --name bridge --restart unless-stopped --memory 1700m -p 127.0.0.1:8130:8130 -v bridge-profile:/data google-translate-bridge
```

Chrome runs with a window in a virtual screen (`xvfb`, `HEADLESS=0`), not headless: Google does not always honour the token of a headless Chrome
on Linux (the answer is then the classic one, without the Advanced model). The texts that came back as the classic answer, of 36 asked on a
server of a data center (12 Japanese titles, 3 times): 13 headless, 2 with a window in `xvfb`. `CHROME_ARGS` adds arguments for Chrome
(`--no-sandbox` is in the image: the container is the sandbox). The port is published on `127.0.0.1` only: Docker opens a published port to the
world past the firewall, so add `BRIDGE_TOKEN` (`-e BRIDGE_TOKEN=...`) and the address of the interface that the API reaches it by when
it is not the same machine.

## When Chrome is stuck

A call to Chrome that is not answered in 20 seconds, or a text that takes more than 50 seconds in all, means that Chrome is stuck (the
page does not answer, and everything behind it in the queue waited for it for ever before this check): the request fails with 502, Chrome
and its helper processes are killed, and the next text starts a new Chrome. Chrome is also started again after 3 texts in a row that the
page made no request for, and when Chrome exits or its DevTools socket closes. A Chrome left on the profile by a crash is killed when a new
one starts. `/health` has `restarts`, the number of times Chrome was started again.
