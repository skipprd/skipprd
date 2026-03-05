# skippr benchmark

Generate synthetic data and measure ingestion throughput.

## Usage

```bash
skippr benchmark --num-files <N> --records-per-file <N> --record-size <bytes> [--name <name>] [--description <text>]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--num-files, -f` | Yes | Number of files to generate |
| `--records-per-file, -r` | Yes | Number of records per file |
| `--record-size, -s` | Yes | Average record size in bytes |
| `--name, -n` | No | Benchmark name. Default: `baseline` |
| `--description, -d` | No | Description of what's being benchmarked |

## Example

```bash
skippr benchmark --num-files 10 --records-per-file 100000 --record-size 512 --name "baseline"
```

Generates 10 files with 100K records each (~512 bytes per record), ingests them, and reports throughput metrics.
