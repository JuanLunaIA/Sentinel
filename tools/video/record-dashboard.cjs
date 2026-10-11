#!/usr/bin/env node
/**
 * record-dashboard.cjs — records the Sentinel dashboard with Playwright.
 * CommonJS so NODE_PATH resolution finds the globally installed playwright.
 *
 * Usage:
 *   NODE_PATH=$HOME/.npm-global/lib/node_modules \
 *     node record-dashboard.cjs <url> <out-dir> <duration-secs> [scroll]
 */
const { chromium } = require('playwright');

const url = process.argv[2] || 'http://localhost:8081/';
const outDir = process.argv[3] || 'docs/video/recordings';
const durationSecs = parseInt(process.argv[4] || '30', 10);
const doScroll = process.argv[5] === 'scroll';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  // Resolve a usable Chromium: env override, the playwright cache build,
  // then the system Chrome. Avoids a version-pinned download requirement.
  const candidates = [
    process.env.CHROME_BIN,
    `${process.env.HOME}/.cache/ms-playwright/chromium-1243/chrome-linux64/chrome`,
    '/usr/bin/google-chrome-stable',
  ].filter(Boolean);
  const executablePath = candidates.find((p) => require('fs').existsSync(p));
  if (!executablePath) {
    throw new Error('no Chromium binary found; set CHROME_BIN');
  }
  console.log(`[record] using chrome: ${executablePath}`);

  const browser = await chromium.launch({
    headless: true,
    executablePath,
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

  await sleep(4000);
  console.log('[record] dashboard loaded, panels populating...');

  if (doScroll) {
    const height = await page.evaluate(() => document.body.scrollHeight);
    const viewport = 1080;
    const steps = Math.max(1, Math.ceil((height - viewport) / viewport));
    for (let i = 0; i <= steps; i++) {
      const y = i * viewport;
      await page.evaluate((yy) => window.scrollTo({ top: yy, behavior: 'smooth' }), y);
      console.log(`[record] scroll step ${i + 1}/${steps + 1} -> y=${y}`);
      await sleep(Math.max(2500, (durationSecs * 1000) / (steps + 1)));
    }
    await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'smooth' }));
    await sleep(2000);
  } else {
    await sleep(durationSecs * 1000);
  }

  // Finalize the recording by closing the PAGE, then copy the artifact
  // before the context/browser teardown removes the temp file.
  const video = page.video();
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const label = process.argv[6] || 'dashboard';
  const target = `${outDir}/${label}-${stamp}.webm`;
  await page.close();
  await video.saveAs(target);
  await context.close();
  await browser.close();

  console.log(`[record] video saved: ${target}`);
  console.log(`VIDEO_PATH=${target}`);
})().catch((e) => {
  console.error('[record] FAILED:', e.message);
  process.exit(1);
});
