#!/usr/bin/env node
import { createInterface } from 'node:readline';
import { chromium } from 'playwright-core';
import {
  absentTargetRows,
  buildSearchUrl,
  detectBlockedPage,
  extractOrganicResults,
  findTargetMatches,
  hashSearchUrl,
  normalizeDomain,
} from './lib/serp.mjs';

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });

let browser;

function viewportForDevice(device) {
  if (device === 'mobile') {
    return { width: 390, height: 844 };
  }
  return { width: 1350, height: 940 };
}

function userAgentForDevice(device, override) {
  if (override) {
    return override;
  }
  if (device === 'mobile') {
    return 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1';
  }
  return 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36';
}

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
  const targets = (job.targets || []).map((t) => normalizeDomain(t)).filter(Boolean);
  if (!job.keyword || targets.length === 0) {
    return {
      job_id: job.job_id || 'unknown',
      ok: false,
      status: 'error',
      blocked_reason: null,
      organic_results: [],
      target_matches: [],
      results_inspected: 0,
      pages_fetched: 0,
      search_url_hash: null,
      error: { code: 'INVALID_JOB', message: 'keyword and targets are required' },
    };
  }

  const maxDepth = Math.max(1, Math.min(job.max_depth ?? 30, 100));
  const captureResults = Boolean(job.capture_results);
  const stopAfterFirst = job.stop_after_first_target_match !== false;
  const timeout = job.navigation_timeout_ms ?? 45_000;

  const browserInstance = await ensureBrowser();
  const context = await browserInstance.newContext({
    viewport: viewportForDevice(job.device || 'desktop'),
    userAgent: userAgentForDevice(job.device, job.user_agent),
    locale: job.language || 'en-GB',
  });
  const page = await context.newPage();

  const organicResults = [];
  let pagesFetched = 0;
  let blockedReason = null;
  let searchUrlHash = null;

  try {
    for (let start = 0; start < maxDepth && !blockedReason; start += 10) {
      const searchUrl = buildSearchUrl({
        keyword: job.keyword,
        country: job.country,
        language: job.language,
        start,
      });
      if (!searchUrlHash) {
        searchUrlHash = hashSearchUrl(searchUrl);
      }

      await page.goto(searchUrl, {
        waitUntil: 'domcontentloaded',
        timeout,
      });
      pagesFetched += 1;

      const title = await page.title();
      const bodyText = await page.evaluate(() => document.body?.innerText || '');
      blockedReason = detectBlockedPage({
        url: page.url(),
        title,
        bodyText,
      });
      if (blockedReason) {
        break;
      }

      const pageResults = await extractOrganicResults(page, start);
      if (pageResults.length === 0) {
        break;
      }
      organicResults.push(...pageResults);

      const matches = findTargetMatches(organicResults, targets, stopAfterFirst);
      if (stopAfterFirst && matches.length > 0) {
        break;
      }
      if (organicResults.length >= maxDepth) {
        break;
      }
    }

    if (blockedReason) {
      return {
        job_id: job.job_id,
        ok: true,
        status: 'blocked',
        blocked_reason: blockedReason,
        organic_results: captureResults ? organicResults : [],
        target_matches: [],
        results_inspected: organicResults.length,
        pages_fetched: pagesFetched,
        search_url_hash: searchUrlHash,
        error: null,
      };
    }

    let targetMatches = findTargetMatches(organicResults, targets, false);
    targetMatches = targetMatches.concat(absentTargetRows(targets, targetMatches));

    return {
      job_id: job.job_id,
      ok: true,
      status: 'ok',
      blocked_reason: null,
      organic_results: captureResults ? organicResults.slice(0, maxDepth) : [],
      target_matches: targetMatches,
      results_inspected: Math.min(organicResults.length, maxDepth),
      pages_fetched: pagesFetched,
      search_url_hash: searchUrlHash,
      error: null,
    };
  } catch (err) {
    return {
      job_id: job.job_id,
      ok: false,
      status: 'error',
      blocked_reason: null,
      organic_results: captureResults ? organicResults : [],
      target_matches: [],
      results_inspected: organicResults.length,
      pages_fetched: pagesFetched,
      search_url_hash: searchUrlHash,
      error: {
        code: 'NAVIGATION_ERROR',
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
          status: 'error',
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
    void browser.close().catch(() => {});
  }
  process.exit(0);
});
