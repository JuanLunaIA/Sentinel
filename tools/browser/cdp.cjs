#!/usr/bin/env node
/**
 * cdp.cjs — small CDP driver for the debuggable Chrome on :9222.
 *
 * Subcommands:
 *   eval <js>              evaluate JS in the first page, print the result
 *   goto <url> [waitMs]    navigate the first page and wait
 *   text                   print document.body.innerText of the first page
 *   shot <out.png>         screenshot the first page
 *   pages                  list page targets
 *
 * Env: CDP_PORT (default 9222)
 *
 * Uses a raw WebSocket so there is no dependency on Playwright/puppeteer
 * versions (that mismatch is what broke the earlier recorder).
 */
const fs = require('fs');
const http = require('http');

const PORT = process.env.CDP_PORT || '9222';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function getJson(path) {
  return new Promise((resolve, reject) => {
    http.get(`http://127.0.0.1:${PORT}${path}`, (res) => {
      let d = '';
      res.on('data', (c) => (d += c));
      res.on('end', () => { try { resolve(JSON.parse(d)); } catch (e) { reject(e); } });
    }).on('error', reject);
  });
}

async function connect() {
  const list = await getJson('/json/list');
  const page = list.find((t) => t.type === 'page');
  if (!page) throw new Error('no page target');
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const pending = new Map();
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    }
  };
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const i = ++id;
    pending.set(i, { resolve, reject });
    ws.send(JSON.stringify({ id: i, method, params }));
  });
  return { ws, send };
}

(async () => {
  const [cmd, ...rest] = process.argv.slice(2);
  const { send } = await connect();
  await send('Page.enable').catch(() => {});
  await send('Runtime.enable').catch(() => {});

  if (cmd === 'eval') {
    const r = await send('Runtime.evaluate', {
      expression: rest.join(' '), returnByValue: true, awaitPromise: true,
    });
    console.log(JSON.stringify(r.result?.value ?? r.result, null, 2));
  } else if (cmd === 'goto') {
    const url = rest[0];
    await send('Page.navigate', { url });
    await sleep(parseInt(rest[1] || '3000', 10));
    const r = await send('Runtime.evaluate', { expression: 'document.title', returnByValue: true });
    console.log(`title: ${r.result?.value}`);
  } else if (cmd === 'text') {
    const r = await send('Runtime.evaluate', {
      expression: 'document.body ? document.body.innerText.slice(0,6000) : "(no body)"',
      returnByValue: true,
    });
    console.log(r.result?.value);
  } else if (cmd === 'shot') {
    const r = await send('Page.captureScreenshot', { format: 'png' });
    fs.writeFileSync(rest[0] || '/tmp/cdp-shot.png', Buffer.from(r.data, 'base64'));
    console.log(`wrote ${rest[0] || '/tmp/cdp-shot.png'}`);
  } else if (cmd === 'pages') {
    const list = await getJson('/json/list');
    for (const t of list) console.log(`${t.type}\t${(t.title||'').slice(0,50)}\t${(t.url||'').slice(0,90)}`);
  } else {
    console.log('usage: cdp.cjs eval|goto|text|shot|pages');
    process.exit(1);
  }
  process.exit(0);
})().catch((e) => { console.error('FAILED:', e.message); process.exit(1); });
