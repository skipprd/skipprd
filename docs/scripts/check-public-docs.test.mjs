import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  checkPage,
  checkTree,
  optionKeys,
  parsePyiClasses,
  requiredSections,
} from './check-public-docs.mjs'

const PYI = `
class DataSinkSnowflake:
    def __new__(
        cls,
        *,
        account: str,
        user: str,
        stage: str | None = None,
    ) -> DataSinkSnowflake:
        ...
`
const classes = parsePyiClasses(PYI)

const outputPage = (body) => `---
title: Snowflake
description: Load data into Snowflake.
---

# Snowflake

## Before you begin

## Configure

::: code-group

tabs here

:::

## Options

${body}

## How data lands

## Troubleshooting
`

test('a page without frontmatter or with two H1s fails', () => {
  const problems = checkPage('concepts/schema.md', '# One\n\n# Two\n\n## Next steps\n')
  assert.ok(problems.some((p) => p.includes('`title`')))
  assert.ok(problems.some((p) => p.includes('`description`')))
  assert.ok(problems.some((p) => p.includes('exactly one H1')))
})

test('headings inside code fences do not count', () => {
  const md = '---\ntitle: t\ndescription: d\n---\n\n# Real\n\n```bash\n# not a heading\n```\n\n## Next steps\n'
  assert.deepEqual(checkPage('concepts/schema.md', md), [])
})

test('required sections follow page type', () => {
  assert.deepEqual(requiredSections('cli/sync.md'), ['Usage', 'Options', 'Examples'])
  assert.deepEqual(requiredSections('cli/overview.md'), [])
  assert.deepEqual(requiredSections('connectors/index.md'), [])
  assert.deepEqual(requiredSections('getting-started/quickstart-postgres.md'), [
    'Before you begin',
    'Check it worked',
    'Troubleshooting',
    'Next steps',
  ])
  const problems = checkPage('cli/sync.md', '---\ntitle: t\ndescription: d\n---\n\n# skipprd sync\n\n## Flags\n')
  assert.ok(problems.some((p) => p.includes('"## Options"')))
})

test('internal wording is rejected; `sled` as a config value is allowed', () => {
  const base = '---\ntitle: t\ndescription: d\n---\n\n# Store\n\n## Next steps\n\n'
  assert.deepEqual(checkPage('configuration/skippr-store.md', `${base}Set \`store.type\` to \`sled\`.\n`), [])
  for (const text of [
    'The host opens sled on startup.',
    'Plugins talk over TCP.',
    'Each write reuses a compaction_id.',
    'Offsets are a materialized view.',
    'Set DATA_OUTPUT_PLUGIN_NAME=Snowflake.',
    'Set DATA_SOURCE_S3_BUCKET.',
    'See maintainers/runtime.md.',
    'Lives at {tenant}/{workspace}.',
    'For plugin authors, see the runtime guide.',
    'The host stores that position after the batch lands.',
    'Receives compacted Parquet from the WAL compactor.',
    'The output plugin selected Athena.',
    'Starting stream pipeline: ingest worker active.',
    'Could not find a published runtime named Snowflake.',
    'A Tokio 1.x context was found.',
  ]) {
    assert.ok(checkPage('configuration/skippr-store.md', base + text).some((p) => p.includes('internal wording')), text)
  }
})

test('connector option rows must be typed config fields', () => {
  const good = outputPage('| Key | Description |\n|---|---|\n| `account` | Account |\n| `stage` | Stage |')
  assert.deepEqual(checkPage('connectors/outputs/snowflake.md', good, { classes }), [])

  const envKeyed = outputPage('| Variable | Description |\n|---|---|\n| `SNOWFLAKE_ACCOUNT` | Account |')
  assert.ok(
    checkPage('connectors/outputs/snowflake.md', envKeyed, { classes }).some((p) =>
      p.includes('`SNOWFLAKE_ACCOUNT` is not a field of DataSinkSnowflake'),
    ),
  )

  const envSection = outputPage(
    '| Key | Description |\n|---|---|\n| `account` | Account |\n\n### Environment variables\n\n| Variable | Description |\n|---|---|\n| `SNOWFLAKE_TIMEOUT` | t |',
  )
  assert.deepEqual(checkPage('connectors/outputs/snowflake.md', envSection, { classes }), [])
})

test('connector pages without a typed class must name one', () => {
  const problems = checkPage('connectors/outputs/mystery.md', outputPage(''), { classes })
  assert.ok(problems.some((p) => p.includes('config_class')))
  const named = outputPage('| Key | Description |\n|---|---|\n| `account` | a |').replace(
    'title: Snowflake',
    'title: Mystery\nconfig_class: DataSinkSnowflake',
  )
  assert.deepEqual(checkPage('connectors/outputs/mystery.md', named, { classes }), [])
})

test('option keys are read only from the Options section', () => {
  const md = '## Configure\n\n| `x` | y |\n\n## Options\n\n| `a` | b |\n\n### Environment variables\n\n| `B_C` | d |\n\n## Troubleshooting\n\n| `oops` | z |\n'
  assert.deepEqual(optionKeys(md), [
    { key: 'a', env: false },
    { key: 'B_C', env: true },
  ])
})

test('pages missing from the sidebar fail', () => {
  const md = '---\ntitle: t\ndescription: d\n---\n\n# T\n\n## Next steps\n'
  const navLinks = new Set(['/concepts/schema'])
  assert.deepEqual(checkPage('concepts/schema.md', md, { navLinks }), [])
  assert.ok(checkPage('concepts/orphan.md', md, { navLinks }).some((p) => p.includes('sidebar')))
})

test('connector Configure must show Python, CLI, and YAML tabs', () => {
  const noTabs = outputPage('| `account` | Account |').replace(
    /::: code-group[\s\S]*?:::\n/,
    '',
  )
  assert.ok(
    checkPage('connectors/outputs/snowflake.md', noTabs, { classes }).some((p) =>
      p.includes('code-group'),
    ),
  )
  assert.deepEqual(checkPage('connectors/outputs/snowflake.md', outputPage('| `account` | Account |'), { classes }), [])
})

test('the public docs tree passes', () => {
  assert.deepEqual(checkTree(), [])
})
