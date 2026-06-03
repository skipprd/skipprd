#!/usr/bin/env node
import { createInterface } from 'node:readline';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { chromium } from 'playwright-core';

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });

// Avoid --single-process: Chromium often crashes on Lambda before first navigation.
const CHROMIUM_ARGS = [
  '--no-sandbox',
  '--disable-setuid-sandbox',
  '--disable-dev-shm-usage',
  '--disable-gpu',
  '--disable-gpu-compositing',
  '--disable-software-rasterizer',
];

const BODY_SNIPPET_MAX = 32_000;
const MIXED_CONTENT_CAP = 30;

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

function headerValue(headers, name) {
  const target = name.toLowerCase();
  for (const [k, v] of Object.entries(headers || {})) {
    if (k.toLowerCase() === target) {
      return v;
    }
  }
  return undefined;
}

function buildResponseHeaders(headers) {
  const csp = headerValue(headers, 'content-security-policy');
  const xfo = headerValue(headers, 'x-frame-options');
  const hasFrameAncestors =
    typeof csp === 'string' && /frame-ancestors/i.test(csp);
  return {
    has_csp: headerPresent(headers, 'content-security-policy'),
    has_hsts: headerPresent(headers, 'strict-transport-security'),
    has_x_frame_options: headerPresent(headers, 'x-frame-options') || hasFrameAncestors,
    has_x_content_type_options: headerPresent(headers, 'x-content-type-options'),
    csp,
    hsts: headerValue(headers, 'strict-transport-security'),
    x_frame_options: xfo,
    referrer_policy: headerValue(headers, 'referrer-policy'),
    permissions_policy:
      headerValue(headers, 'permissions-policy') ||
      headerValue(headers, 'feature-policy'),
    cross_origin_opener_policy: headerValue(headers, 'cross-origin-opener-policy'),
    cross_origin_embedder_policy: headerValue(headers, 'cross-origin-embedder-policy'),
    cross_origin_resource_policy: headerValue(headers, 'cross-origin-resource-policy'),
    x_xss_protection: headerValue(headers, 'x-xss-protection'),
  };
}

async function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function openContext(job) {
  const executablePath = process.env.PLAYWRIGHT_EXECUTABLE_PATH || undefined;
  const profileDir = await mkdtemp(join(tmpdir(), 'site-sec-'));
  const viewport = {
    width: job.viewport?.width ?? 1350,
    height: job.viewport?.height ?? 940,
  };
  let lastError;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    try {
      const context = await chromium.launchPersistentContext(profileDir, {
        headless: true,
        executablePath,
        args: CHROMIUM_ARGS,
        viewport,
        userAgent: job.user_agent || undefined,
        timeout: 120_000,
        handleSIGINT: false,
        handleSIGTERM: false,
      });
      return { context, profileDir };
    } catch (err) {
      lastError = err;
      await rm(profileDir, { recursive: true, force: true }).catch(() => {});
      await sleep(400 * attempt);
    }
  }
  throw lastError;
}

function emitFatal(jobId, err) {
  process.stdout.write(
    `${JSON.stringify({
      job_id: jobId,
      ok: false,
      error: { code: 'WORKER_ERROR', message: String(err?.message || err) },
    })}\n`,
  );
}

process.on('unhandledRejection', (err) => {
  emitFatal('unknown', err);
  process.exit(1);
});

process.on('uncaughtException', (err) => {
  emitFatal('unknown', err);
  process.exit(1);
});

async function collectFromPage(page, context, job) {
  const mixedHttpUrls = [];
  const onRequest = (req) => {
    const u = req.url();
    if (u.startsWith('http://') && mixedHttpUrls.length < MIXED_CONTENT_CAP) {
      mixedHttpUrls.push(u);
    }
  };
  page.on('request', onRequest);

  const response = await page.goto(job.url, {
    waitUntil: job.wait_until || 'load',
    timeout: job.navigation_timeout_ms || 60000,
  });
  page.off('request', onRequest);

  const finalUrl = page.url();
  const status = response?.status() ?? 0;
  const rawHeaders = response?.headers() || {};
  const pageHost = hostOf(finalUrl);
  const isHttps = finalUrl.startsWith('https://');

  const jarCookies = await context.cookies(finalUrl);
  const jarCookieRows = jarCookies.map((c) => ({
    entry_name: c.name,
    domain: c.domain || '',
    path: c.path || '/',
    secure: Boolean(c.secure),
    http_only: Boolean(c.httpOnly),
    same_site: c.sameSite || 'None',
    value_length: (c.value || '').length,
    pii_hints: piiHints(c.name, c.value),
  }));

  const storagePayload = await page.evaluate(() => {
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

    const scriptEls = [...document.querySelectorAll('script')];
    const withSrc = scriptEls.filter((el) => el.getAttribute('src'));
    const inlineScripts = scriptEls.filter((el) => !el.getAttribute('src'));
    const withSri = withSrc.filter((el) => el.getAttribute('integrity'));
    const withoutSri = withSrc.filter((el) => !el.getAttribute('integrity'));

    const scripts = withSrc.map((el) => ({
      src: el.getAttribute('src') || '',
      async: el.hasAttribute('async'),
      defer: el.hasAttribute('defer'),
      has_integrity: Boolean(el.getAttribute('integrity')),
    }));

    const insecureForms = [];
    for (const form of document.querySelectorAll('form[action]')) {
      const action = form.getAttribute('action') || '';
      if (action.startsWith('http://')) {
        insecureForms.push(action);
      }
    }

    return {
      local_storage: readStore(localStorage),
      session_storage: readStore(sessionStorage),
      scripts,
      dom: {
        inline_script_count: inlineScripts.length,
        external_script_count: withSrc.length,
        scripts_with_sri: withSri.length,
        scripts_without_sri: withoutSri.length,
        insecure_form_count: insecureForms.length,
        insecure_form_actions: insecureForms.slice(0, 10),
      },
    };
  });

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
        has_integrity: s.has_integrity,
      };
    });

  const bodyHtml = await page.content();
  const body_snippet = bodyHtml.slice(0, BODY_SNIPPET_MAX);

  const dom = storagePayload.dom;
  const mixed_content_urls = isHttps ? mixedHttpUrls : [];

  return {
    final_url: finalUrl,
    status,
    headers: buildResponseHeaders(rawHeaders),
    jar_cookies: jarCookieRows,
    cookies: jarCookieRows.map((c) => ({
      storage_kind: 'cookie',
      entry_name: c.entry_name,
      value_length: c.value_length,
      pii_hints: c.pii_hints,
    })),
    local_storage: localStorage,
    session_storage: sessionStorage,
    scripts,
    dom,
    mixed_content_urls,
    body_snippet,
    cookie_count: jarCookieRows.length,
    local_storage_key_count: localStorage.length,
    session_storage_key_count: sessionStorage.length,
    third_party_script_count: scripts.filter((s) => s.is_third_party).length,
  };
}

async function runJob(job) {
  let context;
  let profileDir;
  try {
    ({ context, profileDir } = await openContext(job));
    const page = context.pages()[0] ?? (await context.newPage());
    const scan = await collectFromPage(page, context, job);
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
        code: 'WORKER_ERROR',
        message: String(err?.message || err),
      },
    };
  } finally {
    if (context) {
      await context.close().catch(() => {});
    }
    if (profileDir) {
      await rm(profileDir, { recursive: true, force: true }).catch(() => {});
    }
  }
}

let pending = Promise.resolve();

rl.on('line', (line) => {
  pending = pending
    .then(async () => {
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
    })
    .catch((err) => {
      process.stdout.write(
        `${JSON.stringify({
          job_id: 'unknown',
          ok: false,
          error: { code: 'WORKER_ERROR', message: String(err?.message || err) },
        })}\n`,
      );
    });
});

rl.on('close', async () => {
  await pending;
  process.exit(0);
});
