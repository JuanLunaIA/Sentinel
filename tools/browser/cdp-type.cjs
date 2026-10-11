#!/usr/bin/env node
/**
 * cdp-type.cjs — type text into a focused element using real CDP input events
 * (Input.insertText + Input.dispatchKeyEvent), which React and friends actually
 * observe. Synthetic DOM events often do not register.
 *
 * Usage:
 *   node cdp-type.cjs <selector> <text> [--enter]
 *
 * Env: CDP_PORT (default 9222)
 */
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

(async () => {
  const args = process.argv.slice(2);
  const selector = args[0];
  const text = args[1];
  const doEnter = args.includes('--enter');
  if (!selector || text === undefined) {
    console.error('usage: cdp-type.cjs <selector> <text> [--enter]');
    process.exit(1);
  }

  const list = await getJson('/json/list');
  const page = list.find((t) => t.type === 'page');
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

  await send('Runtime.enable');
  // Focus the target.
  const r = await send('Runtime.evaluate', {
    expression: `(()=>{const el=document.querySelector(${JSON.stringify(selector)}); if(!el) return 'NOT_FOUND'; el.focus(); el.click&&el.click(); return 'focused';})()`,
    returnByValue: true,
  });
  console.log('focus:', r.result && r.result.value);
  await sleep(400);

  // Real text input.
  await send('Input.insertText', { text });
  await sleep(300);

  if (doEnter) {
    for (const type of ['rawKeyDown', 'char', 'keyUp']) {
      await send('Input.dispatchKeyEvent', {
        type, key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13,
        text: type === 'char' ? '\r' : undefined, unmodifiedText: type === 'char' ? '\r' : undefined,
      });
      await sleep(120);
    }
  }
  const v = await send('Runtime.evaluate', {
    expression: `(()=>{const el=document.querySelector(${JSON.stringify(selector)}); return el?el.value:'(gone)';})()`,
    returnByValue: true,
  });
  console.log('value now:', JSON.stringify(v.result && v.result.value));
  process.exit(0);
})().catch((e) => { console.error('FAILED:', e.message); process.exit(1); });
