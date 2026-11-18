#!/usr/bin/env node
// Google Translate through a real (headless) Chrome, for okazuri-search-api.
//
// Google answers the Advanced (Gemini) translation only to a request that has a token of its bot check (the header
// `x-goog-batchexecute-bgr`), which only Google's own script can make, for one text at a time. So this program opens the page
// of Google Translate with the text in its address (the first load of the page is the request that has the token), reads the
// answer of the page, and gives the translation back:
//
//   GET /translate?from=ja&to=en&text=...   ->  {"translation":"The Phaethon Assault","model":1}
//   GET /token?from=ja&to=en&text=...      ->  {"model":1,"request":{"url":...,"headers":{...},"body":"f.req=..."}}
//   GET /health                            ->  {"ok":true}
//
// /token is for okazuri-search-api: Chrome only makes the request of the page (with its token), which is held back and given to the
// caller, who sends it itself (the token is good for this text, with no cookie of the browser).
//
// No package is needed: Node 22+ (WebSocket, fetch) and Google Chrome. Everything is set with environment variables:
//   HOST          address to listen on (default 127.0.0.1; put the Tailscale address of this Mac to reach it from the server)
//   PORT          default 8130
//   BRIDGE_TOKEN  if set, a request needs `Authorization: Bearer <token>`
//   CHROME        path of Chrome (default: the one of macOS)
//   PROFILE       the profile of Chrome, where "Advanced" is remembered (default: ./profile)
//   MIN_DELAY_MS  the least time between two translations (default 1500)
//   HEADLESS      0 shows the window of Chrome (default 1)

import { spawn } from 'node:child_process';
import http from 'node:http';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HOST = process.env.HOST ?? '127.0.0.1';
const PORT = Number(process.env.PORT ?? 8130);
const TOKEN = process.env.BRIDGE_TOKEN ?? '';
const CHROME = process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const PROFILE = resolve(process.env.PROFILE ?? fileURLToPath(new URL('./profile', import.meta.url)));
const MIN_DELAY_MS = Number(process.env.MIN_DELAY_MS ?? 1500);
const HEADLESS = (process.env.HEADLESS ?? '1') !== '0';
const ANSWER_TIMEOUT_MS = 30_000;
const MAX_TEXT_CHARS = 300;
const ADVANCED = 1;

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

/** The translation in the answer of the RPC `MkEWBc` (the same reading as `gtranslate.rs`). */
export function parse(body) {
  for (const line of body.split('\n')) {
    let entries;
    try {
      entries = JSON.parse(line);
    } catch {
      continue;
    }
    if (!Array.isArray(entries)) continue;

    for (const entry of entries) {
      if (entry?.[0] !== 'wrb.fr' || entry?.[1] !== 'MkEWBc') continue;
      let inner;
      try {
        inner = JSON.parse(entry[2]);
      } catch {
        continue;
      }

      const sentences = inner?.[1]?.[0]?.[0]?.[5];
      if (!Array.isArray(sentences)) return null;
      const translation = sentences.map((s) => s?.[0]).filter((s) => typeof s === 'string').join(' ').trim();
      const echo = inner?.[0]?.[6]; // [text, from, to, true, null, model]

      return translation ? { translation, source: echo?.[0], model: echo?.[5], tail: JSON.stringify(inner[inner.length - 1]) } : null;
    }
  }
  return null;
}

/** A minimal client of the DevTools protocol of Chrome. */
class Cdp {
  constructor(socket) {
    this.socket = socket;
    this.last = 0;
    this.pending = new Map();
    this.listeners = new Set();
    socket.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
      if (message.id) {
        const waiting = this.pending.get(message.id);
        this.pending.delete(message.id);
        if (waiting) message.error ? waiting.reject(new Error(message.error.message)) : waiting.resolve(message.result);
      } else {
        for (const listener of this.listeners) listener(message);
      }
    });
  }

  send(method, params = {}, sessionId) {
    const id = ++this.last;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.send(JSON.stringify({ id, method, params, sessionId }));
    });
  }

  on(listener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }
}

function startChrome() {
  return new Promise((resolveStart, reject) => {
    const proc = spawn(
      CHROME,
      [...(HEADLESS ? ['--headless=new'] : ['--window-position=-3000,0']), '--remote-debugging-port=0', `--user-data-dir=${PROFILE}`, '--no-first-run', '--disable-blink-features=AutomationControlled',  '--no-default-browser-check', '--disable-gpu', '--window-size=1200,900', '--lang=ja', 'about:blank'],
      { stdio: ['ignore', 'ignore', 'pipe'] },
    );
    let seen = '';
    const timer = setTimeout(() => reject(new Error('Chrome did not start')), 20_000);

    proc.stderr.on('data', (chunk) => {
      seen += chunk;
      const found = seen.match(/DevTools listening on (ws:\/\/\S+)/);
      if (found) {
        clearTimeout(timer);
        resolveStart({ proc, url: found[1] });
      }
    });
    proc.on('exit', (code) => reject(new Error(`Chrome exited with ${code}`)));
  });
}

async function openPage() {
  const { proc, url } = await startChrome();
  const socket = new WebSocket(url);
  await new Promise((done, fail) => {
    socket.addEventListener('open', done);
    socket.addEventListener('error', fail);
  });

  const cdp = new Cdp(socket);
  const { userAgent } = await cdp.send('Browser.getVersion');
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const call = (method, params) => cdp.send(method, params, sessionId);

  await call('Network.enable');
  await call('Page.enable');
  // the page of a browser that says it is headless is not the page of a person
  await call('Network.setUserAgentOverride', { userAgent: userAgent.replace('HeadlessChrome', 'Chrome') });

  return { proc, cdp, sessionId, call };
}

const pagePromise = openPage();
pagePromise.catch((error) => {
  console.error(`cannot start Chrome: ${error.message}`);
  process.exit(1);
});

/** Opens the page of Google Translate with the text, and waits for the answer of the page. */
async function translateOnce({ cdp, sessionId, call }, text, from, to) {
  const answer = new Promise((resolveAnswer, reject) => {
    const requests = new Set();
    const sent = new Map();
    const timer = setTimeout(() => {
      off();
      reject(new Error('Google Translate did not answer'));
    }, ANSWER_TIMEOUT_MS);
    const off = cdp.on(async (message) => {
      if (message.sessionId !== sessionId) return;
      const { method, params } = message;

      if (method === 'Network.requestWillBeSent' && params.request.url.includes('rpcids=MkEWBc')) {
        sent.set(params.requestId, { url: params.request.url, headers: params.request.headers, body: params.request.postData ?? '' });
      }
      if (method === 'Network.responseReceived' && params.response.url.includes('rpcids=MkEWBc')) {
        requests.add(params.requestId);
      } else if (method === 'Network.loadingFinished' && requests.has(params.requestId)) {
        try {
          const { body, base64Encoded } = await call('Network.getResponseBody', { requestId: params.requestId });
          const parsed = parse(base64Encoded ? Buffer.from(body, 'base64').toString() : body);
          if (process.env.DEBUG) console.error('answer', params.requestId, JSON.stringify(parsed));

          if (parsed && parsed.source === text) {
            clearTimeout(timer);
            off();
            resolveAnswer({ ...parsed, request: sent.get(params.requestId) });
          }
        } catch {
          // a request that was cancelled by the page has no body
        }
      }
    });
  });
  answer.catch(() => {});

  const address = `https://translate.google.com/?sl=${from}&tl=${to}&text=${encodeURIComponent(text)}&op=translate&hl=ja`;
  await call('Page.navigate', { url: address });

  return answer;
}

/**
 * Opens the page with the text and takes the request that the page makes for the translation (with its token), without
 * letting it go to Google: the caller sends it. Returns `{ url, headers, body, model }`.
 */
async function tokenOnce({ cdp, sessionId, call }, text, from, to) {
  const taken = new Promise((resolveToken, reject) => {
    const timer = setTimeout(() => {
      off();
      reject(new Error('the page did not make the request'));
    }, ANSWER_TIMEOUT_MS);
    const off = cdp.on(async (message) => {
      if (message.sessionId !== sessionId || message.method !== 'Fetch.requestPaused') return;
      const { request, requestId } = message.params;

      call('Fetch.failRequest', { requestId, errorReason: 'BlockedByClient' }).catch(() => {});
      try {
        const body = request.postData ?? '';
        const outer = JSON.parse(decodeURIComponent(body.replace(/^f\.req=/, '').replace(/&$/, '')));
        const inner = JSON.parse(outer[0][0][1]);

        if (process.env.DEBUG) console.error('paused request:', JSON.stringify(inner[0]), 'bgr:', 'x-goog-batchexecute-bgr' in request.headers);
        if (inner[0][0] === text) {
          clearTimeout(timer);
          off();
          resolveToken({ url: request.url, headers: request.headers, body, model: inner[0][5] });
        }
      } catch {
        // a request of the page that is not the translation of this text
      }
    });
  });
  taken.catch(() => {});

  await call('Fetch.enable', { patterns: [{ urlPattern: '*rpcids=MkEWBc*', requestStage: 'Request' }] });
  try {
    await call('Page.navigate', { url: `https://translate.google.com/?sl=${from}&tl=${to}&text=${encodeURIComponent(text)}&op=translate&hl=ja` });

    return await taken;
  } finally {
    await call('Fetch.disable').catch(() => {});
  }
}

/** Chooses 高度 (Advanced) in the menu of the models; the profile of Chrome remembers it. */
async function chooseAdvanced({ call }) {
  const { result } = await call('Runtime.evaluate', {
    awaitPromise: true,
    returnByValue: true,
    expression: `(async () => {
      const wait = (ms) => new Promise((done) => setTimeout(done, ms));
      const shown = (e) => e.getClientRects().length > 0;
      const find = (pattern, longest) => [...document.querySelectorAll('button, [role=button], [role=menuitem], [role=option], li, div, span')]
        .filter((e) => shown(e) && pattern.test((e.innerText || '').trim()) && (e.innerText || '').trim().length < longest).pop();
      const classic = find(/^(クラシック|Classic)$/, 20);
      if (!classic) return 'the menu of the models was not found';
      classic.click();
      await wait(700);
      const advanced = find(/^(高度|Advanced)/, 80);
      if (!advanced) return 'the Advanced entry was not found';
      advanced.click();
      await wait(900);
      return 'ok';
    })()`,
  });

  return result.value;
}

let page = null;
let chosen = false;

async function token(text, from, to) {
  page ??= await pagePromise;
  let got = await tokenOnce(page, text, from, to);

  if (got.model !== ADVANCED && !chosen) {
    chosen = true;
    // the menu of the models is on a page that has a translation: let this one through, and choose
    await translateOnce(page, text, from, to).catch(() => {});
    console.error(`the model was ${got.model}: choosing Advanced: ${await chooseAdvanced(page)}`);
    got = await tokenOnce(page, text, from, to);
  }
  if (got.model !== ADVANCED) throw new Error(`the page used the model ${got.model}, not Advanced (${ADVANCED})`);

  return { model: got.model, request: { url: got.url, headers: got.headers, body: got.body } };
}

async function translate(text, from, to) {
  page ??= await pagePromise;
  let got = await translateOnce(page, text, from, to);

  if (got.model !== ADVANCED && !chosen) {
    chosen = true;
    const done = await chooseAdvanced(page);
    console.error(`the model was ${got.model}: choosing Advanced: ${done}`);
    got = await translateOnce(page, text, from, to);
  }
  if (got.model !== ADVANCED) throw new Error(`the page used the model ${got.model}, not Advanced (${ADVANCED})`);

  return { translation: got.translation, model: got.model, request: got.request };
}

// one translation at a time, with a pause between two
let queue = Promise.resolve();
function enqueue(job) {
  const run = queue.then(job);
  queue = run.catch(() => {}).then(() => sleep(MIN_DELAY_MS));
  return run;
}

function reply(res, status, body) {
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(body));
}

const languages = new Set(['ja', 'en']);

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');

  if (url.pathname === '/health') return reply(res, 200, { ok: true });
  if (url.pathname !== '/translate' && url.pathname !== '/token') return reply(res, 404, { error: 'not found' });
  if (TOKEN && req.headers.authorization !== `Bearer ${TOKEN}`) return reply(res, 401, { error: 'unauthorized' });

  const text = (url.searchParams.get('text') ?? '').trim();
  const from = url.searchParams.get('from') ?? '';
  const to = url.searchParams.get('to') ?? '';

  if (!text || [...text].length > MAX_TEXT_CHARS) return reply(res, 422, { error: `text has to be 1 to ${MAX_TEXT_CHARS} characters` });
  if (!languages.has(from) || !languages.has(to) || from === to) return reply(res, 422, { error: 'from and to are ja and en' });

  try {
    if (url.pathname === '/token') return reply(res, 200, await enqueue(() => token(text, from, to)));

    const { request, ...answer } = await enqueue(() => translate(text, from, to));
    reply(res, 200, answer);
  } catch (error) {
    console.error(`translate failed: ${error.message}`);
    reply(res, 502, { error: error.message });
  }
});

server.listen(PORT, HOST, () => console.error(`google-translate-bridge on http://${HOST}:${PORT}`));

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, async () => {
    try {
      (await pagePromise).proc.kill();
    } finally {
      process.exit(0);
    }
  });
}
