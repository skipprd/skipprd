#!/usr/bin/env node
import { createInterface } from 'node:readline';
import { chromium } from 'playwright-core';

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });

const CHROMIUM_ARGS = [
  '--no-sandbox',
  '--disable-setuid-sandbox',
  '--disable-dev-shm-usage',
  '--disable-gpu',
  '--disable-gpu-compositing',
  '--disable-software-rasterizer',
  '--no-zygote',
  '--disable-features=IsolateOrigins,site-per-process',
];

async function render(job) {
  let browser;
  let context;
  try {
    browser = await chromium.launch({
      headless: true,
      executablePath: process.env.PLAYWRIGHT_EXECUTABLE_PATH || undefined,
      args: CHROMIUM_ARGS,
      timeout: 120_000,
      handleSIGINT: false,
      handleSIGTERM: false,
    });
    context = await browser.newContext({
      userAgent: job.user_agent || undefined,
      viewport: { width: 1350, height: 940 },
    });
    const page = await context.newPage();
    const waitUntil = job.wait_until || 'load';
    const timeout = job.navigation_timeout_ms || 60_000;
    const response = await page.goto(job.url, { waitUntil, timeout });
    await page.waitForTimeout(1500);
    return {
      job_id: job.job_id,
      ok: true,
      final_url: page.url(),
      status: response?.status?.() || 200,
      html: await page.content(),
      error: null,
    };
  } catch (err) {
    return {
      job_id: job.job_id || 'unknown',
      ok: false,
      error: { code: 'WORKER_ERROR', message: String(err?.message || err) },
    };
  } finally {
    if (context) await context.close().catch(() => {});
    if (browser) await browser.close().catch(() => {});
  }
}

let pending = Promise.resolve();

rl.on('line', (line) => {
  pending = pending.then(async () => {
    const trimmed = line.trim();
    if (!trimmed) return;
    let job;
    try {
      job = JSON.parse(trimmed);
    } catch (err) {
      process.stdout.write(`${JSON.stringify({
        job_id: 'unknown',
        ok: false,
        error: { code: 'INVALID_JOB', message: String(err?.message || err) },
      })}\n`);
      return;
    }
    process.stdout.write(`${JSON.stringify(await render(job))}\n`);
  }).catch((err) => {
    process.stdout.write(`${JSON.stringify({
      job_id: 'unknown',
      ok: false,
      error: { code: 'WORKER_ERROR', message: String(err?.message || err) },
    })}\n`);
  });
});

process.on('SIGTERM', () => process.exit(0));
