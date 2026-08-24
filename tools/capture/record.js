// Records what a browser does while somebody walks Instagram by hand.
//
//     node tools/capture/record.js
//
// Launches the installed Chrome with a throwaway profile, opens instagram.com,
// and writes one JSON line per event to out/events.jsonl until the last tab
// is closed. Log in inside that window: it is not your everyday profile, it
// lives in a temporary directory Playwright owns, and it is deleted with the
// browser when the recording ends. Nothing of the session touches out/.
//
//   nav    the main frame moved: full navigations and SPA pushState alike
//   load   a page finished loading, with its title
//   api    a request to instagram.com under /api/, /graphql/, /ajax/ or /web/,
//          with request headers, post data, response headers and the body
//   doc    an HTML document, with its body: on a cold load the profile header
//          is embedded in the page rather than fetched
//   other  everything else -- CDN, static assets: method, url, status, type,
//          size, and no body
//
// Secrets are redacted before anything is written: the cookie header keeps
// its cookie *names* and loses the values, and the csrf token, the LSD and
// DTSG tokens and the session id are replaced wherever they appear, in
// headers, post data and bodies. Presence and order are findings; values are
// not. What is left still holds real usernames, so out/ is gitignored, and
// the August 2026 practice is to delete it once the findings are written
// into AGENTS.md.
//
// Needs Node and the `playwright` package. It is resolved from the global
// install when it is not local, so `npm i -g playwright` is enough; the
// browser is the Chrome already on the machine, not Playwright's Chromium,
// because Instagram treats Chrome for Testing differently from Chrome.
//
// Read it back with summarize.js.
'use strict';

const path = require('path');
const fs = require('fs');
const { execSync } = require('child_process');

function playwright() {
  try {
    return require('playwright');
  } catch {
    const globalRoot = execSync('npm root -g', { encoding: 'utf8' }).trim();
    return require(path.join(globalRoot, 'playwright'));
  }
}

const OUT = path.join(__dirname, 'out');
const LOG = path.join(OUT, 'events.jsonl');
const MAX_BODY = 8 * 1024 * 1024;

const SECRET_HEADERS = new Set([
  'cookie', 'set-cookie', 'x-csrftoken', 'x-fb-lsd', 'x-ig-www-claim',
  'authorization', 'x-fb-dtsg',
]);

function redactText(s) {
  if (!s) return s;
  return s
    .replace(/"csrf_token":"[^"]*"/g, '"csrf_token":"<redacted>"')
    .replace(/"token":"[^"]*"/g, '"token":"<redacted>"')
    .replace(/"fb_dtsg":"[^"]*"/g, '"fb_dtsg":"<redacted>"')
    .replace(/sessionid=[^;&"\s]*/g, 'sessionid=<redacted>')
    .replace(/fb_dtsg=[^&"\s]*/g, 'fb_dtsg=<redacted>')
    .replace(/(^|&)lsd=[^&"\s]*/g, '$1lsd=<redacted>');
}

function redactHeaders(headers) {
  const out = {};
  for (const [name, value] of Object.entries(headers || {})) {
    const key = name.toLowerCase();
    if (!SECRET_HEADERS.has(key)) {
      out[name] = value;
    } else if (key === 'cookie') {
      // Which cookies travel is a finding; their values are not.
      out[name] = value.split(';').map(c => c.trim().split('=')[0]).join('; ') + ' <values redacted>';
    } else {
      out[name] = `<redacted:${value.length}>`;
    }
  }
  return out;
}

function isApi(url) {
  try {
    const u = new URL(url);
    return /(^|\.)instagram\.com$/.test(u.hostname) && /^\/(api\/|graphql\/|ajax\/|web\/)/.test(u.pathname);
  } catch {
    return false;
  }
}

async function bodyOf(response) {
  try {
    const buffer = await response.body();
    return buffer.length > MAX_BODY ? `<truncated:${buffer.length}>` : buffer.toString('utf8');
  } catch (e) {
    return `<unavailable:${e.message}>`;
  }
}

(async () => {
  fs.rmSync(OUT, { recursive: true, force: true });
  fs.mkdirSync(OUT, { recursive: true });
  const stream = fs.createWriteStream(LOG, { flags: 'a' });
  let seq = 0;
  const emit = event => {
    event.seq = ++seq;
    event.t = new Date().toISOString();
    stream.write(JSON.stringify(event) + '\n');
  };

  // `launch` rather than a persistent context on purpose: the profile is a
  // temporary directory Playwright creates and removes in `browser.close()`,
  // so there is no profile of ours to delete afterwards -- and a persistent
  // one could not be, because Chrome kept running in the background after
  // its window closed and held the directory open.
  const { chromium } = playwright();
  const browser = await chromium.launch({
    channel: 'chrome',
    headless: false,
    args: ['--start-maximized', '--disable-blink-features=AutomationControlled'],
    ignoreDefaultArgs: ['--enable-automation'],
  });
  const context = await browser.newContext({ viewport: null });

  const counts = { nav: 0, api: 0, doc: 0, other: 0 };
  const tick = () => process.stderr.write(
    `\r[recording] nav ${counts.nav}  api ${counts.api}  doc ${counts.doc}  other ${counts.other}   `);

  function wire(page) {
    page.on('framenavigated', frame => {
      if (frame !== page.mainFrame()) return;
      counts.nav++; tick();
      emit({ kind: 'nav', url: frame.url() });
    });
    page.on('load', async () => {
      let title = '';
      try { title = await page.title(); } catch { /* the page went away */ }
      emit({ kind: 'load', url: page.url(), title });
    });
    page.on('response', async response => {
      const request = response.request();
      const url = request.url();
      const type = request.resourceType();
      const base = { method: request.method(), url, status: response.status(), type, page: page.url() };
      try {
        if (isApi(url)) {
          counts.api++; tick();
          emit({
            kind: 'api', ...base,
            requestHeaders: redactHeaders(await request.allHeaders()),
            postData: redactText(request.postData()),
            responseHeaders: redactHeaders(await response.allHeaders()),
            body: redactText(await bodyOf(response)),
          });
        } else if (type === 'document' && /instagram\.com/.test(url)) {
          counts.doc++; tick();
          emit({
            kind: 'doc', ...base,
            requestHeaders: redactHeaders(await request.allHeaders()),
            responseHeaders: redactHeaders(await response.allHeaders()),
            body: redactText(await bodyOf(response)),
          });
        } else {
          counts.other++; tick();
          let size = null;
          try { size = (await response.body()).length; } catch { /* not kept */ }
          emit({ kind: 'other', ...base, size });
        }
      } catch (e) {
        emit({ kind: 'error', url, message: String(e) });
      }
    });
  }

  // The recording ends when the last tab is gone, whichever tab that is.
  const lastTabClosed = new Promise(resolve => {
    context.on('page', page => {
      wire(page);
      page.on('close', () => {
        if (context.pages().length === 0) resolve();
      });
    });
  });

  const page = await context.newPage();
  await page.goto('https://www.instagram.com/');
  emit({ kind: 'start', note: 'browser open; close its last tab to stop recording' });
  process.stderr.write('Browser is open. Log in, browse, and close the window when done.\n');

  await lastTabClosed;
  emit({ kind: 'end', counts });
  // Closing the browser is what removes its temporary profile, session and
  // all. Playwright kills the process tree if it does not go on its own.
  await browser.close();
  stream.end(() => {
    process.stderr.write(`\nDone: ${JSON.stringify(counts)} -> ${LOG}\n`);
    process.stderr.write('The browser and its profile are gone. Delete out/ once the findings are written down.\n');
    process.exit(0);
  });
})().catch(e => {
  console.error(e && e.stack || e);
  process.exit(1);
});
