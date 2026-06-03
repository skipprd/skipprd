#!/usr/bin/env node
import { createInterface } from 'node:readline';
import { chromium } from 'playwright-core';

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });

let browser;

const PII_PATTERNS = [
  { id: 'email', re: /[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}/ },
  { id: 'phone', re: /\b(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b/ },
  { id: 'ssn_like', re: /\b\d{3}-\d{2}-\d{4}\b/ },
  { id: 'card_like', re: /\b(?:\d[ -]*?){13,19}\b/ },
  { id: 'jwt', re: /\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/ },
];

const SENSITIVE_NAME = /(password|passwd|secret|token|api[_-]?key|auth|session|ssn|credit)/i;

function piiHints(name, value) {
  const hints = new Set();
  if (SENSITIVE_NAME.test(name || '')) {
    hints.add('sensitive_name');
  }
  const sample = String(value || '').slice(0, 512);
  for (const { id, re } of PII_PATTERNS) {
    if (re.test(sample)) {
      hints.add(id);
    }
  }
  return [...hints];
}

function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return '';
  }
}

function isThirdParty(pageHost, resourceUrl) {
  const h = hostOf(resourceUrl);
  if (!h || !pageHost) return false;
  return h !== pageHost && !h.endsWith(`.${pageHost}`) && !pageHost.endsWith(`.${h}`);
}

function headerPresent(headers, name) {
  const target = name.toLowerCase();
  return Object.keys(headers || {}).some((k) => k.toLowerCase() === target);
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

async function collectFromPage(page, job) {
  const response = await page.goto(job.url, {
    waitUntil: job.wait_until || 'load',
    timeout: job.navigation_timeout_ms || 60000,
  });
  const finalUrl = page.url();
  const status = response?.status() ?? 0;
  const headers = response?.headers() || {};

  const pageHost = hostOf(finalUrl);

  const storagePayload = await page.evaluate(() => {
    const cookies = document.cookie
      .split(';')
      .map((part) => part.trim())
      .filter(Boolean)
      .map((part) => {
        const eq = part.indexOf('=');
        const name = eq >= 0 ? part.slice(0, eq).trim() : part;
        const value = eq >= 0 ? part.slice(eq + 1) : '';
        return { name, value_length: value.length, value };
      });

    const readStore = (store) => {
      const out = [];
      for (let i = 0; i < store.length; i += 1) {
        const name = store.key(i);
        if (!name) continue;
        const value = store.getItem(name) || '';
        out.push({ name, value_length: value.length, value });
      }
      return out;
    };

    const scripts = [...document.querySelectorAll('script[src]')].map((el) => ({
      src: el.getAttribute('src') || '',
      async: el.hasAttribute('async'),
      defer: el.hasAttribute('defer'),
    }));

    return {
      cookies,
      local_storage: readStore(localStorage),
      session_storage: readStore(sessionStorage),
      scripts,
    };
  });

  const cookies = storagePayload.cookies.map((c) => ({
    storage_kind: 'cookie',
    entry_name: c.name,
    value_length: c.value_length,
    pii_hints: piiHints(c.name, c.value),
  }));

  const localStorage = storagePayload.local_storage.map((e) => ({
    storage_kind: 'localStorage',
    entry_name: e.name,
    value_length: e.value_length,
    pii_hints: piiHints(e.name, e.value),
  }));

  const sessionStorage = storagePayload.session_storage.map((e) => ({
    storage_kind: 'sessionStorage',
    entry_name: e.name,
    value_length: e.value_length,
    pii_hints: piiHints(e.name, e.value),
  }));

  const scripts = storagePayload.scripts
    .filter((s) => s.src)
    .map((s) => {
      let absolute = s.src;
      try {
        absolute = new URL(s.src, finalUrl).href;
      } catch {
        /* keep relative */
      }
      return {
        script_url: absolute,
        script_host: hostOf(absolute),
        is_third_party: isThirdParty(pageHost, absolute),
        async: s.async,
        defer: s.defer,
      };
    });

  return {
    final_url: finalUrl,
    status,
    headers: {
      has_csp: headerPresent(headers, 'content-security-policy'),
      has_hsts: headerPresent(headers, 'strict-transport-security'),
      has_x_frame_options: headerPresent(headers, 'x-frame-options'),
      has_x_content_type_options: headerPresent(headers, 'x-content-type-options'),
    },
    cookies,
    local_storage: localStorage,
    session_storage: sessionStorage,
    scripts,
    cookie_count: cookies.length,
    local_storage_key_count: localStorage.length,
    session_storage_key_count: sessionStorage.length,
    third_party_script_count: scripts.filter((s) => s.is_third_party).length,
  };
}

async function runJob(job) {
  const browserInstance = await ensureBrowser();
  const context = await browserInstance.newContext({
    viewport: {
      width: job.viewport?.width ?? 1350,
      height: job.viewport?.height ?? 940,
    },
    userAgent: job.user_agent || undefined,
  });
  const page = await context.newPage();
  try {
    const scan = await collectFromPage(page, job);
    return {
      job_id: job.job_id,
      ok: true,
      ...scan,
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
    void browser.close().catch(() => {});
  }
  process.exit(0);
});
