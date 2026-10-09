---
title: skipprd sql-help
description: List the SQL statements skipprd query accepts, or export that reference to a file.
---

# skipprd sql-help

Print help for the SQL that [`skipprd query`](/cli/query) and [`skipprd serve`](/cli/serve) accept. Use it when you are writing a statement and want the grammar, not a general warehouse tutorial.

## Usage

```bash
skipprd sql-help [--command "<SQL>"] [--output <path>] [--format md|html|json]
```

## Options

| Option | Required | Description |
|---|---|---|
| `--command`, `-c` | No | Help for one statement. Omit to list every statement. |
| `--output`, `-o` | No | Write the reference to a file instead of stdout. |
| `--format`, `-f` | No | `md` (default), `html`, or `json`. |

## Examples

List every statement:

```bash
skipprd sql-help
```

Help for one command:

```bash
skipprd sql-help --command "ENABLE PIPELINE"
```

Write a markdown file you can keep next to a runbook:

```bash
skipprd sql-help --output sql-reference.md --format md
```

## Next steps

- [skipprd query](/cli/query)
- [skipprd serve](/cli/serve)
- [Datalake](/concepts/datalake)
