import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import {
  absentTargetRows,
  buildSearchUrl,
  detectBlockedPage,
  domainMatchesTarget,
  findTargetMatches,
  hashSearchUrl,
  normalizeDomain,
} from './serp.mjs';

describe('buildSearchUrl', () => {
  it('includes q, hl, gl, start, and num=10', () => {
    const url = buildSearchUrl({
      keyword: 'widgets',
      country: 'uk',
      language: 'en',
      start: 10,
    });
    assert.match(url, /[?&]q=widgets/);
    assert.match(url, /[?&]hl=en/);
    assert.match(url, /[?&]gl=uk/);
    assert.match(url, /[?&]start=10/);
    assert.match(url, /[?&]num=10/);
  });
});

describe('normalizeDomain', () => {
  it('strips scheme, path, and www', () => {
    assert.equal(normalizeDomain('https://www.Example.com/page'), 'example.com');
  });
});

describe('domainMatchesTarget', () => {
  it('matches exact and subdomain', () => {
    assert.equal(domainMatchesTarget('example.com', ['example.com']), true);
    assert.equal(domainMatchesTarget('blog.example.com', ['example.com']), true);
    assert.equal(domainMatchesTarget('notexample.com', ['example.com']), false);
  });
});

describe('detectBlockedPage', () => {
  it('detects captcha and consent and sorry redirect', () => {
    assert.equal(
      detectBlockedPage({
        url: 'https://www.google.com/search',
        title: 'Google',
        bodyText: 'recaptcha challenge',
      }),
      'captcha',
    );
    assert.equal(
      detectBlockedPage({
        url: 'https://www.google.com/search',
        title: 'Before you continue',
        bodyText: 'consent.google.com preferences',
      }),
      'consent_wall',
    );
    assert.equal(
      detectBlockedPage({
        url: 'https://www.google.com/sorry/index',
        title: 'Sorry',
        bodyText: '',
      }),
      'google_sorry_redirect',
    );
  });

  it('returns null for normal SERP text', () => {
    assert.equal(
      detectBlockedPage({
        url: 'https://www.google.com/search?q=test',
        title: 'widgets - Google Search',
        bodyText:
          'About 1,000,000 results (0.32 seconds). '.repeat(8) +
          'Example listings for widgets with titles, snippets, and URLs.',
      }),
      null,
    );
  });
});

describe('findTargetMatches', () => {
  const organic = [
    {
      position: 1,
      title: 'A',
      url: 'https://competitor.com/',
      domain: 'competitor.com',
      snippet: null,
      page_start: 0,
    },
    {
      position: 2,
      title: 'B',
      url: 'https://www.example.com/page',
      domain: 'example.com',
      snippet: null,
      page_start: 0,
    },
  ];

  it('finds target and can stop after first', () => {
    const matches = findTargetMatches(organic, ['example.com'], true);
    assert.equal(matches.length, 1);
    assert.equal(matches[0].found, true);
    assert.equal(matches[0].position, 2);
  });

  it('absentTargetRows fills not-found targets', () => {
    const found = findTargetMatches(organic, ['example.com'], false);
    const absent = absentTargetRows(['example.com', 'other.com'], found);
    assert.equal(absent.length, 1);
    assert.equal(absent[0].target_site, 'other.com');
    assert.equal(absent[0].found, false);
  });
});

describe('hashSearchUrl', () => {
  it('is stable sha256 prefix', () => {
    const a = hashSearchUrl('https://www.google.com/search?q=a');
    const b = hashSearchUrl('https://www.google.com/search?q=a');
    assert.equal(a, b);
    assert.match(a, /^sha256:[0-9a-f]{64}$/);
  });
});
