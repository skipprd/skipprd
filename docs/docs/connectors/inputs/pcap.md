---
title: PCAP
description: Read packet-capture files Skipprd can decode into records. Use it for network traces, not application databases.
---

# PCAP

Reads packet-capture files the Skipprd host can see. There are no extra keys — point the pipeline at this source when you already have capture files in the working environment the connector expects.

## Before you begin

- Capture files available to the Skipprd process.
- This is a specialist source. Prefer [Kafka](/connectors/inputs/kafka), [S3](/connectors/inputs/s3), or a database when you have application data.

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourcePcap, Pipeline

cfg = Config.discover()
src = cfg.data_source("trace", DataSourcePcap())
cfg.pipeline("trace", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source pcap \
  --pipeline trace \
  --name trace
```

```yaml [YAML]
data_sources:
  trace:
    Pcap: {}
```

:::

## Options

This connector has no configuration keys.

## What gets synced

Decoded packets become records. Re-running over the same files can duplicate rows in append-only destinations.

## Troubleshooting

| Symptom | Fix |
|---|---|
| No rows | Confirm capture files are present for the process user |
| Permission denied | Grant read on the capture path |

## Next steps

- [Socket](/connectors/inputs/socket)
- [OTLP](/connectors/inputs/otlp)
