#!/usr/bin/env node
/**
 * cdp-record.cjs — record any URL to a sequence of JPEG frames using the
 * Chrome DevTools Protocol Page.startScreencast, then hand the frames to
 * ffmpeg. No Playwright/version coupling: talks raw CDP over WebSocket.
 *
 * Usage:
 *   node cdp-record.cjs <url> <out-dir> <duration-secs> [label] [scroll]
 *
 * Env:
 *   CHROME_BIN   chrome/chromium path (default: system google-chrome-stable)
 *   SCREEN_W/H   viewport (default 1600x900)
 *   FPS          output fps cap (default 10)
 */
const { spawn } = require('child_process');
const fs = require('fs');
const http = require('http');
const path = require('path');

const URL_ARG = process.argv[2] || 'http://localhost:8081/';
const OUT_DIR = process.argv[3] || 'docs/video/recordings';
const DURATION = parseInt(process.argv[4] || '20', 10);
const LABEL = process.argv[5] || 'clip';
const DO_SCROLL = process.argv[6] === 'scroll';

const CHROME = process.env.CHROME_BIN || '/usr/bin/google-chrome-stable';
const W = parseInt(process.env.SCREEN_W || '1600', 10);
const H = parseInt(process.env.SCREEN_H || '900', 10);
const FPS = parseInt(process.env.FPS || '10', 10);
const PORT = 9333 + Math.floor(Math.random() * 200);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function getJson(url) {
  return new Promise((resolve, reject) => {
    http.get(url, (res) => {
      let d = '';
      res.on('data', (c) => (d += c));
      res.on('end', () => {
        try { resolve(JSON.parse(d)); } catch (e) { reject(e); }
      });
    }).on('error', reject);
  });
}

(async () => {
  fs.mkdirSync(OUT_DIR, { recursive: true });
  const frameDir = path.join(OUT_DIR, `${LABEL}-frames-${Date.now()}`);
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
  ], { stdio: 'ignore', detached: false });

  // Wait for the DevTools endpoint.
  let version = null;
  for (let i = 0; i < 60; i++) {
    try { version = await getJson(`http://127.0.0.1:${PORT}/json/version`); break; }
    catch { await sleep(250); }
  }
  if (!version) throw new Error('CDP endpoint never came up');

  const wsUrl = version.webSocketDebuggerUrl;
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });

  let msgId = 0;
  const pending = new Map();
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const id = ++msgId;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params, sessionId }));
  });

  const events = [];
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    } else if (m.method) {
      events.push(m);
    }
  };

  // Attach to a fresh page target.
  const { targetId } = await send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await send('Target.attachToTarget', { targetId, flatten: true });

  await send('Page.enable', {}, sessionId);
  await send('Page.navigate', { url: URL_ARG }, sessionId);
  await sleep(3500); // let the page render + first fetch settle

  // Collect screencast frames.
  let frameCount = 0;
  const onFrame = async (m) => {
    if (m.method !== 'Page.screencastFrame' || m.sessionId !== sessionId) return;
    const { data, sessionId: frameSession, metadata } = m.params;
    const file = path.join(frameDir, `f${String(frameCount).padStart(6, '0')}.jpg`);
    fs.writeFileSync(file, Buffer.from(data, 'base64'));
    frameCount++;
    // Ack so the next frame is delivered.
    try {
      await send('Page.screencastFrameAck', { sessionId: frameSession }, sessionId);
    } catch {}
  };
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    onFrame(m).catch(() => {});
  });

  await send('Page.startScreencast', {
    format: 'jpeg', quality: 82, maxWidth: W, maxHeight: H, everyNthFrame: 1,
  }, sessionId);
  console.log(`[cdp] screencast started → ${frameDir}`);

  const started = Date.now();
  const totalMs = DURATION * 1000;
  if (DO_SCROLL) {
    // Smooth scroll over the duration so lower panels get airtime.
    const steps = 6;
    const stepMs = totalMs / (steps + 1);
    for (let i = 1; i <= steps; i++) {
      await sleep(stepMs);
      const expr = `window.scrollTo({top: (document.body.scrollHeight - window.innerHeight) * ${i / steps}, behavior:'smooth'})`;
      await send('Runtime.evaluate', { expression: expr }, sessionId).catch(() => {});
      console.log(`[cdp] scroll ${i}/${steps}`);
    }
    await sleep(stepMs);
  } else {
    await sleep(totalMs);
  }

  await send('Page.stopScreencast', {}, sessionId).catch(() => {});
  console.log(`[cdp] captured ${frameCount} frames in ${((Date.now() - started) / 1000).toFixed(1)}s`);

  try { ws.close(); } catch {}
  try { chrome.kill('SIGKILL'); } catch {}

  // Write a manifest ffmpeg can consume.
  const manifest = path.join(OUT_DIR, `${LABEL}.frames.json`);
  fs.writeFileSync(manifest, JSON.stringify({ frameDir, fps: FPS, count: frameCount }, null, 2));
  console.log(`FRAME_DIR=${frameDir}`);
  console.log(`FRAMES=${frameCount}`);
  console.log(`MANIFEST=${manifest}`);
  process.exit(0);
})().catch((e) => {
  console.error('[cdp] FAILED:', e.message);
  process.exit(1);
});
