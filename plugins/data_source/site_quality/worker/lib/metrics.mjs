import AxeBuilder from '@axe-core/playwright';
import { launch as launchChrome } from 'chrome-launcher';
import lighthouse from 'lighthouse';
import { createHash } from 'node:crypto';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const WEB_VITALS_IIFE = join(__dirname, '../node_modules/web-vitals/dist/web-vitals.iife.js');

const DEFAULT_LIGHTHOUSE_CATEGORIES = ['performance', 'accessibility', 'best-practices', 'seo'];
const DEFAULT_VITALS_SETTLE_MS = 2500;

function resolveLighthouseFormFactor(job) {
  const fromJob = job.lighthouse_form_factor || job.device_profile;
  return fromJob === 'mobile' ? 'mobile' : 'desktop';
}

function shouldCollectMobileHeuristics(job) {
  return job.device_profile === 'mobile';
}

function cleanMetaValue(value) {
  if (typeof value !== 'string') {
    return null;
  }
  const cleaned = value.replace(/\s+/g, ' ').trim();
  return cleaned.length > 0 ? cleaned : null;
}

function absoluteUrl(value, baseUrl) {
  const cleaned = cleanMetaValue(value);
  if (!cleaned) {
    return null;
  }
  try {
    return new URL(cleaned, baseUrl).toString();
  } catch {
    return cleaned;
  }
}

function missingFields(fields) {
  return Object.entries(fields)
    .filter(([, value]) => !value)
    .map(([field]) => field);
}

export function normalizeSocialPreview(raw, pageUrl) {
  const title = cleanMetaValue(raw?.title);
  const description = cleanMetaValue(raw?.description);
  const image = absoluteUrl(raw?.image, pageUrl);
  const url = absoluteUrl(raw?.url, pageUrl);
  const card = cleanMetaValue(raw?.card);
  const cardTitle = cleanMetaValue(raw?.card_title);
  const cardDescription = cleanMetaValue(raw?.card_description);
  const cardImage = absoluteUrl(raw?.card_image, pageUrl);

  const openGraphMissing = missingFields({
    title,
    description,
    image,
    url,
  });
  const cardMissing = missingFields({
    card,
    title: cardTitle || title,
    description: cardDescription || description,
    image: cardImage || image,
  });

  return {
    title,
    description,
    image,
    url,
    card,
    card_title: cardTitle,
    card_description: cardDescription,
    card_image: cardImage,
    title_present: Boolean(title),
    description_present: Boolean(description),
    image_present: Boolean(image),
    url_present: Boolean(url),
    card_present: Boolean(card),
    card_title_present: Boolean(cardTitle || title),
    card_description_present: Boolean(cardDescription || description),
    card_image_present: Boolean(cardImage || image),
    missing_fields: openGraphMissing,
    card_missing_fields: cardMissing,
    complete: openGraphMissing.length === 0 && cardMissing.length === 0,
  };
}

async function collectSocialPreview(page, pageUrl) {
  const raw = await page.evaluate(() => {
    const firstMeta = (names) => {
      const wanted = new Set(names.map((name) => name.toLowerCase()));
      for (const meta of document.querySelectorAll('meta')) {
        const property = meta.getAttribute('property')?.toLowerCase();
        const name = meta.getAttribute('name')?.toLowerCase();
        if (wanted.has(property) || wanted.has(name)) {
          const content = meta.getAttribute('content')?.replace(/\s+/g, ' ').trim();
          if (content) {
            return content;
          }
        }
      }
      return null;
    };

    return {
      title: firstMeta(['og:title']),
      description: firstMeta(['og:description']),
      image: firstMeta(['og:image', 'og:image:url', 'og:image:secure_url']),
      url: firstMeta(['og:url']),
      card: firstMeta(['twitter:card']),
      card_title: firstMeta(['twitter:title']),
      card_description: firstMeta(['twitter:description']),
      card_image: firstMeta(['twitter:image', 'twitter:image:src']),
    };
  });

  return normalizeSocialPreview(raw, pageUrl);
}

async function collectMobileHeuristics(page) {
  return page.evaluate(() => {
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
}

async function installWebVitalsCollection(page) {
  await page.addInitScript({ path: WEB_VITALS_IIFE });
  await page.addInitScript(() => {
    window.__sqVitals = {
      lcp: undefined,
      cls: undefined,
      inp: undefined,
      fcp: undefined,
      ttfb: undefined,
    };
    webVitals.onTTFB((m) => {
      window.__sqVitals.ttfb = m.value;
    });
    webVitals.onFCP((m) => {
      window.__sqVitals.fcp = m.value;
    });
    webVitals.onLCP((m) => {
      window.__sqVitals.lcp = m.value;
    });
    webVitals.onCLS(
      (m) => {
        window.__sqVitals.cls = m.value;
      },
      { reportAllChanges: true },
    );
    webVitals.onINP((m) => {
      window.__sqVitals.inp = m.value;
    });
  });
}

async function collectWebVitals(page, job) {
  const settleMs = job.web_vitals_settle_ms ?? DEFAULT_VITALS_SETTLE_MS;
  await page.waitForTimeout(settleMs);

  if (job.collect_inp !== false) {
    const viewport = page.viewportSize();
    if (viewport) {
      await page.mouse
        .click(Math.floor(viewport.width / 2), Math.floor(viewport.height / 2))
        .catch(() => {});
      await page.waitForTimeout(400);
    }
  }

  return page.evaluate(() => {
    const v = window.__sqVitals || {};
    const round = (n) =>
      typeof n === 'number' && Number.isFinite(n) ? Math.round(n * 1000) / 1000 : undefined;
    return {
      lcp: round(v.lcp),
      cls: round(v.cls ?? 0),
      inp: round(v.inp),
      fcp: round(v.fcp),
      ttfb: round(v.ttfb),
    };
  });
}

export async function collectPageMetrics(page, job) {
  await installWebVitalsCollection(page);

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

  const web_vitals = await collectWebVitals(page, job);

  const mobile_heuristics = shouldCollectMobileHeuristics(job)
    ? await collectMobileHeuristics(page)
    : null;
  const social_preview = await collectSocialPreview(page, finalUrl);

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
    social_preview,
    lighthouse: lighthouseScores,
    axe_violations,
  };
}

async function runLighthouse(url, job) {
  const chrome = await launchChrome({ chromeFlags: ['--headless'] });
  try {
    const formFactor = resolveLighthouseFormFactor(job);
    const width = job.viewport?.width ?? (formFactor === 'mobile' ? 390 : 1350);
    const height = job.viewport?.height ?? (formFactor === 'mobile' ? 844 : 940);
    const options = {
      logLevel: 'error',
      output: 'json',
      onlyCategories:
        job.lighthouse_categories?.length > 0
          ? job.lighthouse_categories
          : DEFAULT_LIGHTHOUSE_CATEGORIES,
      port: chrome.port,
      settings: {
        formFactor,
        screenEmulation: {
          mobile: formFactor === 'mobile',
          width,
          height,
          deviceScaleFactor: formFactor === 'mobile' ? 2 : 1,
          disabled: false,
        },
      },
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
