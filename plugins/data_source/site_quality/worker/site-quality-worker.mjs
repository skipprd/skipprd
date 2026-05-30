#!/usr/bin/env node
import { createInterface } from 'node:readline';
import { chromium } from 'playwright-core';
import { collectPageMetrics } from './lib/metrics.mjs';

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });

let browser;

async function ensureBrowser() {
  if (!browser) {
    browser = await chromium.launch({
      headless: true,
      executablePath: process.env.PLAYWRIGHT_EXECUTABLE_PATH || undefined,
    });
  }
  return browser;
}

async function runJob(job) {
  const browserInstance = await ensureBrowser();
  const context = await browserInstance.newContext({
    viewport: {
      width: job.viewport.width,
      height: job.viewport.height,
    },
    userAgent: job.user_agent || undefined,
  });
  const page = await context.newPage();
  try {
    const metrics = await collectPageMetrics(page, job);
    const prior = job.prior_checkpoint;
    const unchanged =
      prior &&
      metrics.render_hash &&
      prior.render_hash === metrics.render_hash;
    if (job.skip_heavy_audits || unchanged) {
      if (prior) {
        metrics.lighthouse = {
          performance: prior.lh_performance,
          accessibility: prior.lh_accessibility,
          best_practices: prior.lh_best_practices,
          seo: prior.lh_seo,
          top_failing_audits: [],
        };
      }
      metrics.axe_violations = [];
      metrics.skip_heavy_audits = true;
    }
    return {
      job_id: job.job_id,
      ok: true,
      ...metrics,
      error: null,
    };
  } catch (err) {
    return {
      job_id: job.job_id,
      ok: false,
      error: {
        code: 'NAVIGATION_TIMEOUT',
        message: String(err?.message || err),
      },
    };
  } finally {
    await context.close();
  }
}

let pending = Promise.resolve();

rl.on('line', (line) => {
  pending = pending.then(async () => {
    const trimmed = line.trim();
    if (!trimmed) {
      return;
    }
    let job;
    try {
      job = JSON.parse(trimmed);
    } catch (err) {
      process.stdout.write(
        `${JSON.stringify({
          job_id: 'unknown',
          ok: false,
          error: { code: 'INVALID_JOB', message: String(err) },
        })}\n`,
      );
      return;
    }
    const result = await runJob(job);
    process.stdout.write(`${JSON.stringify(result)}\n`);
  });
});

rl.on('close', async () => {
  await pending;
  if (browser) {
    // Do not block process exit on browser teardown; skipprd waits on child.wait().
    void browser.close().catch(() => {});
  }
  process.exit(0);
});
