import AxeBuilder from '@axe-core/playwright';
import { launch as launchChrome } from 'chrome-launcher';
import lighthouse from 'lighthouse';
import { createHash } from 'node:crypto';

export async function collectPageMetrics(page, job) {
  const response = await page.goto(job.url, {
    waitUntil: job.wait_until || 'load',
    timeout: job.navigation_timeout_ms || 45000,
  });
  const status = response?.status() ?? 0;
  const finalUrl = page.url();

  const timings_ms = await page.evaluate(() => {
    const nav = performance.getEntriesByType('navigation')[0];
    if (!nav) {
      return {};
    }
    return {
      dom_content_loaded: nav.domContentLoadedEventEnd,
      load: nav.loadEventEnd,
      fully_loaded: nav.loadEventEnd,
    };
  });

  const web_vitals = await page.evaluate(() => {
    const nav = performance.getEntriesByType('navigation')[0];
    const lcpEntries = performance.getEntriesByType('largest-contentful-paint');
    const lcp = lcpEntries.length ? lcpEntries[lcpEntries.length - 1].startTime : undefined;
    const paint = performance.getEntriesByType('paint');
    const fcp = paint.find((e) => e.name === 'first-contentful-paint')?.startTime;
    return {
      lcp,
      inp: undefined,
      cls: 0,
      fcp,
      ttfb: nav?.responseStart,
    };
  });

  const mobile_heuristics = await page.evaluate(() => {
    const viewport = document.querySelector('meta[name="viewport"]');
    const viewport_meta_ok = Boolean(
      viewport && /width\s*=/i.test(viewport.getAttribute('content') || ''),
    );
    const horizontal_scroll = document.documentElement.scrollWidth > window.innerWidth;
    const text_too_small_count = [...document.querySelectorAll('body *')]
      .filter((el) => {
        const style = window.getComputedStyle(el);
        const size = parseFloat(style.fontSize || '0');
        return size > 0 && size < 12;
      }).length;
    const tap_target_issues = [...document.querySelectorAll('a, button')]
      .filter((el) => {
        const rect = el.getBoundingClientRect();
        return rect.width > 0 && rect.height > 0 && (rect.width < 48 || rect.height < 48);
      }).length;
    return {
      viewport_meta_ok,
      horizontal_scroll,
      text_too_small_count,
      tap_target_issues,
    };
  });

  const render_hash = await page
    .evaluate(() => {
      const text = document.body?.innerText?.replace(/\s+/g, ' ').trim() || '';
      const landmarks = [...document.querySelectorAll('main, nav, header, footer, h1')]
        .map((el) => el.tagName + (el.textContent || '').slice(0, 80))
        .join('|');
      return text + landmarks;
    })
    .then((payload) => `sha256:${createHash('sha256').update(payload).digest('hex')}`);

  let lighthouseScores = null;
  let axe_violations = [];

  if (job.lighthouse_enabled) {
    lighthouseScores = await runLighthouse(finalUrl, job).catch(() => null);
  }
  if (job.axe_enabled) {
    const axe = await new AxeBuilder({ page })
      .withTags(job.axe_tags || ['wcag2a', 'wcag2aa'])
      .analyze();
    axe_violations = (axe.violations || []).map((v) => ({
      id: v.id,
      impact: v.impact,
      help: v.help,
      nodes: v.nodes?.length ?? 0,
    }));
  }

  return {
    final_url: finalUrl,
    status,
    redirect_count: 0,
    timings_ms,
    web_vitals,
    render_hash,
    mobile_heuristics,
    lighthouse: lighthouseScores,
    axe_violations,
  };
}

async function runLighthouse(url, job) {
  const chrome = await launchChrome({ chromeFlags: ['--headless'] });
  try {
    const options = {
      logLevel: 'error',
      output: 'json',
      onlyCategories: job.lighthouse_categories || ['performance'],
      port: chrome.port,
    };
    const runnerResult = await lighthouse(url, options);
    const cats = runnerResult.lhr.categories;
    const audits = runnerResult.lhr.audits || {};
    const top_failing_audits = Object.values(audits)
      .filter((a) => a.score !== null && a.score < 0.9)
      .slice(0, 5)
      .map((a) => ({
        id: a.id,
        score: a.score,
        display_value: a.displayValue,
      }));
    return {
      performance: (cats.performance?.score ?? 0) * 100,
      accessibility: (cats.accessibility?.score ?? 0) * 100,
      best_practices: (cats['best-practices']?.score ?? 0) * 100,
      seo: (cats.seo?.score ?? 0) * 100,
      top_failing_audits,
    };
  } finally {
    await chrome.kill();
  }
}
