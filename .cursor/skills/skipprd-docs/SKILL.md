---
name: skipprd-docs
description: >-
  Write and review skipprd public engine docs. Use when editing skipprd/docs/docs,
  composing skippr.io, publishing skippr-web after a skipprd release, or when
  /skippr-release calls this skill. Surgical edits only: complete and accurate,
  no sweeping rewrites.
---

# skipprd public documentation

SoT for engine how-tos is [docs/docs](../../../docs/docs) in this repo. `/skippr-release` calls this skill. Do not restate Cloud `public-docs`.

## Where to edit

| Surface | Path | Action |
|---------|------|--------|
| Engine how-tos | `skipprd/docs/docs/` (`getting-started`, `cli`, `connectors`, `cdc`, `configuration`, `concepts`, `operations`, `python.md`, `index.md`) | Edit here |
| skippr.io copies | `skippr-web/docs/getting-started`, `cli`, `connectors`, `cdc`, `configuration`, `concepts`, `operations`, `python.md`, `skipprd.md` | MUST NOT hand-edit — `npm run docs:compose` copies them |
| skippr-web-owned | `skippr-web/docs/index.md`, `about.md`, `contact.md`, blog, GTM | Change a drifted version/install pin only |

Compose: sibling [skippr-web/scripts/compose-skipprd-docs.mjs](../../../../skippr-web/scripts/compose-skipprd-docs.mjs). Publish: [cloud/scripts/publish-skippr-web.sh](../../../../cloud/scripts/publish-skippr-web.sh) after the engine tag is on PyPI/GitHub/CDN.

## Surgical rule

Change the drifted fact (version, URL, flag, connector name, one clause). Do not rewrite surrounding sentences, restyle pages, or expand scope. If a page is already accurate for this release, leave it.

After a published tag whose `docs/docs` changed: compose in skippr-web; publish if Objects creds are available. Commit skippr-web only for owned-page edits, not generated copies.
