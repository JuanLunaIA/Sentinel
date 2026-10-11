#!/usr/bin/env node
/**
 * record-dashboard.mjs — records the Sentinel dashboard with Playwright.
 *
 * Usage:
 *   node record-dashboard.mjs <url> <out-dir> <duration-secs> [scroll]
 *
 * Produces a .webm in <out-dir> (Playwright's native recording), which
 * build-videos.sh converts to mp4.
 */
import { chromium } from 'playwright';

const url = process.argv[2] || 'http://localhost:8081/';
const outDir = process.argv[3] || 'docs/video/recordings';
const durationSecs = parseInt(process.argv[4] || '30', 10);
const doScroll = process.argv[5] === 'scroll';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const browser = await chromium.launch({
  headless: true,
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});

const context = await browser.newContext({
  viewport: { width: 1920, height: 1080 },
  deviceScaleFactor: 1,
  recordVideo: { dir: outDir, size: { width: 1920, height: 1080 } },
});

const page = await context.newPage();
console.log(`[record] navigating to ${url}`);
await page.goto(url, { waitUntil: 'networkidle', timeout: 30000 });

// Let the dashboard poll /api/state at least twice so panels populate.
await sleep(4000);
console.log('[record] dashboard loaded, panels populating...');

if (doScroll) {
  // Walk the page top-to-bottom in smooth steps so each panel gets airtime.
  const height = await page.evaluate(() => document.body.scrollHeight);
  const viewport = 1080;
  const steps = Math.max(1, Math.ceil((height - viewport) / viewport));
  for (let i = 0; i <= steps; i++) {
    const y = i * viewport;
    await page.evaluate((yy) => window.scrollTo({ top: yy, behavior: 'smooth' }), y);
    console.log(`[record] scroll step ${i + 1}/${steps + 1} → y=${y}`);
    await sleep(Math.max(2500, (durationSecs * 1000) / (steps + 1)));
  }
  await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'smooth' }));
  await sleep(2000);
} else {
  await sleep(durationSecs * 1000);
}

const video = page.video();
await context.close();
await browser.close();

const path = await video.path();
console.log(`[record] video saved: ${path}`);
console.log(`VIDEO_PATH=${path}`);
