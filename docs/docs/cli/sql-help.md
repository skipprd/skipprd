# skipprd sql-help

Display documentation for supported SQL statements, or export it to a file.

## Usage

```bash
skipprd sql-help [--command "<SQL>"] [--output <path>] [--format md|html|json]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--command, -c` | No | Show help for a specific SQL command. If omitted, lists all commands. |
| `--output, -o` | No | Write documentation to a file instead of stdout. |
| `--format, -f` | No | Output format: `md` (default), `html`, or `json`. |

## Examples

List all supported SQL commands:

```bash
skipprd sql-help
```

Get help for a specific command:

```bash
skipprd sql-help --command "ENABLE PIPELINE"
```

Export docs to markdown:

```bash
skipprd sql-help --output sql-docs.md --format md
```
