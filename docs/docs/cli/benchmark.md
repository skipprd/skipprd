---
title: skipprd benchmark
description: Generate synthetic files and measure how fast this machine can ingest them.
---

# skipprd benchmark

Measure ingest throughput on this host. The command writes synthetic files, runs them through a pipeline, and prints rows per second. Use it to size a machine, not to test a real source.

## Usage

```bash
skipprd benchmark --num-files <N> --records-per-file <N> --record-size <bytes> [--name <name>] [--description <text>]
```

## Options

| Option | Required | Description |
|---|---|---|
| `--num-files`, `-f` | Yes | How many files to generate |
| `--records-per-file`, `-r` | Yes | Records in each file |
| `--record-size`, `-s` | Yes | Average record size in bytes |
| `--name`, `-n` | No | Label in the report. Default: `baseline` |
| `--description`, `-d` | No | What you are comparing |

## Examples

```bash
skipprd benchmark --num-files 10 --records-per-file 100000 --record-size 512 --name baseline
```

Writes 10 files of 100,000 records (~512 bytes each), ingests them, and prints throughput.

Compare two machines with the same flags and different `--name` values. Keep `DATA_DIR` on the disk you care about measuring.

## Next steps

- [skipprd sync](/cli/sync)
- [WAL and buffering](/configuration/buffering)
- [Logging](/operations/logging)
