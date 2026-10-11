#!/usr/bin/env node
/**
 * text-to-png.cjs — render a block of terminal text (ansi-free) as a
 * terminal-styled PNG, for embedding in docs/articles/*.md.
 *
 * Usage:
 *   node text-to-png.cjs <text-file> <out.png> <title> [width] [height]
 *
 * Env: CHROME_BIN (default system chrome)
 */
const { spawn } = require('child_process');
const fs = require('fs');
const http = require('http');
const path = require('path');

const [textFile, outPng, title] = process.argv.slice(2);
const W = parseInt(process.argv[5] || '1300', 10);
const H = parseInt(process.argv[6] || '620', 10);
const CHROME = process.env.CHROME_BIN || '/usr/bin/google-chrome-stable';
const PORT = 9800 + Math.floor(Math.random() * 150);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function getJson(url) {
  return new Promise((resolve, reject) => {
    http.get(url, (res) => {
      let d = '';
      res.on('data', (c) => (d += c));
      res.on('end', () => { try { resolve(JSON.parse(d)); } catch (e) { reject(e); } });
    }).on('error', reject);
  });
}

(async () => {
  const raw = fs.readFileSync(textFile, 'utf8').replace(/\u001b\[[0-9;?]*[A-Za-z]/g, '');
  const esc = raw.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  const html = `<!doctype html><html><head><meta charset="utf-8"><style>
  *{box-sizing:border-box}html,body{margin:0;background:#0b0b10;height:100%}
  body{display:flex;align-items:center;justify-content:center;font-family:"JetBrains Mono",Menlo,Consolas,monospace}
  .win{width:100%;height:100%;display:flex;flex-direction:column;background:#0d0d14}
  .bar{display:flex;align-items:center;gap:9px;padding:11px 15px;background:#12121b;border-bottom:1px solid #1e1e2c}
  .d{width:11px;height:11px;border-radius:50%}.r{background:#ff5f57}.y{background:#febc2e}.g{background:#28c840}
  .t{margin-left:9px;color:#8a8aa3;font-size:13px}
  .term{flex:1;padding:16px 20px;overflow:hidden}
  pre{margin:0;color:#e6e6f0;font-size:15px;line-height:1.45;white-space:pre-wrap;word-break:break-word}
  </style></head><body><div class="win"><div class="bar">
  <span class="d r"></span><span class="d y"></span><span class="d g"></span>
  <span class="t">${(title || 'Sentinel').replace(/&/g, '&amp;').replace(/</g, '&lt;')}</span>
  </div><div class="term"><pre>${esc}</pre></div></div></body></html>`;
  const tmp = '/tmp/t2p.html';
  fs.writeFileSync(tmp, html);

  const chrome = spawn(CHROME, [
    '--headless=new', `--remote-debugging-port=${PORT}`, `--window-size=${W},${H}`,
    '--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--hide-scrollbars',
    '--force-device-scale-factor=2', 'about:blank',
  ], { stdio: 'ignore' });

  let version = null;
  for (let i = 0; i < 60; i++) {
    try { version = await getJson(`http://127.0.0.1:${PORT}/json/version`); break; }
    catch { await sleep(250); }
  }
  if (!version) throw new Error('CDP never came up');
  const ws = new WebSocket(version.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0; const pending = new Map();
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id); pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    }
  };
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const i = ++id; pending.set(i, { resolve, reject });
    ws.send(JSON.stringify({ id: i, method, params, sessionId }));
  });
  const { targetId } = await send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await send('Target.attachToTarget', { targetId, flatten: true });
  await send('Page.enable', {}, sessionId);
  await send('Emulation.setDeviceMetricsOverride',
    { width: W, height: H, deviceScaleFactor: 2, mobile: false }, sessionId);
  await send('Page.navigate', { url: `file://${tmp}` }, sessionId);
  await sleep(1400);
  const shot = await send('Page.captureScreenshot', { format: 'png' }, sessionId);
  fs.writeFileSync(outPng, Buffer.from(shot.data, 'base64'));
  try { ws.close(); } catch {}
  try { chrome.kill('SIGKILL'); } catch {}
  console.log(`wrote ${outPng} (${fs.statSync(outPng).size} bytes)`);
  process.exit(0);
})().catch((e) => { console.error('FAILED:', e.message); process.exit(1); });
