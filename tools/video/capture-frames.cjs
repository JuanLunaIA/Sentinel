#!/usr/bin/env node
/**
 * capture-frames.cjs — deterministic frame capture for a URL.
 *
 * Drives headless Chrome over CDP and calls Page.captureScreenshot on a fixed
 * cadence, so the frame count is exactly duration*fps (smooth, predictable
 * video) instead of depending on repaint events.
 *
 * Usage:
 *   node capture-frames.cjs <url> <out-dir> <duration-secs> [label] [scroll]
 *
 * Env:
 *   CHROME_BIN   chrome/chromium path (default: system google-chrome-stable)
 *   SCREEN_W/H   viewport (default 1600x900)
 *   FPS          capture rate (default 12)
 */
const { spawn } = require('child_process');
const fs = require('fs');
const http = require('http');
const path = require('path');

const URL_ARG = process.argv[2] || 'http://localhost:8081/';
const OUT_DIR = process.argv[3] || 'docs/video/recordings';
const DURATION = parseFloat(process.argv[4] || '20');
const LABEL = process.argv[5] || 'clip';
const DO_SCROLL = process.argv[6] === 'scroll';

const CHROME = process.env.CHROME_BIN || '/usr/bin/google-chrome-stable';
const W = parseInt(process.env.SCREEN_W || '1600', 10);
const H = parseInt(process.env.SCREEN_H || '900', 10);
const FPS = parseInt(process.env.FPS || '12', 10);
const PORT = 9600 + Math.floor(Math.random() * 300);

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
  fs.mkdirSync(OUT_DIR, { recursive: true });
  const frameDir = path.join(OUT_DIR, `${LABEL}-frames`);
  fs.rmSync(frameDir, { recursive: true, force: true });
  fs.mkdirSync(frameDir, { recursive: true });

  const chrome = spawn(CHROME, [
    '--headless=new',
    `--remote-debugging-port=${PORT}`,
    `--window-size=${W},${H}`,
    '--no-sandbox',
    '--disable-dev-shm-usage',
    '--disable-gpu',
    '--hide-scrollbars',
    '--force-device-scale-factor=1',
    'about:blank',
  ], { stdio: 'ignore' });

  let version = null;
  for (let i = 0; i < 80; i++) {
    try { version = await getJson(`http://127.0.0.1:${PORT}/json/version`); break; }
    catch { await sleep(250); }
  }
  if (!version) throw new Error('CDP endpoint never came up');

  const ws = new WebSocket(version.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });

  let msgId = 0;
  const pending = new Map();
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const id = ++msgId;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params, sessionId }));
  });
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    }
  };

  const { targetId } = await send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await send('Target.attachToTarget', { targetId, flatten: true });
  await send('Page.enable', {}, sessionId);
  await send('Emulation.setDeviceMetricsOverride',
    { width: W, height: H, deviceScaleFactor: 1, mobile: false }, sessionId);

  await send('Page.navigate', { url: URL_ARG }, sessionId);
  await sleep(4000); // let it render + first poll settle
  console.log(`[cap] page ready, capturing ${DURATION}s @ ${FPS}fps`);

  const totalFrames = Math.round(DURATION * FPS);
  const interval = 1000 / FPS;
  const scrollExpr = (frac) =>
    `window.scrollTo({top:(document.body.scrollHeight-window.innerHeight)*${frac},behavior:'smooth'})`;

  const t0 = Date.now();
  for (let i = 0; i < totalFrames; i++) {
    const target = t0 + i * interval;
    const now = Date.now();
    if (target > now) await sleep(target - now);

    if (DO_SCROLL) {
      // Ease through 0 → 1 over the clip so panels each get screen time.
      const frac = Math.min(1, i / Math.max(1, totalFrames - 1));
      await send('Runtime.evaluate', { expression: scrollExpr(frac) }, sessionId).catch(() => {});
    }

    const shot = await send('Page.captureScreenshot',
      { format: 'jpeg', quality: 85, captureBeyondViewport: false }, sessionId);
    const file = path.join(frameDir, `f${String(i).padStart(6, '0')}.jpg`);
    fs.writeFileSync(file, Buffer.from(shot.data, 'base64'));

    if (i % FPS === 0) console.log(`[cap] ${i}/${totalFrames} frames (${((Date.now() - t0) / 1000).toFixed(1)}s)`);
  }

  try { ws.close(); } catch {}
  try { chrome.kill('SIGKILL'); } catch {}

  console.log(`[cap] done: ${totalFrames} frames → ${frameDir}`);
  console.log(`FRAME_DIR=${frameDir}`);
  console.log(`FRAMES=${totalFrames}`);
  console.log(`FPS=${FPS}`);
  process.exit(0);
})().catch((e) => {
  console.error('[cap] FAILED:', e.message);
  process.exit(1);
});
