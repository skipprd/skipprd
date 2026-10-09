# Skipprd docs

Public engineer docs for [skippr.io](https://skippr.io).

Markdown lives in `docs/docs/`. VitePress is the public renderer (`srcDir: docs`). Local preview:

```bash
npm --prefix docs ci
npm --prefix docs run dev
```

The live apex site is composed in sibling `skippr-web` (`npm run docs:compose` copies this tree and `.vitepress/engine-nav.mjs`) and published with `../cloud/scripts/publish-skippr-web.sh`. `https://elt.skippr.io` **301s** to `https://skippr.io`.

## One authority

- `docs/docs/` is the only source for engine docs on skippr.io. Never edit the composed copies in `skippr-web`.
- `.vitepress/engine-nav.mjs` is the only sidebar and public-page list. Both this site and skippr-web import it. Every public page is in it; every link in it has a page.
- `docs/docs/maintainers/` and `docs/docs/query/` are internal and never linked from public pages.
- Connector options come from the typed config in `../skippr.pyi` (generated from the plugin config structs). Do not document a key that is not a field there. Configure connectors in `skippr.yml`, Python, or `skipprd connect`; environment variables appear only as `${NAME}` references for secrets or as documented engine settings.

`npm --prefix docs test` enforces all of the above plus the page shapes below.

## Who we write for

- **Operators** who install, schedule, monitor, and recover Skipprd.
- **Developers and analysts** who connect a source, land data in a warehouse, and query it.

Neither audience needs to know how Skipprd is built. Explain what happens to their data, what they must provide, and what to do when it goes wrong.

## Voice

- Second person, present tense, active voice: "Run `skipprd sync`", not "Sync is run".
- Lead with the task and the outcome. Explain *why* in one sentence when a choice matters.
- Short paragraphs. One idea per sentence. Define a term the first time it appears.
- Every code block is copyable and complete. Show Python, CLI, and YAML tabs (`::: code-group`) for configuration.
- Prefer a numbered list for steps and a table for options.
- Name the product **Skipprd**; the command is `skipprd`.
- Never mention plugin transports, registries, internal databases, compaction identifiers, source file paths, or crate names.

## Page shapes

| Page type | Required `##` sections (in this order) |
|---|---|
| Quickstart (`getting-started/quickstart*.md`) | Before you begin, numbered steps, Check it worked, Troubleshooting, Next steps |
| Install | Install the CLI / Install the Python package, Troubleshooting, Next steps |
| Python | Before you begin, guide sections, Next steps |
| CLI (`cli/*.md`, not overview) | Usage, Options, Examples (then Troubleshooting when useful) |
| Data source (`connectors/inputs/*`) | Before you begin, Configure, Options, What gets synced, Troubleshooting |
| Destination (`connectors/outputs/*`) | Before you begin, Configure, Options, How data lands, Troubleshooting |
| Schema sink (`connectors/schema_sinks/*`) | Before you begin, Configure, Options, Troubleshooting |
| Concept, configuration, operations, CDC | Guide sections, Next steps |

Every page has frontmatter `title` and `description` and exactly one `#` heading.

### Connector Options tables

The first column is the YAML key in backticks. Columns: Key, Type, Required/Default, Description. Nested keys use dots (`privacy.mode`). Engine environment variables that tune the connector go under `### Environment variables` inside `## Options`.

Pages whose config shape is another class (paired schema sinks) set frontmatter `config_class`, for example `config_class: DataSinkDuckdb`.
