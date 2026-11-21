#!/usr/bin/env node
// Google Translate through a real (headless) Chrome, for okazuri-search-api.
//
// Google answers the Advanced (Gemini) translation only to a request that has a token of its bot check (the header
// `x-goog-batchexecute-bgr`), which only Google's own script can make, for one text at a time. So this program opens the page
// of Google Translate with the text in its address (the first load of the page is the request that has the token), reads the
// answer of the page, and gives the translation back:
//
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

import { spawn, spawnSync } from 'node:child_process';
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
// A call to Chrome that is not answered, and a text that takes longer than this in all, mean that Chrome is stuck: it is started again.
const CALL_TIMEOUT_MS = 20_000;
const JOB_TIMEOUT_MS = 50_000;
// Failures of the page itself (Google did not answer) in a row, after which Chrome is started again too.
const MAX_FAILURES = 3;
const MAX_TEXT_CHARS = 300;
const ADVANCED = 1;

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

/** Chrome does not answer (or is gone): a reason to start it again, not a failure of one text. */
class StuckError extends Error {}

/** A minimal client of the DevTools protocol of Chrome. */
class Cdp {
  constructor(socket) {
    this.socket = socket;
    this.last = 0;
    this.pending = new Map();
    this.listeners = new Set();
    socket.addEventListener('close', () => {
      for (const waiting of this.pending.values()) waiting.reject(new Error('the DevTools socket closed'));
      this.pending.clear();
    });
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

  send(method, params = {}, sessionId, timeoutMs = CALL_TIMEOUT_MS) {
    const id = ++this.last;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new StuckError(`${method} was not answered in ${timeoutMs / 1000} seconds`));
      }, timeoutMs);
      this.pending.set(id, { resolve: (value) => (clearTimeout(timer), resolve(value)), reject: (error) => (clearTimeout(timer), reject(error)) });
      try {
        this.socket.send(JSON.stringify({ id, method, params, sessionId }));
      } catch (error) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(new StuckError(error.message));
      }
    });
  }

  on(listener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }
}

function startChrome() {
  // a Chrome that a bridge before this one (or a crash) left on the profile would refuse this one: it is killed first
  spawnSync('pkill', ['-9', '-f', '--', `--user-data-dir=${PROFILE}`]);
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

let pagePromise = null;
let restarts = 0;
let failures = 0;

/** Chrome with its page; started when it is wanted, and again after `restartBrowser`. */
function getPage() {
  if (pagePromise) return pagePromise;

  const promise = openPage();
  pagePromise = promise;
  promise.then(
    (page) => {
      const gone = (why) => {
        if (pagePromise !== promise) return;
        console.error(`${why}: Chrome is started again at the next text`);
        pagePromise = null;
        resetState();
      };
      page.proc.on('exit', (code) => gone(`Chrome exited with ${code}`));
      page.cdp.socket.addEventListener('close', () => gone('the DevTools socket closed'));
    },
    (error) => {
      console.error(`cannot start Chrome: ${error.message}`);
      if (pagePromise === promise) pagePromise = null;
    },
  );

  return promise;
}

/** Kills Chrome and all the processes under it (its helpers), the children first. */
function killChrome(proc) {
  let pids = [proc.pid];
  try {
    const table = spawnSync('ps', ['-axo', 'pid=,ppid='], { encoding: 'utf8' }).stdout.trim().split('\n').map((line) => line.trim().split(/\s+/).map(Number));
    for (let i = 0; i < pids.length; i++) for (const [pid, ppid] of table) if (ppid === pids[i] && !pids.includes(pid)) pids.push(pid);
  } catch {
    // the tree is not known: only Chrome itself is killed
  }
  for (const pid of pids.reverse()) {
    try {
      process.kill(pid, 'SIGKILL');
    } catch {
      // gone already
    }
  }
}

/** Kills Chrome (and its page) and forgets the state of the page: the next text starts a new one. */
function restartBrowser(reason) {
  const old = pagePromise;
  pagePromise = null;
  restarts++;
  failures = 0;
  console.error(`restarting Chrome (${restarts}): ${reason}`);
  resetState();
  old?.then((page) => killChrome(page.proc)).catch(() => {});
}

// What the page may do: its request for the translation is taken (and failed, so that Google does not get it twice), what it does
// not need to make one (images, fonts, the other RPCs) is failed, and while `letThrough` is set everything goes on (to choose 高度).
const PATTERNS = [{ urlPattern: '*rpcids=*' }, { urlPattern: '*', resourceType: 'Image' }, { urlPattern: '*', resourceType: 'Font' }, { urlPattern: '*', resourceType: 'Media' }];
const RELOAD_EVERY = 200;
const FAST_TIMEOUT_MS = 5_000;

let waiting = null; // { text, resolve }
let letThrough = false;
let warm = false; // the page is open, and the next text can be given to it without loading it again
let uses = 0;
let chosen = false;
let handlerSet = false;

function resetState() {
  waiting = null;
  letThrough = false;
  warm = false;
  uses = 0;
  chosen = false;
  handlerSet = false;
}

async function setUp({ cdp, sessionId, call }) {
  if (handlerSet) return;
  handlerSet = true;

  cdp.on((message) => {
    if (message.sessionId !== sessionId || message.method !== 'Fetch.requestPaused') return;
    const { request, requestId } = message.params;
    const act = (method, params = {}) => call(method, { requestId, ...params }).catch(() => {});

    if (letThrough) return void act('Fetch.continueRequest');
    if (waiting && request.url.includes('rpcids=MkEWBc')) {
      try {
        const outer = JSON.parse(decodeURIComponent((request.postData ?? '').replace(/^f\.req=/, '').replace(/&$/, '')));
        const inner = JSON.parse(outer[0][0][1]);

        if (inner[0][0] === waiting.text) {
          waiting.resolve({ url: request.url, headers: request.headers, body: request.postData, model: inner[0][5] });
          waiting = null;
        }
      } catch {
        // not a request for a translation
      }
    }
    act('Fetch.failRequest', { errorReason: 'BlockedByClient' });
  });
  await call('Fetch.enable', { patterns: PATTERNS.map((p) => ({ ...p, requestStage: 'Request' })) });
}

function expectRequest(text, timeoutMs) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      waiting = null;
      reject(new Error('the page did not make the request'));
    }, timeoutMs);
    waiting = { text, resolve: (value) => (clearTimeout(timer), resolve(value)) };
  });
}

const addressOf = (text, from, to) => `/?sl=${from}&tl=${to}&text=${encodeURIComponent(text)}&op=translate&hl=ja`;

/** The request of the page for the text. The page that is open is told of the new text in its address (no load: some 0.1 s); a page
 * that is not open (or that failed) is loaded (some 1.5 s). */
async function request(page, text, from, to) {
  if (warm && uses < RELOAD_EVERY) {
    const taken = expectRequest(text, FAST_TIMEOUT_MS);
    taken.catch(() => {});
    await page.call('Runtime.evaluate', {
      expression: `history.pushState({}, '', ${JSON.stringify(addressOf(text, from, to))}); window.dispatchEvent(new PopStateEvent('popstate', { state: {} }));`,
    });
    try {
      const got = await taken;
      uses++;
      return got;
    } catch {
      warm = false;
    }
  }

  const taken = expectRequest(text, ANSWER_TIMEOUT_MS);
  taken.catch(() => {});
  await page.call('Page.navigate', { url: `https://translate.google.com${addressOf(text, from, to)}` });
  const got = await taken;
  warm = true;
  uses = 0;

  return got;
}

/** Chooses 高度 (Advanced) in the menu of the models (a page that has a translation shows it); the profile of Chrome remembers it. */
async function chooseAdvanced(page, text, from, to) {
  letThrough = true;
  try {
    await page.call('Page.navigate', { url: `https://translate.google.com${addressOf(text, from, to)}` });
    await sleep(5000);
    const { result } = await page.call('Runtime.evaluate', {
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
  } finally {
    letThrough = false;
    warm = false;
  }
}

async function token(text, from, to) {
  const page = await getPage();
  await setUp(page);
  let got = await request(page, text, from, to);

  if (got.model !== ADVANCED && !chosen) {
    chosen = true;
    console.error(`the model was ${got.model}: choosing Advanced: ${await chooseAdvanced(page, text, from, to)}`);
    got = await request(page, text, from, to);
  }
  if (got.model !== ADVANCED) {
    warm = false;
    throw new Error(`the page used the model ${got.model}, not Advanced (${ADVANCED})`);
  }

  return { model: got.model, request: { url: got.url, headers: got.headers, body: got.body } };
}

/** One text, with a limit of time. Chrome that does not answer (or a text that takes too long) is started again, and so is Chrome
 * after `MAX_FAILURES` texts in a row that the page did not make a request for. */
async function guarded(job) {
  let timer;
  try {
    const result = await Promise.race([
      job(),
      new Promise((_, reject) => (timer = setTimeout(() => reject(new StuckError(`the text took more than ${JOB_TIMEOUT_MS / 1000} seconds`)), JOB_TIMEOUT_MS))),
    ]);
    failures = 0;

    return result;
  } catch (error) {
    if (error instanceof StuckError) restartBrowser(error.message);
    else if (++failures >= MAX_FAILURES) restartBrowser(`${failures} texts in a row failed (${error.message})`);

    throw error;
  } finally {
    clearTimeout(timer);
  }
}

// one text at a time, with a pause between two
let queue = Promise.resolve();
function enqueue(job) {
  const run = queue.then(() => guarded(job));
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

  if (url.pathname === '/health') return reply(res, 200, { ok: true, warm, uses, restarts });
  if (url.pathname !== '/token') return reply(res, 404, { error: 'not found' });
  if (TOKEN && req.headers.authorization !== `Bearer ${TOKEN}`) return reply(res, 401, { error: 'unauthorized' });

  const text = (url.searchParams.get('text') ?? '').trim();
  const from = url.searchParams.get('from') ?? '';
  const to = url.searchParams.get('to') ?? '';

  if (!text || [...text].length > MAX_TEXT_CHARS) return reply(res, 422, { error: `text has to be 1 to ${MAX_TEXT_CHARS} characters` });
  if (!languages.has(from) || !languages.has(to) || from === to) return reply(res, 422, { error: 'from and to are ja and en' });

  try {
    reply(res, 200, await enqueue(() => token(text, from, to)));
  } catch (error) {
    console.error(`token failed: ${error.message}`);
    reply(res, 502, { error: error.message });
  }
});

server.listen(PORT, HOST, () => console.error(`google-translate-bridge on http://${HOST}:${PORT}`));
getPage(); // Chrome is started now, so that the first text does not wait for it

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, async () => {
    try {
      const page = await pagePromise;
      if (page) killChrome(page.proc);
    } finally {
      process.exit(0);
    }
  });
}
