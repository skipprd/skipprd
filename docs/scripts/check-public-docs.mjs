#!/usr/bin/env node
import { readFileSync, readdirSync, statSync } from 'node:fs'
import { dirname, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import { engineSections, engineSrcExclude } from '../.vitepress/engine-nav.mjs'

const docsRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
export const DEFAULT_SRC = join(docsRoot, 'docs')
export const DEFAULT_PYI = resolve(docsRoot, '../skippr.pyi')

// Each entry names wording that exposes engine internals instead of telling
// the reader what to do. Real config values are allowed in backticks.
export const BANNED = [
  { pattern: /\bsled\b/i, allowCode: true, why: 'name the local state store; `sled` only as a config value' },
  { pattern: /\bTCP\b/, why: 'plugin transport is internal' },
  { pattern: /manifest-index/i, why: 'plugin registry layout is internal' },
  { pattern: /runtime protocol/i, why: 'plugin protocol is internal' },
  { pattern: /compaction[_ ]ids?\b/i, why: 'compaction identity is internal' },
  { pattern: /host[- ]owned|host process/i, why: 'say "Skipprd", not "the host"' },
  { pattern: /materiali[sz]ed (view|index)/i, why: 'storage internals' },
  { pattern: /\bBallista\b/, why: 'query engine internals' },
  { pattern: /schema sync worker/i, why: 'internal component name' },
  { pattern: /\bcrates?\b/i, why: 'Rust packaging is internal' },
  { pattern: /DATA_(?:INPUT|OUTPUT)_PLUGIN_NAME|DATA_(?:SOURCE|OUTPUT)_[A-Z0-9_]+/, why: 'configure connectors in skippr.yml, not plugin env vars' },
  { pattern: /WAL-visible|replay-safe/i, why: 'internal recovery jargon' },
  { pattern: /\{tenant\}/, why: 'internal storage key layout' },
  { pattern: /metadata\.json/, why: 'internal storage file layout' },
  { pattern: /\bmaintainers\//, why: 'maintainer docs are not public' },
  { pattern: /\b(?:src|plugins)\/[\w/.-]+\.rs\b/, why: 'source paths are not user docs' },
  { pattern: /\bTODO\b|\bTBD\b/, why: 'unfinished page' },
  { pattern: /plugin authors?/i, why: 'plugin authoring is not public' },
  { pattern: /the host stores|the host lost|host-provided/i, why: 'say Skipprd, not the host' },
  { pattern: /\bWAL compactor\b|\bcompactor\b/i, allowCode: true, why: 'internal pipeline stage; quote log lines, do not narrate the component' },
  { pattern: /output plugin|source plugin/i, allowCode: true, why: 'name the connector, not a plugin' },
  { pattern: /ingest worker/i, why: 'internal process name' },
  { pattern: /\bChitchat\b/, why: 'cluster gossip internals' },
  { pattern: /published runtime/i, allowCode: true, why: 'quote the log line; say Skipprd could not download a connector' },
  { pattern: /\bTokio\b/, why: 'async runtime internals; do not ask operators to grep Tokio' },
]

const CONNECTOR_SECTIONS = {
  'connectors/inputs/': ['Before you begin', 'Configure', 'Options', 'What gets synced', 'Troubleshooting'],
  'connectors/outputs/': ['Before you begin', 'Configure', 'Options', 'How data lands', 'Troubleshooting'],
  'connectors/schema_sinks/': ['Before you begin', 'Configure', 'Options', 'Troubleshooting'],
}

const CLASS_PREFIX = {
  'connectors/inputs/': 'DataSource',
  'connectors/outputs/': 'DataSink',
  'connectors/schema_sinks/': 'SchemaSink',
}

export function requiredSections(page) {
  for (const [prefix, sections] of Object.entries(CONNECTOR_SECTIONS)) {
    if (page.startsWith(prefix)) return sections
  }
  if (/^getting-started\/quickstart[\w-]*\.md$/.test(page)) {
    return ['Before you begin', 'Check it worked', 'Troubleshooting', 'Next steps']
  }
  if (page === 'getting-started/install.md') return ['Troubleshooting', 'Next steps']
  if (page === 'python.md') return ['Before you begin', 'Next steps']
  if (page.startsWith('cli/') && page !== 'cli/overview.md') return ['Usage', 'Options', 'Examples']
  if (/^(concepts|cdc|configuration|operations)\//.test(page)) return ['Next steps']
  return []
}

export function stripCode(markdown) {
  return markdown.replace(/^(```|~~~)[^\n]*\n[\s\S]*?^\1[^\n]*$/gm, '')
}

export function parseFrontmatter(markdown) {
  const match = markdown.match(/^---\n([\s\S]*?)\n---\n/)
  if (!match) return null
  const fields = {}
  for (const line of match[1].split('\n')) {
    const kv = line.match(/^([A-Za-z_][\w-]*):\s*(.*)$/)
    if (kv) fields[kv[1]] = kv[2].replace(/^["']|["']$/g, '').trim()
  }
  return fields
}

export function headings(markdown, level) {
  const marker = '#'.repeat(level)
  return stripCode(markdown)
    .split('\n')
    .filter((line) => line.startsWith(`${marker} `))
    .map((line) => line.slice(level + 1).trim())
}

export function parsePyiClasses(pyi) {
  const classes = new Map()
  const blocks = pyi.split(/^class /m).slice(1)
  for (const block of blocks) {
    const name = block.match(/^(\w+)/)[1]
    const sig = block.match(/def __new__\(([\s\S]*?)\)\s*->/)
    if (!sig) continue
    const params = new Set()
    for (const part of sig[1].split(',')) {
      const param = part.trim().match(/^(\w+)\s*:/)
      if (param) params.add(param[1])
    }
    classes.set(name, params)
  }
  return classes
}

export function connectorClassName(page, frontmatter, classes) {
  if (frontmatter?.config_class) return frontmatter.config_class
  const prefix = Object.keys(CLASS_PREFIX).find((p) => page.startsWith(p))
  if (!prefix) return null
  const stem = page.slice(prefix.length).replace(/\.md$/, '').replace(/_/g, '').toLowerCase()
  if (stem === 'index') return null
  const wanted = `${CLASS_PREFIX[prefix]}${stem}`.toLowerCase()
  for (const name of classes.keys()) {
    if (name.toLowerCase() === wanted) return name
  }
  return null
}

export function optionKeys(markdown) {
  const lines = stripCode(markdown).split('\n')
  const keys = []
  let inOptions = false
  let subsection = ''
  for (const line of lines) {
    if (line.startsWith('## ')) {
      inOptions = line.slice(3).trim() === 'Options'
      subsection = ''
      continue
    }
    if (!inOptions) continue
    if (line.startsWith('### ')) {
      subsection = line.slice(4).trim()
      continue
    }
    const row = line.match(/^\|\s*`([^`]+)`/)
    if (row) keys.push({ key: row[1], env: subsection === 'Environment variables' })
  }
  return keys
}

export function checkPage(page, markdown, { classes = new Map(), navLinks = null } = {}) {
  const problems = []
  const add = (message) => problems.push(`${page}: ${message}`)

  const frontmatter = parseFrontmatter(markdown)
  if (!frontmatter?.title) add('frontmatter `title` is required')
  if (!frontmatter?.description) add('frontmatter `description` is required')

  const h1 = headings(markdown, 1)
  if (h1.length !== 1) add(`expected exactly one H1, found ${h1.length}`)

  const h2 = headings(markdown, 2)
  for (const section of requiredSections(page)) {
    if (!h2.includes(section)) add(`missing section "## ${section}"`)
  }

  for (const { pattern, allowCode, why } of BANNED) {
    const text = allowCode ? markdown.replace(/`[^`\n]*`/g, '') : markdown
    const hit = text.match(pattern)
    if (hit) add(`internal wording "${hit[0]}" — ${why}`)
  }

  if (
    Object.keys(CLASS_PREFIX).some((p) => page.startsWith(p)) &&
    !page.endsWith('index.md') &&
    !markdown.includes('::: code-group')
  ) {
    add('Configure must show Python, CLI, and YAML tabs (`::: code-group`)')
  }

  const className = connectorClassName(page, frontmatter, classes)
  if (Object.keys(CLASS_PREFIX).some((p) => page.startsWith(p)) && !page.endsWith('index.md')) {
    const params = className ? classes.get(className) : null
    if (!params) {
      add('no typed config class in skippr.pyi; set frontmatter `config_class`')
    } else {
      for (const { key, env } of optionKeys(markdown)) {
        if (env) {
          if (!/^[A-Z][A-Z0-9_]*$/.test(key)) add(`environment variable row \`${key}\` must be UPPER_SNAKE`)
          continue
        }
        const root = key.split(/[.[]/)[0]
        if (!params.has(root)) add(`option \`${key}\` is not a field of ${className}`)
      }
    }
  }

  if (navLinks && page !== 'index.md') {
    const link = `/${page.replace(/(^|\/)index\.md$/, '$1').replace(/\.md$/, '')}`
    if (!navLinks.has(link)) add(`not linked from the sidebar (${link})`)
  }

  return problems
}

export function flattenNav(sections) {
  const links = new Set()
  const walk = (items) => {
    for (const item of items) {
      if (item.link) links.add(item.link)
      if (item.items) walk(item.items)
    }
  }
  walk(sections)
  return links
}

function isExcluded(page) {
  return engineSrcExclude.some((glob) =>
    glob.endsWith('/**') ? page.startsWith(glob.slice(0, -2)) : page === glob,
  )
}

export function listPublicPages(srcRoot) {
  const pages = []
  const walk = (dir) => {
    for (const entry of readdirSync(dir)) {
      const full = join(dir, entry)
      if (statSync(full).isDirectory()) walk(full)
      else if (entry.endsWith('.md')) {
        const page = relative(srcRoot, full).split(sep).join('/')
        if (!isExcluded(page)) pages.push(page)
      }
    }
  }
  walk(srcRoot)
  return pages.sort()
}

export function checkTree(srcRoot = DEFAULT_SRC, pyiPath = DEFAULT_PYI) {
  const classes = parsePyiClasses(readFileSync(pyiPath, 'utf8'))
  const navLinks = flattenNav(engineSections)
  const pages = listPublicPages(srcRoot)
  const problems = []
  for (const page of pages) {
    problems.push(...checkPage(page, readFileSync(join(srcRoot, page), 'utf8'), { classes, navLinks }))
  }
  const pageLinks = new Set(
    pages.map((p) => `/${p.replace(/(^|\/)index\.md$/, '$1').replace(/\.md$/, '')}`),
  )
  for (const link of navLinks) {
    if (!pageLinks.has(link)) problems.push(`sidebar: ${link} has no page`)
  }
  return problems
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const problems = checkTree()
  for (const problem of problems) console.error(problem)
  if (problems.length) {
    console.error(`\n${problems.length} public docs problem(s)`)
    process.exit(1)
  }
  console.log('public docs: ok')
}
