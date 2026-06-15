import { createHash } from 'node:crypto';

const BLOCKED_PATTERNS = [
  /unusual traffic/i,
  /systems have detected/i,
  /not a robot/i,
  /recaptcha/i,
  /consent\.google/i,
  /before you continue/i,
];

export function buildSearchUrl({ keyword, country, language, start }) {
  const params = new URLSearchParams();
  params.set('q', keyword);
  params.set('hl', language || 'en');
  if (country) {
    params.set('gl', country);
  }
  params.set('start', String(start ?? 0));
  params.set('num', '10');
  return `https://www.google.com/search?${params.toString()}`;
}

export function hashSearchUrl(url) {
  return `sha256:${createHash('sha256').update(url).digest('hex')}`;
}

export function detectBlockedPage({ url, title, bodyText }) {
  const haystack = `${url}\n${title}\n${bodyText}`.slice(0, 50_000);
  if (/\/sorry\//i.test(url)) {
    return 'google_sorry_redirect';
  }
  for (const pattern of BLOCKED_PATTERNS) {
    if (pattern.test(haystack)) {
      if (/consent/i.test(pattern.source) || /before you continue/i.test(pattern.source)) {
        return 'consent_wall';
      }
      if (/recaptcha|not a robot/i.test(pattern.source)) {
        return 'captcha';
      }
      return 'unusual_traffic';
    }
  }
  if (bodyText.length < 200 && /google/i.test(title)) {
    return 'empty_or_challenge_page';
  }
  return null;
}

function decodeGoogleRedirect(href) {
  try {
    const parsed = new URL(href, 'https://www.google.com');
    if (parsed.pathname === '/url' || parsed.pathname.startsWith('/url')) {
      const target = parsed.searchParams.get('q') || parsed.searchParams.get('url');
      if (target) {
        return target;
      }
    }
  } catch {
    // ignore
  }
  return href;
}

export function normalizeDomain(input) {
  if (!input) {
    return '';
  }
  let value = input.trim().toLowerCase();
  value = value.replace(/^https?:\/\//, '');
  value = value.split('/')[0] ?? value;
  value = value.split('?')[0] ?? value;
  value = value.split('#')[0] ?? value;
  if (value.startsWith('www.')) {
    value = value.slice(4);
  }
  return value;
}

export function domainMatchesTarget(domain, targets) {
  const normalized = normalizeDomain(domain);
  if (!normalized) {
    return false;
  }
  return targets.some((target) => {
    const t = normalizeDomain(target);
    return normalized === t || normalized.endsWith(`.${t}`);
  });
}

/**
 * Detect SERP feature blocks on the first results page.
 * @param {import('playwright-core').Page} page
 * @param {string[]} targets
 */
export async function extractSerpFeatures(page, targets) {
  return page.evaluate((targetDomains) => {
    const normalize = (input) => {
      if (!input) return '';
      let value = String(input).trim().toLowerCase();
      value = value.replace(/^https?:\/\//, '').split('/')[0] ?? value;
      if (value.startsWith('www.')) value = value.slice(4);
      return value;
    };
    const targetSet = (targetDomains || []).map(normalize).filter(Boolean);
    const ownsDomain = (root) => {
      if (!root || !targetSet.length) return false;
      const links = root.querySelectorAll('a[href]');
      for (const anchor of links) {
        const href = anchor.getAttribute('href') || '';
        const host = normalize(href.includes('://') ? href : `https://${href}`);
        if (targetSet.some((target) => host === target || host.endsWith(`.${target}`))) {
          return true;
        }
      }
      return false;
    };

    const hasAiOverview = Boolean(
      document.querySelector('[data-subtree="aim"], .LGOjhe, [data-attrid="SGE"], .Y3BBE'),
    );
    const hasPaa = Boolean(
      document.querySelector('[jsname="Cpkphb"], .related-question-pair, [data-q]'),
    );
    const hasVideo = Boolean(document.querySelector('video, [data-attrid="kc:/video"], g-scrolling-carousel'));
    const hasSitelinks = Boolean(document.querySelector('.HiHjFd, .usJj9c, table.jmjoTe'));
    const featuredRoot =
      document.querySelector('[data-attrid="wa:/description"]')?.closest('.g, .xpdopen') ||
      document.querySelector('.xpdopen .g');
    const hasFeaturedSnippet = Boolean(featuredRoot);
    const ownsFeaturedSnippet = hasFeaturedSnippet && ownsDomain(featuredRoot);

    return {
      has_ai_overview: hasAiOverview,
      has_paa: hasPaa,
      has_video: hasVideo,
      has_sitelinks: hasSitelinks,
      has_featured_snippet: hasFeaturedSnippet,
      owns_featured_snippet: ownsFeaturedSnippet,
    };
  }, targets);
}

 * @param {import('playwright-core').Page} page
 * @param {number} pageStart - zero-based offset for this SERP page
 */
export async function extractOrganicResults(page, pageStart) {
  const raw = await page.evaluate(() => {
    const results = [];
    const seen = new Set();
    const containers = document.querySelectorAll('#search .g, #rso .g, div[data-sokoban-container]');
    const nodes = containers.length ? containers : document.querySelectorAll('#search a[href]');

    const considerAnchor = (anchor, titleText, snippetText) => {
      const href = anchor.getAttribute('href');
      if (!href || href.startsWith('#') || href.startsWith('javascript:')) {
        return;
      }
      if (
        href.includes('google.com/maps') ||
        href.includes('google.com/search') ||
        href.includes('/aclk?') ||
        href.includes('webcache')
      ) {
        return;
      }
      const key = href;
      if (seen.has(key)) {
        return;
      }
      seen.add(key);
      results.push({
        title: titleText?.trim() || null,
        url: href,
        snippet: snippetText?.trim() || null,
      });
    };

    if (containers.length) {
      containers.forEach((block) => {
        const anchor =
          block.querySelector('a[href^="http"]') ||
          block.querySelector('a[href^="/url"]') ||
          block.querySelector('a[href]');
        if (!anchor) {
          return;
        }
        const title =
          block.querySelector('h3')?.textContent ||
          anchor.textContent ||
          '';
        const snippet =
          block.querySelector('.VwiC3b, .st, .IsZvec')?.textContent || '';
        considerAnchor(anchor, title, snippet);
      });
    } else {
      document.querySelectorAll('#search a[href]').forEach((anchor) => {
        const h3 = anchor.querySelector('h3');
        if (!h3) {
          return;
        }
        considerAnchor(anchor, h3.textContent, '');
      });
    }
    return results;
  });

  return raw.map((row, index) => {
    const resolvedUrl = decodeGoogleRedirect(row.url);
    let domain = '';
    try {
      domain = normalizeDomain(new URL(resolvedUrl).hostname);
    } catch {
      domain = normalizeDomain(resolvedUrl);
    }
    return {
      position: pageStart + index + 1,
      title: row.title,
      url: resolvedUrl,
      domain,
      snippet: row.snippet,
      page_start: pageStart,
    };
  });
}

export function findTargetMatches(organicResults, targets, stopAfterFirst) {
  const matches = [];
  const seenTargets = new Set();

  for (const result of organicResults) {
    for (const target of targets) {
      const normalizedTarget = normalizeDomain(target);
      if (!domainMatchesTarget(result.domain, [target])) {
        continue;
      }
      if (seenTargets.has(normalizedTarget)) {
        continue;
      }
      matches.push({
        target_site: normalizedTarget,
        matched_url: result.url,
        matched_domain: result.domain,
        position: result.position,
        page_start: result.page_start,
        found: true,
      });
      seenTargets.add(normalizedTarget);
      if (stopAfterFirst) {
        return matches;
      }
    }
  }
  return matches;
}

export function absentTargetRows(targets, existingMatches) {
  const found = new Set(existingMatches.map((m) => normalizeDomain(m.target_site)));
  return targets
    .map((t) => normalizeDomain(t))
    .filter((t) => t && !found.has(t))
    .map((target_site) => ({
      target_site,
      matched_url: null,
      matched_domain: null,
      position: null,
      page_start: null,
      found: false,
    }));
}
