#!/usr/bin/env node
/**
 * cast-to-html.cjs — render an asciinema v2 cast into a self-contained
 * terminal-styled HTML page that types the session out in real time.
 *
 * This avoids any external renderer (agg/vhs): the .cast is parsed here, the
 * output is embedded as escaped text, and the page replays it with a caret
 * and a realistic pace. capture-frames.cjs then records the page.
 *
 * Usage:
 *   node cast-to-html.cjs <in.cast> <out.html> [title]
 */
const fs = require('fs');

const [inPath, outPath, title] = process.argv.slice(2);
if (!inPath || !outPath) {
  console.error('usage: cast-to-html.cjs <in.cast> <out.html> [title]');
  process.exit(1);
}

const raw = fs.readFileSync(inPath, 'utf8');
const lines = raw.split('\n').filter(Boolean);

// First line is the header; the rest are [time, "o"|"i", data] events.
let header = { width: 120, height: 34 };
const events = [];
for (const line of lines) {
  try {
    const v = JSON.parse(line);
    if (v.version) { header = v; continue; }
    if (Array.isArray(v) && v[1] === 'o') events.push({ t: v[0], d: v[2] });
  } catch { /* skip malformed */ }
}

// Reconstruct the full script and per-character timing.
// Strip ANSI color codes (we style the whole terminal uniformly) but keep
// carriage returns as real newlines for the <pre>.
const ANSI = /\u001b\[[0-9;?]*[A-Za-z]/g;
let full = '';
const charTimes = [];
for (const ev of events) {
  const clean = ev.d.replace(ANSI, '').replace(/\r\n/g, '\n').replace(/\r/g, '\n');
  for (const ch of clean) {
    full += ch;
    charTimes.push(ev.t);
  }
}
// Collapse runs of blank lines a little so the video stays snappy.
full = full.replace(/\n{4,}/g, '\n\n\n');
const totalSecs = charTimes.length ? charTimes[charTimes.length - 1] : 5;
// Stretch or compress the typing to a target so a clip reads comfortably.
const TARGET = parseFloat(process.env.TARGET_SECS || '0');
const playSecs = TARGET > 0 ? TARGET : Math.max(6, Math.min(totalSecs, 25));

const escaped = full
  .replace(/&/g, '&amp;')
  .replace(/</g, '&lt;')
  .replace(/>/g, '&gt;');

const html = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>${title || 'Sentinel demo'}</title>
<style>
  :root { --bg:#0b0b10; --fg:#e6e6f0; --dim:#8a8aa3; --accent:#a78bfa; --ok:#4ade80; --warn:#fbbf24; --bad:#f87171; }
  * { box-sizing: border-box; }
  html,body { margin:0; height:100%; background:var(--bg); }
  body { display:flex; align-items:center; justify-content:center; font-family:"JetBrains Mono","Fira Code",Menlo,Consolas,monospace; }
  .win {
    width:100%; height:100%; display:flex; flex-direction:column;
    background:linear-gradient(180deg,#0d0d14 0%,#0a0a0f 100%);
  }
  .bar { display:flex; align-items:center; gap:10px; padding:12px 16px; background:#12121b; border-bottom:1px solid #1e1e2c; }
  .dot { width:12px; height:12px; border-radius:50%; }
  .r{background:#ff5f57}.y{background:#febc2e}.g{background:#28c840}
  .title { margin-left:10px; color:var(--dim); font-size:14px; letter-spacing:.3px; }
  .term { flex:1; padding:22px 26px; overflow:hidden; }
  pre {
    margin:0; color:var(--fg); font-size:19px; line-height:1.5; white-space:pre-wrap; word-break:break-word;
  }
  .cursor { display:inline-block; width:10px; height:20px; background:var(--accent); vertical-align:-3px; animation:blink 1s steps(1) infinite; }
  @keyframes blink { 50%{opacity:0} }
</style>
</head>
<body>
  <div class="win">
    <div class="bar">
      <span class="dot r"></span><span class="dot y"></span><span class="dot g"></span>
      <span class="title">${title || 'Sentinel'} — terminal</span>
    </div>
    <div class="term"><pre id="p"></pre><span class="cursor"></span></div>
  </div>
<script>
  const FULL = ${JSON.stringify(full)};
  const T0 = performance.now();
  const DUR = ${playSecs.toFixed(2)} * 1000;
  const el = document.getElementById('p');
  // Type it out over DUR ms, chunked for smoothness.
  const chars = [...FULL];
  const total = chars.length;
  function tick() {
    const elapsed = performance.now() - T0;
    const frac = Math.min(1, elapsed / DUR);
    const n = Math.floor(frac * total);
    el.textContent = chars.slice(0, n).join('');
    if (frac < 1) requestAnimationFrame(tick);
    else el.textContent = FULL;
  }
  // Expose readiness for the recorder.
  window.__READY__ = true;
  requestAnimationFrame(tick);
</script>
</body>
</html>`;

fs.writeFileSync(outPath, html);
console.log(`[cast] ${inPath} -> ${outPath} (${full.length} chars, ~${totalSecs.toFixed(1)}s)`);
